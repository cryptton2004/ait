//! Board, activity feed and the local dashboard server.

mod common;
use common::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::Child;
use std::time::{Duration, Instant};

fn seed(t: &T) {
    t.ok_at(None, T0, &["add", "CORE", "in progress one"]);
    t.ok_at(None, T0, &["add", "CORE", "stale one"]);
    t.ok_at(
        None,
        T0,
        &[
            "add",
            "CORE",
            "needs a decision",
            "--state",
            "decision",
            "--reason",
            "pick a DB",
        ],
    );
    t.ok_at(None, T0, &["add", "CORE", "ready one"]);
    t.ok_at(None, T0, &["add", "CORE", "finished"]);
    t.ok_at(Some("old"), T0 - 120 * MIN, &["claim", "CORE-1-01"]);
    t.ok_at(Some("a1"), T0, &["claim", "CORE-1-00"]);
    t.ok_at(
        None,
        T0,
        &["done", "CORE-1-04", "--summary", "shipped it", "--force"],
    );
}

#[test]
fn board_shows_progress_by_exception() {
    let t = T::new("board");
    seed(&t);
    let v = t.ok_at(None, T0 + MIN, &["board"]);
    assert_eq!(v["counts"]["in_progress"], 1);
    assert_eq!(v["counts"]["stale"], 1);
    assert_eq!(v["counts"]["decision"], 1);
    assert_eq!(v["counts"]["offered"], 1);
    assert_eq!(v["counts"]["done"], 1);
    assert_eq!(v["in_progress"][0]["session"], "a1");
    assert_eq!(v["stale"][0]["id"], "CORE-1-01");
    assert_eq!(v["attention"][0]["reason"], "pick a DB");
    assert_eq!(v["ready"][0]["id"], "CORE-1-03");
    assert_eq!(v["done_recent"][0]["summary"], "shipped it");
    // The text view stays on one screen even with many issues.
    for i in 0..40 {
        t.ok_at(None, T0, &["add", "PAP", &format!("bulk {i}")]);
    }
    let mut c = t.bare();
    c.arg("--db")
        .arg(t.db())
        .arg("board")
        .env("AIT_NOW_MS", (T0 + MIN).to_string());
    let o = t.exec(c);
    assert_eq!(o.code, 0);
    assert!(o.stdout.lines().count() <= 16, "{}", o.stdout);
    assert!(o.stdout.contains("more"), "{}", o.stdout);
}

#[test]
fn activity_feed_filters_by_time_sequence_and_issue() {
    let t = T::new("activity");
    t.ok_at(None, T0, &["add", "CORE", "a"]);
    t.ok_at(None, T0 + 30 * MIN, &["add", "CORE", "b"]);
    t.ok_at(Some("a1"), T0 + 31 * MIN, &["claim", "CORE-1-01"]);
    let all = t.ok_at(None, T0 + 32 * MIN, &["activity"]);
    let evs = all["events"].as_array().unwrap();
    assert_eq!(evs.len(), 3);
    assert_eq!(all["head"], 3);
    assert_eq!(evs[2]["action"], "claim");
    assert_eq!(evs[2]["session"], "a1");
    let recent = t.ok_at(None, T0 + 32 * MIN, &["activity", "--since", "10m"]);
    assert_eq!(recent["events"].as_array().unwrap().len(), 2);
    let after = t.ok_at(None, T0 + 32 * MIN, &["activity", "--after", "2"]);
    assert_eq!(after["events"].as_array().unwrap().len(), 1);
    let one = t.ok_at(None, T0 + 32 * MIN, &["activity", "--issue", "CORE-1-00"]);
    assert_eq!(one["events"].as_array().unwrap().len(), 1);
    assert_eq!(t.err(None, &["activity", "--since", "soon"]).1, "USAGE");
}

