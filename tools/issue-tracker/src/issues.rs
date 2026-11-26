//! Planning commands: add, edit, state, delete, move, link, unlink, show, list.

use crate::args::Args;
use crate::claims;
use crate::config::{Config, valid_word};
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::ids;
use crate::notes;
use crate::output::{Output, line, s};
use crate::roadmap;
use crate::time::{iso, iso_opt};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

pub const STATES: &[&str] = &[
    "idea",
    "planned",
    "ready",
    "blocked",
    "decision",
    "dropped",
    "in_review",
    "done",
    "released",
];
/// States that count as finished work.
pub const FINISHED: &[&str] = &["done", "released"];
/// States a `needs` link may wait for (`--needs ID:in_review`).
pub const UNTIL_STATES: &[&str] = &["planned", "ready", "in_review", "done", "released"];

/// SQL rank of a state for "has reached": idea < planned < ready (and
/// blocked, decision) < in_review < done < released; dropped never reaches.
pub fn rank_sql(expr: &str) -> String {
    format!(
        "(CASE {expr} WHEN 'idea' THEN 0 WHEN 'planned' THEN 1 WHEN 'ready' THEN 2 \
         WHEN 'blocked' THEN 2 WHEN 'decision' THEN 2 WHEN 'in_review' THEN 3 \
         WHEN 'done' THEN 4 WHEN 'released' THEN 5 ELSE -1 END)"
    )
}

/// SQL condition, over `links l` joined to the needed issue `d`: the need is
/// not met yet.
pub fn need_unmet_sql() -> String {
    format!(
        "(d.state IS NULL OR {} < {})",
        rank_sql("d.state"),
        rank_sql("COALESCE(l.until_state, 'done')")
    )
}
pub const NEEDS_REASON: &[&str] = &["blocked", "decision", "dropped"];
pub const LINK_KINDS: &[&str] = &["needs", "relates", "discovered_from"];
/// Issue types. Releases and milestones can be the `release` of other issues.
/// `review` issues are checks (code, security, legal...) on their parent:
/// they start only while the parent is `in_review`.
pub const TYPES: &[&str] = &["task", "feature", "milestone", "release", "review"];
pub const PRIORITIES: &[&str] = &["P0", "P1", "P2", "P3", "P4"];
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

const MAX_TITLE: usize = 300;
const MAX_BODY: usize = 1_000_000;

pub const COLS: &str = "id, title, body, state, reason, ord, size, spec, parent, legacy, summary, version, created_at, updated_at, done_at, type, priority, agent, effort, reviewer, release, ran, released_at, release_ref";
/// Number of columns in `COLS` (extra joined columns start at this index).
pub const NCOLS: usize = 24;

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
        "type": r.get::<_, String>(15)?,
        "priority": r.get::<_, String>(16)?,
        "agent": r.get::<_, Option<String>>(17)?,
        "effort": r.get::<_, Option<String>>(18)?,
        "reviewer": r.get::<_, Option<String>>(19)?,
        "release": r.get::<_, Option<String>>(20)?,
        "ran": r.get::<_, Option<String>>(21)?,
        "released_at": iso_opt(r.get::<_, Option<i64>>(22)?),
        "release_ref": r.get::<_, Option<String>>(23)?,
    }))
}

/// Adds `agent_model`: the model the configured tier maps to.
pub fn with_model(mut v: Value, cfg: &Config) -> Value {
    let model = v
        .get("agent")
        .and_then(Value::as_str)
        .and_then(|t| cfg.model_for(t))
        .map(str::to_string);
    if let Some(o) = v.as_object_mut() {
        o.insert("agent_model".to_string(), json!(model));
    }
    v
}

