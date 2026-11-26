//! `ait serve`: a local dashboard on 127.0.0.1, written on plain `std::net`
//! (no web framework). The page polls `/api/head` and reloads its data when
//! the newest event number changes.
//!
//! Writes go through `POST /api/run`, which runs the same command code as the
//! command line (so the same checks apply), for a short list of planning
//! commands only. A write must carry the per-start token embedded in the page
//! (`X-Ait-Token`), a JSON content type, and, when the browser sends them, a
//! same-origin `Origin` / `Sec-Fetch-Site`. Other web pages cannot read the
//! token, so they cannot send writes. `--read-only` turns writes off.

use crate::args::Args;
use crate::board;
use crate::claims;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::issues;
use crate::output::Output;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const PAGE: &str = include_str!("dashboard.html");

/// The session name dashboard writes are recorded under.
pub const DASHBOARD_SESSION: &str = "dashboard";

/// Commands the dashboard may run. Everything else (claims, delete, import,
/// serve, backup...) stays on the command line.
const WRITE_COMMANDS: &[&str] = &[
    "add", "edit", "state", "move", "done", "note", "link", "unlink", "ship", "criteria", "claim",
    "delete", "release",
];

/// Options a dashboard request may not pass (they would pick another
/// database, config or identity, or read and write files on this PC).
const FORBIDDEN_OPTS: &[&str] = &[
    "db",
    "config",
    "as",
    "help",
    "body-file",
    "text-file",
    "from-file",
    "out",
];

const MAX_BODY: usize = 2 * 1024 * 1024;

/// Version of the HTTP and `--json` contract described in API.md.
pub const API_VERSION: i64 = 1;

thread_local! {
    /// Per-request headers (framing policy, CORS); reset by every request.
    static CORS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

struct Server {
    port: u16,
    token: String,
    writable: bool,
}

/// A random per-start token (std's randomly seeded hasher; no extra crate).
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = String::new();
    for i in 0..2_u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

pub fn cmd_serve(_c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["port", "read-only"], 0)?;
    let port = match a.opt_i64("port")? {
        Some(p) => u16::try_from(p).map_err(|_| AitError::usage("--port must be 0..65535"))?,
        None => env.cfg.port,
    };
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        AitError::new(
            "IO_ERROR",
            format!("cannot listen on 127.0.0.1:{port}: {e}"),
        )
    })?;
    let actual = listener.local_addr()?.port();
    let server = Arc::new(Server {
        port: actual,
        token: new_token(),
        writable: !a.flag("read-only"),
    });
    {
        let mut out = std::io::stdout().lock();
        let mode = if server.writable {
            "editable"
        } else {
            "read-only"
        };
        let _ = writeln!(
            out,
            "ait dashboard: http://127.0.0.1:{actual}/  ({mode}; Ctrl+C to stop)"
        );
        let _ = out.flush();
    }
    // One short-lived thread (with its own connection) per request, so an
    // idle browser connection cannot block the others.
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let cfg = env.cfg.clone();
        let db_path = env.db_path.clone();
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let base = Env {
                now: 0,
                session: None,
                cfg,
                db_path,
            };
            let _ = handle(stream, &base, &server);
        });
    }
    Ok(Output::new(json!({"stopped": true}), ""))
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n{}Connection: close\r\n\r\n",
        body.len(),
        CORS.with(|c| c.borrow().clone())
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

fn json_resp(s: &mut TcpStream, status: &str, v: &Value) -> std::io::Result<()> {
    respond(
        s,
        status,
        "application/json; charset=utf-8",
        v.to_string().as_bytes(),
    )
}

