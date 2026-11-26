//! `ait export` / `ait import`: a readable, deterministic JSONL dump of the
//! whole database (sorted rows, sorted keys), so export -> import -> export
//! is byte-for-byte identical.
//!
//! Both imports validate everything inside one transaction and write nothing
//! if any problem is found; every problem is listed in the error.
//! `ait import --issues <file>` loads a simple one-issue-per-line inventory
//! (the shape documented in README.md) instead of an export.

use crate::args::Args;
use crate::check::{self, Checks};
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::ids;
use crate::issues::{self, PRIORITIES, STATES, TYPES};
use crate::output::{Output, line, s};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Format 2 names each record with `record` (format 1 used `type`, which
/// is now an issue field); both are read.
const FORMAT: i64 = 2;

/// (record type, table, columns, order by)
const TABLES: &[(&str, &str, &[&str], &str)] = &[
    (
        "issue",
        "issues",
        &[
            "id",
            "title",
            "body",
            "state",
            "reason",
            "ord",
            "size",
            "spec",
            "parent",
            "legacy",
            "summary",
            "version",
            "created_at",
            "updated_at",
            "done_at",
            "type",
            "priority",
            "agent",
            "effort",
            "reviewer",
            "release",
            "ran",
            "released_at",
            "release_ref",
        ],
        "id",
    ),
    ("retired", "retired_ids", &["id", "at"], "id"),
    (
        "link",
        "links",
        &["src", "kind", "dst", "until_state"],
        "src, kind, dst",
    ),
    (
        "claim",
        "claims",
        &[
            "issue",
            "session",
            "generation",
            "claimed_at",
            "lease_until",
            "lease_ms",
        ],
        "issue",
    ),
    (
        "session",
        "sessions",
        &["name", "machine", "started", "last_seen"],
        "name",
    ),
    (
        "note",
        "notes",
        &["id", "issue", "session", "kind", "body", "generation", "at"],
        "id",
    ),
    (
        "event",
        "events",
        &["seq", "at", "session", "issue", "action", "details"],
        "seq",
    ),
    (
        "criterion",
        "criteria",
        &[
            "id",
            "issue",
            "pos",
            "text",
            "done_at",
            "done_by",
            "created_at",
        ],
        "id",
    ),
    ("meta", "meta", &["key", "value"], "key"),
];

/// Meta keys that describe this copy of the DB, not its content.
const LOCAL_META: &[&str] = &["last_backup_day"];

/// Problems listed in full in the error's `details`, and the first few in
/// its message.
fn invalid(problems: Vec<Value>, what: &str) -> AitError {
    let mut msg = format!(
        "{} problem(s) found; nothing was imported ({what})",
        problems.len()
    );
    for p in problems.iter().take(25) {
        let _ = write!(msg, "\n  {}", line(s(p, "message"), 200));
    }
    if problems.len() > 25 {
        let _ = write!(
            msg,
            "\n  ... and {} more (see details)",
            problems.len() - 25
        );
    }
    AitError::new("IMPORT_INVALID", msg).with_details(json!({ "problems": problems }))
}

fn problem(code: &str, message: String) -> Value {
    json!({"code": code, "message": message})
}

fn to_json(v: Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => json!(f),
        Sql::Text(t) => Value::String(t),
        Sql::Blob(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
    }
}

pub fn export_string(c: &Connection) -> Result<(String, usize)> {
    let mut out = String::new();
    let header = json!({"record": "ait-export", "format": FORMAT, "schema": db::schema_version()});
    out.push_str(&header.to_string());
    out.push('\n');
    let mut n = 0;
    for (kind, table, cols, order) in TABLES {
        let sql = format!("SELECT {} FROM {table} ORDER BY {order}", cols.join(", "));
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut m = Map::new();
            m.insert("record".to_string(), Value::String((*kind).to_string()));
            for (i, col) in cols.iter().enumerate() {
                m.insert((*col).to_string(), to_json(r.get::<_, Sql>(i)?));
            }
            if *kind == "meta"
                && LOCAL_META.contains(&m.get("key").and_then(Value::as_str).unwrap_or(""))
            {
                continue;
            }
            out.push_str(&Value::Object(m).to_string());
            out.push('\n');
            n += 1;
        }
    }
    Ok((out, n))
}

