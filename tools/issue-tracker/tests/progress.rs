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
    t.ok_at(None, T0, &["done", "CORE-1-04", "--summary", "shipped it"]);
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