/// One line describing the recommended agent: `agent: deep (opus), effort high, reviewer codex`.
pub fn agent_line(i: &Value) -> String {
    let mut parts = Vec::new();
    if !s(i, "agent").is_empty() {
        let model = s(i, "agent_model");
        if model.is_empty() {
            parts.push(format!("agent: {}", s(i, "agent")));
        } else {
            parts.push(format!("agent: {} ({model})", s(i, "agent")));
        }
    }
    if !s(i, "effort").is_empty() {
        parts.push(format!("effort {}", s(i, "effort")));
    }
    if !s(i, "reviewer").is_empty() {
        parts.push(format!("reviewer {}", line(s(i, "reviewer"), 40)));
    }
    parts.join(", ")
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

/// The issue with `agent_model` resolved from the config.
pub fn get_full(c: &Connection, cfg: &Config, id: &str) -> Result<Value> {
    Ok(with_model(get(c, id)?, cfg))
}

pub fn state_of(c: &Connection, id: &str) -> Result<String> {
    c.query_row("SELECT state FROM issues WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| AitError::not_found(id))
}

pub fn check_title(t: &str) -> Result<String> {
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

pub fn check_type(t: &str) -> Result<()> {
    if TYPES.contains(&t) {
        Ok(())
    } else {
        Err(AitError::new(
            "INVALID_INPUT",
            format!("unknown type '{t}' (use {})", TYPES.join(", ")),
        ))
    }
}

pub fn check_priority(p: &str) -> Result<()> {
    if PRIORITIES.contains(&p) {
        Ok(())
    } else {
        Err(AitError::new(
            "INVALID_INPUT",
            format!("unknown priority '{p}' (use P0 most urgent .. P4)"),
        ))
    }
}

pub fn check_effort(e: &str) -> Result<()> {
    if EFFORTS.contains(&e) {
        Ok(())
    } else {
        Err(AitError::new(
            "INVALID_INPUT",
            format!("unknown effort '{e}' (use {})", EFFORTS.join(", ")),
        ))
    }
}

/// An agent tier: one of the configured `agent.<tier>` keys, or any short
/// lowercase word when no tiers are configured.
pub fn check_agent(a: &str, cfg: &Config) -> Result<()> {
    if cfg.agents.is_empty() {
        if valid_word(a) {
            return Ok(());
        }
        return Err(AitError::new(
            "INVALID_INPUT",
            format!("agent '{a}' must be a short lowercase word such as deep or standard"),
        ));
    }
    if cfg.model_for(a).is_some() {
        Ok(())
    } else {
        let tiers: Vec<&str> = cfg.agents.iter().map(|(t, _)| t.as_str()).collect();
        Err(AitError::new(
            "INVALID_INPUT",
            format!(
                "unknown agent tier '{a}' (configured: {})",
                tiers.join(", ")
            ),
        ))
    }
}

pub fn check_reviewer(r: &str) -> Result<()> {
    if r.is_empty() || r.chars().count() > 64 || r.contains('\n') {
        Err(AitError::new(
            "INVALID_INPUT",
            "reviewer must be one line of at most 64 characters",
        ))
    } else {
        Ok(())
    }
}

/// `release` must point at an issue whose type is release or milestone.
fn check_release_target(c: &Connection, target: &str) -> Result<()> {
    let kind: String = c.query_row("SELECT type FROM issues WHERE id = ?1", [target], |r| {
        r.get(0)
    })?;
    if kind == "release" || kind == "milestone" {
        Ok(())
    } else {
        Err(AitError::new(
            "INVALID_INPUT",
            format!("{target} is a {kind}; --release must point at a release or milestone"),
        ))
    }
}

/// Lowercase option value, trimmed; `""` means "clear the field".
fn lower_opt(a: &Args, name: &str) -> Option<String> {
    a.opt(name).map(|v| v.trim().to_ascii_lowercase())
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

pub fn add_link(c: &Connection, src: &str, kind: &str, dst: &str) -> Result<bool> {
    add_link_until(c, src, kind, dst, None)
}

/// Adds a link; for `needs`, `until` is the state the other issue must reach
/// (default done). Re-linking with another `until` updates it.
pub fn add_link_until(
    c: &Connection,
    src: &str,
    kind: &str,
    dst: &str,
    until: Option<&str>,
) -> Result<bool> {
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
    let until = until.filter(|u| *u != "done" && kind == "needs");
    let before: Option<Option<String>> = c
        .query_row(
            "SELECT until_state FROM links WHERE src = ?1 AND kind = ?2 AND dst = ?3",
            params![src, kind, dst],
            |r| r.get(0),
        )
        .optional()?;
    match before {
        Some(b) if b.as_deref() == until => Ok(false),
        Some(_) => {
            c.execute(
                "UPDATE links SET until_state = ?4 WHERE src = ?1 AND kind = ?2 AND dst = ?3",
                params![src, kind, dst, until],
            )?;
            Ok(true)
        }
        None => {
            c.execute(
                "INSERT INTO links(src, kind, dst, until_state) VALUES (?1, ?2, ?3, ?4)",
                params![src, kind, dst, until],
            )?;
            Ok(true)
        }
    }
}

pub fn check_until(u: &str) -> Result<()> {
    if UNTIL_STATES.contains(&u) {
        Ok(())
    } else {
        Err(AitError::new(
            "INVALID_INPUT",
            format!(
                "a need can wait for {} (not '{u}')",
                UNTIL_STATES.join(", ")
            ),
        ))
    }
}

/// `A,B:in_review` -> [(A, None), (B, Some("in_review"))].
fn need_list(c: &Connection, v: Option<&str>) -> Result<Vec<(String, Option<String>)>> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    if let Some(v) = v {
        for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (r, until) = split_until(part);
            if let Some(u) = until {
                check_until(u)?;
            }
            let id = resolve(c, r)?;
            if !out.iter().any(|(x, _)| *x == id) {
                out.push((id, until.map(str::to_string)));
            }
        }
    }
    Ok(out)
}

/// `ID:state` -> (ID, Some(state)) when the suffix is a state name.
pub fn split_until(part: &str) -> (&str, Option<&str>) {
    match part.rsplit_once(':') {
        Some((r, u)) if STATES.contains(&u.trim()) => (r.trim(), Some(u.trim())),
        _ => (part, None),
    }
}

fn link_kind(k: &str) -> Result<&'static str> {
    match k {
        "needs" | "blocked-by" | "blocked_by" => Ok("needs"),
        "relates" => Ok("relates"),
        "discovered-from" | "discovered_from" => Ok("discovered_from"),
        _ => Err(AitError::usage(format!(
            "unknown link kind '{k}' (use needs, relates, discovered-from)"
        ))),
    }
}

fn brief_list(c: &Connection, sql: &str, id: &str) -> Result<Vec<Value>> {
    let mut st = c.prepare(sql)?;
    let with_until = st.column_count() > 3;
    let rows = st.query_map([id], |r| {
        let mut v = json!({
            "id": r.get::<_, String>(0)?,
            "title": r.get::<_, Option<String>>(1)?,
            "state": r.get::<_, Option<String>>(2)?,
        });
        if with_until && let Some(o) = v.as_object_mut() {
            let u: Option<String> = r.get(3)?;
            o.insert(
                "until".to_string(),
                json!(u.unwrap_or_else(|| "done".to_string())),
            );
        }
        Ok(v)
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
    let needs_sql = "SELECT l.dst, i.title, i.state, l.until_state FROM links l
                     LEFT JOIN issues i ON i.id = l.dst
                     WHERE l.src = ?1 AND l.kind = 'needs' ORDER BY l.dst";
    Ok(json!({
        "needs": brief_list(c, needs_sql, id)?,
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

/// Validated values of the per-issue planning fields given as options.
struct Fields {
    kind: Option<String>,
    priority: Option<String>,
    agent: Option<String>,
    effort: Option<String>,
    reviewer: Option<String>,
}

fn read_fields(a: &Args, cfg: &Config) -> Result<Fields> {
    let kind = lower_opt(a, "type");
    if let Some(k) = &kind {
        check_type(k)?;
    }
    let priority = a.opt("priority").map(|p| p.trim().to_ascii_uppercase());
    if let Some(p) = &priority {
        check_priority(p)?;
    }
    let agent = lower_opt(a, "agent");
    if let Some(x) = agent.as_deref().filter(|x| !x.is_empty()) {
        check_agent(x, cfg)?;
    }
    let effort = lower_opt(a, "effort");
    if let Some(x) = effort.as_deref().filter(|x| !x.is_empty()) {
        check_effort(x)?;
    }
    let reviewer = a.opt("reviewer").map(|r| r.trim().to_string());
    if let Some(x) = reviewer.as_deref().filter(|x| !x.is_empty()) {
        check_reviewer(x)?;
    }
    Ok(Fields {
        kind,
        priority,
        agent,
        effort,
        reviewer,
    })
}

fn none_if_empty(v: Option<String>) -> Option<String> {
    v.filter(|x| !x.is_empty())
}

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
            "type",
            "priority",
            "agent",
            "effort",
            "reviewer",
            "release",
            "criteria",
        ],
        2,
    )?;
    let spec = a.req_pos(0, "PREFIX (e.g. CORE), group (CORE-2) or full id")?;
    let new_criteria = a
        .opt("criteria")
        .map(crate::criteria::parse_lines)
        .transpose()?
        .unwrap_or_default();
    let title = check_title(a.req_pos(1, "title")?)?;
    let body = read_text(a, "body")?.unwrap_or_default();
    let state = a.opt("state").unwrap_or("ready");
    if ["done", "released", "in_review"].contains(&state) {
        return Err(AitError::new(
            "INVALID_STATE",
            format!("new issues cannot start as {state}"),
        ));
    }
    check_state(state, a.opt("reason"))?;
    let reason = a.opt("reason").filter(|r| !r.trim().is_empty());
    let f = read_fields(a, &env.cfg)?;
    if f.kind.as_deref() == Some("review") && a.opt("parent").is_none() {
        return Err(AitError::new(
            "INVALID_INPUT",
            "a review needs --parent <the issue it reviews>",
        ));
    }
    let issue = db::write(c, |c| {
        let after = a.opt("after").map(|x| resolve(c, x)).transpose()?;
        let parent = a.opt("parent").map(|x| resolve(c, x)).transpose()?;
        let release = a.opt("release").map(|x| resolve(c, x)).transpose()?;
        if let Some(r) = &release {
            check_release_target(c, r)?;
        }
        let needs = need_list(c, a.opt("needs"))?;
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
                                 version, created_at, updated_at, type, priority, agent, effort,
                                 reviewer, release)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                env.now,
                f.kind.as_deref().unwrap_or("task"),
                f.priority.as_deref().unwrap_or("P2"),
                none_if_empty(f.agent.clone()),
                none_if_empty(f.effort.clone()),
                none_if_empty(f.reviewer.clone()),
                release,
            ],
        )?;
        for (n, until) in &needs {
            add_link_until(c, &id, "needs", n, until.as_deref())?;
        }
        let needs: Vec<String> = needs
            .iter()
            .map(|(n, u)| match u {
                Some(u) => format!("{n}:{u}"),
                None => n.clone(),
            })
            .collect();
        if let Some(d) = &disc {
            add_link(c, &id, "discovered_from", d)?;
        }
        crate::criteria::add_all(c, env, &id, &new_criteria)?;
        db::event(
            c,
            env,
            Some(&id),
            "add",
            json!({"title": title, "state": state, "needs": needs, "discovered_from": disc}),
        )?;
        get_full(c, &env.cfg, &id)
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
            "type",
            "priority",
            "agent",
            "effort",
            "reviewer",
            "release",
            "force",
            "gen",
        ],
        1,
    )?;
    let input = a.req_pos(0, "issue id")?;
    let title = a.opt("title").map(check_title).transpose()?;
    let body = read_text(a, "body")?;
    let expected = a.opt_i64("version")?;
    let want = a.opt_i64("gen")?;
    let f = read_fields(a, &env.cfg)?;
    let issue = db::write(c, |c| {
        let id = resolve(c, input)?;
        claims::guard_write(c, env, &id, a.flag("force"), want)?;
        let (version, kind): (i64, String) = c.query_row(
            "SELECT version, type FROM issues WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
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
        if let Some(k) = &f.kind {
            if (kind == "release" || kind == "milestone") && k != "release" && k != "milestone" {
                let members: i64 = c.query_row(
                    "SELECT COUNT(*) FROM issues WHERE release = ?1",
                    [&id],
                    |r| r.get(0),
                )?;
                if members > 0 {
                    return Err(AitError::new(
                        "IN_USE",
                        format!(
                            "{members} issue(s) belong to {id} as their release; move them first"
                        ),
                    ));
                }
            }
            set("type", Some(k.clone()))?;
        }
        if let Some(p) = &f.priority {
            set("priority", Some(p.clone()))?;
        }
        if f.agent.is_some() {
            set("agent", none_if_empty(f.agent.clone()))?;
        }
        if f.effort.is_some() {
            set("effort", none_if_empty(f.effort.clone()))?;
        }
        if f.reviewer.is_some() {
            set("reviewer", none_if_empty(f.reviewer.clone()))?;
        }
        if let Some(v) = a.opt("release") {
            let r = if v.trim().is_empty() {
                None
            } else {
                let r = resolve(c, v)?;
                if r == id {
                    return Err(AitError::new("CYCLE", "an issue cannot be its own release"));
                }
                check_release_target(c, &r)?;
                Some(r)
            };
            set("release", r)?;
        }
        if changed.is_empty() {
            return Err(AitError::usage(
                "nothing to change (use --title, --body, --size, --spec, --parent, --legacy, --type, --priority, --agent, --effort, --reviewer, --release)",
            ));
        }
        c.execute(
            "UPDATE issues SET version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![id, env.now],
        )?;
        db::event(c, env, Some(&id), "edit", json!({ "fields": changed }))?;
        get_full(c, &env.cfg, &id)
    })?;
    let text = format!(
        "edited {} (version {})",
        s(&issue, "id"),
        issue.get("version").and_then(Value::as_i64).unwrap_or(0)
    );
    Ok(Output::new(json!({ "issue": issue }), text))
}

pub fn cmd_state(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["reason", "force", "gen"], 2)?;
    let input = a.req_pos(0, "issue id")?;
    let state = a.req_pos(1, "state")?;
    if state == "done" {
        return Err(AitError::new(
            "INVALID_STATE",
            "use `ait done <id> --summary \"...\"` to close an issue",
        ));
    }
    if state == "released" {
        return Err(AitError::new(
            "INVALID_STATE",
            "use `ait ship <id>` to mark finished work as released",
        ));
    }
    check_state(state, a.opt("reason"))?;
    let reason = a.opt("reason").filter(|r| !r.trim().is_empty());
    let force = a.flag("force");
    let want = a.opt_i64("gen")?;
    let issue = db::write(c, |c| {
        let id = resolve(c, input)?;
        claims::guard_write(c, env, &id, force, want)?;
        let from = state_of(c, &id)?;
        if FINISHED.contains(&from.as_str()) && !force {
            return Err(AitError::new(
                "ALREADY_DONE",
                format!("{id} is {from}; reopening it needs --force"),
            ));
        }
        if state == "in_review" {
            claims::submit_for_review(c, env, &id, force)?;
        }
        c.execute(
            "UPDATE issues SET state = ?2, reason = ?3, done_at = NULL, released_at = NULL,
                 release_ref = NULL, version = version + 1, updated_at = ?4 WHERE id = ?1",
            params![id, state, reason, env.now],
        )?;
        db::event(
            c,
            env,
            Some(&id),
            "state",
            json!({"from": from, "to": state, "reason": reason, "forced": force}),
        )?;
        let rolled = roadmap::auto_rollup(c, env, &id)?;
        Ok((get_full(c, &env.cfg, &id)?, rolled))
    })?;
    let (issue, rolled) = issue;
    let mut text = format!("{} is now {}", s(&issue, "id"), state);
    for r in &rolled {
        let _ = write!(
            text,
            "\n{} moved to {} (all children)",
            s(r, "id"),
            s(r, "to")
        );
    }
    Ok(Output::new(
        json!({ "issue": issue, "rollup": rolled }),
        text,
    ))
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
                  + (SELECT COUNT(*) FROM issues WHERE parent = ?1)
                  + (SELECT COUNT(*) FROM issues WHERE release = ?1)",
            [&id],
            |r| r.get(0),
        )?;
        if refs > 0 && !force {
            return Err(AitError::new(
                "IN_USE",
                format!(
                    "{refs} issue(s) need {id} or have it as parent or release; use --force to delete anyway"
                ),
            ));
        }
        c.execute("DELETE FROM links WHERE src = ?1 OR dst = ?1", [&id])?;
        c.execute("UPDATE issues SET parent = NULL WHERE parent = ?1", [&id])?;
        c.execute("UPDATE issues SET release = NULL WHERE release = ?1", [&id])?;
        c.execute("DELETE FROM claims WHERE issue = ?1", [&id])?;
        c.execute("DELETE FROM notes WHERE issue = ?1", [&id])?;
        c.execute("DELETE FROM criteria WHERE issue = ?1", [&id])?;
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
        get_full(c, &env.cfg, &id)
    })?;
    Ok(Output::new(
        json!({ "issue": issue }),
        format!("moved {}", s(&issue, "id")),
    ))
}

