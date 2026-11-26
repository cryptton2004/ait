//! Planning commands: add, edit, state, delete, move, link, unlink, show, list.

use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::ids;
use crate::notes;
use crate::output::{Output, line, s};
use crate::time::{iso, iso_opt};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fmt::Write as _;

pub const STATES: &[&str] = &[
    "idea", "planned", "ready", "blocked", "decision", "dropped", "done",
];
pub const NEEDS_REASON: &[&str] = &["blocked", "decision", "dropped"];
pub const LINK_KINDS: &[&str] = &["needs", "relates", "discovered_from"];

const MAX_TITLE: usize = 300;
const MAX_BODY: usize = 1_000_000;

pub const COLS: &str = "id, title, body, state, reason, ord, size, spec, parent, legacy, summary, version, created_at, updated_at, done_at";

pub fn row_json(r: &Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "title": r.get::<_, String>(1)?,
        "body": r.get::<_, String>(2)?,
        "state": r.get::<_, String>(3)?,
        "reason": r.get::<_, Option<String>>(4)?,
        "order": r.get::<_, i64>(5)?,
        "size": r.get::<_, Option<String>>(6)?,
        "spec": r.get::<_, Option<String>>(7)?,
        "parent": r.get::<_, Option<String>>(8)?,
        "legacy": r.get::<_, Option<String>>(9)?,
        "summary": r.get::<_, Option<String>>(10)?,
        "version": r.get::<_, i64>(11)?,
        "created_at": iso(r.get::<_, i64>(12)?),
        "updated_at": iso(r.get::<_, i64>(13)?),
        "done_at": iso_opt(r.get::<_, Option<i64>>(14)?),
    }))
}

pub fn exists(c: &Connection, id: &str) -> Result<bool> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM issues WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    Ok(n > 0)
}

/// Accepts an id (any case) or a legacy alias; returns the canonical id.
pub fn resolve(c: &Connection, input: &str) -> Result<String> {
    let t = input.trim();
    if t.is_empty() {
        return Err(AitError::usage("empty issue id"));
    }
    if exists(c, t)? {
        return Ok(t.to_string());
    }
    let up = t.to_ascii_uppercase();
    if exists(c, &up)? {
        return Ok(up);
    }
    let legacy: Option<String> = c
        .query_row(
            "SELECT id FROM issues WHERE legacy = ?1 COLLATE NOCASE",
            [t],
            |r| r.get(0),
        )
        .optional()?;
    legacy.ok_or_else(|| AitError::not_found(t))
}

pub fn get(c: &Connection, id: &str) -> Result<Value> {
    let sql = format!("SELECT {COLS} FROM issues WHERE id = ?1");
    c.query_row(&sql, [id], row_json)
        .optional()?
        .ok_or_else(|| AitError::not_found(id))
}

pub fn state_of(c: &Connection, id: &str) -> Result<String> {
    c.query_row("SELECT state FROM issues WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| AitError::not_found(id))
}

fn check_title(t: &str) -> Result<String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(AitError::new("INVALID_INPUT", "title must not be empty"));
    }
    if t.contains('\n') || t.contains('\r') {
        return Err(AitError::new("INVALID_INPUT", "title must be one line"));
    }
    if t.chars().count() > MAX_TITLE {
        return Err(AitError::new(
            "INVALID_INPUT",
            format!("title is longer than {MAX_TITLE} characters"),
        ));
    }
    Ok(t.to_string())
}

/// Text from `--<name>` or `--<name>-file <path>` (`-` reads stdin).
pub fn read_text(a: &Args, name: &str) -> Result<Option<String>> {
    let file_opt = format!("{name}-file");
    let text = match (a.opt(name), a.opt(&file_opt)) {
        (Some(_), Some(_)) => {
            return Err(AitError::usage(format!(
                "use --{name} or --{file_opt}, not both"
            )));
        }
        (Some(t), None) => Some(t.to_string()),
        (None, Some("-")) => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            Some(s)
        }
        (None, Some(p)) => Some(
            std::fs::read_to_string(p)
                .map_err(|e| AitError::new("INVALID_INPUT", format!("cannot read {p}: {e}")))?,
        ),
        (None, None) => None,
    };
    if let Some(t) = &text
        && t.len() > MAX_BODY
    {
        return Err(AitError::new("INVALID_INPUT", "text is larger than 1 MB"));
    }
    Ok(text)
}

