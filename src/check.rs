//! `ait check`: database integrity and consistency. `--git` audits done
//! issues against the merges on the main branch (one git call).
//!
//! The git audit is strict, like the factory's own trailer parser: only
//! first-parent **merge** commits on `main_branch` of the configured `repo`
//! count, and only a line `<trailer>: <ID>` (one id, nothing else) in the
//! **last paragraph** of the message. Key and id compare case-insensitively.
//! Ids listed in `git_baseline` (done before ait) count as merged.

use crate::args::Args;
use crate::claims;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::ids;
use crate::issues::{
    EFFORTS, FINISHED, LINK_KINDS, NEEDS_REASON, PRIORITIES, STATES, TYPES, UNTIL_STATES,
};
use crate::output::{Output, line, s};
use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn problem(code: &str, issue: Option<&str>, message: String) -> Value {
    json!({"code": code, "issue": issue, "message": message})
}

/// Rows as optional strings (numbers are shown as text, so damaged rows
/// are reported instead of failing the check).
fn strings(c: &Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut st = c.prepare(sql)?;
    let n = st.column_count();
    let rows = st
        .query_map([], |r| {
            (0..n)
                .map(|i| {
                    r.get::<_, Sql>(i).map(|v| match v {
                        Sql::Null => None,
                        Sql::Integer(x) => Some(x.to_string()),
                        Sql::Real(x) => Some(x.to_string()),
                        Sql::Text(t) => Some(t),
                        Sql::Blob(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                    })
                })
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

/// Ids named by trailers in the last paragraph of a merge message.
pub fn trailer_ids(message: &str, trailer: &str) -> Vec<String> {
    let text = message.replace("\r\n", "\n");
    let last = text
        .trim()
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .last()
        .unwrap_or("");
    let mut out = Vec::new();
    for l in last.lines() {
        if let Some((k, v)) = l.split_once(':')
            && k.trim_end() == k
            && k.eq_ignore_ascii_case(trailer)
        {
            let id = v.trim().to_ascii_uppercase();
            if ids::is_id(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Merged ids (id -> merge sha) from first-parent merges of `rev` in `repo`.
pub fn git_merged(
    repo: &Path,
    rev: &str,
    trailer: &str,
) -> std::result::Result<HashMap<String, String>, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--first-parent",
            "--merges",
            "--format=%H%x00%B%x1e",
            rev,
            "--",
        ])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git -C {} log {rev} failed: {}",
            repo.display(),
            line(&String::from_utf8_lossy(&out.stderr), 200)
        ));
    }
    let mut merged = HashMap::new();
    for rec in String::from_utf8_lossy(&out.stdout).split('\u{1e}') {
        let Some((sha, msg)) = rec.trim_start().split_once('\0') else {
            continue;
        };
        for id in trailer_ids(msg, trailer) {
            merged.entry(id).or_insert_with(|| sha.trim().to_string());
        }
    }
    Ok(merged)
}

fn repo_for(env: &Env, explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(r) = explicit {
        return Ok(PathBuf::from(r));
    }
    env.cfg.repo_dir(&env.db_path).ok_or_else(|| {
        AitError::new(
            "CONFIG",
            "check --git needs the repository: set repo = in default.cfg or pass --repo <path>",
        )
    })
}

pub struct Checks {
    pub git: bool,
    pub repo: Option<PathBuf>,
    /// Include the approvals file (not wanted when validating an import).
    pub approvals: bool,
}

pub fn run_checks(c: &Connection, env: &Env, opt: &Checks) -> Result<(Vec<Value>, i64)> {
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
        "SELECT id, state, reason, parent, legacy, type, priority, effort, release FROM issues ORDER BY id",
    )?;
    let all: HashSet<String> = issues.iter().filter_map(|r| r[0].clone()).collect();
    let kind_of: HashMap<String, String> = issues
        .iter()
        .filter_map(|r| Some((r[0].clone()?, r[5].clone().unwrap_or_default())))
        .collect();
    let mut parents: HashMap<String, String> = HashMap::new();
    let mut legacy_seen: HashMap<String, String> = HashMap::new();
    for r in &issues {
        let id = r[0].clone().unwrap_or_default();
        let state = r[1].clone().unwrap_or_default();
        if !ids::is_id(&id) {
            p.push(problem(
                "BAD_ID",
                Some(&id),
                format!("'{id}' does not match PREFIX-N-NN (uppercase)"),
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
        if let Some(l) = &r[4] {
            let key = l.to_lowercase();
            if let Some(other) = legacy_seen.get(&key) {
                p.push(problem(
                    "LEGACY_DUP",
                    Some(&id),
                    format!("legacy alias '{l}' is also used by {other}"),
                ));
            } else {
                legacy_seen.insert(key, id.clone());
            }
        }
        let kind = r[5].clone().unwrap_or_default();
        if kind == "review" && r[3].is_none() {
            p.push(problem(
                "REVIEW_NO_PARENT",
                Some(&id),
                format!("{id} is a review with no parent issue to review"),
            ));
        }
        if !TYPES.contains(&kind.as_str()) {
            p.push(problem(
                "BAD_TYPE",
                Some(&id),
                format!("unknown type '{kind}'"),
            ));
        }
        let prio = r[6].clone().unwrap_or_default();
        if !PRIORITIES.contains(&prio.as_str()) {
            p.push(problem(
                "BAD_PRIORITY",
                Some(&id),
                format!("unknown priority '{prio}'"),
            ));
        }
        if let Some(e) = &r[7]
            && !EFFORTS.contains(&e.as_str())
        {
            p.push(problem(
                "BAD_EFFORT",
                Some(&id),
                format!("unknown effort '{e}'"),
            ));
        }
        if let Some(rel) = &r[8] {
            match kind_of.get(rel).map(String::as_str) {
                None => p.push(problem(
                    "MISSING_RELEASE",
                    Some(&id),
                    format!("release {rel} does not exist"),
                )),
                Some("release" | "milestone") => {}
                Some(k) => p.push(problem(
                    "BAD_RELEASE",
                    Some(&id),
                    format!("release {rel} is a {k}, not a release or milestone"),
                )),
            }
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
        "SELECT src, kind, dst, until_state FROM links ORDER BY src, kind, dst",
    )? {
        if let Some(u) = &r[3]
            && (!UNTIL_STATES.contains(&u.as_str()) || r[1].as_deref() != Some("needs"))
        {
            p.push(problem(
                "BAD_LINK",
                r[0].as_deref(),
                format!(
                    "link {} waits for '{u}', which is not allowed",
                    r[0].clone().unwrap_or_default()
                ),
            ));
        }
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
        if src == dst {
            p.push(problem("CYCLE", Some(&src), format!("{src} {kind} itself")));
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
        if kind == "needs" && src != dst {
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
            Some(st @ ("done" | "dropped" | "released" | "in_review")) if r[2].is_some() => {
                p.push(problem(
                    "CLAIM_ON_CLOSED",
                    Some(&id),
                    format!("{id} is {st} but still claimed"),
                ))
            }
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
    for r in strings(
        c,
        "SELECT DISTINCT k.issue FROM criteria k LEFT JOIN issues i ON i.id = k.issue WHERE i.id IS NULL",
    )? {
        let id = r[0].clone().unwrap_or_default();
        p.push(problem(
            "ORPHAN_CRITERIA",
            Some(&id),
            format!("acceptance criteria for missing issue {id}"),
        ));
    }
    if opt.approvals
        && let Err(e) = claims::load_approvals(env)
    {
        p.push(problem("APPROVALS", None, e.message));
    }
    if opt.git {
        git_checks(c, env, opt.repo.as_deref(), &mut p)?;
    }
    Ok((p, i64::try_from(all.len()).unwrap_or(0)))
}

/// Ids listed in `git_baseline` (done before ait tracked merges).
pub fn load_baseline(env: &Env, repo: &Path, p: &mut Vec<Value>) -> Result<HashSet<String>> {
    let mut baseline: HashSet<String> = HashSet::new();
    if let Some(src) = &env.cfg.git_baseline {
        match src.read(Some(repo))? {
            None => p.push(problem(
                "BASELINE",
                None,
                format!("git_baseline {} does not exist", src.describe()),
            )),
            Some(text) => {
                for l in text.lines() {
                    let l = l.trim();
                    if l.is_empty() || l.starts_with('#') {
                        continue;
                    }
                    let id = l.to_ascii_uppercase();
                    if ids::is_id(&id) {
                        baseline.insert(id);
                    } else {
                        p.push(problem(
                            "BASELINE",
                            None,
                            format!("baseline line '{l}' is not an id"),
                        ));
                    }
                }
            }
        }
    }
    Ok(baseline)
}

fn git_checks(c: &Connection, env: &Env, repo: Option<&Path>, p: &mut Vec<Value>) -> Result<()> {
    let repo = repo
        .map(Path::to_path_buf)
        .or_else(|| env.cfg.repo_dir(&env.db_path));
    let Some(repo) = repo else {
        p.push(problem(
            "GIT",
            None,
            "check --git needs the repository: set repo = in default.cfg or pass --repo <path>"
                .to_string(),
        ));
        return Ok(());
    };
    let merged = match git_merged(&repo, &env.cfg.main_branch, &env.cfg.trailer) {
        Ok(m) => m,
        Err(e) => {
            p.push(problem("GIT", None, e));
            return Ok(());
        }
    };
    let baseline = load_baseline(env, &repo, p)?;
    // Only tasks and bugs are merged; containers and reviews have no merge of their own.
    let states: HashMap<String, String> = strings(
        c,
        "SELECT id, state FROM issues WHERE type IN ('task', 'bug') ORDER BY id",
    )?
    .into_iter()
    .filter_map(|r| Some((r[0].clone()?, r[1].clone().unwrap_or_default())))
    .collect();
    let mut ids: Vec<&String> = states.keys().collect();
    ids.sort();
    for id in ids {
        let state = &states[id];
        let up = id.to_ascii_uppercase();
        let is_merged = merged.contains_key(&up) || baseline.contains(&up);
        if FINISHED.contains(&state.as_str()) && !is_merged {
            p.push(problem(
                "DONE_NOT_MERGED",
                Some(id),
                format!(
                    "{id} is done but no first-parent merge on {} carries `{}: {id}` (and it is not in the baseline)",
                    env.cfg.main_branch, env.cfg.trailer
                ),
            ));
        }
        if !FINISHED.contains(&state.as_str())
            && let Some(sha) = merged.get(&up)
        {
            p.push(problem(
                "MERGED_NOT_DONE",
                Some(id),
                format!(
                    "{id} was merged in {} but is {state}; run `ait done {id} --force --summary ...`",
                    &sha[..sha.len().min(12)]
                ),
            ));
        }
    }
    Ok(())
}

pub fn cmd_check(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["git", "repo"], 0)?;
    let repo = if a.flag("git") {
        Some(repo_for(env, a.opt("repo")).ok()).flatten()
    } else {
        None
    };
    let opt = Checks {
        git: a.flag("git"),
        repo,
        approvals: true,
    };
    let (problems, n) = db::read(c, |c| run_checks(c, env, &opt))?;
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

    #[test]
    fn strict_trailers() {
        let t = "Backlog-ID";
        // Only the last paragraph, one id per line, exact key.
        assert_eq!(
            trailer_ids("Merge x\n\nBacklog-ID: core-3-10", t),
            vec!["CORE-3-10"]
        );
        assert_eq!(
            trailer_ids("Merge x\n\nbacklog-id: CORE-3-10\r\n", t),
            vec!["CORE-3-10"]
        );
        assert!(
            trailer_ids(
                "Merge\n\nBacklog-ID: core-3-10\nwas mentioned as prose\n\nlast para",
                t
            )
            .is_empty()
        );
        assert!(trailer_ids("Merge\n\nBacklog-ID: bug-40-00, core-100-00", t).is_empty());
        assert!(trailer_ids("Merge\n\nX-Backlog-ID: core-1-00", t).is_empty());
        assert!(trailer_ids("Merge\n\nBacklog-ID : core-1-00", t).is_empty());
        assert_eq!(
            trailer_ids(
                "Merge\n\nSigned-off-by: a\nBacklog-ID: core-1-00\nBacklog-ID: core-1-01",
                t
            ),
            vec!["CORE-1-00", "CORE-1-01"]
        );
    }
}
