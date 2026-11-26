//! Coordination: readiness, `next [--claim] [--frame]`, `claim [--take]`,
//! `release`, `done`, `heartbeat`, `verify`, and the rule that only the live
//! claimant writes to a claimed issue. Every claim is one `BEGIN IMMEDIATE`
//! transaction, so two agents can never win the same issue.

use crate::approvals::{self, Approvals};
use crate::args::Args;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::issues;
use crate::notes;
use crate::output::{Output, line, s};
use crate::time::{iso, iso_opt};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fmt::Write as _;

#[derive(Debug, Clone)]
pub struct ClaimRow {
    pub issue: String,
    pub session: Option<String>,
    pub generation: i64,
    pub claimed_at: Option<i64>,
    pub lease_until: Option<i64>,
}

impl ClaimRow {
    pub fn live(&self, now: i64) -> bool {
        self.session.is_some() && self.lease_until.is_some_and(|l| l > now)
    }

    pub fn stale(&self, now: i64) -> bool {
        self.session.is_some() && !self.live(now)
    }
}

pub fn claim_row(c: &Connection, id: &str) -> Result<Option<ClaimRow>> {
    Ok(c.query_row(
        "SELECT issue, session, generation, claimed_at, lease_until FROM claims WHERE issue = ?1",
        [id],
        |r| {
            Ok(ClaimRow {
                issue: r.get(0)?,
                session: r.get(1)?,
                generation: r.get(2)?,
                claimed_at: r.get(3)?,
                lease_until: r.get(4)?,
            })
        },
    )
    .optional()?)
}

pub fn claim_json(r: &ClaimRow, now: i64) -> Value {
    let status = if r.live(now) {
        "live"
    } else if r.stale(now) {
        "stale"
    } else {
        "released"
    };
    json!({
        "issue": r.issue,
        "session": r.session,
        "generation": r.generation,
        "claimed_at": iso_opt(r.claimed_at),
        "lease_until": iso_opt(r.lease_until),
        "status": status,
    })
}

pub fn load_approvals(env: &Env) -> Result<Approvals> {
    let repo = env.cfg.repo_dir(&env.db_path);
    approvals::load(env.cfg.approvals.as_ref(), repo.as_deref())
}

fn today(env: &Env) -> i64 {
    crate::time::day_number(env.now)
}

/// Refuses if `want` was given and is not the current claim generation.
pub fn check_gen(row: Option<&ClaimRow>, id: &str, want: Option<i64>) -> Result<()> {
    if let Some(g) = want {
        let cur = row.map(|r| r.generation);
        if cur != Some(g) || row.is_some_and(|r| r.session.is_none()) {
            let holder = row
                .and_then(|r| r.session.clone())
                .map(|h| format!(", now held by {h}"))
                .unwrap_or_default();
            return Err(AitError::new(
                "STALE_GENERATION",
                format!(
                    "generation {g} of {id} is no longer current (current: {}{holder}); your claim was taken over or released",
                    cur.map_or("none".to_string(), |c| c.to_string())
                ),
            ));
        }
    }
    Ok(())
}

/// The write rule: on an issue someone else holds with a live claim, only
/// that claimant may write (note, handoff, state, edit), unless `force`.
/// A given `--gen` must be current. Returns the caller's generation if the
/// caller holds the claim.
pub fn guard_write(
    c: &Connection,
    env: &Env,
    id: &str,
    force: bool,
    want: Option<i64>,
) -> Result<Option<i64>> {
    let row = claim_row(c, id)?;
    check_gen(row.as_ref(), id, want)?;
    let Some(r) = row else {
        return Ok(None);
    };
    let mine = r.session.is_some() && r.session == env.session;
    if mine {
        return Ok(Some(r.generation));
    }
    if r.live(env.now) && !force {
        return Err(AitError::new(
            "NOT_CLAIMANT",
            format!(
                "{id} is claimed by {} (generation {}); only the claimant may change it (or use --force)",
                r.session.as_deref().unwrap_or(""),
                r.generation
            ),
        ));
    }
    Ok(None)
}

