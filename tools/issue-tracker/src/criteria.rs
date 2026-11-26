//! Acceptance criteria: a checklist on an issue. Every criterion must be
//! checked before the issue moves to in_review or done (unless --force).

use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::issues;
use crate::output::{Output, line, s};
use crate::time::iso_opt;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fmt::Write as _;

const MAX_TEXT: usize = 500;

/// One criterion per non-empty line.
pub fn parse_lines(text: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let l = l.trim_start_matches(['-', '*']).trim();
        let l = l
            .strip_prefix("[ ]")
            .or_else(|| l.strip_prefix("[x]"))
            .unwrap_or(l)
            .trim();
        if l.is_empty() {
            continue;
        }
        if l.chars().count() > MAX_TEXT {
            return Err(AitError::new(
                "INVALID_INPUT",
                format!("a criterion is longer than {MAX_TEXT} characters"),
            ));
        }
        out.push(l.to_string());
    }
    Ok(out)
}

/// Appends criteria to `id` (inside the caller's transaction).
pub fn add_all(c: &Connection, env: &Env, id: &str, texts: &[String]) -> Result<()> {
    let mut pos: i64 = c.query_row(
        "SELECT COALESCE(MAX(pos), 0) FROM criteria WHERE issue = ?1",
        [id],
        |r| r.get(0),
    )?;
    for t in texts {
        pos += 1;
        c.execute(
            "INSERT INTO criteria(issue, pos, text, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, pos, t, env.now],
        )?;
    }
    Ok(())
}

pub fn list(c: &Connection, id: &str) -> Result<Vec<Value>> {
    let mut st = c.prepare(
        "SELECT pos, text, done_at, done_by FROM criteria WHERE issue = ?1 ORDER BY pos",
    )?;
    let rows = st
        .query_map([id], |r| {
            let done_at: Option<i64> = r.get(2)?;
            Ok(json!({
                "n": r.get::<_, i64>(0)?,
                "text": r.get::<_, String>(1)?,
                "done": done_at.is_some(),
                "done_at": iso_opt(done_at),
                "done_by": r.get::<_, Option<String>>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Positions of unchecked criteria.
pub fn open(c: &Connection, id: &str) -> Result<Vec<i64>> {
    let mut st =
        c.prepare("SELECT pos FROM criteria WHERE issue = ?1 AND done_at IS NULL ORDER BY pos")?;
    let rows = st
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Refuses moving `id` on while criteria are unchecked.
pub fn require_all_checked(c: &Connection, id: &str, to: &str) -> Result<()> {
    let open = open(c, id)?;
    if open.is_empty() {
        return Ok(());
    }
    let ns: Vec<String> = open.iter().map(|n| format!("#{n}")).collect();
    Err(AitError::new(
        "CRITERIA_OPEN",
        format!(
            "{id} has unchecked acceptance criteria ({}); check them with `ait criteria {id} --check N` before {to} (or --force)",
            ns.join(", ")
        ),
    ))
}

fn render(id: &str, rows: &[Value]) -> String {
    if rows.is_empty() {
        return format!("{id} has no acceptance criteria");
    }
    let mut out = format!("acceptance criteria of {id}:");
    for r in rows {
        let mark = if r["done"].as_bool().unwrap_or(false) {
            "x"
        } else {
            " "
        };
        let _ = write!(out, "\n  [{mark}] {}. {}", r["n"], line(s(r, "text"), 160));
    }
    out
}

/// `ait criteria <ID> [--add "text"] [--check N] [--uncheck N] [--remove N]
/// [--gen N] [--force]`. Without an option it lists them. Changes follow the
/// same rule as edit: only the live claimant (or --force) on a claimed issue.
pub fn cmd_criteria(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["add", "check", "uncheck", "remove", "gen", "force"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let adding = a.opt("add").map(parse_lines).transpose()?;
    let check = a.opt_i64("check")?;
    let uncheck = a.opt_i64("uncheck")?;
    let remove = a.opt_i64("remove")?;
    let changes = [
        adding.is_some(),
        check.is_some(),
        uncheck.is_some(),
        remove.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if changes > 1 {
        return Err(AitError::usage(
            "use one of --add, --check, --uncheck, --remove at a time",
        ));
    }
    if changes == 0 {
        let (id, rows) = db::read(c, |c| {
            let id = issues::resolve(c, input)?;
            Ok((id.clone(), list(c, &id)?))
        })?;
        let text = render(&id, &rows);
        return Ok(Output::new(json!({"id": id, "criteria": rows}), text));
    }
    let force = a.flag("force");
    let want = a.opt_i64("gen")?;
    let (id, rows) = db::write(c, |c| {
        let id = issues::resolve(c, input)?;
        claims::guard_write(c, env, &id, force, want)?;
        let state = issues::state_of(c, &id)?;
        if issues::FINISHED.contains(&state.as_str()) && !force {
            return Err(AitError::new(
                "ALREADY_DONE",
                format!("{id} is {state}; its criteria change only with --force"),
            ));
        }
        let exists = |n: i64| -> Result<()> {
            let k: i64 = c.query_row(
                "SELECT COUNT(*) FROM criteria WHERE issue = ?1 AND pos = ?2",
                params![id, n],
                |r| r.get(0),
            )?;
            if k == 0 {
                return Err(AitError::not_found(&format!("criterion #{n} of {id}")));
            }
            Ok(())
        };
        let details = if let Some(texts) = &adding {
            if texts.is_empty() {
                return Err(AitError::new("INVALID_INPUT", "criterion text is empty"));
            }
            add_all(c, env, &id, texts)?;
            json!({"added": texts})
        } else if let Some(n) = check {
            exists(n)?;
            c.execute(
                "UPDATE criteria SET done_at = ?3, done_by = ?4 WHERE issue = ?1 AND pos = ?2",
                params![id, n, env.now, env.session],
            )?;
            json!({"checked": n})
        } else if let Some(n) = uncheck {
            exists(n)?;
            c.execute(
                "UPDATE criteria SET done_at = NULL, done_by = NULL WHERE issue = ?1 AND pos = ?2",
                params![id, n],
            )?;
            json!({"unchecked": n})
        } else if let Some(n) = remove {
            exists(n)?;
            c.execute(
                "DELETE FROM criteria WHERE issue = ?1 AND pos = ?2",
                params![id, n],
            )?;
            // Keep positions 1..N.
            c.execute(
                "UPDATE criteria SET pos = pos - 1 WHERE issue = ?1 AND pos > ?2",
                params![id, n],
            )?;
            json!({"removed": n})
        } else {
            Value::Null
        };
        c.execute(
            "UPDATE issues SET version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![id, env.now],
        )?;
        db::event(c, env, Some(&id), "criteria", details)?;
        Ok((id.clone(), list(c, &id)?))
    })?;
    let text = render(&id, &rows);
    Ok(Output::new(json!({"id": id, "criteria": rows}), text))
}
