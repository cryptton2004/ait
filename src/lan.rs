//! The host side of the LAN mode: `ait serve --lan` adds a TLS 1.3 listener
//! for other PCs next to the local dashboard. It answers two requests:
//!
//! - `GET /api/version`
//! - `POST /api/cli`: runs one command with the same `dispatch()` as the
//!   command line, authenticated by a per-PC client token.
//!
//! Connections from addresses outside `lan_allow` are closed without a reply.
//! The host never reads or writes files a remote PC names: the client sends
//! file contents along and writes output files itself.

use crate::args::Args;
use crate::clients;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::output::Output;
use serde_json::{Value, json};
use std::fmt;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Host jobs that never run for a remote PC.
pub const REFUSED_COMMANDS: &[&str] = &["serve", "init", "import", "backup", "remote"];

/// Options a remote call may not pass: they would pick another database,
/// config, identity or repository on the host, or write a host file.
pub const REFUSED_OPTS: &[&str] = &["db", "config", "as", "remote", "out", "repo"];

const MAX_CLI_BODY: usize = 8 * 1024 * 1024;
const MAX_CONNS: usize = 64;
static CONNS: AtomicUsize = AtomicUsize::new(0);

/// An address range such as `192.168.1.0/24` (a bare address is one host).
#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(v: &str) -> std::result::Result<Cidr, String> {
        let bad = || format!("'{v}' is not an address range such as 192.168.1.0/24");
        let (addr, bits) = match v.trim().split_once('/') {
            Some((a, b)) => (a.trim(), Some(b.trim())),
            None => (v.trim(), None),
        };
        let net: IpAddr = addr.parse().map_err(|_| bad())?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = match bits {
            None => max,
            Some(b) => b.parse::<u8>().ok().filter(|b| *b <= max).ok_or_else(bad)?,
        };
        Ok(Cidr { net, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.bits)).unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.bits))
                    .unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.net, self.bits)
    }
}

/// IPv4 addresses that arrive as IPv6 (`::ffff:a.b.c.d`) count as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// Private ranges and loopback; never public addresses.
pub fn default_allow() -> Vec<Cidr> {
    let v4 = |a, b, c, d, bits| Cidr {
        net: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        bits,
    };
    vec![
        v4(10, 0, 0, 0, 8),
        v4(172, 16, 0, 0, 12),
        v4(192, 168, 0, 0, 16),
        v4(127, 0, 0, 0, 8),
        Cidr {
            net: IpAddr::V6(Ipv6Addr::LOCALHOST),
            bits: 128,
        },
        // IPv6 unique-local and link-local addresses.
        Cidr {
            net: IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0)),
            bits: 7,
        },
        Cidr {
            net: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)),
            bits: 10,
        },
    ]
}

pub fn allowed(list: &[Cidr], ip: IpAddr) -> bool {
    list.iter().any(|c| c.contains(ip))
}

/// A bound, ready LAN listener.
pub struct Lan {
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    allow: Vec<Cidr>,
    pub addr: std::net::SocketAddr,
    pub fingerprint: String,
}

impl Lan {
    pub fn allow_text(&self) -> String {
        self.allow
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Reads the certificate and binds the listener (before anything is
/// printed, so a missing certificate stops `ait serve --lan` at once).
pub fn prepare(env: &Env, a: &Args, dashboard_port: u16) -> Result<Lan> {
    let cfg = &env.cfg;
    let bind: IpAddr = match a.opt("bind") {
        Some(b) => b
            .parse()
            .map_err(|_| AitError::usage("--bind must be an IP address such as 0.0.0.0"))?,
        None => cfg.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    };
    let port = match a.opt_i64("lan-port")? {
        Some(p) => u16::try_from(p).map_err(|_| AitError::usage("--lan-port must be 0..65535"))?,
        None => match cfg.lan_port {
            Some(p) => p,
            // Port 0 (any free port) stays 0; otherwise the next port.
            None if dashboard_port == 0 => 0,
            None => dashboard_port.checked_add(1).ok_or_else(|| {
                AitError::new("CONFIG", "set lan_port: the dashboard port is 65535")
            })?,
        },
    };
    let tls_dir = env
        .db_path
        .parent()
        .map(|d| d.join("tls"))
        .unwrap_or_else(|| PathBuf::from("tls"));
    let cert = cfg
        .tls_cert
        .clone()
        .unwrap_or_else(|| tls_dir.join("cert.pem"));
    let key = cfg
        .tls_key
        .clone()
        .unwrap_or_else(|| tls_dir.join("key.pem"));
    let (tls, fingerprint) = crate::tls::server_config(&cert, &key)?;
    let listener = TcpListener::bind((bind, port)).map_err(|e| {
        AitError::new(
            "IO_ERROR",
            format!("cannot listen on {bind}:{port} for the LAN: {e}"),
        )
    })?;
    let addr = listener.local_addr()?;
    Ok(Lan {
        listener,
        tls,
        allow: cfg.lan_allow.clone().unwrap_or_else(default_allow),
        addr,
        fingerprint,
    })
}

struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accept loop; one short-lived thread per connection.
pub fn run(lan: Lan, base: Env) {
    let base = Arc::new(base);
    for stream in lan.listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ok_peer = stream
            .peer_addr()
            .is_ok_and(|p| allowed(&lan.allow, p.ip()));
        if !ok_peer {
            // Closed without a reply.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            continue;
        }
        if CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            CONNS.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            continue;
        }
        let slot = Slot;
        let tls = Arc::clone(&lan.tls);
        let base = Arc::clone(&base);
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = handle(stream, tls, &base);
        });
    }
}

