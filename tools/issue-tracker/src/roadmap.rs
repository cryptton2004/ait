//! Releases, milestones and features: rollups and `ait roadmap`.
//!
//! The members of a container are its descendants through `parent`, plus every
//! issue whose `release` is the container (and their descendants).

use crate::approvals::Approvals;
use crate::args::Args;
use crate::check;
use crate::claims;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::issues;
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
    let (mut in_review, mut released) = (0, 0);
    let mut decisions = Vec::new();
    let mut blocked = Vec::new();
    for (mid, state, session, lease) in &rows {
        match state.as_str() {
            "dropped" => dropped += 1,
            "done" | "released" => {
                done += 1;
                total += 1;
                if state == "released" {
                    released += 1;
                }
            }
            "in_review" => {
                in_review += 1;
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
    // A container with no members is as far along as its own state: a done
    // or released empty feature is 100%, not 0%.
    let own: String = c.query_row("SELECT state FROM issues WHERE id = ?1", [id], |r| r.get(0))?;
    let percent = if total > 0 {
        done * 100 / total
    } else if issues::FINISHED.contains(&own.as_str()) {
        100
    } else {
        0
    };
    Ok(json!({
        "total": total,
        "done": done,
        "open": total - done,
        "dropped": dropped,
        "in_progress": in_progress,
        "in_review": in_review,
        "released": released,
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
    if r["in_review"].as_i64().unwrap_or(0) > 0 {
        let _ = write!(out, ", {} in review", r["in_review"]);
    }
    if r["released"].as_i64().unwrap_or(0) > 0 {
        let _ = write!(out, ", {} released", r["released"]);
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

/// Every member of a container (children through `parent`, and everything
/// whose `release` is it), recursively, with its state.
pub fn members(c: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    let mut st = c.prepare(
        "WITH RECURSIVE m(id) AS (
             SELECT id FROM issues WHERE (parent = ?1 OR release = ?1) AND id <> ?1
             UNION
             SELECT i.id FROM issues i JOIN m ON (i.parent = m.id OR i.release = m.id)
             WHERE i.id <> ?1
         )
         SELECT i.id, i.state FROM issues i JOIN m ON m.id = i.id ORDER BY i.ord, i.id",
    )?;
    let rows = st
        .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Runs git in `repo`; true when it exits 0.
fn git_ok(repo: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `ait ship <ID> [--ref R] [--force]`: marks a finished release, milestone,
/// feature (or a single issue) and all its done members as released.
///
/// Gates (all skipped by --force): every member is done, released or
/// dropped; with a repository, each done member has a first-parent merge on
/// main carrying its trailer (or is in git_baseline), and when
/// `origin/<main>` exists, that merge has been pushed to it.
pub fn cmd_ship(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["ref", "force"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let force = a.flag("force");
    let reference = a
        .opt("ref")
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string);
    let repo = env.cfg.repo_dir(&env.db_path);
    // Read git outside the write transaction.
    let (target, to_check) = db::read(c, |c| {
        let id = issues::resolve(c, input)?;
        let mut list = members(c, &id)?;
        list.insert(0, (id.clone(), issues::state_of(c, &id)?));
        Ok((id, list))
    })?;
    let mut warnings: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let has_members = to_check.len() > 1;
    for (mid, st) in &to_check {
        let is_target = *mid == target;
        if st == "dropped" || st == "released" {
            continue;
        }
        // A container's own state does not matter; its members do.
        if is_target && has_members {
            continue;
        }
        if st != "done" {
            problems.push(format!("{mid} is {st}, not done"));
        }
    }
    // Only tasks carry merges; containers and reviews do not.
    let tasks: std::collections::HashSet<String> = db::read(c, |c| {
        let mut st = c.prepare("SELECT id FROM issues WHERE type = 'task'")?;
        let ids = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    })?;
    let done: Vec<&String> = to_check
        .iter()
        .filter(|(i, st)| st == "done" && tasks.contains(i))
        .map(|(i, _)| i)
        .collect();
    if !force && problems.is_empty() && !done.is_empty() {
        match &repo {
            None => warnings.push(
                "no repository configured (repo =): merges and pushes were not checked".to_string(),
            ),
            Some(repo) => {
                let merged = check::git_merged(repo, &env.cfg.main_branch, &env.cfg.trailer)
                    .map_err(|e| AitError::new("GIT", e))?;
                let mut base_problems = Vec::new();
                let baseline = check::load_baseline(env, repo, &mut base_problems)?;
                let remote = format!("refs/remotes/origin/{}", env.cfg.main_branch);
                let has_remote = git_ok(repo, &["rev-parse", "--verify", "--quiet", &remote]);
                if !has_remote {
                    warnings.push(format!(
                        "origin/{} not found: pushes were not checked",
                        env.cfg.main_branch
                    ));
                }
                for id in &done {
                    match merged.get(id.as_str()) {
                        Some(sha) => {
                            if has_remote
                                && !git_ok(repo, &["merge-base", "--is-ancestor", sha, &remote])
                            {
                                problems.push(format!(
                                    "{id} is merged in {} but not pushed to origin/{}",
                                    &sha[..sha.len().min(12)],
                                    env.cfg.main_branch
                                ));
                            }
                        }
                        None if baseline.contains(id.as_str()) => {}
                        None => problems.push(format!(
                            "{id} has no merge on {} with `{}: {id}`",
                            env.cfg.main_branch, env.cfg.trailer
                        )),
                    }
                }
            }
        }
    }
    if !problems.is_empty() && !force {
        let mut msg = format!("{target} cannot be released yet:");
        for p in &problems {
            let _ = write!(msg, "\n  {p}");
        }
        return Err(AitError::new("NOT_SHIPPABLE", msg).with_details(json!({"problems": problems})));
    }
    let shipped = db::write(c, |c| {
        let mut ids = Vec::new();
        for (mid, _) in &to_check {
            // Re-read inside the transaction: only finished work is released.
            let st = issues::state_of(c, mid)?;
            let is_container = *mid == target && has_members;
            if st == "done"
                || (is_container && st != "dropped" && st != "released")
                || (force && *mid == target && st != "released")
            {
                c.execute(
                    "UPDATE issues SET state = 'released', reason = NULL, released_at = ?2,
                         release_ref = ?3, done_at = COALESCE(done_at, ?2),
                         version = version + 1, updated_at = ?2 WHERE id = ?1",
                    params![mid, env.now, reference],
                )?;
                c.execute(
                    "UPDATE claims SET session = NULL, lease_until = NULL WHERE issue = ?1",
                    [mid],
                )?;
                ids.push(mid.clone());
            }
        }
        db::event(
            c,
            env,
            Some(&target),
            "ship",
            json!({"released": ids, "ref": reference, "forced": force, "warnings": warnings}),
        )?;
        Ok(ids)
    })?;
    let mut text = format!("released {} issue(s): {}", shipped.len(), shipped.join(" "));
    for w in &warnings {
        let _ = write!(text, "\nnote: {w}");
    }
    Ok(Output::new(
        json!({"released": shipped, "ref": reference, "warnings": warnings}),
        text,
    ))
}

const CONTAINERS: &[&str] = &["feature", "milestone", "release"];

fn parent_of(c: &Connection, id: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(
        c.query_row("SELECT parent FROM issues WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten(),
    )
}

/// Whether the container's last state change was made by this rollup.
fn auto_moved(c: &Connection, id: &str) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let d: Option<String> = c
        .query_row(
            "SELECT details FROM events WHERE issue = ?1 AND action IN ('state', 'done')
             ORDER BY seq DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(d.and_then(|d| serde_json::from_str::<Value>(&d).ok())
        .is_some_and(|v| v["auto"] == true))
}

/// After `id` changed state, moves its container ancestors (feature,
/// milestone, release via `parent`) along: to in_review once every child
/// (reviews aside) is in review or finished, to done once every child is
/// finished, its own reviews are done and its criteria are checked. A
/// container this rollup moved to in_review goes back to planned when a
/// child is reopened. Returns the changes made.
pub fn auto_rollup(c: &Connection, env: &Env, id: &str) -> Result<Vec<Value>> {
    let mut changes = Vec::new();
    let mut cur = parent_of(c, id)?;
    while let Some(p) = cur {
        let (kind, state): (String, String) =
            c.query_row("SELECT type, state FROM issues WHERE id = ?1", [&p], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        if !CONTAINERS.contains(&kind.as_str()) || state == "dropped" {
            break;
        }
        let mut st = c.prepare(
            "SELECT state FROM issues WHERE parent = ?1 AND type <> 'review' AND state <> 'dropped'",
        )?;
        let kids: Vec<String> = st
            .query_map([&p], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if kids.is_empty() {
            break;
        }
        let finished = |s: &String| issues::FINISHED.contains(&s.as_str());
        let all_finished = kids.iter().all(finished);
        let all_reviewing = kids.iter().all(|s| finished(s) || s == "in_review");
        let (_, open_reviews) = claims::open_reviews(c, &p)?;
        let criteria_open = !crate::criteria::open(c, &p)?.is_empty();
        let to = if all_finished && open_reviews.is_empty() && !criteria_open {
            (!issues::FINISHED.contains(&state.as_str())).then_some("done")
        } else if all_reviewing {
            (state != "in_review" && !issues::FINISHED.contains(&state.as_str()))
                .then_some("in_review")
        } else if state == "in_review" && auto_moved(c, &p)? {
            Some("planned")
        } else {
            None
        };
        let Some(to) = to else {
            break;
        };
        if to == "done" {
            let summary = format!("all {} children finished (automatic rollup)", kids.len());
            c.execute(
                "UPDATE issues SET state = 'done', reason = NULL, summary = ?2, done_at = ?3,
                     version = version + 1, updated_at = ?3 WHERE id = ?1",
                params![p, summary, env.now],
            )?;
            c.execute(
                "UPDATE claims SET session = NULL, lease_until = NULL WHERE issue = ?1",
                [&p],
            )?;
            db::event(
                c,
                env,
                Some(&p),
                "done",
                json!({"summary": summary, "from": state, "auto": true, "after": id}),
            )?;
        } else {
            c.execute(
                "UPDATE issues SET state = ?2, reason = NULL, done_at = NULL,
                     version = version + 1, updated_at = ?3 WHERE id = ?1",
                params![p, to, env.now],
            )?;
            db::event(
                c,
                env,
                Some(&p),
                "state",
                json!({"from": state, "to": to, "auto": true, "after": id}),
            )?;
        }
        changes.push(json!({"id": p, "from": state, "to": to}));
        cur = parent_of(c, &p)?;
    }
    Ok(changes)
}
