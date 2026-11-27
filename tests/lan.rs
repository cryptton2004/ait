//! LAN mode (phase 1): `ait serve --lan` on the host, remote mode on other
//! PCs. Everything binds to 127.0.0.1, which stands in for the LAN. The
//! certificate is the TEST-ONLY pair in tests/fixtures.

mod common;
use common::*;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// SHA-256 of tests/fixtures/test-only-cert.pem.
const FP: &str = "5ce167cbe56f133d87a5513e8c77ca54866a7cec26750d7c49b327a4077080dc";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Kills the server when the test ends, pass or fail.
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Host {
    _server: Server,
    lan_port: u16,
    dash_port: u16,
    fingerprint: String,
}

/// Puts the test certificate where `ait serve --lan` looks by default
/// (`<db folder>/tls/`).
fn install_cert(t: &T) {
    let dir = t.path("tls");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("test-only-cert.pem"), dir.join("cert.pem")).unwrap();
    std::fs::copy(fixture("test-only-key.pem"), dir.join("key.pem")).unwrap();
}

/// Starts `ait serve --port 0 --lan --bind 127.0.0.1` (plus `extra`) on
/// `t`'s database and waits for its three start-up lines.
fn start_host(t: &T, extra: &[&str], now: Option<i64>) -> Host {
    install_cert(t);
    let mut c = t.cmd(
        None,
        &["serve", "--port", "0", "--lan", "--bind", "127.0.0.1"],
    );
    c.args(extra);
    if let Some(n) = now {
        c.env("AIT_NOW_MS", n.to_string());
    }
    let mut server = Server(t.spawn(c));
    let stdout = server.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut lines = Vec::new();
    for _ in 0..3 {
        lines.push(
            rx.recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("server did not start; got {lines:?}")),
        );
    }
    let dash_port: u16 = lines[0]
        .split("127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no dashboard port in {lines:?}"));
    let lan_port: u16 = lines[1]
        .split("listening on 127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split(' ').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no LAN port in {lines:?}"));
    let fingerprint = lines[2]
        .split("fingerprint ")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    Host {
        _server: server,
        lan_port,
        dash_port,
        fingerprint,
    }
}

