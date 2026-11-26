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
        &["delete", "CORE-1-01"][..],
        &["claim", "CORE-1-01"],
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
    assert_eq!(v["result"]["issue"]["id"], "CORE-1-02");
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

    // Rank: move changes what `next` offers first.
    t.ok(None, &["add", "CORE", "third"]);
    t.ok(None, &["add", "CORE", "fourth"]);
    let (_, nv) = get(port, "/api/next");
    let nv: serde_json::Value = serde_json::from_str(&nv).unwrap();
    assert_eq!(nv["build"][0]["id"], "CORE-1-03");
    let (code, _) = post(port, &same, &["move", "CORE-1-04", "--top"]);
    assert_eq!(code, 200);
    let (_, nv) = get(port, "/api/next");
    let nv: serde_json::Value = serde_json::from_str(&nv).unwrap();
    assert_eq!(nv["build"][0]["id"], "CORE-1-04");
    let (code, _) = post(port, &same, &["note", "CORE-1-04", "looked at it"]);
    assert_eq!(code, 200);

    // Dashboard writes are recorded as the "dashboard" session.
    let v = t.ok(None, &["activity", "--issue", "CORE-1-04"]);
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
