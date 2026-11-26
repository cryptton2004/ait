//! `ait export` / `ait import`: a readable, deterministic JSONL dump of the
//! whole database (sorted rows, sorted keys), so export -> import -> export
//! is byte-for-byte identical.

use crate::args::Args;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::output::Output;
use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value, json};

const FORMAT: i64 = 1;

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
        ],
        "id",
    ),
    ("retired", "retired_ids", &["id", "at"], "id"),
    ("link", "links", &["src", "kind", "dst"], "src, kind, dst"),
    (
        "claim",
        "claims",
        &[
            "issue",
            "session",
            "generation",
            "claimed_at",
            "lease_until",
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
    ("meta", "meta", &["key", "value"], "key"),
];

/// Meta keys that describe this copy of the DB, not its content.
const LOCAL_META: &[&str] = &["last_backup_day"];

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
    let header = json!({"type": "ait-export", "format": FORMAT, "schema": db::schema_version()});
    out.push_str(&header.to_string());
    out.push('\n');
    let mut n = 0;
    for (kind, table, cols, order) in TABLES {
        let sql = format!("SELECT {} FROM {table} ORDER BY {order}", cols.join(", "));
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut m = Map::new();
            m.insert("type".to_string(), Value::String((*kind).to_string()));
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

fn to_sql(v: &Value, line: usize, col: &str) -> Result<Sql> {
    match v {
        Value::Null => Ok(Sql::Null),
        Value::String(s) => Ok(Sql::Text(s.clone())),
        Value::Number(n) => n.as_i64().map(Sql::Integer).ok_or_else(|| {
            AitError::new(
                "INVALID_INPUT",
                format!("line {line}: {col} must be a whole number"),
            )
        }),
        _ => Err(AitError::new(
            "INVALID_INPUT",
            format!("line {line}: {col} must be text, a number or null"),
        )),
    }
}

pub fn import_str(c: &Connection, text: &str) -> Result<usize> {
    db::write(c, |c| {
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
        let mut n = 0;
        let mut saw_header = false;
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            if raw.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(raw)
                .map_err(|e| AitError::new("INVALID_INPUT", format!("line {ln}: {e}")))?;
            let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
            if !saw_header {
                if kind != "ait-export" {
                    return Err(AitError::new(
                        "INVALID_INPUT",
                        "line 1 must be the ait-export header",
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
            let (_, table, cols, _) = TABLES.iter().find(|t| t.0 == kind).ok_or_else(|| {
                AitError::new("INVALID_INPUT", format!("line {ln}: unknown type '{kind}'"))
            })?;
            let obj = v.as_object().ok_or_else(|| {
                AitError::new("INVALID_INPUT", format!("line {ln}: not an object"))
            })?;
            let vals = cols
                .iter()
                .map(|col| to_sql(obj.get(*col).unwrap_or(&Value::Null), ln, col))
                .collect::<Result<Vec<_>>>()?;
            let marks: Vec<String> = (1..=cols.len()).map(|k| format!("?{k}")).collect();
            let verb = if *table == "meta" || *table == "sessions" {
                "INSERT OR REPLACE"
            } else {
                "INSERT"
            };
            let sql = format!(
                "{verb} INTO {table} ({}) VALUES ({})",
                cols.join(", "),
                marks.join(", ")
            );
            c.execute(&sql, rusqlite::params_from_iter(vals))
                .map_err(|e| AitError::new("INVALID_INPUT", format!("line {ln}: {e}")))?;
            n += 1;
        }
        if !saw_header {
            return Err(AitError::new("INVALID_INPUT", "empty export file"));
        }
        // No "import" event: the restored copy must export byte-for-byte
        // identical to the original.
        Ok(n)
    })
}

pub fn cmd_import(c: &Connection, _env: &Env, a: &Args) -> Result<Output> {
    a.expect(&[], 1)?;
    let path = a.req_pos(0, "export file (JSONL)")?;
    let text = if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        s
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| AitError::new("INVALID_INPUT", format!("cannot read {path}: {e}")))?
    };
    let n = import_str(c, &text)?;
    Ok(Output::new(
        json!({"imported": n}),
        format!("imported {n} records"),
    ))
}