/// A token for a new client, made on the host (local mode).
fn add_client(t: &T, name: &str) -> String {
    t.ok(None, &["remote", "add", name])["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// An "agent PC": a folder with its own default.cfg and token file.
struct Pc {
    dir: PathBuf,
}

impl Pc {
    fn new(t: &T, name: &str, port: u16, token: &str, extra_cfg: &str) -> Pc {
        let dir = t.path(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pc.token"), format!("{token}\n")).unwrap();
        std::fs::write(
            dir.join("default.cfg"),
            format!(
                "remote = 127.0.0.1:{port}\nremote_token_file = pc.token\nremote_fingerprint = sha256:{FP}\nremote_timeout_ms = 3000\n{extra_cfg}"
            ),
        )
        .unwrap();
        Pc { dir }
    }

    /// A remote-mode `ait` call. `AIT_DB` points at a canary file that
    /// must never be created.
    fn cmd(&self, session: Option<&str>, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.dir)
            .env_remove("AIT_SESSION")
            .env_remove("AIT_CONFIG")
            .env_remove("AIT_REMOTE")
            .env_remove("AIT_REMOTE_TOKEN")
            .env_remove("AIT_NOW_MS")
            .env("AIT_DB", self.dir.join("canary.db"))
            .env("COMPUTERNAME", "PC2-TEST")
            .env("HOSTNAME", "PC2-TEST")
            .arg("--config")
            .arg(self.dir.join("default.cfg"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(s) = session {
            c.env("AIT_SESSION", s);
        }
        c
    }

    fn run(&self, session: Option<&str>, args: &[&str]) -> Out {
        wait(self.cmd(session, args).spawn().unwrap())
    }

    fn json(&self, session: Option<&str>, args: &[&str]) -> Out {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        self.run(session, &a)
    }

    fn ok(&self, session: Option<&str>, args: &[&str]) -> Value {
        let o = self.json(session, args);
        assert_eq!(o.code, 0, "remote {args:?}: {}{}", o.stdout, o.stderr);
        o.json
    }

    /// Files in the PC folder other than its config and token.
    fn extra_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "default.cfg" && n != "pc.token")
            .collect();
        v.sort();
        v
    }
}

#[test]
fn remote_mode_runs_commands_on_the_host_and_never_opens_a_local_db() {
    let t = T::new("lan_basic");
    t.ok(None, &["add", "CORE", "first"]);
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    assert_eq!(host.fingerprint, format!("sha256:{FP}"));
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");

    let v = pc.ok(Some("pc2-a1"), &["list"]);
    assert_eq!(v["issues"][0]["id"], "CORE-1-00");
    let v = pc.ok(Some("pc2-a1"), &["claim", "CORE-1-00"]);
    assert_eq!(v["claim"]["session"], "pc2-a1", "{v}");
    // The host's database holds the claim, with this PC's name.
    let v = t.ok(None, &["show", "CORE-1-00"]);
    assert_eq!(v["claim"]["session"], "pc2-a1", "{v}");
    let db = rusqlite::Connection::open(t.db()).unwrap();
    let machine: String = db
        .query_row(
            "SELECT machine FROM sessions WHERE name = 'pc2-a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(machine, "PC2-TEST");
    // Text mode prints the host's text.
    let o = pc.run(Some("pc2-a1"), &["show", "CORE-1-00"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("first"), "{}", o.stdout);

    // version tells the mode; the remote one needs no host round trip.
    let v = pc.ok(None, &["version"]);
    assert_eq!(v["mode"], "remote");
    assert_eq!(v["remote"], format!("127.0.0.1:{}", host.lan_port));
    assert_eq!(t.ok(None, &["version"])["mode"], "local");

    // The local dashboard is unchanged and reports the LAN listener.
    let mut s = TcpStream::connect(("127.0.0.1", host.dash_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /api/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        host.dash_port
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("\"lan\":true"), "{out}");

    assert_eq!(pc.extra_files(), Vec::<String>::new(), "no local files");
}

/// Runs `args` locally on `db` (the clock fixed at `T0`).
fn local(t: &T, db: &Path, session: Option<&str>, args: &[&str]) -> Out {
    let mut c = t.bare();
    // Same PC name as the remote client, so `sessions.machine` matches.
    c.env("AIT_NOW_MS", T0.to_string())
        .env("COMPUTERNAME", "PC2-TEST")
        .env("HOSTNAME", "PC2-TEST")
        .arg("--db")
        .arg(db)
        .args(args);
    if let Some(s) = session {
        c.env("AIT_SESSION", s);
    }
    t.exec(c)
}

#[test]
fn local_and_remote_output_are_identical() {
    // Two databases built by the same commands at the same clock: one used
    // locally, one through the host. Every answer must match byte for byte.
    let t = T::new("lan_same");
    let a = t.path("a/tracker.db");
    std::fs::create_dir_all(a.parent().unwrap()).unwrap();
    let token = t.ok_at(None, T0, &["remote", "add", "pc2"])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let host = start_host(&t, &[], Some(T0));
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    // The client's token is not part of the other database; give it one too
    // so both start from the same state.
    local(&t, &a, None, &["remote", "add", "pc2"]);

    let steps: &[(Option<&str>, &[&str], bool)] = &[
        (
            None,
            &["add", "CORE", "parser", "--body", "first body"],
            false,
        ),
        (None, &["add", "CORE", "lexer", "--label", "ui,core"], false),
        (None, &["add", "PAP", "paper"], false),
        (None, &["edit", "CORE-1-01", "--priority", "P1"], false),
        (
            None,
            &["criteria", "CORE-1-00", "add", "parses input"],
            false,
        ),
        (None, &["list"], true),
        (None, &["list", "--all"], true),
        (None, &["show", "CORE-1-00"], true),
        (None, &["next"], true),
        (Some("a1"), &["claim", "CORE-1-00"], false),
        (Some("a2"), &["claim", "CORE-1-00"], true),
        (Some("a1"), &["note", "CORE-1-00", "half way"], false),
        (Some("a1"), &["heartbeat"], false),
        (None, &["board"], true),
        (None, &["roadmap"], true),
        (None, &["stats"], true),
        (None, &["activity"], true),
        (None, &["show", "NOPE-1-00"], true),
        (None, &["list", "--bogus"], true),
        (Some("a1"), &["criteria", "CORE-1-00", "check", "1"], false),
        (
            Some("a1"),
            &["done", "CORE-1-00", "--summary", "built", "--gen", "1"],
            false,
        ),
        (None, &["show", "CORE-1-00"], true),
        (None, &["brief", "CORE-1-01"], true),
        (None, &["export"], true),
        (None, &["check"], true),
        (None, &["help"], true),
        (None, &["guide"], true),
    ];
    for (session, args, both) in steps {
        let modes: &[bool] = if *both { &[true, false] } else { &[true] };
        for json_mode in modes {
            let mut full = Vec::new();
            if *json_mode {
                full.push("--json");
            }
            full.extend_from_slice(args);
            let l = local(&t, &a, *session, &full);
            let r = pc.run(*session, &full);
            assert_eq!(
                (l.code, &l.stdout, &l.stderr),
                (r.code, &r.stdout, &r.stderr),
                "local vs remote differ for {full:?}"
            );
        }
    }
    assert_eq!(pc.extra_files(), Vec::<String>::new());
}

#[test]
fn racing_remote_claims_have_exactly_one_winner() {
    let t = T::new("lan_race");
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let pc1 = Pc::new(&t, "pc1", host.lan_port, &token, "");
    let pc2 = Pc::new(&t, "pc2", host.lan_port, &token, "");
    for round in 0..50 {
        let id = id(&t.ok(None, &["add", "CORE", &format!("race {round}")]));
        let s1 = format!("p1-{round}");
        let s2 = format!("p2-{round}");
        let c1 = pc1
            .cmd(Some(&s1), &["--json", "claim", &id])
            .spawn()
            .unwrap();
        let c2 = pc2
            .cmd(Some(&s2), &["--json", "claim", &id])
            .spawn()
            .unwrap();
        let (o1, o2) = (wait(c1), wait(c2));
        let mut codes = [o1.code, o2.code];
        codes.sort();
        assert_eq!(
            codes,
            [0, 4],
            "round {round}: {} / {}",
            o1.stdout,
            o2.stdout
        );
        let loser = if o1.code == 4 { &o1 } else { &o2 };
        assert_eq!(err_code(&loser.json), "ALREADY_CLAIMED");
        let winner = if o1.code == 0 { &s1 } else { &s2 };
        assert_eq!(t.ok(None, &["show", &id])["claim"]["session"], *winner);
    }
}

#[test]
fn host_down_fails_fast_with_remote_unreachable_and_no_files() {
    let t = T::new("lan_down");
    // A port with nothing listening.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let pc = Pc::new(&t, "pc2", port, "00", "remote_timeout_ms = 1000\n");
    let start = Instant::now();
    let o = pc.json(Some("pc2-a1"), &["claim", "CORE-1-00"]);
    assert_eq!(o.code, 5, "{}", o.stdout);
    assert_eq!(err_code(&o.json), "REMOTE_UNREACHABLE");
    assert_eq!(o.json["error"]["retryable"], true);
    assert!(start.elapsed() < Duration::from_secs(20));
    // Text mode: one error line on stderr, nothing on stdout.
    let o = pc.run(None, &["list"]);
    assert_eq!(o.code, 5);
    assert!(o.stdout.is_empty());
    assert!(o.stderr.contains("REMOTE_UNREACHABLE"), "{}", o.stderr);

    // Something that accepts connections but never speaks TLS: the
    // handshake times out within remote_timeout_ms.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let sport = silent.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        let start = Instant::now();
        for s in silent.incoming() {
            held.push(s);
            if start.elapsed() > Duration::from_secs(30) {
                break;
            }
        }
    });
    let pc_silent = Pc::new(&t, "pc3", sport, "00", "remote_timeout_ms = 500\n");
    let start = Instant::now();
    let o = pc_silent.json(None, &["list"]);
    assert_eq!(err_code(&o.json), "REMOTE_UNREACHABLE", "{}", o.stdout);
    assert_eq!(o.code, 5);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(pc.extra_files(), Vec::<String>::new());
    assert_eq!(pc_silent.extra_files(), Vec::<String>::new());
}

#[test]
fn a_host_that_stops_answering_after_the_request_is_remote_timeout() {
    // A fake host: completes the TLS handshake, reads the request, never
    // answers.
    let t = T::new("lan_timeout");
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(fixture("test-only-cert.pem"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("test-only-key.pem")).unwrap();
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Some(Ok(s)) = l.incoming().next() {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let conn = rustls::ServerConnection::new(cfg).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, s);
            let mut buf = [0u8; 4096];
            let _ = tls.read(&mut buf);
            std::thread::sleep(Duration::from_secs(10));
        }
    });
    let pc = Pc::new(&t, "pc2", port, "00", "remote_reply_timeout_ms = 500\n");
    let start = Instant::now();
    let o = pc.json(Some("a1"), &["claim", "CORE-1-00"]);
    assert_eq!(err_code(&o.json), "REMOTE_TIMEOUT", "{}", o.stdout);
    assert_eq!(o.code, 5);
    assert_eq!(o.json["error"]["retryable"], true);
    assert!(start.elapsed() < Duration::from_secs(8));
}

#[test]
fn wrong_fingerprint_is_refused_as_remote_tls() {
    let t = T::new("lan_fp");
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    let cfg = std::fs::read_to_string(pc.dir.join("default.cfg")).unwrap();
    std::fs::write(
        pc.dir.join("default.cfg"),
        cfg.replace(FP, &"ab".repeat(32)),
    )
    .unwrap();
    let o = pc.json(None, &["list"]);
    assert_eq!(o.code, 2, "{}", o.stdout);
    assert_eq!(err_code(&o.json), "REMOTE_TLS");
    // The message names what the host presented, so it can be compared.
    assert!(
        o.json["error"]["message"].as_str().unwrap().contains(FP),
        "{}",
        o.stdout
    );
    // Missing pin: a config error before anything is sent.
    std::fs::write(
        pc.dir.join("default.cfg"),
        format!(
            "remote = 127.0.0.1:{}\nremote_token_file = pc.token\n",
            host.lan_port
        ),
    )
    .unwrap();
    let o = pc.json(None, &["list"]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "CONFIG"));
}