fn handle(stream: TcpStream, tls: Arc<rustls::ServerConfig>, base: &Env) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let conn = rustls::ServerConnection::new(tls).map_err(std::io::Error::other)?;
    let mut s = rustls::StreamOwned::new(conn, stream);
    let (status, body) = match crate::serve::read_request(&mut s, MAX_CLI_BODY) {
        Ok(Ok(req)) => route(&req, base),
        Ok(Err(m)) => (
            if m.contains("too large") {
                "413 Payload Too Large"
            } else {
                "400 Bad Request"
            },
            error_json("BAD_REQUEST", &m),
        ),
        // A failed handshake or a dropped connection: nothing to answer.
        Err(_) => return Ok(()),
    };
    let text = body.to_string();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        text.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(text.as_bytes())?;
    s.conn.send_close_notify();
    s.flush()
}

fn error_json(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message, "retryable": false}})
}

fn route(req: &crate::serve::Request, base: &Env) -> (&'static str, Value) {
    let path = req.target.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("GET", "/api/version") => (
            "200 OK",
            json!({
                "api": crate::serve::API_VERSION,
                "ait": env!("CARGO_PKG_VERSION"),
                "schema": db::schema_version(),
                "lan": true,
            }),
        ),
        ("POST", "/api/cli") => cli(req, base),
        (_, "/api/version" | "/api/cli") => (
            "405 Method Not Allowed",
            error_json("METHOD", "GET /api/version or POST /api/cli"),
        ),
        _ => (
            "404 Not Found",
            error_json(
                "NOT_FOUND",
                "the LAN port serves only /api/cli and /api/version",
            ),
        ),
    }
}

/// The body of `POST /api/cli`.
struct CliRequest {
    args: Vec<String>,
    session: Option<String>,
    machine: Option<String>,
    client: Option<String>,
    files: Vec<(String, String)>,
}

fn parse_body(body: &[u8]) -> std::result::Result<CliRequest, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let shape = "body must be {\"args\": [\"command\", ...], \"session\", \"machine\", \"client\", \"files\"}";
    let args = match v.get("args").and_then(Value::as_array) {
        Some(a) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => return Err(shape.to_string()),
    };
    let text = |k: &str| -> std::result::Result<Option<String>, String> {
        match v.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("\"{k}\" must be a string or null")),
        }
    };
    let files = match v.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(m)) => {
            let mut out = Vec::new();
            for (k, x) in m {
                let t = x
                    .as_str()
                    .ok_or_else(|| "\"files\" must map paths to text".to_string())?;
                out.push((k.clone(), t.to_string()));
            }
            out
        }
        Some(_) => return Err("\"files\" must map paths to text".to_string()),
    };
    Ok(CliRequest {
        args,
        session: text("session")?,
        machine: text("machine")?,
        client: text("client")?,
        files,
    })
}

fn cli(req: &crate::serve::Request, base: &Env) -> (&'static str, Value) {
    let conn = match db::open(&base.db_path) {
        Ok(c) => c,
        Err(e) => return ("500 Internal Server Error", e.to_json()),
    };
    let token = req.header("x-ait-client-token").unwrap_or("");
    let client_name = match clients::authenticate(&conn, token) {
        Ok(n) => n,
        Err((revoked, e)) => {
            let status = if revoked {
                "403 Forbidden"
            } else {
                "401 Unauthorized"
            };
            return (status, e.to_json());
        }
    };
    let ctype = req.header("content-type").unwrap_or("");
    if !ctype.to_ascii_lowercase().starts_with("application/json") {
        return (
            "415 Unsupported Media Type",
            error_json("BAD_REQUEST", "send application/json"),
        );
    }
    let body = match parse_body(&req.body) {
        Ok(b) => b,
        Err(m) => return ("400 Bad Request", error_json("BAD_REQUEST", &m)),
    };
    let client_version = body.client.clone();
    let mut reply = match run_cli(&conn, base, body, &client_name) {
        Ok(out) => json!({"ok": true, "exit": out.exit, "json": out.json, "text": out.text}),
        Err(e) => json!({
            "ok": false,
            "exit": e.exit_code(),
            "json": e.to_json(),
            "text": e.text_line(),
        }),
    };
    let host = env!("CARGO_PKG_VERSION");
    if let Some(cv) = client_version
        && cv != host
        && let Some(o) = reply.as_object_mut()
    {
        o.insert(
            "warning".to_string(),
            json!(format!(
                "ait versions differ: this PC has {}, the host has {host}; update the older one",
                crate::output::line(&cv, 40)
            )),
        );
    }
    ("200 OK", reply)
}