/// Kills the server when the test ends, pass or fail.
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http(port: u16, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map(|x| x.1.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get(port: u16, path: &str) -> (u16, String) {
    http(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    )
}

#[test]
fn serve_answers_dashboard_and_api_on_localhost_only() {
    let t = T::new("serve");
    t.ok(None, &["add", "CORE", "visible <script>alert(1)</script>"]);
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    let mut c = t.cmd(None, &["serve", "--port", "0"]);
    c.arg("--json");
    let mut server = Server(t.spawn(c));
    let stdout = server.0.stdout.take().unwrap();
    // Read the "listening" line on a thread so a broken server cannot hang us.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server did not start");
    let port: u16 = line
        .split("127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {line:?}"));

    let (code, page) = get(port, "/");
    assert_eq!(code, 200);
    assert!(page.contains("<title>ait board</title>"));
    // Without allowed_origins no other page may frame the dashboard.
    let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    raw.write_all(
        format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://localhost:5173\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut head = String::new();
    raw.read_to_string(&mut head).unwrap();
    let head = head.split("\r\n\r\n").next().unwrap_or("");
    assert!(head.contains("X-Frame-Options: DENY"), "{head}");
    assert!(head.contains("frame-ancestors 'none'"), "{head}");
    assert!(!head.contains("Access-Control-Allow-Origin"), "{head}");
    let (code, head) = get(port, "/api/head");
    assert_eq!(code, 200);
    let head: serde_json::Value = serde_json::from_str(&head).unwrap();
    assert_eq!(head["head"], 2);
    let (_, issues) = get(port, "/api/issues");
    let issues: serde_json::Value = serde_json::from_str(&issues).unwrap();
    assert_eq!(issues["issues"][0]["column"], "in_progress");
    assert_eq!(
        issues["issues"][0]["title"],
        "visible <script>alert(1)</script>"
    );
    let (code, one) = get(port, "/api/issue/CORE-1-00");
    assert_eq!(code, 200);
    assert!(one.contains("\"claim\""));
    assert_eq!(get(port, "/api/issue/NOPE-1-00").0, 404);
    assert_eq!(get(port, "/api/board").0, 200);
    assert_eq!(get(port, "/api/activity?after=1&limit=5").0, 200);
    // Pages larger than 1000 (dashboard "load more") are accepted.
    let (code, feed) = get(port, "/api/activity?limit=5000");
    assert_eq!(code, 200);
    assert!(feed.contains("\"events\""), "{feed}");
    let (code, rm) = get(port, "/api/roadmap");
    assert_eq!(code, 200);
    let rm: serde_json::Value = serde_json::from_str(&rm).unwrap();
    assert!(
        rm["releases"].is_array() && rm["features"].is_array(),
        "{rm}"
    );
    assert_eq!(issues["issues"][0]["priority"], "P2");
    assert_eq!(get(port, "/nothing").0, 404);
    // Writes are not possible and foreign Host headers are refused.
    let (code, _) = http(
        port,
        &format!("POST /api/head HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(code, 405);
    let (code, _) = http(
        port,
        "GET /api/head HTTP/1.1\r\nHost: evil.example:80\r\n\r\n",
    );
    assert_eq!(code, 403);
    // New events show up without restarting the server.
    t.ok(None, &["add", "CORE", "later"]);
    let start = Instant::now();
    loop {
        let (_, h) = get(port, "/api/head");
        if h.contains("\"head\":3") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "head never advanced: {h}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(server);
}

/// Starts `ait serve --port 0` (plus `extra`) and returns it with its port.
fn start_server(t: &T, extra: &[&str]) -> (Server, u16) {
    let mut c = t.cmd(None, &["serve", "--port", "0"]);
    c.args(extra);
    let mut server = Server(t.spawn(c));
    let stdout = server.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server did not start");
    let port: u16 = line
        .split("127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {line:?}"));
    (server, port)
}

fn page_token(port: u16) -> String {
    let (_, page) = get(port, "/");
    let marker = "name=\"ait-token\" content=\"";
    let start = page.find(marker).expect("token meta") + marker.len();
    page[start..].split('"').next().unwrap().to_string()
}

/// POST /api/run; `headers` are extra header lines. Returns (status, JSON).
fn post(port: u16, headers: &str, args: &[&str]) -> (u16, serde_json::Value) {
    let body = serde_json::json!({ "args": args }).to_string();
    let (code, text) = http(
        port,
        &format!(
            "POST /api/run HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        ),
    );
    (code, serde_json::from_str(&text).unwrap_or_default())
}

#[test]
fn dashboard_writes_pass_the_cli_gates_and_only_from_its_own_page() {
    let t = T::new("serve_write");
    t.ok(None, &["add", "CORE", "first"]);
    t.ok(None, &["add", "CORE", "second"]);
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    let (_server, port) = start_server(&t, &[]);
    let token = page_token(port);
    assert_eq!(token.len(), 32, "{token}");
    // The token appears once (in its meta tag); the page script must not
    // contain the placeholder text the server replaces.
    assert_eq!(get(port, "/").1.matches(&token).count(), 1);
    let ok = format!("Content-Type: application/json\r\nX-Ait-Token: {token}\r\n");

    // Guards: token, content type, origin, fetch site.
    let (code, v) = post(
        port,
        "Content-Type: application/json\r\n",
        &["add", "CORE", "x"],
    );
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (403, Some("FORBIDDEN"))
    );
    let wrong = "Content-Type: application/json\r\nX-Ait-Token: 0000\r\n";
    assert_eq!(post(port, wrong, &["add", "CORE", "x"]).0, 403);
    let plain = format!("Content-Type: text/plain\r\nX-Ait-Token: {token}\r\n");
    assert_eq!(post(port, &plain, &["add", "CORE", "x"]).0, 415);
    let evil = format!("{ok}Origin: http://evil.example\r\n");
    assert_eq!(post(port, &evil, &["add", "CORE", "x"]).0, 403);
    let cross = format!("{ok}Sec-Fetch-Site: cross-site\r\n");
    assert_eq!(post(port, &cross, &["add", "CORE", "x"]).0, 403);
    let same = format!("{ok}Origin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\n");

    // Only planning commands, and no options that pick files or identity.
    for args in [
        &["backup"][..],
        &["heartbeat"],
        &["import", "x.jsonl"],
        &["add", "CORE", "x", "--db", "other.db"],
        &["add", "CORE", "x", "--as", "a1"],
        &["add", "CORE", "x", "--body-file", "Cargo.toml"],
    ] {
        let (code, v) = post(port, &same, args);
        assert_eq!(
            (code, v["error"]["code"].as_str()),
            (400, Some("USAGE")),
            "{args:?}"
        );
    }

    // add, with the same checks as `ait add`.
    let (code, v) = post(
        port,
        &same,
        &[
            "add",
            "CORE",
            "from the dashboard",
            "--priority",
            "P1",
            "--type",
            "feature",
        ],
    );
    assert_eq!(code, 200, "{v}");
    // A feature added with a bare prefix opens its own group.
    assert_eq!(v["result"]["issue"]["id"], "CORE-2-00");
    let (code, v) = post(port, &same, &["add", "CORE", "bad", "--priority", "P9"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (400, Some("INVALID_INPUT"))
    );
    let (code, v) = post(port, &same, &["add", "CORE", "x", "--state", "blocked"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (400, Some("INVALID_INPUT"))
    );

    // edit and state of an agent's claimed issue need --force, as on the CLI.
    let (code, v) = post(port, &same, &["edit", "CORE-1-00", "--priority", "P0"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("NOT_CLAIMANT"))
    );
    let (code, v) = post(
        port,
        &same,
        &["edit", "CORE-1-00", "--priority", "P0", "--force"],
    );
    assert_eq!(code, 200, "{v}");
    let (code, v) = post(
        port,
        &same,
        &["edit", "CORE-1-01", "--title", "x", "--version", "1"],
    );
    assert_eq!(code, 200, "{v}");
    let (code, v) = post(
        port,
        &same,
        &["edit", "CORE-1-01", "--title", "y", "--version", "1"],
    );
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("VERSION_CONFLICT"))
    );

    // done: summary required; without a claim it is refused unless --force.
    let (code, _) = post(port, &same, &["done", "CORE-1-01"]);
    assert_eq!(code, 400);
    let (code, v) = post(port, &same, &["done", "CORE-1-01", "--summary", "ok"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("NOT_CLAIMED"))
    );
    let (code, v) = post(
        port,
        &same,
        &["done", "CORE-1-01", "--summary", "ok", "--force"],
    );
    assert_eq!(code, 200, "{v}");
    let (code, v) = post(port, &same, &["state", "CORE-1-01", "ready"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("ALREADY_DONE"))
    );

    // Rank: move changes what `next` offers first. (The feature added above
    // opened group 2, so these loose tasks continue group 1.)
    t.ok(None, &["add", "CORE", "third"]);
    t.ok(None, &["add", "CORE", "fourth"]);
    let (_, nv) = get(port, "/api/next");
    let nv: serde_json::Value = serde_json::from_str(&nv).unwrap();
    assert_eq!(nv["build"][0]["id"], "CORE-1-02");
    let (code, _) = post(port, &same, &["move", "CORE-1-03", "--top"]);
    assert_eq!(code, 200);
    let (_, nv) = get(port, "/api/next");
    let nv: serde_json::Value = serde_json::from_str(&nv).unwrap();
    assert_eq!(nv["build"][0]["id"], "CORE-1-03");
    let (code, _) = post(port, &same, &["note", "CORE-1-03", "looked at it"]);
    assert_eq!(code, 200);

    // The owner can take an issue from the dashboard with their own lease,
    // close it with that claim's generation, and others are kept out.
    let (code, v) = post(port, &same, &["claim", "CORE-1-02", "--lease", "2d"]);
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["result"]["claim"]["session"], "dashboard");
    assert_eq!(v["result"]["claim"]["lease_minutes"], 2880);
    assert_eq!(
        t.err(Some("a9"), &["claim", "CORE-1-02"]).1,
        "ALREADY_CLAIMED"
    );
    let (code, v) = post(port, &same, &["release", "CORE-1-02", "--gen", "1"]);
    assert_eq!(code, 200, "{v}");
    let (code, v) = post(port, &same, &["claim", "CORE-1-02", "--lease", "1h"]);
    assert_eq!(code, 200, "{v}");
    let (code, v) = post(
        port,
        &same,
        &["done", "CORE-1-02", "--summary", "by hand", "--gen", "2"],
    );
    assert_eq!(code, 200, "{v}");
    // An agent's live claim is not released from the dashboard without --force.
    t.ok(Some("a1"), &["claim", "CORE-1-03"]);
    let (code, v) = post(port, &same, &["release", "CORE-1-03"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("NOT_CLAIMANT")),
        "{v}"
    );

    // Dashboard writes are recorded as the "dashboard" session.
    let v = t.ok(None, &["activity", "--issue", "CORE-1-03"]);
    let sessions: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["session"].as_str())
        .collect();
    assert!(sessions.contains(&"dashboard"), "{v}");
}

#[test]
fn read_only_dashboard_refuses_writes() {
    let t = T::new("serve_ro");
    t.ok(None, &["add", "CORE", "first"]);
    let (_server, port) = start_server(&t, &["--read-only"]);
    assert_eq!(page_token(port), "");
    let (code, v) = post(
        port,
        "Content-Type: application/json\r\nX-Ait-Token: \r\n",
        &["add", "CORE", "x"],
    );
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (403, Some("READ_ONLY"))
    );
    assert_eq!(t.ok(None, &["list"])["count"], 1);
}

#[test]
fn dashboard_columns_follow_the_flow() {
    let t = T::new("serve_cols");
    t.ok_at(None, T0, &["add", "CORE", "built"]);
    t.ok_at(
        None,
        T0,
        &[
            "add",
            "CORE",
            "review",
            "--type",
            "review",
            "--parent",
            "CORE-1-00",
        ],
    );
    t.ok_at(None, T0, &["add", "CORE", "server"]);
    t.ok_at(None, T0, &["add", "CORE", "client", "--needs", "CORE-1-02"]);
    t.ok_at(None, T0, &["add", "CORE", "idea", "--state", "idea"]);
    t.ok_at(None, T0, &["add", "CORE", "old"]);
    t.ok_at(Some("gone"), T0 - 120 * MIN, &["claim", "CORE-1-05"]);
    // A ready container is shown in Ready (where it was dragged), with why
    // agents are not offered it.
    t.ok_at(None, T0, &["add", "CORE", "epic", "--type", "feature"]);
    let (_server, port) = start_server(&t, &[]);
    let (_, v) = get(port, "/api/issues");
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    let col = |id: &str| {
        v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .map(|i| {
                (
                    i["column"].as_str().unwrap().to_string(),
                    i["stale"] == true,
                )
            })
            .unwrap()
    };
    assert_eq!(col("CORE-1-00"), ("ready".to_string(), false));
    assert_eq!(col("CORE-1-01"), ("backlog".to_string(), false));
    assert_eq!(col("CORE-1-02"), ("ready".to_string(), false));
    assert_eq!(col("CORE-1-03"), ("waiting".to_string(), false));
    assert_eq!(col("CORE-1-04"), ("backlog".to_string(), false));
    // A stale claim stays in progress, flagged.
    assert_eq!(col("CORE-1-05"), ("in_progress".to_string(), true));
    assert_eq!(col("CORE-2-00"), ("ready".to_string(), false));
    let epic = v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "CORE-2-00")
        .unwrap();
    assert!(
        epic["wait"].as_str().unwrap().contains("not offered"),
        "{epic}"
    );
    // An offered task has no note.
    let task = v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "CORE-1-00")
        .unwrap();
    assert!(task["wait"].is_null(), "{task}");
}

