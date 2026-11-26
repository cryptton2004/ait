//! `ait check`: database integrity and consistency; `--git` adds the
//! "done but not merged on main" check (one git call).

use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::Result;
use crate::ids;
use crate::issues::{LINK_KINDS, NEEDS_REASON, STATES};
use crate::output::{Output, line, s};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

fn problem(code: &str, issue: Option<&str>, message: String) -> Value {
    json!({"code": code, "issue": issue, "message": message})
}

fn strings(c: &Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut st = c.prepare(sql)?;
    let n = st.column_count();
    let rows = st
        .query_map([], |r| {
            (0..n)
                .map(|i| r.get::<_, Option<String>>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn find_cycle(edges: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    // Iterative DFS with colours: 0 new, 1 on stack, 2 finished.
    let mut colour: HashMap<&str, u8> = HashMap::new();
    let mut keys: Vec<&String> = edges.keys().collect();
    keys.sort();
    for start in keys {
        if colour.get(start.as_str()).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        colour.insert(start.as_str(), 1);
        while let Some((node, idx)) = stack.pop() {
            let next = edges.get(node).and_then(|v| v.get(idx));
            match next {
                Some(n) => {
                    stack.push((node, idx + 1));
                    match colour.get(n.as_str()).copied().unwrap_or(0) {
                        0 => {
                            colour.insert(n.as_str(), 1);
                            stack.push((n.as_str(), 0));
                        }
                        1 => {
                            let mut path: Vec<String> =
                                stack.iter().map(|(x, _)| (*x).to_string()).collect();
                            if let Some(pos) = path.iter().position(|x| x == n) {
                                path.drain(..pos);
                            }
                            path.push(n.clone());
                            return Some(path);
                        }
                        _ => {}
                    }
                }
                None => {
                    colour.insert(node, 2);
                }
            }
        }
    }
    None
}

fn git_trailers(env: &Env) -> std::result::Result<HashSet<String>, String> {
    let out = std::process::Command::new("git")
        .args([
            "log",
            "--first-parent",
            "--format=%B%x1e",
            &env.cfg.main_branch,
            "--",
        ])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log {} failed: {}",
            env.cfg.main_branch,
            line(&String::from_utf8_lossy(&out.stderr), 200)
        ));
    }
    let key = env.cfg.trailer.to_ascii_lowercase();
    let mut ids = HashSet::new();
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, v)) = l.trim().split_once(':')
            && k.trim().to_ascii_lowercase() == key
        {
            for id in v.split(',') {
                ids.insert(id.trim().to_ascii_uppercase());
            }
        }
    }
    Ok(ids)
}