pub fn check_state(state: &str, reason: Option<&str>) -> Result<()> {
    if !STATES.contains(&state) {
        return Err(AitError::new(
            "INVALID_STATE",
            format!("unknown state '{state}' (use {})", STATES.join(", ")),
        ));
    }
    if NEEDS_REASON.contains(&state) && reason.is_none_or(|r| r.trim().is_empty()) {
        return Err(AitError::new(
            "INVALID_INPUT",
            format!("state '{state}' needs --reason"),
        ));
    }
    Ok(())
}

fn id_list(c: &Connection, v: Option<&str>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(v) = v {
        for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let id = resolve(c, part)?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- ordering

pub enum Place<'a> {
    End,
    Top,
    After(&'a str),
    Before(&'a str),
}

fn ord_of(c: &Connection, id: &str) -> Result<i64> {
    c.query_row("SELECT ord FROM issues WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| AitError::not_found(id))
}

fn rebalance(c: &Connection) -> Result<()> {
    let ids: Vec<String> = {
        let mut st = c.prepare("SELECT id FROM issues ORDER BY ord, id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (i, id) in ids.iter().enumerate() {
        let ord = (i64::try_from(i).unwrap_or(i64::MAX / 2048) + 1) * 1024;
        c.execute("UPDATE issues SET ord = ?2 WHERE id = ?1", params![id, ord])?;
    }
    Ok(())
}

/// Computes a sort key for `pos`, ignoring `moving` itself. Keys are sparse;
/// when there is no gap left, all keys are renumbered once.
pub fn place(c: &Connection, pos: &Place, moving: Option<&str>) -> Result<i64> {
    let mv = moving.unwrap_or("");
    for _ in 0..2 {
        let r: Option<i64> = match pos {
            Place::End => {
                let m: Option<i64> =
                    c.query_row("SELECT MAX(ord) FROM issues WHERE id <> ?1", [mv], |r| {
                        r.get(0)
                    })?;
                Some(m.map_or(1024, |m| m + 1024))
            }
            Place::Top => {
                let m: Option<i64> =
                    c.query_row("SELECT MIN(ord) FROM issues WHERE id <> ?1", [mv], |r| {
                        r.get(0)
                    })?;
                Some(m.map_or(1024, |m| m - 1024))
            }
            Place::After(x) => {
                let xo = ord_of(c, x)?;
                let next: Option<i64> = c.query_row(
                    "SELECT MIN(ord) FROM issues WHERE ord >= ?1 AND id <> ?2 AND id <> ?3",
                    params![xo, mv, x],
                    |r| r.get(0),
                )?;
                match next {
                    None => Some(xo + 1024),
                    Some(n) if n - xo >= 2 => Some(xo + (n - xo) / 2),
                    Some(_) => None,
                }
            }
            Place::Before(x) => {
                let xo = ord_of(c, x)?;
                let prev: Option<i64> = c.query_row(
                    "SELECT MAX(ord) FROM issues WHERE ord <= ?1 AND id <> ?2 AND id <> ?3",
                    params![xo, mv, x],
                    |r| r.get(0),
                )?;
                match prev {
                    None => Some(xo - 1024),
                    Some(p) if xo - p >= 2 => Some(p + (xo - p) / 2),
                    Some(_) => None,
                }
            }
        };
        if let Some(o) = r {
            return Ok(o);
        }
        rebalance(c)?;
    }
    Err(AitError::new(
        "INTERNAL",
        "could not find a free sort position",
    ))
}

// ------------------------------------------------------------------- links

/// True if `from` reaches `target` through `needs` links.
fn reaches(c: &Connection, from: &str, target: &str) -> Result<bool> {
    let hit: Option<i64> = c
        .query_row(
            "WITH RECURSIVE r(id) AS (
                 SELECT ?1
                 UNION
                 SELECT l.dst FROM links l JOIN r ON l.src = r.id WHERE l.kind = 'needs'
             )
             SELECT 1 FROM r WHERE id = ?2 LIMIT 1",
            params![from, target],
            |r| r.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

fn add_link(c: &Connection, src: &str, kind: &str, dst: &str) -> Result<bool> {
    if src == dst {
        return Err(AitError::new(
            "CYCLE",
            format!("{src} cannot link to itself"),
        ));
    }
    if kind == "needs" && reaches(c, dst, src)? {
        return Err(AitError::new(
            "CYCLE",
            format!("{src} needs {dst} would create a cycle ({dst} already needs {src})"),
        ));
    }
    let n = c.execute(
        "INSERT OR IGNORE INTO links(src, kind, dst) VALUES (?1, ?2, ?3)",
        params![src, kind, dst],
    )?;
    Ok(n > 0)
}

fn link_kind(k: &str) -> Result<&'static str> {
    match k {
        "needs" => Ok("needs"),
        "relates" => Ok("relates"),
        "discovered-from" | "discovered_from" => Ok("discovered_from"),
        _ => Err(AitError::usage(format!(
            "unknown link kind '{k}' (use needs, relates, discovered-from)"
        ))),
    }
}

fn brief_list(c: &Connection, sql: &str, id: &str) -> Result<Vec<Value>> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([id], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "title": r.get::<_, Option<String>>(1)?,
            "state": r.get::<_, Option<String>>(2)?,
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn links_json(c: &Connection, id: &str) -> Result<Value> {
    let out = |kind: &str| {
        format!(
            "SELECT l.dst, i.title, i.state FROM links l LEFT JOIN issues i ON i.id = l.dst
             WHERE l.src = ?1 AND l.kind = '{kind}' ORDER BY l.dst"
        )
    };
    let inc = |kind: &str| {
        format!(
            "SELECT l.src, i.title, i.state FROM links l LEFT JOIN issues i ON i.id = l.src
             WHERE l.dst = ?1 AND l.kind = '{kind}' ORDER BY l.src"
        )
    };
    let mut relates = brief_list(c, &out("relates"), id)?;
    relates.extend(brief_list(c, &inc("relates"), id)?);
    let parent: Option<String> = c
        .query_row("SELECT parent FROM issues WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let parent = match parent {
        Some(p) => brief_list(c, "SELECT id, title, state FROM issues WHERE id = ?1", &p)?
            .into_iter()
            .next()
            .unwrap_or_else(|| json!({"id": p, "title": null, "state": null})),
        None => Value::Null,
    };
    Ok(json!({
        "needs": brief_list(c, &out("needs"), id)?,
        "needed_by": brief_list(c, &inc("needs"), id)?,
        "relates": relates,
        "discovered_from": brief_list(c, &out("discovered_from"), id)?,
        "discovered": brief_list(c, &inc("discovered_from"), id)?,
        "parent": parent,
        "children": brief_list(c, "SELECT id, title, state FROM issues WHERE parent = ?1 ORDER BY ord, id", id)?,
    }))
}

fn check_parent(c: &Connection, id: &str, parent: &str) -> Result<()> {
    if parent == id {
        return Err(AitError::new("CYCLE", "an issue cannot be its own parent"));
    }
    let mut cur = Some(parent.to_string());
    let mut steps = 0;
    while let Some(p) = cur {
        if p == id {
            return Err(AitError::new(
                "CYCLE",
                format!("{parent} is inside {id}; parent loop"),
            ));
        }
        steps += 1;
        if steps > 10_000 {
            break;
        }
        cur = c
            .query_row("SELECT parent FROM issues WHERE id = ?1", [&p], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
    }
    Ok(())
}

fn legacy_free(c: &Connection, legacy: &str, except: &str) -> Result<()> {
    let other: Option<String> = c
        .query_row(
            "SELECT id FROM issues WHERE legacy = ?1 COLLATE NOCASE AND id <> ?2",
            params![legacy, except],
            |r| r.get(0),
        )
        .optional()?;
    match other {
        Some(o) => Err(AitError::new(
            "ID_TAKEN",
            format!("legacy alias '{legacy}' already belongs to {o}"),
        )),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- commands

pub fn cmd_add(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(
        &[
            "body",
            "body-file",
            "after",
            "state",
            "reason",
            "size",
            "spec",
            "parent",
            "legacy",
            "needs",
            "discovered-from",
        ],
        2,
    )?;
    let spec = a.req_pos(0, "PREFIX (e.g. CORE), group (CORE-2) or full id")?;
    let title = check_title(a.req_pos(1, "title")?)?;
    let body = read_text(a, "body")?.unwrap_or_default();
    let state = a.opt("state").unwrap_or("ready");
    if state == "done" {
        return Err(AitError::new(
            "INVALID_STATE",
            "new issues cannot start as done",
        ));
    }
    check_state(state, a.opt("reason"))?;
    let reason = a.opt("reason").filter(|r| !r.trim().is_empty());
    let issue = db::write(c, |c| {
        let after = a.opt("after").map(|x| resolve(c, x)).transpose()?;
        let parent = a.opt("parent").map(|x| resolve(c, x)).transpose()?;
        let needs = id_list(c, a.opt("needs"))?;
        let disc = a
            .opt("discovered-from")
            .map(|x| resolve(c, x))
            .transpose()?;
        let legacy = a.opt("legacy").map(str::trim).filter(|l| !l.is_empty());
        if let Some(l) = legacy {
            legacy_free(c, l, "")?;
        }
        let id = ids::allocate(c, &env.cfg.prefixes, spec, after.as_deref())?;
        let pos = match after.as_deref() {
            Some(x) => Place::After(x),
            None => Place::End,
        };
        let ord = place(c, &pos, None)?;
        c.execute(
            "INSERT INTO issues (id, title, body, state, reason, ord, size, spec, parent, legacy,
                                 version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?11)",
            params![
                id,
                title,
                body,
                state,
                reason,
                ord,
                a.opt("size"),
                a.opt("spec"),
                parent,
                legacy,
                env.now
            ],
        )?;
        for n in &needs {
            add_link(c, &id, "needs", n)?;
        }
        if let Some(d) = &disc {
            add_link(c, &id, "discovered_from", d)?;
        }
        db::event(
            c,
            env,
            Some(&id),
            "add",
            json!({"title": title, "state": state, "needs": needs, "discovered_from": disc}),
        )?;
        get(c, &id)
    })?;
    let text = format!(
        "added {}  {}",
        s(&issue, "id"),
        line(s(&issue, "title"), 80)
    );
    Ok(Output::new(json!({ "issue": issue }), text))
}

pub fn cmd_edit(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(
        &[
            "title",
            "body",
            "body-file",
            "size",
            "spec",
            "parent",
            "legacy",
            "version",
        ],
        1,
    )?;
    let input = a.req_pos(0, "issue id")?;
    let title = a.opt("title").map(check_title).transpose()?;
    let body = read_text(a, "body")?;
    let expected = a.opt_i64("version")?;
    let issue = db::write(c, |c| {
        let id = resolve(c, input)?;
        let version: i64 = c.query_row("SELECT version FROM issues WHERE id = ?1", [&id], |r| {
            r.get(0)
        })?;
        if let Some(v) = expected
            && v != version
        {
            return Err(AitError::new(
                "VERSION_CONFLICT",
                format!("{id} is at version {version}, not {v}; re-read it and retry"),
            )
            .retryable());
        }
        let mut changed: Vec<&str> = Vec::new();
        let mut set = |col: &'static str, v: Option<String>| -> Result<()> {
            c.execute(
                &format!("UPDATE issues SET {col} = ?2 WHERE id = ?1"),
                params![id, v],
            )?;
            changed.push(col);
            Ok(())
        };
        if let Some(t) = &title {
            set("title", Some(t.clone()))?;
        }
        if let Some(b) = &body {
            set("body", Some(b.clone()))?;
        }
        if let Some(v) = a.opt("size") {
            set("size", Some(v.to_string()).filter(|v| !v.is_empty()))?;
        }
        if let Some(v) = a.opt("spec") {
            set("spec", Some(v.to_string()).filter(|v| !v.is_empty()))?;
        }
        if let Some(v) = a.opt("parent") {
            let p = if v.trim().is_empty() {
                None
            } else {
                let p = resolve(c, v)?;
                check_parent(c, &id, &p)?;
                Some(p)
            };
            set("parent", p)?;
        }
        if let Some(v) = a.opt("legacy") {
            let l = v.trim();
            if !l.is_empty() {
                legacy_free(c, l, &id)?;
            }
            set("legacy", Some(l.to_string()).filter(|l| !l.is_empty()))?;
        }
        if changed.is_empty() {
            return Err(AitError::usage(
                "nothing to change (use --title, --body, --size, --spec, --parent, --legacy)",
            ));
        }
        c.execute(
            "UPDATE issues SET version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![id, env.now],
        )?;
        db::event(c, env, Some(&id), "edit", json!({ "fields": changed }))?;
        get(c, &id)
    })?;
    let text = format!(
        "edited {} (version {})",
        s(&issue, "id"),
        issue.get("version").and_then(Value::as_i64).unwrap_or(0)
    );
    Ok(Output::new(json!({ "issue": issue }), text))
}

pub fn cmd_state(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["reason"], 2)?;
    let input = a.req_pos(0, "issue id")?;
    let state = a.req_pos(1, "state")?;
    if state == "done" {
        return Err(AitError::new(
            "INVALID_STATE",
            "use `ait done <id> --summary \"...\"` to close an issue",
        ));
    }
    check_state(state, a.opt("reason"))?;
    let reason = a.opt("reason").filter(|r| !r.trim().is_empty());
    let issue = db::write(c, |c| {
        let id = resolve(c, input)?;
        let from = state_of(c, &id)?;
        c.execute(
            "UPDATE issues SET state = ?2, reason = ?3, done_at = CASE WHEN ?2 = 'done' THEN done_at ELSE NULL END,
                 version = version + 1, updated_at = ?4 WHERE id = ?1",
            params![id, state, reason, env.now],
        )?;
        db::event(
            c,
            env,
            Some(&id),
            "state",
            json!({"from": from, "to": state, "reason": reason}),
        )?;
        get(c, &id)
    })?;
    let text = format!("{} is now {}", s(&issue, "id"), state);
    Ok(Output::new(json!({ "issue": issue }), text))
}

pub fn cmd_delete(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["force"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let force = a.flag("force");
    let (id, title) = db::write(c, |c| {
        let id = resolve(c, input)?;
        let issue = get(c, &id)?;
        if let Some(cl) = claims::claim_row(c, &id)?
            && let Some(holder) = &cl.session
        {
            let mine = env.session.as_deref() == Some(holder.as_str());
            if !mine && !force {
                return Err(AitError::new(
                    "IN_USE",
                    format!("{id} is claimed by {holder}; use --force to delete anyway"),
                ));
            }
        }
        let refs: i64 = c.query_row(
            "SELECT (SELECT COUNT(*) FROM links WHERE dst = ?1 AND kind = 'needs')
                  + (SELECT COUNT(*) FROM issues WHERE parent = ?1)",
            [&id],
            |r| r.get(0),
        )?;
        if refs > 0 && !force {
            return Err(AitError::new(
                "IN_USE",
                format!(
                    "{refs} issue(s) need {id} or have it as parent; use --force to delete anyway"
                ),
            ));
        }
        c.execute("DELETE FROM links WHERE src = ?1 OR dst = ?1", [&id])?;
        c.execute("UPDATE issues SET parent = NULL WHERE parent = ?1", [&id])?;
        c.execute("DELETE FROM claims WHERE issue = ?1", [&id])?;
        c.execute("DELETE FROM notes WHERE issue = ?1", [&id])?;
        c.execute("DELETE FROM issues WHERE id = ?1", [&id])?;
        c.execute(
            "INSERT OR IGNORE INTO retired_ids(id, at) VALUES (?1, ?2)",
            params![id, env.now],
        )?;
        let title = s(&issue, "title").to_string();
        db::event(
            c,
            env,
            Some(&id),
            "delete",
            json!({"title": title, "state": s(&issue, "state"), "forced": force}),
        )?;
        Ok((id, title))
    })?;
    let text = format!("deleted {id}  {}", line(&title, 80));
    Ok(Output::new(json!({ "deleted": id, "title": title }), text))
}

pub fn cmd_move(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["before", "after", "top", "bottom"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let given = [
        a.opt("before").is_some(),
        a.opt("after").is_some(),
        a.flag("top"),
        a.flag("bottom"),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if given != 1 {
        return Err(AitError::usage(
            "use exactly one of --before <id>, --after <id>, --top, --bottom",
        ));
    }
    let issue = db::write(c, |c| {
        let id = resolve(c, input)?;
        let target = match (a.opt("before"), a.opt("after")) {
            (Some(x), _) | (None, Some(x)) => Some(resolve(c, x)?),
            _ => None,
        };
        if target.as_deref() == Some(id.as_str()) {
            return Err(AitError::usage("cannot move an issue relative to itself"));
        }
        let pos = match (&target, a.opt("before").is_some(), a.flag("top")) {
            (Some(t), true, _) => Place::Before(t),
            (Some(t), false, _) => Place::After(t),
            (None, _, true) => Place::Top,
            (None, _, false) => Place::End,
        };
        let ord = place(c, &pos, Some(&id))?;
        c.execute(
            "UPDATE issues SET ord = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, ord, env.now],
        )?;
        db::event(
            c,
            env,
            Some(&id),
            "move",
            json!({"order": ord, "relative_to": target}),
        )?;
        get(c, &id)
    })?;
    Ok(Output::new(
        json!({ "issue": issue }),
        format!("moved {}", s(&issue, "id")),
    ))
}

pub fn cmd_link(c: &Connection, env: &Env, a: &Args, remove: bool) -> Result<Output> {
    a.expect(&[], 3)?;
    let src_in = a.req_pos(0, "issue id")?;
    let kind = link_kind(a.req_pos(1, "link kind (needs, relates, discovered-from)")?)?;
    let dst_in = a.req_pos(2, "other issue id")?;
    let (src, dst, changed) = db::write(c, |c| {
        let src = resolve(c, src_in)?;
        let dst = resolve(c, dst_in)?;
        let changed = if remove {
            let mut n = c.execute(
                "DELETE FROM links WHERE src = ?1 AND kind = ?2 AND dst = ?3",
                params![src, kind, dst],
            )?;
            if kind == "relates" {
                n += c.execute(
                    "DELETE FROM links WHERE src = ?3 AND kind = ?2 AND dst = ?1",
                    params![src, kind, dst],
                )?;
            }
            n > 0
        } else {
            add_link(c, &src, kind, &dst)?
        };
        if changed {
            let action = if remove { "unlink" } else { "link" };
            db::event(
                c,
                env,
                Some(&src),
                action,
                json!({"kind": kind, "other": dst}),
            )?;
        }
        Ok((src, dst, changed))
    })?;
    let verb = match (remove, changed) {
        (false, true) => "linked",
        (false, false) => "already linked",
        (true, true) => "unlinked",
        (true, false) => "was not linked",
    };
    Ok(Output::new(
        json!({"src": src, "kind": kind, "dst": dst, "changed": changed}),
        format!("{verb}: {src} {kind} {dst}"),
    ))
}

/// Everything needed to resume work on an issue, in one response.
pub fn show_json(c: &Connection, env: &Env, id: &str, note_limit: i64) -> Result<Value> {
    let issue = get(c, id)?;
    let claim = claims::claim_row(c, id)?.map(|r| claims::claim_json(&r, env.now));
    let approvals = claims::load_approvals(env)?;
    let blockers = claims::blockers(c, env, &approvals, id)?;
    Ok(json!({
        "issue": issue,
        "links": links_json(c, id)?,
        "claim": claim,
        "ready": blockers.is_empty(),
        "blockers": blockers,
        "latest_handoff": notes::latest_handoff(c, id)?,
        "notes": notes::recent(c, id, note_limit)?,
        "note_count": notes::count(c, id)?,
    }))
}

pub fn cmd_show(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["notes"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let limit = a.opt_i64("notes")?.unwrap_or(10).clamp(0, 1000);
    let v = db::read(c, |c| {
        let id = resolve(c, input)?;
        show_json(c, env, &id, limit)
    })?;
    let text = render_show(&v, env.now);
    Ok(Output::new(v, text))
}

fn render_refs(out: &mut String, label: &str, v: &Value) {
    if let Some(arr) = v.as_array()
        && !arr.is_empty()
    {
        let items: Vec<String> = arr
            .iter()
            .map(|x| format!("{} ({})", s(x, "id"), s(x, "state")))
            .collect();
        let _ = writeln!(out, "{label}: {}", items.join(", "));
    }
}

fn render_show(v: &Value, now: i64) -> String {
    let i = &v["issue"];
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", s(i, "id"), line(s(i, "title"), 120));
    let mut st = format!("state: {}", s(i, "state"));
    if !s(i, "reason").is_empty() {
        let _ = write!(st, " ({})", line(s(i, "reason"), 100));
    }
    let _ = write!(st, "   order: {}", i["order"]);
    if !s(i, "size").is_empty() {
        let _ = write!(st, "   size: {}", line(s(i, "size"), 20));
    }
    let _ = writeln!(out, "{st}");
    if !s(i, "spec").is_empty() {
        let _ = writeln!(out, "spec: {}", line(s(i, "spec"), 200));
    }
    if !s(i, "legacy").is_empty() {
        let _ = writeln!(out, "legacy: {}", line(s(i, "legacy"), 60));
    }
    if let Value::Object(_) = &v["claim"] {
        let cl = &v["claim"];
        if !s(cl, "session").is_empty() {
            let _ = writeln!(
                out,
                "claim: {} (generation {}, {}, lease until {})",
                s(cl, "session"),
                cl["generation"],
                s(cl, "status"),
                s(cl, "lease_until")
            );
        }
    }
    if v["ready"].as_bool() == Some(true) {
        let _ = writeln!(out, "ready: yes");
    } else if let Some(b) = v["blockers"].as_array() {
        let r: Vec<String> = b.iter().map(|x| line(s(x, "message"), 100)).collect();
        let _ = writeln!(out, "not ready: {}", r.join("; "));
    }
    let l = &v["links"];
    render_refs(&mut out, "needs", &l["needs"]);
    render_refs(&mut out, "needed by", &l["needed_by"]);
    render_refs(&mut out, "relates", &l["relates"]);
    render_refs(&mut out, "discovered from", &l["discovered_from"]);
    render_refs(&mut out, "discovered", &l["discovered"]);
    if l["parent"].is_object() {
        let _ = writeln!(out, "parent: {}", s(&l["parent"], "id"));
    }
    render_refs(&mut out, "children", &l["children"]);
    if !s(i, "summary").is_empty() {
        let _ = writeln!(out, "done summary: {}", line(s(i, "summary"), 200));
    }
    let body = crate::output::clean(s(i, "body"));
    if !body.trim().is_empty() {
        let _ = writeln!(out, "\n{}", body.trim_end());
    }
    if v["latest_handoff"].is_object() {
        let h = &v["latest_handoff"];
        let _ = writeln!(
            out,
            "\n--- latest handoff ({} by {}) ---\n{}",
            s(h, "at"),
            s(h, "session"),
            crate::output::clean(s(h, "body")).trim_end()
        );
    }
    if let Some(ns) = v["notes"].as_array()
        && !ns.is_empty()
    {
        let _ = writeln!(out, "\n--- recent notes ({} total) ---", v["note_count"]);
        for n in ns {
            let at = n["at_ms"]
                .as_i64()
                .map(|t| crate::time::rel(now, t))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "[{} {} {}] {}",
                s(n, "kind"),
                at,
                s(n, "session"),
                line(s(n, "body"), 200)
            );
        }
    }
    out
}

pub fn cmd_list(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(
        &[
            "state", "prefix", "ready", "claimed", "stale", "all", "limit",
        ],
        0,
    )?;
    let states: Vec<String> = a
        .opt("state")
        .map(|v| {
            v.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for st in &states {
        check_state(st, Some("x"))?;
    }
    let prefix = a.opt("prefix").map(|p| p.trim().to_ascii_uppercase());
    let limit = a.opt_i64("limit")?.unwrap_or(500).clamp(1, 100_000);
    let approvals = claims::load_approvals(env)?;
    let rows = db::read(c, |c| {
        let ready: HashSet<String> = if a.flag("ready") {
            claims::ready_ids(c, env, &approvals, false)?
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        let sql = format!(
            "SELECT {COLS}, c.session, c.generation, c.lease_until FROM issues
             LEFT JOIN claims c ON c.issue = issues.id
             ORDER BY ord, id"
        );
        let mut st = c.prepare(&sql)?;
        let mut out = Vec::new();
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut v = row_json(r)?;
            let id = s(&v, "id").to_string();
            let state = s(&v, "state").to_string();
            let session: Option<String> = r.get(15)?;
            let lease: Option<i64> = r.get(17)?;
            let claim_status = match (&session, lease) {
                (Some(_), Some(l)) if l > env.now => "live",
                (Some(_), _) => "stale",
                _ => "",
            };
            if !states.is_empty() {
                if !states.contains(&state) {
                    continue;
                }
            } else if !a.flag("all") && !a.flag("ready") && (state == "done" || state == "dropped")
            {
                continue;
            }
            if let Some(p) = &prefix
                && !id.starts_with(&format!("{p}-"))
            {
                continue;
            }
            if a.flag("ready") && !ready.contains(&id) {
                continue;
            }
            if a.flag("claimed") && claim_status != "live" {
                continue;
            }
            if a.flag("stale") && claim_status != "stale" {
                continue;
            }
            if let Some(o) = v.as_object_mut() {
                o.remove("body");
                o.insert(
                    "claim".to_string(),
                    match &session {
                        Some(sn) => json!({
                            "session": sn,
                            "generation": r.get::<_, i64>(16)?,
                            "lease_until": iso_opt(lease),
                            "status": claim_status,
                        }),
                        None => Value::Null,
                    },
                );
            }
            out.push(v);
            if i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
                break;
            }
        }
        Ok(out)
    })?;
    let mut text = String::new();
    for v in &rows {
        let claim = match &v["claim"] {
            Value::Object(_) => format!(
                "  [{} {}]",
                s(&v["claim"], "status"),
                s(&v["claim"], "session")
            ),
            _ => String::new(),
        };
        let _ = writeln!(
            text,
            "{:<14} {:<9} {}{}",
            s(v, "id"),
            s(v, "state"),
            line(s(v, "title"), 70),
            claim
        );
    }
    if rows.is_empty() {
        text.push_str("no issues match");
    }
    Ok(Output::new(
        json!({ "count": rows.len(), "issues": rows }),
        text,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        db::migrate(&c).unwrap();
        c
    }

    fn add(c: &Connection, id: &str, ord: i64) {
        c.execute(
            "INSERT INTO issues (id, title, state, ord, created_at, updated_at) VALUES (?1, 't', 'ready', ?2, 0, 0)",
            params![id, ord],
        )
        .unwrap();
    }

    #[test]
    fn needs_cycle_is_refused() {
        let c = db();
        add(&c, "A-1-00", 1);
        add(&c, "B-1-00", 2);
        add(&c, "C-1-00", 3);
        assert!(add_link(&c, "A-1-00", "needs", "B-1-00").unwrap());
        assert!(add_link(&c, "B-1-00", "needs", "C-1-00").unwrap());
        assert_eq!(
            add_link(&c, "C-1-00", "needs", "A-1-00").unwrap_err().code,
            "CYCLE"
        );
        assert_eq!(
            add_link(&c, "A-1-00", "needs", "A-1-00").unwrap_err().code,
            "CYCLE"
        );
        // relates links never form a blocking cycle.
        assert!(add_link(&c, "C-1-00", "relates", "A-1-00").unwrap());
    }

    #[test]
    fn placing_between_neighbours_and_rebalancing() {
        let c = db();
        add(&c, "AA-1-00", 1000);
        add(&c, "AA-1-01", 1001);
        // No gap between 1000 and 1001: forces a rebalance.
        let o = place(&c, &Place::After("AA-1-00"), None).unwrap();
        let a = ord_of(&c, "AA-1-00").unwrap();
        let b = ord_of(&c, "AA-1-01").unwrap();
        assert!(a < o && o < b, "{a} < {o} < {b}");
        let top = place(&c, &Place::Top, None).unwrap();
        assert!(top < a);
        let before = place(&c, &Place::Before("AA-1-01"), None).unwrap();
        assert!(a < before && before < b);
    }

    #[test]
    fn reasons_required_for_some_states() {
        assert!(check_state("blocked", None).is_err());
        assert!(check_state("blocked", Some(" ")).is_err());
        assert!(check_state("blocked", Some("waiting on API")).is_ok());
        assert!(check_state("ready", None).is_ok());
        assert!(check_state("bogus", None).is_err());
    }
}