#[test]
fn wrong_and_revoked_tokens_are_remote_auth() {
    let t = T::new("lan_auth");
    t.ok(None, &["add", "CORE", "x"]);
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    assert_eq!(pc.ok(None, &["list"])["issues"][0]["id"], "CORE-1-00");

    let bad = Pc::new(&t, "pc9", host.lan_port, &"0".repeat(32), "");
    let o = bad.json(None, &["list"]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "REMOTE_AUTH"));

    // AIT_REMOTE_TOKEN wins over the token file.
    let mut c = pc.cmd(None, &["--json", "list"]);
    c.env("AIT_REMOTE_TOKEN", "f".repeat(32));
    let o = wait(c.spawn().unwrap());
    assert_eq!(err_code(&o.json), "REMOTE_AUTH");

    // A second active token for the same name is refused; list shows it.
    assert_eq!(t.err(None, &["remote", "add", "pc2"]).1, "IN_USE");
    let v = t.ok(None, &["remote", "list"]);
    assert_eq!(v["clients"][0]["name"], "pc2");
    assert_eq!(v["clients"][0]["active"], true);
    assert!(
        !t.run(None, &["remote", "list"]).stdout.contains(&token),
        "tokens are never shown again"
    );

    t.ok(None, &["remote", "revoke", "pc2"]);
    let o = pc.json(None, &["list"]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "REMOTE_AUTH"));
    assert!(o.stdout.contains("revoked"), "{}", o.stdout);
    assert_eq!(t.err(None, &["remote", "revoke", "pc2"]).1, "NOT_FOUND");
    assert_eq!(
        t.ok(None, &["remote", "list"])["clients"][0]["active"],
        false
    );

    // A new token for the same PC works; the old one stays refused.
    let fresh = add_client(&t, "pc2");
    assert_ne!(fresh, token);
    assert_eq!(err_code(&pc.json(None, &["list"]).json), "REMOTE_AUTH");
    std::fs::write(pc.dir.join("pc.token"), &fresh).unwrap();
    assert_eq!(pc.ok(None, &["list"])["issues"][0]["id"], "CORE-1-00");

    // Only a hash is stored.
    let db = rusqlite::Connection::open(t.db()).unwrap();
    let stored: String = db
        .query_row("SELECT token_hash FROM clients", [], |r| r.get(0))
        .unwrap();
    assert_ne!(stored, fresh);
    assert_eq!(stored.len(), 64);

    // Missing token file: a config error.
    std::fs::remove_file(pc.dir.join("pc.token")).unwrap();
    let o = pc.json(None, &["list"]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "CONFIG"));
}