pub fn cmd_export(c: &Connection, _env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["out"], 0)?;
    let (text, n) = db::read(c, export_string)?;
    match a.opt("out") {
        Some(p) => {
            std::fs::write(p, text.as_bytes())?;
            Ok(Output::new(
                json!({"records": n, "out": p}),
                format!("exported {n} records to {p}"),
            ))
        }
        None => Ok(Output::new(json!({"records": n, "jsonl": text}), text)),
    }
}

fn to_sql(v: &Value) -> std::result::Result<Sql, &'static str> {
    match v {
        Value::Null => Ok(Sql::Null),
        Value::String(s) => Ok(Sql::Text(s.clone())),
        Value::Number(n) => n.as_i64().map(Sql::Integer).ok_or("must be a whole number"),
        _ => Err("must be text, a number or null"),
    }
}

fn require_empty(c: &Connection) -> Result<()> {
    let used: i64 = c.query_row(
        "SELECT (SELECT COUNT(*) FROM issues) + (SELECT COUNT(*) FROM events)
              + (SELECT COUNT(*) FROM notes) + (SELECT COUNT(*) FROM links)
              + (SELECT COUNT(*) FROM claims) + (SELECT COUNT(*) FROM retired_ids)",
        [],
        |r| r.get(0),
    )?;
    if used > 0 {
        return Err(AitError::new(
            "NOT_EMPTY",
            "import needs an empty database; use a new --db path",
        ));
    }
    Ok(())
}

/// Validates the data now in the transaction the way `ait check` would.
fn post_checks(c: &Connection, env: &Env, problems: &mut Vec<Value>) -> Result<()> {
    let opt = Checks {
        git: false,
        repo: None,
        approvals: false,
    };
    let (found, _) = check::run_checks(c, env, &opt)?;
    for p in found {
        problems.push(problem(s(&p, "code"), s(&p, "message").to_string()));
    }
    Ok(())
}

pub fn import_str(c: &Connection, env: &Env, text: &str) -> Result<usize> {
    db::write(c, |c| {
        require_empty(c)?;
        let mut problems = Vec::new();
        let mut n = 0;
        let mut saw_header = false;
        let mut key = "record";
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            if raw.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(e) => {
                    problems.push(problem("BAD_JSON", format!("line {ln}: {e}")));
                    continue;
                }
            };
            if !saw_header {
                if v.get("record").is_none() {
                    key = "type";
                }
                if v.get(key).and_then(Value::as_str) != Some("ait-export") {
                    return Err(AitError::new(
                        "INVALID_INPUT",
                        "line 1 must be the ait-export header (for a list of issues use `ait import --issues <file>`)",
                    ));
                }
                let schema = v.get("schema").and_then(Value::as_i64).unwrap_or(i64::MAX);
                if schema > db::schema_version() {
                    return Err(AitError::new(
                        "SCHEMA_TOO_NEW",
                        format!(
                            "export has schema {schema}; this ait knows {}",
                            db::schema_version()
                        ),
                    ));
                }
                saw_header = true;
                continue;
            }
            let kind = v.get(key).and_then(Value::as_str).unwrap_or("");
            let Some((_, table, cols, _)) = TABLES.iter().find(|t| t.0 == kind) else {
                problems.push(problem(
                    "BAD_RECORD",
                    format!("line {ln}: unknown type '{kind}'"),
                ));
                continue;
            };
            let Some(obj) = v.as_object() else {
                problems.push(problem("BAD_RECORD", format!("line {ln}: not an object")));
                continue;
            };
            let unknown: Vec<&String> = obj
                .keys()
                .filter(|k| *k != key && !cols.contains(&k.as_str()))
                .collect();
            if !unknown.is_empty() {
                let names: Vec<&str> = unknown.iter().map(|k| k.as_str()).collect();
                problems.push(problem(
                    "BAD_RECORD",
                    format!(
                        "line {ln}: unknown field(s) {} for a {kind}",
                        names.join(", ")
                    ),
                ));
                continue;
            }
            // Columns missing from older exports take their defaults.
            let mut names = Vec::new();
            let mut vals = Vec::new();
            let mut bad = false;
            for col in cols.iter() {
                if let Some(x) = obj.get(*col) {
                    match to_sql(x) {
                        Ok(v) => {
                            names.push(*col);
                            vals.push(v);
                        }
                        Err(e) => {
                            problems.push(problem("BAD_RECORD", format!("line {ln}: {col} {e}")));
                            bad = true;
                        }
                    }
                }
            }
            if bad {
                continue;
            }
            let marks: Vec<String> = (1..=names.len()).map(|k| format!("?{k}")).collect();
            let verb = if *table == "meta" || *table == "sessions" {
                "INSERT OR REPLACE"
            } else {
                "INSERT"
            };
            let sql = format!(
                "{verb} INTO {table} ({}) VALUES ({})",
                names.join(", "),
                marks.join(", ")
            );
            match c.execute(&sql, rusqlite::params_from_iter(vals)) {
                Ok(_) => n += 1,
                Err(e) => problems.push(problem("BAD_RECORD", format!("line {ln}: {e}"))),
            }
        }
        if !saw_header {
            return Err(AitError::new("INVALID_INPUT", "empty export file"));
        }
        post_checks(c, env, &mut problems)?;
        if !problems.is_empty() {
            return Err(invalid(problems, "export"));
        }
        // No "import" event: the restored copy must export byte-for-byte
        // identical to the original.
        Ok(n)
    })
}

