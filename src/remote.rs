//! Remote mode: with `remote = host:port` (or `AIT_REMOTE`, or `--remote`)
//! every command is sent to an `ait serve --lan` host over TLS and its
//! answer is printed exactly as a local run would print it. No local
//! database is opened or created.
//!
//! Files named on the command line (`--body-file`, `--text-file`,
//! `--from-file`, `-` = stdin) are read here and sent along; `--out` files
//! (`brief`, `export`) are written here from the answer. One request per
//! connection, `Connection: close`, hand-written HTTP/1.1.

use crate::args::{self, Args};
use crate::config::Config;
use crate::error::{AitError, Result};
use crate::output::Output;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Options whose file this PC reads and sends.
const FILE_OPTS: &[&str] = &["body-file", "text-file", "from-file"];

/// Checks `host:port` (`[v6]:port` for IPv6 addresses).
pub fn check_target(v: &str) -> std::result::Result<String, String> {
    let v = v.trim();
    let bad = || format!("remote must be host:port, e.g. 192.168.1.10:7778 (not '{v}')");
    let (host, port) = v.rsplit_once(':').ok_or_else(bad)?;
    let port: u16 = port.parse().ok().filter(|p| *p > 0).ok_or_else(bad)?;
    let host = host.trim();
    let inner = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let ok = !inner.is_empty()
        && inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:_".contains(&b));
    if !ok || (inner.contains(':') && inner == host) {
        return Err(bad());
    }
    Ok(format!("{host}:{port}"))
}

/// The host to use: `--remote`, then `AIT_REMOTE`, then `remote =`.
/// `off` turns remote mode off for this call.
pub fn target(flag: Option<String>, cfg: Option<&Config>) -> Result<Option<String>> {
    let pick = |v: &str, from: &str| -> Result<Option<String>> {
        if v.trim() == "off" {
            return Ok(None);
        }
        check_target(v)
            .map(Some)
            .map_err(|e| AitError::usage(format!("{from}: {e}")))
    };
    if let Some(v) = flag {
        return pick(&v, "--remote");
    }
    if let Ok(v) = std::env::var("AIT_REMOTE")
        && !v.trim().is_empty()
    {
        return pick(&v, "AIT_REMOTE");
    }
    Ok(cfg.and_then(|c| c.remote.clone()))
}

fn token(cfg: &Config) -> Result<String> {
    if let Ok(t) = std::env::var("AIT_REMOTE_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let Some(p) = &cfg.remote_token_file else {
        return Err(AitError::new(
            "CONFIG",
            "remote mode needs this PC's client token: set remote_token_file in default.cfg (or AIT_REMOTE_TOKEN); the host makes one with `ait remote add <name>`",
        ));
    };
    let t = std::fs::read_to_string(p).map_err(|e| {
        AitError::new(
            "CONFIG",
            format!("cannot read remote_token_file {}: {e}", p.display()),
        )
    })?;
    let t = t.trim().to_string();
    if t.is_empty() {
        return Err(AitError::new(
            "CONFIG",
            format!("remote_token_file {} is empty", p.display()),
        ));
    }
    Ok(t)
}

/// Reads a file named on the command line, with the same error as a local run.
fn read_local(path: &str) -> Result<String> {
    Args::default().read_file(path)
}