#[test]
fn host_jobs_and_host_options_are_refused_remotely() {
    let t = T::new("lan_refuse");
    t.ok(None, &["add", "CORE", "x"]);
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    for args in [
        &["serve"][..],
        &["init"],
        &["import"],
        &["backup"],
        &["remote", "list"],
        &["list", "--db", "other.db"],
        &["check", "--repo", "."],
    ] {
        let o = pc.json(None, args);
        assert_eq!(
            (o.code, err_code(&o.json).as_str()),
            (2, "USAGE"),
            "{args:?}: {}",
            o.stdout
        );
    }
    assert_eq!(pc.extra_files(), Vec::<String>::new());
}

#[test]
fn file_options_are_read_and_output_files_written_on_the_client() {
    let t = T::new("lan_files");
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    std::fs::write(pc.dir.join("body.md"), "body from pc2\n").unwrap();
    std::fs::write(pc.dir.join("note.txt"), "note from pc2").unwrap();
    std::fs::write(pc.dir.join("HANDOFF.md"), "handoff from pc2").unwrap();

    let v = pc.ok(
        None,
        &["add", "CORE", "with body", "--body-file", "body.md"],
    );
    let issue = id(&v);
    assert_eq!(
        t.ok(None, &["show", &issue])["issue"]["body"],
        "body from pc2\n"
    );
    // The host never looks in its own folder for that name.
    assert!(!t.path("body.md").exists());

    // stdin (`-`) is this PC's stdin.
    let mut c = pc.cmd(None, &["--json", "edit", &issue, "--body-file", "-"]);
    c.stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"piped body")
        .unwrap();
    let o = wait(child);
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert_eq!(t.ok(None, &["show", &issue])["issue"]["body"], "piped body");

    pc.ok(Some("pc2-a1"), &["claim", &issue]);
    pc.ok(Some("pc2-a1"), &["note", &issue, "--text-file", "note.txt"]);
    pc.ok(
        Some("pc2-a1"),
        &["handoff", &issue, "--from-file", "HANDOFF.md"],
    );
    let shown = t.run(None, &["show", &issue]).stdout;
    assert!(shown.contains("note from pc2"), "{shown}");
    assert!(shown.contains("handoff from pc2"), "{shown}");

    // A file this PC cannot read fails here, like a local run.
    let o = pc.json(None, &["add", "CORE", "t", "--body-file", "missing.md"]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "INVALID_INPUT"));

    // brief --out / export --out are written in this PC's folder.
    let v = pc.ok(None, &["brief", &issue, "--out", "brief.md"]);
    assert_eq!(v, json!({"id": issue, "out": "brief.md"}));
    let md = std::fs::read_to_string(pc.dir.join("brief.md")).unwrap();
    let host_md = t.ok(None, &["brief", &issue])["markdown"].clone();
    assert_eq!(md, host_md.as_str().unwrap());
    assert!(!t.path("brief.md").exists());
    let o = pc.run(None, &["export", "--out", "all.jsonl"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("exported "), "{}", o.stdout);
    let text = std::fs::read_to_string(pc.dir.join("all.jsonl")).unwrap();
    assert_eq!(text, t.ok(None, &["export"])["jsonl"].as_str().unwrap());
    assert!(!t.path("all.jsonl").exists());
}