// ------------------------------------------------------------ --issues

/// Default id prefixes for records that bring no id of their own.
const DEFAULT_PREFIX_FOR: &[(&str, &str)] = &[
    ("task", "TASK"),
    ("feature", "FEAT"),
    ("milestone", "MS"),
    ("release", "REL"),
];

fn parse_prefix_for(v: Option<&str>) -> Result<HashMap<String, String>> {
    let mut m: HashMap<String, String> = DEFAULT_PREFIX_FOR
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    if let Some(v) = v {
        for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, p) = part.split_once('=').ok_or_else(|| {
                AitError::usage(
                    "--prefix-for takes type=PREFIX pairs, e.g. feature=EPIC,release=REL",
                )
            })?;
            let k = k.trim().to_ascii_lowercase();
            let p = p.trim().to_ascii_uppercase();
            issues::check_type(&k)?;
            if !ids::is_prefix(&p) {
                return Err(AitError::usage(format!(
                    "'{p}' is not a prefix (2-5 letters)"
                )));
            }
            m.insert(k, p);
        }
    }
    Ok(m)
}

struct Rec {
    line: usize,
    v: Map<String, Value>,
    id: String,
    kind: String,
    legacy: Option<String>,
}

fn str_field<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
}

fn list_field(o: &Map<String, Value>, k: &str) -> std::result::Result<Vec<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(|s| s.trim().to_string())
                    .ok_or_else(|| format!("{k} must list strings"))
            })
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(vec![s.trim().to_string()]),
        Some(Value::String(_)) => Ok(Vec::new()),
        Some(_) => Err(format!("{k} must be a list of strings")),
    }
}

const ISSUE_KEYS: &[&str] = &[
    "id",
    "legacy",
    "type",
    "kind",
    "title",
    "body",
    "state",
    "reason",
    "parent",
    "needs",
    "relates",
    "release",
    "labels",
    "size",
    "spec",
    "order",
    "priority",
    "agent",
    "effort",
    "reviewer",
    "done_summary",
    "summary",
    "source",
    "criteria",
];