fn deny(s: &mut TcpStream, status: &str, code: &str, message: &str) -> std::io::Result<()> {
    json_resp(
        s,
        status,
        &json!({"error": {"code": code, "message": message, "retryable": false}}),
    )
}

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// A query value with `%XX` and `+` decoded (invalid escapes stay as typed).
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(x) => {
                        out.push(x);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            x => out.push(x),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_dec(q: &str, key: &str) -> Option<String> {
    query(q, key).map(decode).filter(|v| !v.trim().is_empty())
}

/// `/api/list`: every issue (dropped and released too), newest first by
/// default, with filters, search, sorting and paging for the List view.
fn list_view(c: &Connection, env: &Env, q: &str) -> Result<Value> {
    let csv = |k: &str| -> Vec<String> {
        query_dec(q, k)
            .map(|v| {
                v.split(',')
                    .map(|x| x.trim().to_ascii_lowercase())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let states = csv("state");
    let types = csv("type");
    let labels = csv("label");
    let search = query_dec(q, "q").map(|s| s.to_lowercase());
    let sort = match query(q, "sort").unwrap_or("created") {
        "id" => "i.id",
        "state" => "i.state",
        "title" => "lower(i.title)",
        "priority" => "i.priority",
        "updated" => "i.updated_at",
        "assignee" => "c.session",
        _ => "i.created_at",
    };
    let dir = if query(q, "dir") == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    let offset: i64 = query(q, "offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .max(0);
    let limit: i64 = query(q, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .max(1);
    let mut st = c.prepare(&format!(
        "SELECT i.id, i.title, i.state, i.type, i.priority, i.labels, i.created_at, i.updated_at,
                i.parent, c.session, c.lease_until, lower(i.id || ' ' || i.title || ' ' || i.body || ' ' || i.labels)
         FROM issues i LEFT JOIN claims c ON c.issue = i.id
         ORDER BY {sort} {dir}, i.id {dir}"
    ))?;
    type Row = (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
        String,
    );
    let rows: Vec<Row> = st
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let matching: Vec<Row> = rows
        .into_iter()
        .filter(|r| {
            let row_labels = issues::label_list(&r.5);
            // "open" = not finished and not dropped.
            (states.is_empty()
                || states.contains(&r.2)
                || (states.iter().any(|x| x == "open")
                    && !["done", "released", "dropped"].contains(&r.2.as_str())))
                && (types.is_empty() || types.contains(&r.3))
                && labels.iter().all(|l| row_labels.contains(l))
                && search
                    .as_ref()
                    .is_none_or(|s| s.split_whitespace().all(|w| r.11.contains(w)))
        })
        .collect();
    let total = matching.len();
    let ap = claims::load_approvals(env)?;
    let offered = claims::ready_ids(c, env, &ap, false)?;
    let mut out = Vec::new();
    for r in matching
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(0))
        .take(usize::try_from(limit).unwrap_or(50))
    {
        let finished = issues::FINISHED.contains(&r.2.as_str());
        // Progress: containers by their members, others by their own state
        // or, while open, by their acceptance criteria.
        let percent = if ["feature", "milestone", "release"].contains(&r.3.as_str()) {
            crate::roadmap::rollup_with(c, env, &offered, &r.0)?["percent"]
                .as_i64()
                .unwrap_or(0)
        } else if finished {
            100
        } else {
            let (n, done): (i64, i64) = c.query_row(
                "SELECT COUNT(*), COUNT(done_at) FROM criteria WHERE issue = ?1",
                [&r.0],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )?;
            if n > 0 { done * 100 / n } else { 0 }
        };
        let live = r.9.is_some() && r.10.is_some_and(|l| l > env.now);
        out.push(json!({
            "id": r.0,
            "title": r.1,
            "state": r.2,
            "type": r.3,
            "priority": r.4,
            "labels": issues::label_list(&r.5),
            "created_at": crate::time::iso(r.6),
            "updated_at": crate::time::iso(r.7),
            "parent": r.8,
            "assignee": r.9,
            "claim_status": if r.9.is_none() { Value::Null } else if live { json!("live") } else { json!("stale") },
            "percent": percent,
        }));
    }
    Ok(json!({"total": total, "offset": offset, "issues": out}))
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads one request: the head (at most 16 KB) and a body of
/// `Content-Length` bytes (at most 2 MB). `Err` means a bad request.
fn read_request(s: &mut TcpStream) -> std::io::Result<std::result::Result<Request, String>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16_384 {
            return Ok(Err("request head too large".to_string()));
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Ok(Err("incomplete request".to_string()));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse::<usize>())
        .transpose()
        .map_err(|_| std::io::Error::other("bad Content-Length"))?
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(Err("request body too large".to_string()));
    }
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Ok(Err("incomplete request body".to_string()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Ok(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn handle(mut s: TcpStream, base: &Env, srv: &Server) -> std::io::Result<()> {
    // Reset the per-request headers first, so nothing leaks from an earlier
    // request. Only the dashboard itself and allowed origins may frame it.
    let frame = if base.cfg.allowed_origins.is_empty() {
        "X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\n".to_string()
    } else {
        format!(
            "Content-Security-Policy: frame-ancestors 'self' {}\r\n",
            base.cfg.allowed_origins.join(" ")
        )
    };
    CORS.with(|c| *c.borrow_mut() = frame);
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let req = match read_request(&mut s) {
        Ok(Ok(r)) => r,
        Ok(Err(m)) => return deny(&mut s, "400 Bad Request", "BAD_REQUEST", &m),
        Err(e) => return deny(&mut s, "400 Bad Request", "BAD_REQUEST", &e.to_string()),
    };
    let port = srv.port;
    // Only answer requests addressed to this local server (blocks DNS rebinding).
    let host = req.header("host").unwrap_or("");
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !allowed.iter().any(|a| a == host) {
        return deny(&mut s, "403 Forbidden", "FORBIDDEN", "bad Host header");
    }
    let (path, q) = req
        .target
        .split_once('?')
        .unwrap_or((req.target.as_str(), ""));
    // A configured local origin (the factory UI) may call the API from its
    // own page: answer with CORS headers for it, and only for it.
    let origin = req.header("origin").map(str::to_ascii_lowercase);
    let trusted = origin
        .as_ref()
        .filter(|o| base.cfg.allowed_origins.contains(o))
        .cloned();
    if let Some(o) = &trusted {
        CORS.with(|c| {
            c.borrow_mut().push_str(&format!(
                "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: Content-Type, X-Ait-Token\r\nAccess-Control-Max-Age: 600\r\n"
            ));
        });
    }
    if req.method == "OPTIONS" {
        return if trusted.is_some() {
            respond(&mut s, "204 No Content", "text/plain", b"")
        } else {
            deny(&mut s, "403 Forbidden", "FORBIDDEN", "origin not allowed")
        };
    }
    if req.method == "GET" && path == "/api/token" {
        // Gives the write token to an allowed origin's page; the browser
        // lets only that origin read this response.
        if trusted.is_none() || !srv.writable {
            return deny(
                &mut s,
                "403 Forbidden",
                "FORBIDDEN",
                "the token is only given to an allowed_origins page of an editable dashboard",
            );
        }
        return json_resp(&mut s, "200 OK", &json!({"token": srv.token}));
    }
    if req.method == "POST" && path == "/api/run" {
        return run_write(&mut s, base, srv, &req);
    }
    if req.method != "GET" {
        return deny(
            &mut s,
            "405 Method Not Allowed",
            "METHOD",
            "only GET, and POST /api/run",
        );
    }
    let env = Env {
        now: crate::time::now_ms().unwrap_or(base.now),
        session: None,
        cfg: base.cfg.clone(),
        db_path: base.db_path.clone(),
    };
    if path == "/" || path == "/index.html" {
        let page = PAGE
            .replace("__AIT_TOKEN__", if srv.writable { &srv.token } else { "" })
            .replace(
                "__AIT_WRITABLE__",
                if srv.writable { "true" } else { "false" },
            );
        return respond(
            &mut s,
            "200 OK",
            "text/html; charset=utf-8",
            page.as_bytes(),
        );
    }
    let conn = match db::open(&env.db_path) {
        Ok(c) => c,
        Err(e) => return json_resp(&mut s, "500 Internal Server Error", &e.to_json()),
    };
    let c = &conn;
    let result: Result<Value> = match path {
        "/api/head" => board::head(c).map(|h| json!({"head": h})),
        "/api/version" => Ok(json!({
            "api": API_VERSION,
            "ait": env!("CARGO_PKG_VERSION"),
            "schema": db::schema_version(),
            "writable": srv.writable,
        })),
        "/api/board" => db::read(c, |c| board::board_json(c, &env, 50)),
        "/api/issues" => db::read(c, |c| issues_view(c, &env)),
        "/api/roadmap" => db::read(c, |c| crate::roadmap::roadmap_json(c, &env, false)),
        "/api/next" => next_view(c, &env),
        "/api/list" => db::read(c, |c| list_view(c, &env, q)),
        "/api/stats" => {
            let since = query(q, "since")
                .map(crate::time::parse_duration)
                .transpose()
                .map(|d| d.map(|d| env.now - d));
            match since {
                Ok(since) => db::read(c, |c| crate::stats::stats_json(c, &env, since)),
                Err(e) => Err(e),
            }
        }
        "/api/activity" => {
            let after = query(q, "after").and_then(|v| v.parse().ok());
            let limit = query(q, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50_i64)
                // "Load more" on the dashboard asks for ever larger pages.
                .max(1);
            db::read(c, |c| board::activity_json(c, None, after, None, limit))
        }
        p if p.starts_with("/api/issue/") => {
            let id = &p["/api/issue/".len()..];
            db::read(c, |c| {
                let id = issues::resolve(c, id)?;
                issues::show_json(c, &env, &id, 50)
            })
        }
        _ => Err(AitError::new("NOT_FOUND", "no such page")),
    };
    match result {
        Ok(v) => json_resp(&mut s, "200 OK", &v),
        // Same status mapping as /api/run (a bad ?since= is a 400).
        Err(e) => json_resp(&mut s, status_for(e.exit_code()), &e.to_json()),
    }
}

/// What `ait next` would offer, best first, plus what `next --frame` would.
fn next_view(c: &Connection, env: &Env) -> Result<Value> {
    let ap = claims::load_approvals(env)?;
    db::read(c, |c| {
        let pick = |frame: bool| -> Result<Vec<Value>> {
            let p = claims::Pick {
                frame,
                ..Default::default()
            };
            claims::candidates(c, env, &ap, &p)?
                .iter()
                .take(10)
                .map(|id| {
                    let mut v = issues::get_full(c, &env.cfg, id)?;
                    if let Some(o) = v.as_object_mut() {
                        o.remove("body");
                    }
                    Ok(v)
                })
                .collect()
        };
        Ok(json!({"build": pick(false)?, "frame": pick(true)?}))
    })
}

fn status_for(exit: i32) -> &'static str {
    match exit {
        2 => "400 Bad Request",
        3 => "404 Not Found",
        4 | 6 => "409 Conflict",
        5 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}

/// `POST /api/run` with `{"args": ["edit", "CORE-1-00", "--priority", "P1"]}`.
fn run_write(s: &mut TcpStream, base: &Env, srv: &Server, req: &Request) -> std::io::Result<()> {
    if !srv.writable {
        return deny(
            s,
            "403 Forbidden",
            "READ_ONLY",
            "the dashboard was started with --read-only",
        );
    }
    let port = srv.port;
    let mut origins = vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];
    origins.extend(base.cfg.allowed_origins.iter().cloned());
    let origin = req.header("origin").map(str::to_ascii_lowercase);
    if let Some(o) = &origin
        && !origins.iter().any(|a| a == o)
    {
        return deny(
            s,
            "403 Forbidden",
            "FORBIDDEN",
            "cross-site request refused",
        );
    }
    // The factory UI runs on another local port, so its browser reports
    // same-site; that is fine for an origin the owner listed.
    let from_allowed = origin
        .as_ref()
        .is_some_and(|o| base.cfg.allowed_origins.contains(o));
    if let Some(site) = req.header("sec-fetch-site")
        && site != "same-origin"
        && !from_allowed
    {
        return deny(
            s,
            "403 Forbidden",
            "FORBIDDEN",
            "cross-site request refused",
        );
    }
    if req.header("x-ait-token") != Some(srv.token.as_str()) {
        return deny(
            s,
            "403 Forbidden",
            "FORBIDDEN",
            "missing or wrong dashboard token",
        );
    }
    let ctype = req.header("content-type").unwrap_or("");
    if !ctype.to_ascii_lowercase().starts_with("application/json") {
        return deny(
            s,
            "415 Unsupported Media Type",
            "BAD_REQUEST",
            "send application/json",
        );
    }
    let args: Vec<String> = match serde_json::from_slice::<Value>(&req.body) {
        Ok(v) => match v.get("args").and_then(Value::as_array) {
            Some(a) if a.iter().all(Value::is_string) => a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            _ => {
                return deny(
                    s,
                    "400 Bad Request",
                    "BAD_REQUEST",
                    "body must be {\"args\": [\"command\", ...]}",
                );
            }
        },
        Err(e) => return deny(s, "400 Bad Request", "BAD_REQUEST", &e.to_string()),
    };
    let result = run_args(base, args);
    match result {
        Ok(out) => json_resp(s, "200 OK", &json!({"ok": true, "result": out.json})),
        Err(e) => {
            let mut v = e.to_json();
            if let Some(o) = v.as_object_mut() {
                o.insert("exit".to_string(), json!(e.exit_code()));
            }
            json_resp(s, status_for(e.exit_code()), &v)
        }
    }
}

fn run_args(base: &Env, args: Vec<String>) -> Result<Output> {
    let mut a = Args::parse(args)?;
    for o in FORBIDDEN_OPTS {
        if a.opt(o).is_some() || a.flag(o) {
            return Err(AitError::usage(format!(
                "--{o} is not allowed from the dashboard"
            )));
        }
    }
    let Some(cmd) = a.shift() else {
        return Err(AitError::usage("no command"));
    };
    if !WRITE_COMMANDS.contains(&cmd.as_str()) {
        return Err(AitError::usage(format!(
            "'{cmd}' is not available from the dashboard (allowed: {})",
            WRITE_COMMANDS.join(", ")
        )));
    }
    let env = Env {
        now: crate::time::now_ms()?,
        session: Some(DASHBOARD_SESSION.to_string()),
        cfg: base.cfg.clone(),
        db_path: base.db_path.clone(),
    };
    let conn = db::open(&env.db_path)?;
    crate::dispatch(&cmd, &conn, &env, &a)
}

/// Why a ready issue is not offered by `next`, for its card.
fn ready_note(
    c: &Connection,
    env: &Env,
    ap: &crate::approvals::Approvals,
    id: &str,
) -> Option<String> {
    let first = claims::blockers(c, env, ap, id).ok()?.into_iter().next()?;
    let msg = first["message"].as_str()?;
    Some(if first["code"] == "TYPE" {
        "not offered to agents: it is a container (done when its children are)".to_string()
    } else {
        format!("not offered: {msg}")
    })
}

/// Ids returned by a one-column query.
fn id_set(c: &Connection, sql: &str) -> Result<std::collections::HashSet<String>> {
    let mut st = c.prepare(sql)?;
    let ids = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Every issue with the board column it belongs to, left to right:
/// backlog, ready, waiting, in_progress (stale claims flagged), in_review,
/// done, released.
fn issues_view(c: &Connection, env: &Env) -> Result<Value> {
    let ap = claims::load_approvals(env)?;
    let offered: std::collections::HashSet<String> =
        claims::ready_ids(c, env, &ap, false)?.into_iter().collect();
    let unmet = id_set(
        c,
        &format!(
            "SELECT DISTINCT l.src FROM links l LEFT JOIN issues d ON d.id = l.dst
             WHERE l.kind = 'needs' AND {}",
            issues::need_unmet_sql()
        ),
    )?;
    let mut st = c.prepare(
        "SELECT i.id, i.title, i.state, i.reason, i.ord, i.done_at, c.session, c.generation, c.lease_until,
                i.type, i.priority, i.agent, i.parent, i.released_at, i.labels
         FROM issues i LEFT JOIN claims c ON c.issue = i.id
         WHERE i.state <> 'dropped'
         ORDER BY i.priority, i.ord, i.id",
    )?;
    let rows = st
        .query_map([], |r| {
            let id: String = r.get(0)?;
            let state: String = r.get(2)?;
            let session: Option<String> = r.get(6)?;
            let lease: Option<i64> = r.get(8)?;
            let kind: String = r.get(9)?;
            let parent: Option<String> = r.get(12)?;
            let live = session.is_some() && lease.is_some_and(|l| l > env.now);
            let mut wait: Option<String> = None;
            let column = match state.as_str() {
                "released" => "released",
                "done" => "done",
                "in_review" => "in_review",
                _ if session.is_some() => "in_progress",
                "blocked" | "decision" => "waiting",
                "ready" if offered.contains(&id) => "ready",
                "ready" if unmet.contains(&id) => {
                    wait = Some("waits for its needs".to_string());
                    "waiting"
                }
                "ready" if kind == "review" => {
                    wait = Some(format!(
                        "starts when {} is in review",
                        parent.as_deref().unwrap_or("its parent")
                    ));
                    "backlog"
                }
                // Ready but not offered to agents (a container, not
                // approved for build, its tier at its WIP limit ...): it
                // still shows in Ready, with the reason on the card.
                "ready" => {
                    wait = ready_note(c, env, &ap, &id);
                    "ready"
                }
                _ => "backlog",
            };
            Ok(json!({
                "id": id,
                "title": r.get::<_, String>(1)?,
                "state": state,
                "reason": r.get::<_, Option<String>>(3)?,
                "order": r.get::<_, i64>(4)?,
                "done_at": crate::time::iso_opt(r.get::<_, Option<i64>>(5)?),
                "session": session,
                "generation": r.get::<_, Option<i64>>(7)?,
                "lease_until": crate::time::iso_opt(lease),
                "stale": session.is_some() && !live,
                "column": column,
                "wait": wait,
                "type": kind,
                "priority": r.get::<_, String>(10)?,
                "agent": r.get::<_, Option<String>>(11)?,
                "parent": parent,
                "released_at": crate::time::iso_opt(r.get::<_, Option<i64>>(13)?),
                "labels": issues::label_list(&r.get::<_, String>(14)?),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let wip: serde_json::Map<String, Value> = env
        .cfg
        .wip
        .iter()
        .map(|(k, n)| (k.clone(), json!(n)))
        .collect();
    // Configured agent tiers (tier -> model), so forms can offer only those.
    let tiers: serde_json::Map<String, Value> = env
        .cfg
        .agents
        .iter()
        .map(|(k, m)| (k.clone(), json!(m)))
        .collect();
    Ok(json!({"issues": rows, "head": board::head(c)?, "wip": wip, "agent_tiers": tiers}))
}