/// A page on a configured local origin (the factory UI) can fetch the token,
/// pass the CORS preflight and write through the same gates; any other
/// origin is still refused.
#[test]
fn allowed_origin_can_write_through_the_same_gates() {
    let t = T::new("serve_origins");
    let cfg = t.write(
        "default.cfg",
        "allowed_origins = http://localhost:5173, http://127.0.0.1:3000\nbackup_keep = 0\n",
    );
    t.ok(None, &["add", "CORE", "first"]);
    let (_server, port) = start_server(&t, &["--config", &p(&cfg)]);
    let host = format!("Host: 127.0.0.1:{port}\r\n");
    let ui = "Origin: http://localhost:5173\r\n";

    // Version endpoint (no origin needed).
    let (_, v) = get(port, "/api/issues");
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert!(v["agent_tiers"].is_object(), "{v}");
    let (code, v) = get(port, "/api/version");
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert_eq!(code, 200);
    assert_eq!(v["api"], 1);
    assert_eq!(v["writable"], true);
    assert!(v["schema"].as_i64().unwrap() >= 4, "{v}");
    // Bad query input is a 400 with the usual error object.
    let (code, v) = get(port, "/api/stats?since=soon");
    assert_eq!(code, 400, "{v}");
    assert!(v.contains("\"error\""), "{v}");
    assert_eq!(get(port, "/api/stats?since=7d").0, 200);

    // Preflight: allowed origin gets 204 and CORS headers, others 403.
    let pre = |origin: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            format!(
                "OPTIONS /api/run HTTP/1.1\r\n{host}Origin: {origin}\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type, x-ait-token\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    let ok = pre("http://localhost:5173");
    assert!(ok.starts_with("HTTP/1.1 204"), "{ok}");
    assert!(
        ok.contains("Access-Control-Allow-Origin: http://localhost:5173\r\n"),
        "{ok}"
    );
    assert!(ok.contains("X-Ait-Token"), "{ok}");
    // Allowed origins may show the dashboard in a frame; nobody else may.
    assert!(
        ok.contains(
            "Content-Security-Policy: frame-ancestors 'self' http://localhost:5173 http://127.0.0.1:3000\r\n"
        ),
        "{ok}"
    );
    assert!(!ok.contains("X-Frame-Options"), "{ok}");
    let bad = pre("http://evil.example");
    assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");
    assert!(!bad.contains("Access-Control-Allow-Origin"), "{bad}");

    // The token is handed only to an allowed origin.
    let token_req = |origin: &str| {
        http(
            port,
            &format!("GET /api/token HTTP/1.1\r\n{host}{origin}\r\n"),
        )
    };
    assert_eq!(token_req("").0, 403);
    assert_eq!(token_req("Origin: http://localhost:5174\r\n").0, 403);
    let (code, body) = token_req(ui);
    assert_eq!(code, 200, "{body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(token, page_token(port));

    // A write from the factory UI (browser says same-site) passes, and the
    // CLI gates still apply to it.
    let hdr = format!(
        "Content-Type: application/json\r\nX-Ait-Token: {token}\r\n{ui}Sec-Fetch-Site: same-site\r\n"
    );
    let (code, v) = post(port, &hdr, &["add", "CORE", "from the factory"]);
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["result"]["issue"]["id"], "CORE-1-01");
    let (code, v) = post(port, &hdr, &["add", "CORE", "bad", "--priority", "P9"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (400, Some("INVALID_INPUT"))
    );
    let (code, v) = post(port, &hdr, &["backup"]);
    assert_eq!((code, v["error"]["code"].as_str()), (400, Some("USAGE")));
    // Without the token it is still refused.
    let no_token = format!("Content-Type: application/json\r\n{ui}Sec-Fetch-Site: same-site\r\n");
    assert_eq!(post(port, &no_token, &["add", "CORE", "x"]).0, 403);

    // Other origins stay refused, even with the right token.
    for origin in ["http://evil.example", "http://localhost:5174"] {
        let evil = format!(
            "Content-Type: application/json\r\nX-Ait-Token: {token}\r\nOrigin: {origin}\r\nSec-Fetch-Site: cross-site\r\n"
        );
        let (code, v) = post(port, &evil, &["add", "CORE", "x"]);
        assert_eq!(
            (code, v["error"]["code"].as_str()),
            (403, Some("FORBIDDEN")),
            "{origin}"
        );
    }
    // A cross-site request without an Origin header is refused too.
    let blind = format!(
        "Content-Type: application/json\r\nX-Ait-Token: {token}\r\nSec-Fetch-Site: same-site\r\n"
    );
    assert_eq!(post(port, &blind, &["add", "CORE", "x"]).0, 403);
}

#[test]
fn allowed_origins_must_be_local() {
    let t = T::new("origins_cfg");
    for bad in [
        "https://example.com",
        "http://localhost.evil.com",
        "http://localhost:5173/path",
        "*",
        "file://",
    ] {
        let cfg = t.write("bad.cfg", &format!("allowed_origins = {bad}\n"));
        let mut c = t.cmd(None, &[]);
        c.args(["--config", &p(&cfg), "--json", "list"]);
        let r = t.exec(c);
        assert_eq!(r.code, 2, "{bad}: {}", r.stdout);
        assert!(r.stdout.contains("CONFIG"), "{bad}: {}", r.stdout);
    }
}

/// The List view: every issue (dropped too), newest first, with filters,
/// search across id/title/description/labels, sorting, paging and progress;
/// delete from the dashboard follows the CLI rules.
#[test]
fn list_view_shows_everything_with_filters_and_paging() {
    let t = T::new("listview");
    t.ok_at(None, T0, &["add", "CORE", "Epic", "--type", "feature"]);
    t.ok_at(
        None,
        T0 + MIN,
        &[
            "add",
            "CORE",
            "Login form",
            "--parent",
            "CORE-1-00",
            "--labels",
            "ui",
        ],
    );
    t.ok_at(
        None,
        T0 + 2 * MIN,
        &[
            "add",
            "CORE",
            "Schema",
            "--body",
            "needs a Postgres index",
            "--criteria",
            "a\nb",
        ],
    );
    t.ok_at(None, T0 + 3 * MIN, &["add", "CORE", "Old idea"]);
    t.ok_at(
        None,
        T0 + 4 * MIN,
        &["state", "CORE-2-01", "dropped", "--reason", "no"],
    );
    t.ok_at(
        None,
        T0 + 5 * MIN,
        &["done", "CORE-1-01", "--summary", "ok", "--force"],
    );
    t.ok_at(Some("a1"), T0 + 6 * MIN, &["claim", "CORE-2-00"]);
    t.ok_at(
        Some("a1"),
        T0 + 6 * MIN,
        &["criteria", "CORE-2-00", "--check", "1"],
    );
    let (_server, port) = start_server(&t, &[]);
    let list = |q: &str| -> serde_json::Value {
        let (code, v) = get(port, &format!("/api/list{q}"));
        assert_eq!(code, 200, "{v}");
        serde_json::from_str(&v).unwrap()
    };
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };
    let v = list("");
    assert_eq!(v["total"], 4);
    assert_eq!(
        ids(&v),
        ["CORE-2-01", "CORE-2-00", "CORE-1-01", "CORE-1-00"]
    );
    let by = |id: &str| {
        v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by("CORE-1-00")["percent"], 100);
    assert_eq!(by("CORE-2-00")["percent"], 50);
    assert_eq!(by("CORE-2-00")["assignee"], "a1");
    assert_eq!(by("CORE-1-01")["labels"], serde_json::json!(["ui"]));
    assert_eq!(ids(&list("?state=dropped")), ["CORE-2-01"]);
    assert_eq!(ids(&list("?state=open")), ["CORE-2-00"]);
    assert_eq!(ids(&list("?label=ui")), ["CORE-1-01"]);
    assert_eq!(ids(&list("?q=postgres%20INDEX")), ["CORE-2-00"]);
    assert_eq!(ids(&list("?q=login+form")), ["CORE-1-01"]);
    assert_eq!(
        ids(&list("?sort=title&dir=asc")),
        ["CORE-1-00", "CORE-1-01", "CORE-2-01", "CORE-2-00"]
    );
    let page = list("?limit=3");
    assert_eq!((page["total"].as_i64(), ids(&page).len()), (Some(4), 3));
    assert_eq!(ids(&list("?offset=3&limit=3")), ["CORE-1-00"]);

    // Delete runs `ait delete`: a claimed issue needs --force.
    let token = page_token(port);
    let hdr = format!("Content-Type: application/json\r\nX-Ait-Token: {token}\r\n");
    let (code, v) = post(port, &hdr, &["delete", "CORE-2-00"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (409, Some("IN_USE")),
        "{v}"
    );
    let (code, v) = post(port, &hdr, &["delete", "CORE-2-01"]);
    assert_eq!(code, 200, "{v}");
    assert_eq!(list("")["total"], 3);
}

/// The Releases view: releases by expected date with their planned items,
/// and the features not planned into any release.
#[test]
fn releases_view_groups_features_by_release() {
    let t = T::new("relview");
    t.ok(
        None,
        &[
            "add",
            "REL",
            "v2",
            "--type",
            "release",
            "--due",
            "2027-03-01",
        ],
    );
    t.ok(
        None,
        &[
            "add",
            "REL",
            "v1",
            "--type",
            "release",
            "--due",
            "2026-12-15",
        ],
    );
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "Auth",
            "--type",
            "feature",
            "--release",
            "REL-2-00",
        ],
    );
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "Billing",
            "--type",
            "feature",
            "--release",
            "REL-1-00",
        ],
    );
    t.ok(None, &["add", "CORE", "Search", "--type", "feature"]);
    t.ok(
        None,
        &["add", "CORE", "login task", "--parent", "CORE-1-00"],
    );
    t.ok(None, &["done", "CORE-1-01", "--summary", "ok", "--force"]);
    t.ok(Some("a1"), &["claim", "CORE-2-00"]);
    let (_server, port) = start_server(&t, &[]);
    let (code, v) = get(port, "/api/releases");
    assert_eq!(code, 200, "{v}");
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    let rels = v["releases"].as_array().unwrap();
    let order: Vec<&str> = rels.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(order, ["REL-2-00", "REL-1-00"], "{v}");
    assert_eq!(rels[0]["due"], "2026-12-15");
    assert_eq!(rels[0]["items"][0]["id"], "CORE-1-00");
    assert_eq!(rels[0]["items"][0]["percent"], 100);
    assert_eq!(rels[1]["items"][0]["id"], "CORE-2-00");
    assert_eq!(rels[1]["items"][0]["assignee"], "a1");
    let unplanned: Vec<&str> = v["unplanned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(unplanned, ["CORE-3-00"]);
}

/// `/api/events` streams a `change` event when the tracker changes, then
/// ends after `max` seconds.
#[test]
fn event_stream_pushes_changes() {
    let t = T::new("sse");
    t.ok(None, &["add", "CORE", "first"]);
    let (_server, port) = start_server(&t, &[]);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!("GET /api/events?max=4 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
    )
    .unwrap();
    // Read until the hello arrives, then make a change.
    let mut got = String::new();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    while !got.contains("event: hello") {
        assert!(start.elapsed() < Duration::from_secs(10), "no hello: {got}");
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "stream closed early: {got}");
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(got.starts_with("HTTP/1.1 200"), "{got}");
    assert!(got.contains("text/event-stream"), "{got}");
    t.ok(None, &["add", "CORE", "second"]);
    // The rest of the stream (it ends by itself after max=4 seconds).
    let mut rest = String::new();
    s.read_to_string(&mut rest).unwrap();
    got.push_str(&rest);
    assert!(got.contains("event: change"), "{got}");
    assert!(got.contains("id: 2\n"), "{got}");
    assert!(got.contains("\"title\":\"second\""), "{got}");
}