fn run_cli(
    conn: &rusqlite::Connection,
    base: &Env,
    body: CliRequest,
    client_name: &str,
) -> Result<Output> {
    let mut a = Args::parse(body.args)?;
    for o in REFUSED_OPTS {
        if a.flag(o) {
            return Err(AitError::usage(format!(
                "--{o} is not available from another PC (the host decides it)"
            )));
        }
    }
    let Some(cmd) = a.shift() else {
        return Ok(crate::guide::usage());
    };
    if cmd == "help" || a.flag("help") {
        return Ok(crate::guide::usage());
    }
    if cmd == "guide" {
        return Ok(crate::guide::guide());
    }
    if REFUSED_COMMANDS.contains(&cmd.as_str()) {
        return Err(AitError::usage(format!(
            "'{cmd}' is not available from another PC; run it on the host"
        )));
    }
    let session = body
        .session
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(s) = &session
        && !db::valid_session_name(s)
    {
        return Err(AitError::usage(format!(
            "session name '{s}' must be 1-64 characters of letters, digits and . _ : @ -"
        )));
    }
    let machine = body
        .machine
        .map(|m| crate::output::line(m.trim(), 64))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| client_name.to_string());
    a.inline_files = Some(body.files);
    let env = Env {
        now: crate::time::now_ms()?,
        session,
        cfg: base.cfg.clone(),
        db_path: base.db_path.clone(),
        machine: Some(machine),
    };
    db::touch_session(conn, &env)?;
    // Same as a local call: a failed automatic backup never blocks work.
    let _ = crate::backup::auto(conn, &env);
    crate::dispatch(&cmd, conn, &env, &a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_allow_is_private_ranges_and_loopback_only() {
        let d = default_allow();
        for ok in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "127.0.0.1",
            "::1",
            "::ffff:192.168.1.20",
            "fd12:3456::1",
            "fe80::1",
        ] {
            assert!(allowed(&d, ip(ok)), "{ok} should be allowed");
        }
        for bad in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "192.169.0.1",
            "11.0.0.1",
            "::ffff:8.8.8.8",
            "2001:db8::1",
            "0.0.0.0",
        ] {
            assert!(!allowed(&d, ip(bad)), "{bad} should be refused");
        }
    }

    #[test]
    fn configured_ranges() {
        let one = [Cidr::parse("192.168.1.0/24").unwrap()];
        assert!(allowed(&one, ip("192.168.1.255")));
        assert!(!allowed(&one, ip("192.168.2.1")));
        assert!(!allowed(&one, ip("127.0.0.1")));
        let host = [Cidr::parse("10.0.0.7").unwrap()];
        assert!(allowed(&host, ip("10.0.0.7")));
        assert!(!allowed(&host, ip("10.0.0.8")));
        let all = [Cidr::parse("0.0.0.0/0").unwrap()];
        assert!(allowed(&all, ip("8.8.8.8")));
        assert!(!allowed(&all, ip("2001:db8::1")), "v4 range, v6 address");
        assert_eq!(
            Cidr::parse(" 10.0.0.0/8 ").unwrap().to_string(),
            "10.0.0.0/8"
        );
        for bad in ["10.0.0.0/33", "::/129", "host/8", "10.0.0/8", ""] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cli_body_shape() {
        let b = parse_body(
            br#"{"args":["list"],"session":"pc2-a1","machine":"PC2","client":"0.1.0","files":{"b.md":"x"}}"#,
        )
        .unwrap();
        assert_eq!(b.args, vec!["list"]);
        assert_eq!(b.session.as_deref(), Some("pc2-a1"));
        assert_eq!(b.files, vec![("b.md".to_string(), "x".to_string())]);
        assert!(parse_body(br#"{"args":"list"}"#).is_err());
        assert!(parse_body(br#"{"args":["list"],"session":5}"#).is_err());
        assert!(parse_body(br#"{"args":["list"],"files":{"a":1}}"#).is_err());
        assert!(parse_body(b"not json").is_err());
    }
}