pub fn import_issues(
    c: &Connection,
    env: &Env,
    text: &str,
    prefix_for: &HashMap<String, String>,
) -> Result<Value> {
    db::write(c, |c| {
        require_empty(c)?;
        let mut problems: Vec<Value> = Vec::new();
        let mut recs: Vec<Rec> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            if raw.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(e) => {
                    problems.push(problem("BAD_JSON", format!("line {ln}: {e}")));
                    continue;
                }
            };
            let Value::Object(o) = v else {
                problems.push(problem("BAD_RECORD", format!("line {ln}: not an object")));
                continue;
            };
            if o.get("record").and_then(Value::as_str) == Some("ait-export")
                || o.get("type").and_then(Value::as_str) == Some("ait-export")
            {
                return Err(AitError::new(
                    "INVALID_INPUT",
                    "this is an ait export; import it with `ait import <file>` (without --issues)",
                ));
            }
            let unknown: Vec<&str> = o
                .keys()
                .map(String::as_str)
                .filter(|k| !ISSUE_KEYS.contains(k))
                .collect();
            if !unknown.is_empty() {
                problems.push(problem(
                    "BAD_RECORD",
                    format!("line {ln}: unknown field(s) {}", unknown.join(", ")),
                ));
            }
            let kind = str_field(&o, "type")
                .or_else(|| str_field(&o, "kind"))
                .unwrap_or("task")
                .to_ascii_lowercase();
            if !TYPES.contains(&kind.as_str()) {
                problems.push(problem(
                    "BAD_TYPE",
                    format!(
                        "line {ln}: unknown type '{kind}' (use {})",
                        TYPES.join(", ")
                    ),
                ));
            }
            let legacy = str_field(&o, "legacy").map(str::to_string);
            let explicit = str_field(&o, "id")
                .or_else(|| {
                    legacy
                        .as_deref()
                        .filter(|l| ids::is_id(&l.to_ascii_uppercase()))
                })
                .map(str::to_ascii_uppercase);
            if let Some(id) = &explicit
                && !ids::is_id(id)
            {
                problems.push(problem(
                    "BAD_ID",
                    format!("line {ln}: '{id}' is not a valid id"),
                ));
            }
            recs.push(Rec {
                line: ln,
                id: explicit.unwrap_or_default(),
                kind,
                legacy,
                v: o,
            });
        }
        // Give records without an id one in their type's prefix.
        let mut taken: HashSet<String> = recs
            .iter()
            .map(|r| r.id.clone())
            .filter(|i| !i.is_empty())
            .collect();
        let mut next_n: HashMap<String, u32> = HashMap::new();
        let mut generated: Vec<Value> = Vec::new();
        for r in recs.iter_mut().filter(|r| r.id.is_empty()) {
            let prefix = prefix_for
                .get(&r.kind)
                .cloned()
                .unwrap_or_else(|| "TASK".to_string());
            let n = next_n.entry(prefix.clone()).or_insert(0);
            loop {
                let (g, nn) = (*n / 100 + 1, *n % 100);
                *n += 1;
                let cand = ids::format_id(&prefix, g, nn);
                if taken.insert(cand.clone()) {
                    r.id = cand;
                    break;
                }
            }
            generated.push(json!({"line": r.line, "legacy": r.legacy, "id": r.id}));
        }
        // Each id once, each legacy once; legacy equal to the id is dropped.
        let mut seen_id: HashMap<String, usize> = HashMap::new();
        let mut by_ref: HashMap<String, String> = HashMap::new();
        for r in &recs {
            if let Some(first) = seen_id.insert(r.id.clone(), r.line) {
                problems.push(problem(
                    "DUPLICATE_ID",
                    format!("line {}: id {} is also on line {first}", r.line, r.id),
                ));
            }
            // The first record keeps a duplicated legacy value.
            if let Some(l) = &r.legacy
                && let other = by_ref
                    .entry(l.to_lowercase())
                    .or_insert_with(|| r.id.clone())
                    .clone()
                && other != r.id
            {
                problems.push(problem(
                    "LEGACY_DUP",
                    format!("line {}: legacy '{l}' is also used by {other}", r.line),
                ));
            }
            by_ref
                .entry(r.id.to_lowercase())
                .or_insert_with(|| r.id.clone());
        }
        if !env.cfg.prefixes.is_empty() {
            let mut bad: Vec<String> = recs
                .iter()
                .filter_map(|r| ids::parse_id(&r.id).map(|(p, _, _)| p.to_string()))
                .filter(|p| !env.cfg.prefixes.contains(p))
                .collect();
            bad.sort();
            bad.dedup();
            if !bad.is_empty() {
                problems.push(problem(
                    "UNKNOWN_PREFIX",
                    format!(
                        "prefix(es) {} are not configured (add them to prefixes = or use --prefix-for)",
                        bad.join(", ")
                    ),
                ));
            }
        }
        let resolve = |r: &str| by_ref.get(&r.to_lowercase()).cloned();
        let mut links: Vec<(String, &'static str, String, Option<String>)> = Vec::new();
        let mut inserted = 0;
        for r in &recs {
            let o = &r.v;
            let ln = r.line;
            let mut errs: Vec<String> = Vec::new();
            let title = match str_field(o, "title").map(issues::check_title) {
                Some(Ok(t)) => t,
                Some(Err(e)) => {
                    errs.push(e.message);
                    String::new()
                }
                None => {
                    errs.push("title is missing".to_string());
                    String::new()
                }
            };
            let state = str_field(o, "state")
                .unwrap_or("ready")
                .to_ascii_lowercase();
            let reason = str_field(o, "reason");
            if !STATES.contains(&state.as_str()) {
                errs.push(format!("unknown state '{state}'"));
            } else if let Err(e) = issues::check_state(&state, reason) {
                errs.push(e.message);
            }
            let priority = str_field(o, "priority")
                .unwrap_or("P2")
                .to_ascii_uppercase();
            if !PRIORITIES.contains(&priority.as_str()) {
                errs.push(format!("unknown priority '{priority}'"));
            }
            let agent = str_field(o, "agent").map(str::to_ascii_lowercase);
            if let Some(x) = &agent
                && let Err(e) = issues::check_agent(x, &env.cfg)
            {
                errs.push(e.message);
            }
            let effort = str_field(o, "effort").map(str::to_ascii_lowercase);
            if let Some(x) = &effort
                && let Err(e) = issues::check_effort(x)
            {
                errs.push(e.message);
            }
            let reviewer = str_field(o, "reviewer");
            let mut refs = |k: &str| -> Vec<String> {
                match list_field(o, k) {
                    Err(e) => {
                        errs.push(e);
                        Vec::new()
                    }
                    Ok(v) => v
                        .iter()
                        .map(|x| {
                            // `ref:state` waits for that state (needs only).
                            let (r, until) = issues::split_until(x);
                            let found = resolve(r).map(|id| match until {
                                Some(u) => format!("{id}:{u}"),
                                None => id,
                            });
                            (found, x)
                        })
                        .filter_map(|(found, x)| match found {
                            Some(id) => Some(id),
                            None => {
                                errs.push(format!("{k} '{x}' matches no record in the file"));
                                None
                            }
                        })
                        .collect(),
                }
            };
            let parent = refs("parent").into_iter().next();
            let release = refs("release").into_iter().next();
            let needs = refs("needs");
            let relates = refs("relates");
            let labels = list_field(o, "labels").unwrap_or_default();
            let order = match o.get("order") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => {
                    n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64))
                }
                Some(_) => {
                    errs.push("order must be a number".to_string());
                    None
                }
            };
            if !errs.is_empty() {
                for e in errs {
                    problems.push(problem("BAD_RECORD", format!("line {ln} ({}): {e}", r.id)));
                }
                continue;
            }
            let mut body = o
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !labels.is_empty() {
                body = format!("{}\n\nLabels: {}", body.trim_end(), labels.join(", "));
            }
            if let Some(src) = str_field(o, "source") {
                body = format!("{}\n\nSource: {src}", body.trim_end());
            }
            let done = state == "done";
            let summary = str_field(o, "done_summary")
                .or_else(|| str_field(o, "summary"))
                .map(str::to_string)
                .or_else(|| done.then(|| "imported as done".to_string()));
            let legacy = r.legacy.as_ref().filter(|l| !l.eq_ignore_ascii_case(&r.id));
            let ord = order.unwrap_or(1_000_000_000 + i64::try_from(ln).unwrap_or(0));
            let res = c.execute(
                "INSERT INTO issues (id, title, body, state, reason, ord, size, spec, parent, legacy,
                                     summary, version, created_at, updated_at, done_at, type,
                                     priority, agent, effort, reviewer, release)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19)",
                params![
                    r.id,
                    title,
                    body.trim_start(),
                    state,
                    reason,
                    ord,
                    str_field(o, "size"),
                    str_field(o, "spec"),
                    parent,
                    legacy,
                    summary,
                    env.now,
                    done.then_some(env.now),
                    r.kind,
                    priority,
                    agent,
                    effort,
                    reviewer,
                    release,
                ],
            );
            match res {
                Ok(_) => inserted += 1,
                Err(e) => {
                    problems.push(problem("BAD_RECORD", format!("line {ln} ({}): {e}", r.id)));
                    continue;
                }
            }
            for n in needs {
                let (id, until) = match n.split_once(':') {
                    Some((i, u)) => (i.to_string(), Some(u.to_string())),
                    None => (n, None),
                };
                links.push((r.id.clone(), "needs", id, until));
            }
            for n in relates {
                let id = n.split(':').next().unwrap_or_default().to_string();
                links.push((r.id.clone(), "relates", id, None));
            }
            match list_field(o, "criteria") {
                Ok(list) => {
                    let texts = crate::criteria::parse_lines(&list.join("\n"))?;
                    crate::criteria::add_all(c, env, &r.id, &texts)?;
                    if state == "done" || state == "released" {
                        c.execute(
                            "UPDATE criteria SET done_at = ?2 WHERE issue = ?1",
                            params![r.id, env.now],
                        )?;
                    }
                }
                Err(e) => {
                    problems.push(problem("BAD_RECORD", format!("line {ln} ({}): {e}", r.id)))
                }
            }
        }
        for (src, kind, dst, until) in &links {
            c.execute(
                "INSERT OR IGNORE INTO links(src, kind, dst, until_state) VALUES (?1, ?2, ?3, ?4)",
                params![src, kind, dst, until.as_deref().filter(|u| *u != "done")],
            )?;
        }
        post_checks(c, env, &mut problems)?;
        if !problems.is_empty() {
            return Err(invalid(problems, "inventory"));
        }
        db::event(
            c,
            env,
            None,
            "import",
            json!({"issues": inserted, "links": links.len(), "generated_ids": generated.len()}),
        )?;
        Ok(json!({"imported": inserted, "links": links.len(), "generated_ids": generated}))
    })
}

pub fn cmd_import(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["issues", "prefix-for"], 1)?;
    let path = a.req_pos(0, "file (JSONL)")?;
    let text = if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        s
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| AitError::new("INVALID_INPUT", format!("cannot read {path}: {e}")))?
    };
    if a.flag("issues") {
        let pf = parse_prefix_for(a.opt("prefix-for"))?;
        let v = import_issues(c, env, &text, &pf)?;
        let mut t = format!("imported {} issues and {} links", v["imported"], v["links"]);
        if let Some(g) = v["generated_ids"].as_array()
            && !g.is_empty()
        {
            let _ = write!(t, "\nnew ids for records without one:");
            for x in g {
                let _ = write!(t, "\n  {}  <- {}", s(x, "id"), line(s(x, "legacy"), 80));
            }
        }
        return Ok(Output::new(v, t));
    }
    if a.opt("prefix-for").is_some() {
        return Err(AitError::usage("--prefix-for only applies with --issues"));
    }
    let n = import_str(c, env, &text)?;
    Ok(Output::new(
        json!({"imported": n}),
        format!("imported {n} records"),
    ))
}