/// Why `id` is not offered by `next`; empty means ready.
pub fn blockers(c: &Connection, env: &Env, ap: &Approvals, id: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let (state, reason, kind): (String, Option<String>, String) = c.query_row(
        "SELECT state, reason, type FROM issues WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if state != "ready" {
        let msg = match &reason {
            Some(r) => format!("state is {state} ({r})"),
            None => format!("state is {state}"),
        };
        out.push(json!({"code": "STATE", "message": msg, "state": state}));
    }
    if kind != "task" && kind != "review" {
        out.push(json!({
            "code": "TYPE",
            "message": format!("it is a {kind}; next offers tasks and reviews (use next --type {kind})"),
        }));
    }
    if kind == "review"
        && let Some(w) = review_wait(c, id)?
    {
        out.push(json!({"code": "REVIEW_WAITS", "message": w}));
    }
    let sql = format!(
        "SELECT l.dst, d.state, COALESCE(l.until_state, 'done') FROM links l
         LEFT JOIN issues d ON d.id = l.dst
         WHERE l.src = ?1 AND l.kind = 'needs' AND {} ORDER BY l.dst",
        issues::need_unmet_sql()
    );
    let mut st = c.prepare(&sql)?;
    let open: Vec<(String, Option<String>, String)> = st
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if !open.is_empty() {
        let ids: Vec<&str> = open.iter().map(|(i, _, _)| i.as_str()).collect();
        let desc: Vec<String> = open
            .iter()
            .map(|(i, s, u)| {
                let st = s.as_deref().unwrap_or("missing");
                if u == "done" {
                    format!("{i} ({st})")
                } else {
                    format!("{i} to be {u} ({st})")
                }
            })
            .collect();
        out.push(json!({
            "code": "NEEDS",
            "message": format!("needs {} first", desc.join(", ")),
            "ids": ids,
        }));
    }
    let cover = ap.check(id, today(env), "build");
    if !cover.ok() {
        out.push(json!({"code": cover.code(), "message": cover.message()}));
    }
    if let Some(r) = claim_row(c, id)? {
        if r.live(env.now) {
            out.push(json!({
                "code": "CLAIMED",
                "message": format!("claimed by {} (generation {}) until {}",
                    r.session.as_deref().unwrap_or(""), r.generation, iso(r.lease_until.unwrap_or(0))),
                "session": r.session,
            }));
        } else if r.stale(env.now) {
            out.push(json!({
                "code": "STALE_CLAIM",
                "message": format!("stale claim by {} since {}; use `ait claim {id} --take`",
                    r.session.as_deref().unwrap_or(""), iso(r.lease_until.unwrap_or(0))),
                "session": r.session,
            }));
        }
    }
    Ok(out)
}

/// Why a review cannot start yet: its parent must be in review.
pub fn review_wait(c: &Connection, id: &str) -> Result<Option<String>> {
    let parent: Option<(String, String)> = c
        .query_row(
            "SELECT p.id, p.state FROM issues i JOIN issues p ON p.id = i.parent WHERE i.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match parent {
        None => Some("a review needs a parent issue to review".to_string()),
        Some((p, st)) if st != "in_review" => {
            Some(format!("reviews start when {p} is in review (it is {st})"))
        }
        Some(_) => None,
    })
}

/// Reviews of `id` that are not finished yet.
pub fn open_reviews(c: &Connection, id: &str) -> Result<(i64, Vec<String>)> {
    let mut st = c.prepare(
        "SELECT id, state FROM issues WHERE parent = ?1 AND type = 'review' AND state <> 'dropped'
         ORDER BY ord, id",
    )?;
    let rows: Vec<(String, String)> = st
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let open = rows
        .iter()
        .filter(|(_, s)| !issues::FINISHED.contains(&s.as_str()))
        .map(|(i, _)| i.clone())
        .collect();
    Ok((i64::try_from(rows.len()).unwrap_or(0), open))
}