#[test]
fn addresses_outside_lan_allow_are_closed_without_a_reply() {
    let t = T::new("lan_allow");
    let token = add_client(&t, "pc2");
    let cfg = t.write("host.cfg", "lan_allow = 10.0.0.0/8\n");
    let host = start_host(&t, &["--config", &p(&cfg)], None);
    let pc = Pc::new(&t, "pc2", host.lan_port, &token, "");
    let o = pc.json(None, &["list"]);
    assert_eq!(err_code(&o.json), "REMOTE_UNREACHABLE", "{}", o.stdout);
    assert_eq!(o.code, 5);
}

#[test]
fn serve_lan_refuses_to_start_without_a_certificate() {
    let t = T::new("lan_nocert");
    let o = t.run(
        None,
        &["serve", "--port", "0", "--lan", "--bind", "127.0.0.1"],
    );
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "CONFIG"));
    assert!(o.stdout.contains("make-cert"), "{}", o.stdout);
}

// ---- raw requests to the LAN port (what a misbehaving client could send) ----

#[derive(Debug)]
struct AnyCert(rustls::crypto::WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// One raw HTTPS request; returns (status, JSON body).
fn raw(port: u16, request: &str) -> (u16, Value) {
    let provider = rustls::crypto::ring::default_provider();
    let algs = provider.signature_verification_algorithms;
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(algs)))
        .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from("127.0.0.1").unwrap())
            .unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut s = rustls::StreamOwned::new(conn, tcp);
    s.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let out = String::from_utf8_lossy(&out).into_owned();
    let status = out
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn post_cli(port: u16, token: Option<&str>, body: &Value) -> (u16, Value) {
    let b = body.to_string();
    let tok = token
        .map(|t| format!("X-Ait-Client-Token: {t}\r\n"))
        .unwrap_or_default();
    raw(
        port,
        &format!(
            "POST /api/cli HTTP/1.1\r\nHost: h\r\nContent-Type: application/json\r\n{tok}Content-Length: {}\r\n\r\n{b}",
            b.len()
        ),
    )
}