pub fn cmd_link(c: &Connection, env: &Env, a: &Args, remove: bool) -> Result<Output> {
    a.expect(&["until"], 3)?;
    let until = a.opt("until").map(str::trim);
    if let Some(u) = until {
        check_until(u)?;
    }
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
            if until.is_some() && kind != "needs" {
                return Err(AitError::usage("--until only applies to needs links"));
            }
            add_link_until(c, &src, kind, &dst, until)?
        };
        if changed {
            let action = if remove { "unlink" } else { "link" };
            db::event(
                c,
                env,
                Some(&src),
                action,
                json!({"kind": kind, "other": dst, "until": until}),
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
    let issue = get_full(c, &env.cfg, id)?;
    let claim = claims::claim_row(c, id)?.map(|r| claims::claim_json(&r, env.now));
    let approvals = claims::load_approvals(env)?;
    let blockers = claims::blockers(c, env, &approvals, id)?;
    let has_members: i64 = c.query_row(
        "SELECT COUNT(*) FROM issues WHERE parent = ?1 OR release = ?1",
        [id],
        |r| r.get(0),
    )?;
    let rollup = if s(&issue, "type") != "task" || has_members > 0 {
        roadmap::rollup(c, env, &approvals, id)?
    } else {
        Value::Null
    };
    Ok(json!({
        "issue": issue,
        "links": links_json(c, id)?,
        "claim": claim,
        "ready": blockers.is_empty(),
        "blockers": blockers,
        "rollup": rollup,
        "latest_handoff": notes::latest_handoff(c, id)?,
        "notes": notes::recent(c, id, note_limit)?,
        "note_count": notes::count(c, id)?,
        "criteria": crate::criteria::list(c, id)?,
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
    let mut st = format!(
        "{} {}  state: {}",
        s(i, "type"),
        s(i, "priority"),
        s(i, "state")
    );
    if !s(i, "reason").is_empty() {
        let _ = write!(st, " ({})", line(s(i, "reason"), 100));
    }
    let _ = write!(st, "   order: {}", i["order"]);
    if !s(i, "size").is_empty() {
        let _ = write!(st, "   size: {}", line(s(i, "size"), 20));
    }
    let _ = writeln!(out, "{st}");
    let agent = agent_line(i);
    if !agent.is_empty() {
        let _ = writeln!(out, "{agent}");
    }
    if !s(i, "release").is_empty() {
        let _ = writeln!(out, "release: {}", s(i, "release"));
    }
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
    if v["rollup"].is_object() {
        let _ = writeln!(out, "rollup: {}", roadmap::rollup_line(&v["rollup"]));
    }
    if let Some(cr) = v["criteria"].as_array()
        && !cr.is_empty()
    {
        let _ = writeln!(out, "acceptance criteria:");
        for x in cr {
            let mark = if x["done"].as_bool().unwrap_or(false) {
                "x"
            } else {
                " "
            };
            let _ = writeln!(out, "  [{mark}] {}. {}", x["n"], line(s(x, "text"), 160));
        }
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
    if !s(i, "ran").is_empty() {
        let _ = writeln!(out, "ran: {}", line(s(i, "ran"), 200));
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

fn csv(v: Option<&str>, upper: bool) -> Vec<String> {
    v.map(|v| {
        v.split(',')
            .map(|x| {
                let x = x.trim();
                if upper {
                    x.to_ascii_uppercase()
                } else {
                    x.to_ascii_lowercase()
                }
            })
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

pub fn cmd_list(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(
        &[
            "state", "prefix", "ready", "claimed", "stale", "all", "limit", "type", "priority",
            "agent", "parent", "release", "tree",
        ],
        0,
    )?;
    let states = csv(a.opt("state"), false);
    for st in &states {
        check_state(st, Some("x"))?;
    }
    let types = csv(a.opt("type"), false);
    for t in &types {
        check_type(t)?;
    }
    let priorities = csv(a.opt("priority"), true);
    for p in &priorities {
        check_priority(p)?;
    }
    let agents = csv(a.opt("agent"), false);
    let prefix = a.opt("prefix").map(|p| p.trim().to_ascii_uppercase());
    let limit = a.opt_i64("limit")?.unwrap_or(500).clamp(1, 100_000);
    let tree = a.flag("tree");
    let approvals = claims::load_approvals(env)?;
    let rows = db::read(c, |c| {
        let parent = a.opt("parent").map(|x| resolve(c, x)).transpose()?;
        let release = a.opt("release").map(|x| resolve(c, x)).transpose()?;
        let ready: HashSet<String> = if a.flag("ready") {
            claims::ready_ids(c, env, &approvals, false)?
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        let order = if tree { "ord, id" } else { "priority, ord, id" };
        let sql = format!(
            "SELECT {COLS}, c.session, c.generation, c.lease_until FROM issues
             LEFT JOIN claims c ON c.issue = issues.id
             ORDER BY {order}"
        );
        let mut st = c.prepare(&sql)?;
        let mut all: Vec<Value> = Vec::new();
        let mut pass: HashSet<String> = HashSet::new();
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut v = with_model(row_json(r)?, &env.cfg);
            let id = s(&v, "id").to_string();
            let state = s(&v, "state").to_string();
            let session: Option<String> = r.get(NCOLS)?;
            let lease: Option<i64> = r.get(NCOLS + 2)?;
            let claim_status = match (&session, lease) {
                (Some(_), Some(l)) if l > env.now => "live",
                (Some(_), _) => "stale",
                _ => "",
            };
            let hide_closed = !a.flag("all") && !a.flag("ready") && !tree;
            let ok = (if states.is_empty() {
                !(hide_closed && (state == "done" || state == "dropped"))
                    && !(tree && state == "dropped" && !a.flag("all"))
            } else {
                states.contains(&state)
            }) && prefix
                .as_ref()
                .is_none_or(|p| id.starts_with(&format!("{p}-")))
                && (types.is_empty() || types.iter().any(|t| t == s(&v, "type")))
                && (priorities.is_empty() || priorities.iter().any(|p| p == s(&v, "priority")))
                && (agents.is_empty() || agents.iter().any(|x| x == s(&v, "agent")))
                && parent.as_deref().is_none_or(|p| s(&v, "parent") == p)
                && release.as_deref().is_none_or(|p| s(&v, "release") == p)
                && (!a.flag("ready") || ready.contains(&id))
                && (!a.flag("claimed") || claim_status == "live")
                && (!a.flag("stale") || claim_status == "stale");
            if let Some(o) = v.as_object_mut() {
                o.remove("body");
                o.insert(
                    "claim".to_string(),
                    match &session {
                        Some(sn) => json!({
                            "session": sn,
                            "generation": r.get::<_, i64>(NCOLS + 1)?,
                            "lease_until": iso_opt(lease),
                            "status": claim_status,
                        }),
                        None => Value::Null,
                    },
                );
            }
            if ok {
                pass.insert(id);
            }
            all.push(v);
        }
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if !tree {
            return Ok(all
                .into_iter()
                .filter(|v| pass.contains(s(v, "id")))
                .take(limit)
                .collect::<Vec<_>>());
        }
        Ok(build_tree(all, &pass, parent.as_deref(), limit))
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
        let depth = usize::try_from(v["depth"].as_i64().unwrap_or(0)).unwrap_or(0);
        let kind = if s(v, "type") == "task" {
            String::new()
        } else {
            format!("[{}] ", s(v, "type"))
        };
        let _ = writeln!(
            text,
            "{}{:<12} {} {:<8} {}{}{}",
            "  ".repeat(depth),
            s(v, "id"),
            s(v, "priority"),
            s(v, "state"),
            kind,
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

/// Orders issues as a tree by `parent` (siblings by order), keeping every
/// issue that passed the filters plus its ancestors. Adds `depth`.
fn build_tree(
    all: Vec<Value>,
    pass: &HashSet<String>,
    root: Option<&str>,
    limit: usize,
) -> Vec<Value> {
    let parent_of: HashMap<String, String> = all
        .iter()
        .filter(|v| !s(v, "parent").is_empty())
        .map(|v| (s(v, "id").to_string(), s(v, "parent").to_string()))
        .collect();
    let mut keep: HashSet<String> = HashSet::new();
    for id in pass {
        let mut cur = Some(id.clone());
        let mut steps = 0;
        while let Some(x) = cur {
            if !keep.insert(x.clone()) || steps > 10_000 {
                break;
            }
            steps += 1;
            if root == Some(x.as_str()) {
                break;
            }
            cur = parent_of.get(&x).cloned();
        }
    }
    let ids: HashSet<String> = all.iter().map(|v| s(v, "id").to_string()).collect();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, v) in all.iter().enumerate() {
        let id = s(v, "id");
        if !keep.contains(id) {
            continue;
        }
        let p = s(v, "parent");
        let is_root = match root {
            Some(r) => p == r,
            None => p.is_empty() || !ids.contains(p) || !keep.contains(p),
        };
        if is_root {
            roots.push(i);
        } else {
            children.entry(p.to_string()).or_default().push(i);
        }
    }
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack: Vec<(usize, i64)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
    while let Some((i, depth)) = stack.pop() {
        if out.len() >= limit || !seen.insert(i) {
            continue;
        }
        let mut v = all[i].clone();
        let id = s(&v, "id").to_string();
        if let Some(o) = v.as_object_mut() {
            o.insert("depth".to_string(), json!(depth));
        }
        out.push(v);
        if let Some(kids) = children.get(&id) {
            for k in kids.iter().rev() {
                stack.push((*k, depth + 1));
            }
        }
    }
    out
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

    #[test]
    fn field_values() {
        assert!(check_priority("P0").is_ok());
        assert!(check_priority("P5").is_err());
        assert!(check_type("milestone").is_ok());
        assert!(check_type("epic").is_err());
        assert!(check_effort("xhigh").is_ok());
        assert!(check_effort("huge").is_err());
        let mut cfg = Config::default();
        assert!(check_agent("deep", &cfg).is_ok());
        assert!(check_agent("Deep One", &cfg).is_err());
        cfg.agents.push(("light".into(), "haiku".into()));
        assert!(check_agent("deep", &cfg).is_err());
        assert!(check_agent("light", &cfg).is_ok());
    }
}