/// Moving `id` to in_review hands the work over: the claim is released.
pub fn submit_for_review(c: &Connection, env: &Env, id: &str, force: bool) -> Result<()> {
    let kind: String = c.query_row("SELECT type FROM issues WHERE id = ?1", [id], |r| r.get(0))?;
    if kind == "review" {
        return Err(AitError::new(
            "INVALID_STATE",
            format!("{id} is a review; finish it with `ait done`"),
        ));
    }
    if let Some(r) = claim_row(c, id)?
        && r.session.is_some()
    {
        c.execute(
            "UPDATE claims SET session = NULL, lease_until = NULL WHERE issue = ?1",
            [id],
        )?;
        db::event(
            c,
            env,
            Some(id),
            "release",
            json!({"previous": r.session, "generation": r.generation, "forced": force,
                   "why": "submitted for review"}),
        )?;
    }
    Ok(())
}

/// Filters for `next`.
#[derive(Default)]
pub struct Pick {
    pub frame: bool,
    pub include_stale: bool,
    pub prefix: Option<String>,
    pub agent: Option<String>,
    pub priorities: Vec<String>,
    pub kind: Option<String>,
}

/// Issues `next` would offer, best first (priority, then order, then id).
/// Build mode: `ready`, every need done, build-approved, unclaimed.
/// Frame mode: `idea`/`planned`, frame-approved, unclaimed.
pub fn candidates(c: &Connection, env: &Env, ap: &Approvals, p: &Pick) -> Result<Vec<String>> {
    let sql = if p.frame {
        "SELECT i.id, i.agent, i.priority, i.type FROM issues i LEFT JOIN claims c ON c.issue = i.id
         WHERE i.state IN ('idea', 'planned')
           AND (c.session IS NULL OR (?1 AND c.lease_until <= ?2))
         ORDER BY i.priority, i.ord, i.id"
    } else {
        "SELECT i.id, i.agent, i.priority, i.type FROM issues i LEFT JOIN claims c ON c.issue = i.id
         WHERE i.state = 'ready'
           AND NOT EXISTS (
               SELECT 1 FROM links l LEFT JOIN issues d ON d.id = l.dst
               WHERE l.src = i.id AND l.kind = 'needs' AND NEED_UNMET)
           AND (i.type <> 'review' OR EXISTS (
               SELECT 1 FROM issues p WHERE p.id = i.parent AND p.state = 'in_review'))
           AND (c.session IS NULL OR (?1 AND c.lease_until <= ?2))
         ORDER BY i.priority, i.ord, i.id"
    };
    let sql = sql.replace("NEED_UNMET", &issues::need_unmet_sql());
    let mut st = c.prepare(&sql)?;
    type Row = (String, Option<String>, String, String);
    let rows: Vec<Row> = st
        .query_map(params![p.include_stale, env.now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let t = today(env);
    let scope = if p.frame { "frame" } else { "build" };
    Ok(rows
        .into_iter()
        .filter(|(id, agent, prio, kind)| {
            (p.frame || p.kind.is_some() || kind == "task" || kind == "review")
                && p.kind.as_deref().is_none_or(|k| k == kind)
                && p.prefix.as_ref().is_none_or(|x| id.starts_with(x.as_str()))
                && p.agent
                    .as_ref()
                    .is_none_or(|x| agent.as_deref() == Some(x.as_str()))
                && (p.priorities.is_empty() || p.priorities.contains(prio))
                && ap.check(id, t, scope).ok()
        })
        .map(|r| r.0)
        .collect())
}

/// Tasks `next` would offer with no filters.
pub fn ready_ids(
    c: &Connection,
    env: &Env,
    ap: &Approvals,
    include_stale: bool,
) -> Result<Vec<String>> {
    candidates(
        c,
        env,
        ap,
        &Pick {
            include_stale,
            ..Pick::default()
        },
    )
}

/// Claims `id` for the current session inside the caller's transaction.
fn do_claim(c: &Connection, env: &Env, id: &str, take: bool) -> Result<Value> {
    let me = env.require_session()?;
    let state = issues::state_of(c, id)?;
    if ["done", "dropped", "released", "in_review"].contains(&state.as_str()) {
        let hint = if state == "in_review" {
            "; claim its review issues instead"
        } else {
            ""
        };
        return Err(AitError::new(
            "INVALID_STATE",
            format!("{id} is {state} and cannot be claimed{hint}"),
        ));
    }
    let kind: String = c.query_row("SELECT type FROM issues WHERE id = ?1", [id], |r| r.get(0))?;
    if kind == "review"
        && let Some(w) = review_wait(c, id)?
    {
        return Err(AitError::new("REVIEW_NOT_STARTED", w));
    }
    let prev = claim_row(c, id)?;
    let lease_until = env.now + env.lease_ms();
    let mut taken_from = Value::Null;
    let mut renewed = false;
    let generation = match &prev {
        Some(p) if p.session.as_deref() == Some(me) => {
            renewed = true;
            p.generation
        }
        Some(p) if p.live(env.now) => {
            return Err(AitError::new(
                "ALREADY_CLAIMED",
                format!(
                    "{id} is claimed by {} (generation {}) until {}",
                    p.session.as_deref().unwrap_or(""),
                    p.generation,
                    iso(p.lease_until.unwrap_or(0))
                ),
            ));
        }
        Some(p) if p.session.is_some() => {
            if !take {
                return Err(AitError::new(
                    "CLAIM_STALE",
                    format!(
                        "{id} has a stale claim by {} (lease ended {}); use `ait claim {id} --take`",
                        p.session.as_deref().unwrap_or(""),
                        iso(p.lease_until.unwrap_or(0))
                    ),
                ));
            }
            taken_from = json!({"session": p.session, "generation": p.generation});
            p.generation + 1
        }
        Some(p) => p.generation + 1,
        None => 1,
    };
    if renewed {
        c.execute(
            "UPDATE claims SET lease_until = ?2 WHERE issue = ?1",
            params![id, lease_until],
        )?;
    } else {
        c.execute(
            "INSERT INTO claims(issue, session, generation, claimed_at, lease_until)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(issue) DO UPDATE SET session = excluded.session,
                 generation = excluded.generation, claimed_at = excluded.claimed_at,
                 lease_until = excluded.lease_until",
            params![id, me, generation, env.now, lease_until],
        )?;
        let action = if taken_from.is_null() {
            "claim"
        } else {
            "take"
        };
        db::event(
            c,
            env,
            Some(id),
            action,
            json!({"generation": generation, "taken_from": taken_from}),
        )?;
    }
    let row = claim_row(c, id)?.ok_or_else(|| AitError::new("INTERNAL", "claim vanished"))?;
    Ok(json!({
        "issue": issues::get_full(c, &env.cfg, id)?,
        "claim": claim_json(&row, env.now),
        "renewed": renewed,
        "taken_from": taken_from,
        "latest_handoff": notes::latest_handoff(c, id)?,
        "branch": format!("task/{id}"),
    }))
}

fn render_claim(v: &Value) -> String {
    let i = &v["issue"];
    let cl = &v["claim"];
    let mut out = format!(
        "{} {}  {}\ngeneration {}  lease until {}  (pass --gen {} to note/handoff/release/done)",
        if v["renewed"].as_bool() == Some(true) {
            "renewed"
        } else if v["taken_from"].is_object() {
            "took over"
        } else {
            "claimed"
        },
        s(i, "id"),
        line(s(i, "title"), 100),
        cl["generation"],
        s(cl, "lease_until"),
        cl["generation"],
    );
    let agent = issues::agent_line(i);
    if !agent.is_empty() {
        let _ = write!(out, "\n{agent}");
    }
    if v["taken_from"].is_object() {
        let _ = write!(out, "\nprevious holder: {}", s(&v["taken_from"], "session"));
    }
    if v["latest_handoff"].is_object() {
        let _ = write!(
            out,
            "\n--- latest handoff ---\n{}",
            crate::output::clean(s(&v["latest_handoff"], "body")).trim_end()
        );
    }
    out
}

pub fn cmd_claim(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["take"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    env.require_session()?;
    let v = db::write(c, |c| {
        let id = issues::resolve(c, input)?;
        do_claim(c, env, &id, a.flag("take"))
    })?;
    let text = render_claim(&v);
    Ok(Output::new(v, text))
}

fn csv_upper(v: Option<&str>) -> Vec<String> {
    v.map(|v| {
        v.split(',')
            .map(|x| x.trim().to_ascii_uppercase())
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

pub fn cmd_next(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(
        &[
            "claim",
            "take-stale",
            "explain",
            "prefix",
            "limit",
            "frame",
            "agent",
            "priority",
            "type",
        ],
        0,
    )?;
    let ap = load_approvals(env)?;
    if let Some(x) = a.opt("explain") {
        let v = db::read(c, |c| {
            let id = issues::resolve(c, x)?;
            let b = blockers(c, env, &ap, &id)?;
            let frame = ap.check(&id, today(env), "frame");
            Ok(json!({
                "id": id,
                "ready": b.is_empty(),
                "blockers": b,
                "frame_approval": {"code": frame.code(), "message": frame.message()},
            }))
        })?;
        let text = match v["blockers"].as_array() {
            Some(b) if !b.is_empty() => {
                let lines: Vec<String> = b
                    .iter()
                    .map(|x| format!("- {}", line(s(x, "message"), 200)))
                    .collect();
                format!("{} is not offered:\n{}", s(&v, "id"), lines.join("\n"))
            }
            _ => format!("{} is ready and would be offered", s(&v, "id")),
        };
        return Ok(Output::new(v, text));
    }
    let priorities = csv_upper(a.opt("priority"));
    for p in &priorities {
        issues::check_priority(p)?;
    }
    let kind = a.opt("type").map(|t| t.trim().to_ascii_lowercase());
    if let Some(k) = &kind {
        issues::check_type(k)?;
    }
    let pick = Pick {
        frame: a.flag("frame"),
        include_stale: a.flag("take-stale"),
        prefix: a
            .opt("prefix")
            .map(|p| format!("{}-", p.trim().to_ascii_uppercase())),
        agent: a.opt("agent").map(|x| x.trim().to_ascii_lowercase()),
        priorities,
        kind,
    };
    let what = if pick.frame {
        "no idea or planned issue is approved for framing"
    } else {
        "no ready issue is available to claim"
    };
    if a.flag("claim") {
        env.require_session()?;
        let v = db::write(c, |c| {
            let ids = candidates(c, env, &ap, &pick)?;
            match ids.first() {
                Some(id) => do_claim(c, env, id, true),
                None => Err(AitError::new("NO_WORK", what).retryable()),
            }
        })?;
        let text = render_claim(&v);
        return Ok(Output::new(v, text));
    }
    let limit = usize::try_from(a.opt_i64("limit")?.unwrap_or(5).clamp(1, 1000)).unwrap_or(5);
    let list = db::read(c, |c| {
        let ids: Vec<String> = candidates(c, env, &ap, &pick)?
            .into_iter()
            .take(limit)
            .collect();
        ids.iter()
            .map(|id| {
                let mut v = issues::get_full(c, &env.cfg, id)?;
                if let Some(o) = v.as_object_mut() {
                    o.remove("body");
                }
                Ok(v)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let mut text = String::new();
    for v in &list {
        let _ = writeln!(
            text,
            "{:<12} {} {:<8} {}",
            s(v, "id"),
            s(v, "priority"),
            s(v, "state"),
            line(s(v, "title"), 80)
        );
    }
    if list.is_empty() {
        text.push_str(if pick.frame {
            "nothing to frame"
        } else {
            "nothing is ready"
        });
    }
    Ok(Output::new(
        json!({"mode": if pick.frame { "frame" } else { "build" }, "issues": list}),
        text,
    ))
}

/// The caller must hold the claim on `id` (unless `force`).
fn require_claimant(env: &Env, row: Option<&ClaimRow>, id: &str, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let holder = row.and_then(|r| r.session.as_deref());
    match holder {
        None => Err(AitError::new("NOT_CLAIMED", format!("{id} is not claimed"))),
        Some(h) if env.session.as_deref() == Some(h) => Ok(()),
        Some(h) => Err(AitError::new(
            "NOT_CLAIMANT",
            format!(
                "{id} is claimed by {h}{}; only the claimant (or --force) may do this",
                if env.session.is_none() {
                    " and no AIT_SESSION is set"
                } else {
                    ""
                }
            ),
        )),
    }
}

pub fn cmd_release(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["force", "gen"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let want = a.opt_i64("gen")?;
    let force = a.flag("force");
    let id = db::write(c, |c| {
        let id = issues::resolve(c, input)?;
        let row = claim_row(c, &id)?;
        check_gen(row.as_ref(), &id, want)?;
        if row.as_ref().and_then(|r| r.session.as_ref()).is_none() {
            return Err(AitError::new("NOT_CLAIMED", format!("{id} is not claimed")));
        }
        require_claimant(env, row.as_ref(), &id, force)?;
        let prev = row.as_ref().and_then(|r| r.session.clone());
        c.execute(
            "UPDATE claims SET session = NULL, lease_until = NULL WHERE issue = ?1",
            [&id],
        )?;
        db::event(
            c,
            env,
            Some(&id),
            "release",
            json!({"previous": prev, "generation": row.map(|r| r.generation), "forced": force}),
        )?;
        Ok(id)
    })?;
    Ok(Output::new(
        json!({"released": id}),
        format!("released {id}"),
    ))
}

/// States `done` refuses without `--force`: nobody built anything yet, or the
/// call is the owner's.
const NOT_DONEABLE: &[&str] = &["idea", "decision", "dropped"];

pub fn cmd_done(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["summary", "force", "gen", "ran"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let summary = a
        .opt("summary")
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .ok_or_else(|| AitError::usage("done needs --summary \"what was delivered\""))?
        .to_string();
    let ran = a
        .opt("ran")
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string);
    if ran.as_ref().is_some_and(|r| r.len() > 500) {
        return Err(AitError::new(
            "INVALID_INPUT",
            "--ran is longer than 500 characters",
        ));
    }
    let want = a.opt_i64("gen")?;
    let force = a.flag("force");
    let issue = db::write(c, |c| {
        let id = issues::resolve(c, input)?;
        let state = issues::state_of(c, &id)?;
        if issues::FINISHED.contains(&state.as_str()) {
            return Err(AitError::new(
                "ALREADY_DONE",
                format!("{id} is already {state}"),
            ));
        }
        let (n_reviews, open) = open_reviews(c, &id)?;
        if !open.is_empty() && !force {
            let hint = if state == "in_review" {
                String::new()
            } else {
                format!("; submit it first with `ait state {id} in_review`")
            };
            return Err(AitError::new(
                "REVIEWS_OPEN",
                format!("{id} has open reviews: {}{hint}", open.join(", ")),
            ));
        }
        // Reviewed work is closed by its passed reviews, not by a claim.
        let reviewed = state == "in_review" && n_reviews > 0;
        if state == "in_review" && n_reviews == 0 && !force {
            return Err(AitError::new(
                "REVIEWS_OPEN",
                format!(
                    "{id} is in review with no review issues; add them (ait add ... --type review --parent {id}) or close it as the owner with --force"
                ),
            ));
        }
        if NOT_DONEABLE.contains(&state.as_str()) && !force {
            return Err(AitError::new(
                "INVALID_STATE",
                format!(
                    "{id} is {state}; done refuses idea, decision and dropped issues without --force"
                ),
            ));
        }
        let row = claim_row(c, &id)?;
        check_gen(row.as_ref(), &id, want)?;
        if !force && !reviewed {
            require_claimant(env, row.as_ref(), &id, false)?;
            if row.as_ref().is_some_and(|r| !r.live(env.now)) {
                return Err(AitError::new(
                    "CLAIM_STALE",
                    format!("your claim on {id} has expired; claim it again before done"),
                ));
            }
            if want.is_none() {
                return Err(AitError::usage(format!(
                    "done needs --gen <generation> of your claim on {id} (or --force)"
                )));
            }
        }
        c.execute(
            "UPDATE issues SET state = 'done', reason = NULL, summary = ?2, done_at = ?3, ran = ?4,
                 version = version + 1, updated_at = ?3 WHERE id = ?1",
            params![id, summary, env.now, ran],
        )?;
        c.execute(
            "UPDATE claims SET session = NULL, lease_until = NULL WHERE issue = ?1",
            [&id],
        )?;
        db::event(
            c,
            env,
            Some(&id),
            "done",
            json!({"summary": summary, "from": state, "ran": ran,
                   "generation": row.map(|r| r.generation), "forced": force}),
        )?;
        issues::get_full(c, &env.cfg, &id)
    })?;
    let text = format!("done {}  {}", s(&issue, "id"), line(s(&issue, "title"), 80));
    Ok(Output::new(json!({"issue": issue}), text))
}

pub fn cmd_heartbeat(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["gen"], 1)?;
    let me = env.require_session()?.to_string();
    let want = a.opt_i64("gen")?;
    let rows = db::write(c, |c| {
        let lease_until = env.now + env.lease_ms();
        if let Some(input) = a.pos(0) {
            let id = issues::resolve(c, input)?;
            let row = claim_row(c, &id)?;
            check_gen(row.as_ref(), &id, want)?;
            require_claimant(env, row.as_ref(), &id, false)?;
        }
        c.execute(
            "UPDATE claims SET lease_until = ?2 WHERE session = ?1 AND lease_until > ?3",
            params![me, lease_until, env.now],
        )?;
        let mut st = c.prepare(
            "SELECT issue, session, generation, claimed_at, lease_until FROM claims
             WHERE session = ?1 ORDER BY issue",
        )?;
        let rows: Vec<Value> = st
            .query_map([&me], |r| {
                Ok(ClaimRow {
                    issue: r.get(0)?,
                    session: r.get(1)?,
                    generation: r.get(2)?,
                    claimed_at: r.get(3)?,
                    lease_until: r.get(4)?,
                })
            })?
            .map(|r| r.map(|r| claim_json(&r, env.now)))
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })?;
    let mut text = format!("{me}: {} claim(s) renewed", rows.len());
    for r in &rows {
        let _ = write!(
            text,
            "\n{}  generation {}  lease until {}",
            s(r, "issue"),
            r["generation"],
            s(r, "lease_until")
        );
    }
    Ok(Output::new(json!({"session": me, "claims": rows}), text))
}

pub fn cmd_verify(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["gen"], 1)?;
    let input = a.req_pos(0, "issue id")?;
    let want = a
        .opt_i64("gen")?
        .ok_or_else(|| AitError::usage("verify needs --gen <generation>"))?;
    let v = db::read(c, |c| {
        let id = issues::resolve(c, input)?;
        let row = claim_row(c, &id)?;
        check_gen(row.as_ref(), &id, Some(want))?;
        if env.session.is_some() {
            require_claimant(env, row.as_ref(), &id, false)?;
        }
        let row =
            row.ok_or_else(|| AitError::new("NOT_CLAIMED", format!("{id} is not claimed")))?;
        Ok(json!({"ok": true, "claim": claim_json(&row, env.now)}))
    })?;
    let cl = &v["claim"];
    let text = format!(
        "ok: {} generation {} held by {} ({}, lease {})",
        s(cl, "issue"),
        cl["generation"],
        s(cl, "session"),
        s(cl, "status"),
        cl["lease_until"].as_str().unwrap_or("-"),
    );
    Ok(Output::new(v, text))
}