pub fn run_checks(c: &Connection, env: &Env, git: bool) -> Result<(Vec<Value>, i64)> {
    let mut p = Vec::new();
    let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        p.push(problem(
            "INTEGRITY",
            None,
            format!("sqlite integrity_check: {integrity}"),
        ));
    }
    let issues = strings(
        c,
        "SELECT id, state, reason, parent, legacy FROM issues ORDER BY id",
    )?;
    let all: HashSet<String> = issues.iter().filter_map(|r| r[0].clone()).collect();
    let mut parents: HashMap<String, String> = HashMap::new();
    for r in &issues {
        let id = r[0].clone().unwrap_or_default();
        let state = r[1].clone().unwrap_or_default();
        if !ids::is_id(&id) {
            p.push(problem(
                "BAD_ID",
                Some(&id),
                format!("'{id}' does not match PREFIX-N-NN"),
            ));
        }
        if !STATES.contains(&state.as_str()) {
            p.push(problem(
                "BAD_STATE",
                Some(&id),
                format!("unknown state '{state}'"),
            ));
        }
        if NEEDS_REASON.contains(&state.as_str())
            && r[2].as_deref().is_none_or(|x| x.trim().is_empty())
        {
            p.push(problem(
                "NO_REASON",
                Some(&id),
                format!("state {state} has no reason"),
            ));
        }
        if let Some(par) = &r[3] {
            if !all.contains(par) {
                p.push(problem(
                    "MISSING_PARENT",
                    Some(&id),
                    format!("parent {par} does not exist"),
                ));
            }
            parents.insert(id.clone(), par.clone());
        }
    }
    for id in parents.keys() {
        let mut seen = HashSet::new();
        let mut cur = Some(id.clone());
        while let Some(x) = cur {
            if !seen.insert(x.clone()) {
                p.push(problem(
                    "PARENT_LOOP",
                    Some(id),
                    format!("parent chain of {id} loops"),
                ));
                break;
            }
            cur = parents.get(&x).cloned();
        }
    }
    let mut needs: HashMap<String, Vec<String>> = HashMap::new();
    for r in strings(
        c,
        "SELECT src, kind, dst FROM links ORDER BY src, kind, dst",
    )? {
        let (src, kind, dst) = (
            r[0].clone().unwrap_or_default(),
            r[1].clone().unwrap_or_default(),
            r[2].clone().unwrap_or_default(),
        );
        if !LINK_KINDS.contains(&kind.as_str()) {
            p.push(problem(
                "BAD_LINK",
                Some(&src),
                format!("unknown link kind '{kind}'"),
            ));
        }
        for end in [&src, &dst] {
            if !all.contains(end) {
                p.push(problem(
                    "DANGLING_LINK",
                    Some(&src),
                    format!("link {src} {kind} {dst}: {end} does not exist"),
                ));
            }
        }
        if kind == "needs" {
            needs.entry(src).or_default().push(dst);
        }
    }
    if let Some(cycle) = find_cycle(&needs) {
        p.push(problem(
            "CYCLE",
            cycle.first().map(String::as_str),
            format!("needs cycle: {}", cycle.join(" -> ")),
        ));
    }
    for r in strings(
        c,
        "SELECT c.issue, i.state, c.session FROM claims c LEFT JOIN issues i ON i.id = c.issue ORDER BY c.issue",
    )? {
        let id = r[0].clone().unwrap_or_default();
        match r[1].as_deref() {
            None => p.push(problem(
                "ORPHAN_CLAIM",
                Some(&id),
                format!("claim for missing issue {id}"),
            )),
            Some(st @ ("done" | "dropped")) if r[2].is_some() => p.push(problem(
                "CLAIM_ON_CLOSED",
                Some(&id),
                format!("{id} is {st} but still claimed"),
            )),
            _ => {}
        }
    }
    for r in strings(
        c,
        "SELECT DISTINCT n.issue FROM notes n LEFT JOIN issues i ON i.id = n.issue WHERE i.id IS NULL",
    )? {
        let id = r[0].clone().unwrap_or_default();
        p.push(problem(
            "ORPHAN_NOTE",
            Some(&id),
            format!("notes for missing issue {id}"),
        ));
    }
    if let Err(e) = claims::load_approvals(env) {
        p.push(problem("APPROVALS", None, e.message));
    }
    if git {
        match git_trailers(env) {
            Err(e) => p.push(problem("GIT", None, e)),
            Ok(merged) => {
                for r in strings(c, "SELECT id FROM issues WHERE state = 'done' ORDER BY id")? {
                    let id = r[0].clone().unwrap_or_default();
                    if !merged.contains(&id) {
                        p.push(problem(
                            "DONE_NOT_MERGED",
                            Some(&id),
                            format!(
                                "{id} is done but no commit on {} carries `{}: {id}`",
                                env.cfg.main_branch, env.cfg.trailer
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok((p, i64::try_from(all.len()).unwrap_or(0)))
}

pub fn cmd_check(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["git"], 0)?;
    let (problems, n) = db::read(c, |c| run_checks(c, env, a.flag("git")))?;
    let ok = problems.is_empty();
    let mut text = if ok {
        format!("ok: {n} issues, no problems")
    } else {
        format!("{} problem(s) in {n} issues:", problems.len())
    };
    for x in &problems {
        let _ = write!(text, "\n[{}] {}", s(x, "code"), line(s(x, "message"), 200));
    }
    let mut out = Output::new(json!({"ok": ok, "issues": n, "problems": problems}), text);
    if !ok {
        out.exit = 7;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cycles() {
        let mut e: HashMap<String, Vec<String>> = HashMap::new();
        e.insert("A".into(), vec!["B".into()]);
        e.insert("B".into(), vec!["C".into()]);
        assert!(find_cycle(&e).is_none());
        e.insert("C".into(), vec!["A".into()]);
        let c = find_cycle(&e).unwrap();
        assert_eq!(c.first(), c.last());
        assert_eq!(c.len(), 4);
    }
}
