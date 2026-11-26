//! Progress views: `board` (one capped screen) and `activity` (event feed).

use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::Result;
use crate::issues;
use crate::output::{Output, line, s};
use crate::time::{iso, iso_opt, parse_duration, rel};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fmt::Write as _;

fn rows(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: impl Fn(&rusqlite::Row) -> rusqlite::Result<Value>,
) -> Result<Vec<Value>> {
    let mut st = c.prepare(sql)?;
    let out = st.query_map(p, f)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub fn head(c: &Connection) -> Result<i64> {
    Ok(c.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
}

/// All board data. Lists are capped at `per` items; `counts` are totals.
pub fn board_json(c: &Connection, env: &Env, per: i64) -> Result<Value> {
    let now = env.now;
    let claim_row = |r: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "title": r.get::<_, String>(1)?,
            "session": r.get::<_, String>(2)?,
            "generation": r.get::<_, i64>(3)?,
            "claimed_at": iso_opt(r.get::<_, Option<i64>>(4)?),
            "lease_until": iso_opt(r.get::<_, Option<i64>>(5)?),
            "lease_ms": r.get::<_, Option<i64>>(5)?,
        }))
    };
    let claims_sql = |cmp: &str| {
        format!(
            "SELECT i.id, i.title, c.session, c.generation, c.claimed_at, c.lease_until
             FROM claims c JOIN issues i ON i.id = c.issue
             WHERE c.session IS NOT NULL AND c.lease_until {cmp} ?1
             ORDER BY c.claimed_at, i.id"
        )
    };
    let in_progress = rows(c, &claims_sql(">"), params![now], claim_row)?;
    let stale = rows(c, &claims_sql("<="), params![now], claim_row)?;
    let attention = rows(
        c,
        "SELECT id, title, state, reason FROM issues WHERE state IN ('decision', 'blocked')
         ORDER BY CASE state WHEN 'decision' THEN 0 ELSE 1 END, ord, id",
        [],
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?,
                "state": r.get::<_, String>(2)?,
                "reason": r.get::<_, Option<String>>(3)?,
            }))
        },
    )?;
    let ap = claims::load_approvals(env)?;
    let ready_ids = claims::ready_ids(c, env, &ap, false)?;
    let mut ready = Vec::new();
    for id in ready_ids.iter().take(usize::try_from(per).unwrap_or(3)) {
        let i = issues::get(c, id)?;
        ready.push(json!({"id": id, "title": s(&i, "title")}));
    }
    let done_recent = rows(
        c,
        "SELECT id, title, done_at, summary FROM issues WHERE state = 'done'
         ORDER BY done_at DESC, id LIMIT ?1",
        params![per],
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "title": r.get::<_, String>(1)?,
                "done_at": iso_opt(r.get::<_, Option<i64>>(2)?),
                "done_ms": r.get::<_, Option<i64>>(2)?,
                "summary": r.get::<_, Option<String>>(3)?,
            }))
        },
    )?;
    let mut counts = serde_json::Map::new();
    for st in issues::STATES {
        let n: i64 = c.query_row("SELECT COUNT(*) FROM issues WHERE state = ?1", [st], |r| {
            r.get(0)
        })?;
        counts.insert((*st).to_string(), json!(n));
    }
    counts.insert("offered".into(), json!(ready_ids.len()));
    counts.insert("in_progress".into(), json!(in_progress.len()));
    counts.insert("stale".into(), json!(stale.len()));
    let cap = |v: Vec<Value>| -> Vec<Value> {
        v.into_iter()
            .take(usize::try_from(per).unwrap_or(3))
            .collect()
    };
    Ok(json!({
        "generated_at": iso(now),
        "head": head(c)?,
        "counts": counts,
        "approvals_in_use": ap.in_use(),
        "in_progress": cap(in_progress),
        "stale": cap(stale),
        "attention": cap(attention),
        "ready": ready,
        "done_recent": done_recent,
    }))
}