/// Runs one command on the host. `raw` is the whole command line minus the
/// options this PC already used (`--config`, `--remote`, `--as`).
pub fn run(
    cmd: &str,
    raw: Vec<String>,
    a: &Args,
    session: Option<String>,
    cfg: &Config,
    target: &str,
) -> Result<Output> {
    if crate::lan::REFUSED_COMMANDS.contains(&cmd) {
        return Err(AitError::usage(format!(
            "'{cmd}' is not available in remote mode; run it on the host PC"
        )));
    }
    let fingerprint = cfg.remote_fingerprint.clone().ok_or_else(|| {
        AitError::new(
            "CONFIG",
            "remote mode needs remote_fingerprint = sha256:... in default.cfg (ait serve --lan prints it on the host)",
        )
    })?;
    let token = token(cfg)?;
    let mut files = serde_json::Map::new();
    for o in FILE_OPTS {
        if let Some(p) = a.opt(o) {
            files.insert(p.to_string(), json!(read_local(p)?));
        }
    }
    let (raw, out_file) = if cmd == "brief" || cmd == "export" {
        args::strip_opt(&raw, "out")
    } else {
        (raw, None)
    };
    let body = json!({
        "args": raw,
        "session": session,
        "machine": crate::db::local_machine(),
        "client": env!("CARGO_PKG_VERSION"),
        "files": files,
    })
    .to_string();
    let (status, reply) = exchange(cfg, target, &token, &fingerprint, body.as_bytes())?;
    let v: Value = serde_json::from_slice(&reply).unwrap_or(Value::Null);
    let host_msg = || {
        v["error"]["message"]
            .as_str()
            .map(|m| crate::output::line(m, 300))
            .unwrap_or_else(|| format!("HTTP {status}"))
    };
    match status {
        200 => {}
        401 | 403 => {
            return Err(AitError::new(
                "REMOTE_AUTH",
                format!(
                    "the host {target} refused this PC's client token: {}",
                    host_msg()
                ),
            ));
        }
        413 => {
            return Err(AitError::new(
                "INVALID_INPUT",
                "the request is too large for the host (limit 8 MB)",
            ));
        }
        _ => {
            return Err(AitError::new(
                "REMOTE_PROTOCOL",
                format!("unexpected answer from {target}: {}", host_msg()),
            ));
        }
    }
    let (Some(ok), Some(exit)) = (v["ok"].as_bool(), v["exit"].as_i64()) else {
        return Err(AitError::new(
            "REMOTE_PROTOCOL",
            format!("unexpected answer from {target} (is it an ait host of the same API version?)"),
        ));
    };
    if let Some(w) = v["warning"].as_str() {
        let _ = writeln!(std::io::stderr().lock(), "ait: warning: {w}");
    }
    let exit = i32::try_from(exit).unwrap_or(1);
    let json = v["json"].clone();
    let text = v["text"].as_str().unwrap_or_default().to_string();
    if !ok {
        return Ok(Output {
            json,
            text,
            exit,
            is_error: true,
        });
    }
    if let (Some(path), 0) = (out_file, exit) {
        return write_out(cmd, &path, &json);
    }
    Ok(Output {
        json,
        text,
        exit,
        is_error: false,
    })
}

/// `brief --out` / `export --out`: writes the file here and answers as the
/// local command does.
fn write_out(cmd: &str, path: &str, j: &Value) -> Result<Output> {
    let missing = || AitError::new("REMOTE_PROTOCOL", "the host's answer has no file content");
    if cmd == "brief" {
        let md = j["markdown"].as_str().ok_or_else(missing)?;
        let id = j["id"].as_str().unwrap_or_default().to_string();
        std::fs::write(path, md.as_bytes())?;
        return Ok(Output::new(
            json!({"id": id, "out": path}),
            format!("brief for {id} written to {path}"),
        ));
    }
    let text = j["jsonl"].as_str().ok_or_else(missing)?;
    let n = j["records"].clone();
    std::fs::write(path, text.as_bytes())?;
    Ok(Output::new(
        json!({"records": n, "out": path}),
        format!("exported {n} records to {path}"),
    ))
}

