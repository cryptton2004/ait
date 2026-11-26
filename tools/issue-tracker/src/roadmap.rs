//! Releases, milestones and features: rollups and `ait roadmap`.
//!
//! The members of a container are its descendants through `parent`, plus every
//! issue whose `release` is the container (and their descendants).

use crate::approvals::Approvals;
use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::Result;
use crate::output::{Output, line, s};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fmt::Write as _;

/// Progress of a container. `offered` is what `next` would offer, in order.
pub fn rollup_with(c: &Connection, env: &Env, offered: &[String], id: &str) -> Result<Value> {
    let mut st = c.prepare(
        "WITH RECURSIVE m(id) AS (
             SELECT id FROM issues WHERE (parent = ?1 OR release = ?1) AND id <> ?1
             UNION
             SELECT i.id FROM issues i JOIN m ON (i.parent = m.id OR i.release = m.id)
             WHERE i.id <> ?1
         )
         SELECT i.id, i.state, cl.session, cl.lease_until
         FROM issues i JOIN m ON m.id = i.id
         LEFT JOIN claims cl ON cl.issue = i.id
         ORDER BY i.priority, i.ord, i.id",
    )?;
    type Row = (String, String, Option<String>, Option<i64>);
    let rows: Vec<Row> = st
        .query_map(params![id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (mut total, mut done, mut dropped, mut in_progress) = (0, 0, 0, 0);
    let mut decisions = Vec::new();
    let mut blocked = Vec::new();
    for (mid, state, session, lease) in &rows {
        match state.as_str() {
            "dropped" => dropped += 1,
            "done" => {
                done += 1;
                total += 1;
            }
            _ => total += 1,
        }
        if state == "decision" {
            decisions.push(mid.clone());
        }
        if state == "blocked" {
            blocked.push(mid.clone());
        }
        if session.is_some() && lease.is_some_and(|l| l > env.now) {
            in_progress += 1;
        }
    }
    let next = offered
        .iter()
        .find(|o| rows.iter().any(|r| &r.0 == *o))
        .cloned();
    let percent = if total > 0 { done * 100 / total } else { 0 };
    Ok(json!({
        "total": total,
        "done": done,
        "open": total - done,
        "dropped": dropped,
        "in_progress": in_progress,
        "percent": percent,
        "next": next,
        "decisions": decisions,
        "blocked": blocked,
    }))
}

pub fn rollup(c: &Connection, env: &Env, ap: &Approvals, id: &str) -> Result<Value> {
    let offered = claims::ready_ids(c, env, ap, false)?;
    rollup_with(c, env, &offered, id)
}

/// `5/12 done (41%), 1 in progress, next CORE-3-00, decisions: X`
pub fn rollup_line(r: &Value) -> String {
    let mut out = format!("{}/{} done ({}%)", r["done"], r["total"], r["percent"]);
    if r["in_progress"].as_i64().unwrap_or(0) > 0 {
        let _ = write!(out, ", {} in progress", r["in_progress"]);
    }
    if let Some(n) = r["next"].as_str() {
        let _ = write!(out, ", next {n}");
    }
    for (key, label) in [("decisions", "decisions"), ("blocked", "blocked")] {
        if let Some(d) = r[key].as_array()
            && !d.is_empty()
        {
            let ids: Vec<&str> = d.iter().filter_map(Value::as_str).collect();
            let _ = write!(out, ", {label}: {}", ids.join(" "));
        }
    }
    out
}

pub fn roadmap_json(c: &Connection, env: &Env, all: bool) -> Result<Value> {
    let ap = claims::load_approvals(env)?;
    let offered = claims::ready_ids(c, env, &ap, false)?;
    let mut st = c.prepare(
        "SELECT id, title, state, priority, type, parent, release FROM issues
         WHERE type IN ('release', 'milestone', 'feature') AND (?1 OR state NOT IN ('dropped'))
         ORDER BY priority, ord, id",
    )?;
    type Row = (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = st
        .query_map(params![all], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = json!({"releases": [], "milestones": [], "features": []});
    for (id, title, state, priority, kind, parent, release) in rows {
        let item = json!({
            "id": id,
            "title": title,
            "state": state,
            "priority": priority,
            "type": kind,
            "parent": parent,
            "release": release,
            "rollup": rollup_with(c, env, &offered, &id)?,
        });
        let key = match kind.as_str() {
            "release" => "releases",
            "milestone" => "milestones",
            _ => "features",
        };
        if let Some(arr) = out[key].as_array_mut() {
            arr.push(item);
        }
    }
    Ok(out)
}

pub fn cmd_roadmap(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["all"], 0)?;
    let v = db::read(c, |c| roadmap_json(c, env, a.flag("all")))?;
    let mut text = String::new();
    for (key, label) in [
        ("releases", "RELEASES"),
        ("milestones", "MILESTONES"),
        ("features", "FEATURES"),
    ] {
        let Some(arr) = v[key].as_array() else {
            continue;
        };
        if arr.is_empty() {
            continue;
        }
        let _ = writeln!(text, "{label}");
        for x in arr {
            let _ = writeln!(
                text,
                "  {:<12} {} {:<8} {}\n               {}",
                s(x, "id"),
                s(x, "priority"),
                s(x, "state"),
                line(s(x, "title"), 70),
                rollup_line(&x["rollup"])
            );
        }
    }
    if text.is_empty() {
        text.push_str("no releases, milestones or features yet (ait add ... --type release|milestone|feature)");
    }
    Ok(Output::new(v, text))
}