fn render_board(v: &Value, now: i64) -> String {
    let n = |k: &str| v["counts"][k].as_i64().unwrap_or(0);
    let mut out = format!(
        "board: {} in progress, {} stale, {} waiting (decision/blocked), {} offered, {} done\n",
        n("in_progress"),
        n("stale"),
        n("decision") + n("blocked"),
        n("offered"),
        n("done")
    );
    let more = |shown: usize, total: i64| -> String {
        let extra = total - i64::try_from(shown).unwrap_or(0);
        if extra > 0 {
            format!("  (+{extra} more)")
        } else {
            String::new()
        }
    };
    let section =
        |out: &mut String, label: &str, items: &Value, total: i64, f: &dyn Fn(&Value) -> String| {
            if let Some(arr) = items.as_array() {
                for (i, x) in arr.iter().enumerate() {
                    let tail = if i + 1 == arr.len() {
                        more(arr.len(), total)
                    } else {
                        String::new()
                    };
                    let _ = writeln!(out, "{label:<12} {}{tail}", f(x));
                }
            }
        };
    let claim_line = |x: &Value| {
        format!(
            "{:<12} {:<14} {}",
            s(x, "id"),
            format!(
                "{} lease {}",
                s(x, "session"),
                x["lease_ms"]
                    .as_i64()
                    .map(|l| rel(now, l))
                    .unwrap_or_default()
            ),
            line(s(x, "title"), 50)
        )
    };
    section(
        &mut out,
        "IN PROGRESS",
        &v["in_progress"],
        n("in_progress"),
        &claim_line,
    );
    section(&mut out, "STALE", &v["stale"], n("stale"), &claim_line);
    section(
        &mut out,
        "WAITING",
        &v["attention"],
        n("decision") + n("blocked"),
        &|x| {
            format!(
                "{:<12} {:<8} {} ({})",
                s(x, "id"),
                s(x, "state"),
                line(s(x, "title"), 40),
                line(s(x, "reason"), 40)
            )
        },
    );
    section(&mut out, "READY", &v["ready"], n("offered"), &|x| {
        format!("{:<12} {}", s(x, "id"), line(s(x, "title"), 60))
    });
    section(&mut out, "DONE", &v["done_recent"], n("done"), &|x| {
        format!(
            "{:<12} {:<8} {}",
            s(x, "id"),
            x["done_ms"]
                .as_i64()
                .map(|t| rel(now, t))
                .unwrap_or_default(),
            line(s(x, "title"), 55)
        )
    });
    out
}

pub fn cmd_board(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["limit"], 0)?;
    let per = a.opt_i64("limit")?.unwrap_or(3).clamp(1, 1000);
    let v = db::read(c, |c| board_json(c, env, per))?;
    let text = render_board(&v, env.now);
    Ok(Output::new(v, text))
}

pub fn activity_json(
    c: &Connection,
    since_ms: Option<i64>,
    after: Option<i64>,
    issue: Option<&str>,
    limit: i64,
) -> Result<Value> {
    let events = rows(
        c,
        "SELECT * FROM (
             SELECT seq, at, session, issue, action, details FROM events
             WHERE (?1 IS NULL OR at >= ?1) AND (?2 IS NULL OR seq > ?2) AND (?3 IS NULL OR issue = ?3)
             ORDER BY seq DESC LIMIT ?4
         ) ORDER BY seq",
        params![since_ms, after, issue, limit],
        |r| {
            let details: String = r.get(5)?;
            Ok(json!({
                "seq": r.get::<_, i64>(0)?,
                "at": iso(r.get::<_, i64>(1)?),
                "at_ms": r.get::<_, i64>(1)?,
                "session": r.get::<_, Option<String>>(2)?,
                "issue": r.get::<_, Option<String>>(3)?,
                "action": r.get::<_, String>(4)?,
                "details": serde_json::from_str::<Value>(&details).unwrap_or(Value::String(details)),
            }))
        },
    )?;
    Ok(json!({"head": head(c)?, "events": events}))
}

pub fn event_summary(e: &Value) -> String {
    let d = &e["details"];
    match s(e, "action") {
        "add" => format!("added: {}", line(s(d, "title"), 60)),
        "state" => format!(
            "{} -> {}{}",
            s(d, "from"),
            s(d, "to"),
            if s(d, "reason").is_empty() {
                String::new()
            } else {
                format!(" ({})", line(s(d, "reason"), 50))
            }
        ),
        "claim" => format!("claimed (generation {})", d["generation"]),
        "take" => format!(
            "took over from {} (generation {})",
            s(&d["taken_from"], "session"),
            d["generation"]
        ),
        "release" => format!(
            "released{}",
            if d["forced"].as_bool() == Some(true) {
                " (forced)"
            } else {
                ""
            }
        ),
        "done" => format!("done: {}", line(s(d, "summary"), 60)),
        "link" | "unlink" => format!("{} {} {}", s(e, "action"), s(d, "kind"), s(d, "other")),
        "edit" => format!(
            "edited {}",
            d["fields"]
                .as_array()
                .map(|f| f
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        "delete" => format!("deleted: {}", line(s(d, "title"), 60)),
        other => other.to_string(),
    }
}

pub fn cmd_activity(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["since", "after", "issue", "limit"], 0)?;
    let since = a
        .opt("since")
        .map(parse_duration)
        .transpose()?
        .map(|d| env.now - d);
    let after = a.opt_i64("after")?;
    let limit = a.opt_i64("limit")?.unwrap_or(50).clamp(1, 10_000);
    let v = db::read(c, |c| {
        let issue = a.opt("issue").map(|x| issues::resolve(c, x)).transpose()?;
        activity_json(c, since, after, issue.as_deref(), limit)
    })?;
    let mut text = String::new();
    if let Some(evs) = v["events"].as_array() {
        for e in evs {
            let _ = writeln!(
                text,
                "#{:<5} {} {:<12} {:<12} {}",
                e["seq"],
                &s(e, "at")[..s(e, "at").len().min(16)],
                line(s(e, "session"), 12),
                s(e, "issue"),
                event_summary(e)
            );
        }
        if evs.is_empty() {
            text.push_str("no activity");
        }
    }
    Ok(Output::new(v, text))
}