fn unreachable(target: &str, why: impl std::fmt::Display) -> AitError {
    AitError::new(
        "REMOTE_UNREACHABLE",
        format!(
            "cannot reach the ait host {target}: {why} (is `ait serve --lan` running there, and is this PC allowed by its lan_allow and firewall?)"
        ),
    )
    .retryable()
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// One TLS request/response. Returns the HTTP status and body.
fn exchange(
    cfg: &Config,
    target: &str,
    token: &str,
    fingerprint: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    let connect_t = Duration::from_millis(cfg.remote_timeout_ms);
    let reply_t = Duration::from_millis(cfg.remote_reply_timeout_ms);
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| AitError::usage("remote must be host:port"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| AitError::usage("remote must be host:port"))?;
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare_host, port)
        .to_socket_addrs()
        .map_err(|e| unreachable(target, format!("cannot look up {bare_host}: {e}")))?
        .collect();
    let mut last = None;
    let mut stream = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, connect_t) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let Some(mut tcp) = stream else {
        let why = last
            .map(|e| {
                if timed_out(&e) {
                    format!("no answer within {} ms", cfg.remote_timeout_ms)
                } else {
                    e.to_string()
                }
            })
            .unwrap_or_else(|| "no address found".to_string());
        return Err(unreachable(target, why));
    };
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(connect_t))?;
    tcp.set_write_timeout(Some(connect_t))?;
    let (tls_cfg, pinned) = crate::tls::client_config(fingerprint)?;
    let name = rustls::pki_types::ServerName::try_from(bare_host.to_string())
        .map_err(|_| AitError::usage(format!("'{bare_host}' is not a valid host name")))?;
    let mut conn = rustls::ClientConnection::new(tls_cfg, name)
        .map_err(|e| AitError::new("REMOTE_TLS", format!("TLS setup failed: {e}")))?;
    while conn.is_handshaking() {
        let r = conn.complete_io(&mut tcp);
        let seen = pinned.seen.lock().ok().and_then(|s| s.clone());
        if let Some(seen) = seen
            && seen != fingerprint
        {
            return Err(AitError::new(
                "REMOTE_TLS",
                format!(
                    "the certificate of {target} does not match remote_fingerprint: expected sha256:{fingerprint}, the host presented sha256:{seen}. If the host made a new certificate, copy its fingerprint from `ait serve --lan`; otherwise something else is answering at that address"
                ),
            ));
        }
        match r {
            Ok((0, 0)) if conn.is_handshaking() => {
                return Err(unreachable(
                    target,
                    "the connection closed during the TLS handshake",
                ));
            }
            Ok(_) => {}
            Err(e) if timed_out(&e) => {
                return Err(unreachable(
                    target,
                    format!("no TLS answer within {} ms", cfg.remote_timeout_ms),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(AitError::new(
                    "REMOTE_TLS",
                    format!("the TLS handshake with {target} failed: {e}"),
                ));
            }
            Err(e) => {
                return Err(unreachable(
                    target,
                    format!("the connection failed during the TLS handshake: {e}"),
                ));
            }
        }
    }
    // From here the command may have reached the host: failures are
    // REMOTE_TIMEOUT (it may or may not have run).
    tcp.set_read_timeout(Some(reply_t))?;
    tcp.set_write_timeout(Some(reply_t))?;
    let lost = |why: String| {
        AitError::new(
            "REMOTE_TIMEOUT",
            format!(
                "sent the command to {target} but {why}; it may or may not have run: check (for example `ait list --claimed --as <you>`) before retrying"
            ),
        )
        .retryable()
    };
    let io_why = |e: &std::io::Error| {
        if timed_out(e) {
            format!("got no answer within {} ms", cfg.remote_reply_timeout_ms)
        } else {
            format!("the connection failed: {e}")
        }
    };
    let mut s = rustls::Stream::new(&mut conn, &mut tcp);
    let head = format!(
        "POST /api/cli HTTP/1.1\r\nHost: {target}\r\nContent-Type: application/json\r\nX-Ait-Client-Token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())
        .and_then(|()| s.write_all(body))
        .and_then(|()| s.flush())
        .map_err(|e| lost(io_why(&e)))?;
    read_response(&mut s).map_err(|e| lost(io_why(&e)))
}

/// Reads the status line, headers and a `Content-Length` body.
fn read_response(s: &mut impl Read) -> std::io::Result<(u16, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let closed = || std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed early");
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16_384 {
            return Err(std::io::Error::other("answer head too large"));
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(closed());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status: u16 = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| std::io::Error::other("bad status line"))?;
    let len: usize = head
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .ok_or_else(|| std::io::Error::other("answer without Content-Length"))?;
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("answer too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(closed());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            check_target("192.168.1.10:7778").unwrap(),
            "192.168.1.10:7778"
        );
        assert_eq!(check_target(" ait-host:7778 ").unwrap(), "ait-host:7778");
        assert_eq!(check_target("[::1]:7778").unwrap(), "[::1]:7778");
        for bad in [
            "host",
            "host:",
            "host:0",
            "host:70000",
            ":7778",
            "::1:7778",
            "a b:1",
            "http://h:1",
        ] {
            assert!(check_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn response_parsing() {
        let r = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra";
        let (st, body) = read_response(&mut &r[..]).unwrap();
        assert_eq!((st, body.as_slice()), (200, &b"{}"[..]));
        let short = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}";
        assert!(read_response(&mut &short[..]).is_err());
        let nolen = b"HTTP/1.1 200 OK\r\n\r\n{}";
        assert!(read_response(&mut &nolen[..]).is_err());
    }
}
