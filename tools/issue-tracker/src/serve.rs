//! `ait serve`: a read-only local dashboard on 127.0.0.1, written on plain
//! `std::net` (no web framework). The page polls `/api/head` and reloads its
//! data when the newest event number changes.

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
use std::time::Duration;

const PAGE: &str = include_str!("dashboard.html");

pub fn cmd_serve(_c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["port"], 0)?;
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
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "ait dashboard: http://127.0.0.1:{actual}/  (Ctrl+C to stop)"
        );
        let _ = out.flush();
    }
    // One short-lived thread (with its own read-only use of the DB) per
    // connection, so an idle browser connection cannot block the others.
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let cfg = env.cfg.clone();
        let db_path = env.db_path.clone();
        std::thread::spawn(move || {
            let base = Env {
                now: 0,
                session: None,
                cfg,
                db_path,
            };
            let _ = handle(stream, &base, actual);
        });
    }
    Ok(Output::new(json!({"stopped": true}), ""))
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
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

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn handle(mut s: TcpStream, base: &Env, port: u16) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16_384 {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let mut lines = req.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    // Only answer requests addressed to this local server (blocks DNS rebinding).
    let host = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("host")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default();
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !allowed.contains(&host) {
        return json_resp(
            &mut s,
            "403 Forbidden",
            &json!({"error": {"code": "FORBIDDEN", "message": "bad Host header"}}),
        );
    }
    if method != "GET" {
        return json_resp(
            &mut s,
            "405 Method Not Allowed",
            &json!({"error": {"code": "METHOD", "message": "read-only server"}}),
        );
    }
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let env = Env {
        now: crate::time::now_ms().unwrap_or(base.now),
        session: None,
        cfg: base.cfg.clone(),
        db_path: base.db_path.clone(),
    };
    if path == "/" || path == "/index.html" {
        return respond(
            &mut s,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        );
    }
    let conn = match db::open(&env.db_path) {
        Ok(c) => c,
        Err(e) => return json_resp(&mut s, "500 Internal Server Error", &e.to_json()),
    };
    let c = &conn;
    let result: Result<Value> = match path {
        "/api/head" => board::head(c).map(|h| json!({"head": h})),
        "/api/board" => db::read(c, |c| board::board_json(c, &env, 50)),
        "/api/issues" => db::read(c, |c| issues_view(c, &env)),
        "/api/activity" => {
            let after = query(q, "after").and_then(|v| v.parse().ok());
            let limit = query(q, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50_i64)
                .clamp(1, 1000);
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
        Err(e) if e.code == "NOT_FOUND" => json_resp(&mut s, "404 Not Found", &e.to_json()),
        Err(e) => json_resp(&mut s, "500 Internal Server Error", &e.to_json()),
    }
}

/// Every issue with the board column it belongs to.
fn issues_view(c: &Connection, env: &Env) -> Result<Value> {
    let ap = claims::load_approvals(env)?;
    let offered: std::collections::HashSet<String> =
        claims::ready_ids(c, env, &ap, false)?.into_iter().collect();
    let mut st = c.prepare(
        "SELECT i.id, i.title, i.state, i.reason, i.ord, i.done_at, c.session, c.generation, c.lease_until
         FROM issues i LEFT JOIN claims c ON c.issue = i.id
         WHERE i.state <> 'dropped'
         ORDER BY i.ord, i.id",
    )?;
    let rows = st
        .query_map([], |r| {
            let id: String = r.get(0)?;
            let state: String = r.get(2)?;
            let session: Option<String> = r.get(6)?;
            let lease: Option<i64> = r.get(8)?;
            let live = session.is_some() && lease.is_some_and(|l| l > env.now);
            let column = if state == "done" {
                "done"
            } else if live {
                "in_progress"
            } else if session.is_some() {
                "stale"
            } else if state == "blocked" || state == "decision" {
                "waiting"
            } else if offered.contains(&id) {
                "ready"
            } else {
                "backlog"
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
                "column": column,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"issues": rows, "head": board::head(c)?}))
}