#[test]
fn the_lan_port_checks_every_request() {
    let t = T::new("lan_raw");
    t.ok(None, &["add", "CORE", "x"]);
    let token = add_client(&t, "pc2");
    let host = start_host(&t, &[], None);
    let port = host.lan_port;

    let (st, v) = raw(port, "GET /api/version HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(st, 200);
    assert_eq!(v["lan"], true);
    assert_eq!(v["api"], 1);
    let (st, _) = raw(port, "GET / HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(st, 404, "no dashboard on the LAN port in phase 1");

    let list = json!({"args": ["list"]});
    assert_eq!(post_cli(port, None, &list).0, 401);
    let (st, v) = post_cli(port, Some("nope"), &list);
    assert_eq!((st, err_code(&v).as_str()), (401, "REMOTE_AUTH"));
    let (st, v) = post_cli(port, Some(&token), &list);
    assert_eq!(st, 200);
    assert_eq!(
        (v["ok"].as_bool(), v["exit"].as_i64()),
        (Some(true), Some(0))
    );
    assert_eq!(v["json"]["issues"][0]["id"], "CORE-1-00");
    assert!(v.get("warning").is_none(), "no client version, no warning");

    // Host jobs and host options are refused even from a hand-made request.
    for args in [
        json!(["serve"]),
        json!(["init"]),
        json!(["import"]),
        json!(["backup"]),
        json!(["remote", "add", "evil"]),
        json!(["list", "--db", "x.db"]),
        json!(["list", "--config", "x.cfg"]),
        json!(["list", "--as", "someone"]),
        json!(["list", "--remote", "h:1"]),
        json!(["check", "--repo", "."]),
        json!(["brief", "CORE-1-00", "--out", "x.md"]),
    ] {
        let (st, v) = post_cli(port, Some(&token), &json!({"args": args}));
        assert_eq!(st, 200);
        assert_eq!(v["ok"], false, "{args}");
        assert_eq!(v["exit"], 2, "{args}");
        assert_eq!(v["json"]["error"]["code"], "USAGE", "{args}");
    }
    // A file the client did not send is never read from the host's disk.
    let (_, v) = post_cli(
        port,
        Some(&token),
        &json!({"args": ["add", "CORE", "t", "--body-file", p(&t.db())]}),
    );
    assert_eq!(v["json"]["error"]["code"], "INVALID_INPUT", "{v}");
    let (_, v) = post_cli(
        port,
        Some(&token),
        &json!({"args": ["add", "CORE", "t", "--body-file", "-"]}),
    );
    assert_eq!(v["json"]["error"]["code"], "INVALID_INPUT", "{v}");

    // A client of another version still works, with a warning.
    let (_, v) = post_cli(
        port,
        Some(&token),
        &json!({"args": ["list"], "client": "0.0.1"}),
    );
    assert_eq!(v["ok"], true);
    assert!(v["warning"].as_str().unwrap().contains("0.0.1"), "{v}");
    let (_, v) = post_cli(
        port,
        Some(&token),
        &json!({"args": ["list"], "client": env!("CARGO_PKG_VERSION")}),
    );
    assert!(v.get("warning").is_none());

    // Bad bodies.
    let (st, _) = post_cli(port, Some(&token), &json!({"args": "list"}));
    assert_eq!(st, 400);
    let (st, _) = post_cli(
        port,
        Some(&token),
        &json!({"args": ["list"], "session": "bad name"}),
    );
    assert_eq!(st, 200);

    // A revoked token is a 403.
    t.ok(None, &["remote", "revoke", "pc2"]);
    let (st, v) = post_cli(port, Some(&token), &list);
    assert_eq!((st, err_code(&v).as_str()), (403, "REMOTE_AUTH"));
}
