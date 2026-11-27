//! Flow statistics from the event log: how long issues sit in each board
//! column, cycle time (first claim to done) and lead time (added to done).

use crate::args::Args;
use crate::db::{self, Env};
use crate::error::Result;
use crate::output::Output;
use crate::time::parse_duration;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Board columns, left to right (as on the dashboard).
pub const COLUMNS: &[&str] = &[
    "backlog",
    "ready",
    "waiting",
    "in_progress",
    "in_review",
    "done",
    "released",
];

fn column_of(state: &str, claimed: bool) -> Option<&'static str> {
    let base = match state {
        "idea" | "planned" => "backlog",
        "ready" => "ready",
        "blocked" | "decision" => "waiting",
        "in_review" => "in_review",
        "done" => "done",
        "released" => "released",
        _ => return None,
    };
    if claimed && !["in_review", "done", "released"].contains(&base) {
        Some("in_progress")
    } else {
        Some(base)
    }
}

#[derive(Default)]
struct Track {
    state: String,
    claimed: bool,
    col: Option<&'static str>,
    since: i64,
    added: Option<i64>,
    first_claim: Option<i64>,
}

/// Upper edges (minutes) of the duration buckets: 0-15m, 15-30m, 30-60m,
/// 1-2h, 2-4h and 4h+ (the last bucket has no edge).
pub const BUCKET_MINUTES: &[i64] = &[15, 30, 60, 120, 240];

/// How many durations fall in each bucket (one more bucket than edges).
fn buckets(v: &[i64]) -> Vec<usize> {
    let mut out = vec![0; BUCKET_MINUTES.len() + 1];
    for ms in v {
        let i = BUCKET_MINUTES
            .iter()
            .position(|m| *ms < m * 60_000)
            .unwrap_or(BUCKET_MINUTES.len());
        out[i] += 1;
    }
    out
}

fn summarize(mut v: Vec<i64>) -> Value {
    if v.is_empty() {
        return json!({"count": 0, "avg_hours": null, "median_hours": null, "p90_hours": null, "buckets": buckets(&v)});
    }
    v.sort_unstable();
    let h = |ms: i64| (ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;
    let n = v.len();
    let sum: i64 = v.iter().sum();
    let pick = |q: f64| v[(((n - 1) as f64) * q).round() as usize];
    json!({
        "count": n,
        "avg_hours": h(sum / i64::try_from(n).unwrap_or(1)),
        "median_hours": h(pick(0.5)),
        "p90_hours": h(pick(0.9)),
        "buckets": buckets(&v),
    })
}

/// Moves `t` to the column its state now implies, recording the time spent
/// in the previous one.
fn advance(t: &mut Track, at: i64, keep: bool, segs: &mut HashMap<&'static str, Vec<i64>>) {
    let next = column_of(&t.state, t.claimed);
    if next != t.col {
        // Several events of one command share a timestamp; a stop of 0 ms
        // (for example released, then moved to review) is not a stay.
        if let Some(prev) = t.col
            && keep
            && at > t.since
        {
            segs.entry(prev).or_default().push(at - t.since);
        }
        t.col = next;
        t.since = at;
    }
}

/// Stats over segments that ended at or after `since` (all when None).
pub fn stats_json(c: &Connection, env: &Env, since: Option<i64>) -> Result<Value> {
    let mut st = c.prepare(
        "SELECT at, issue, action, details FROM events WHERE issue IS NOT NULL ORDER BY seq",
    )?;
    type Row = (i64, String, String, String);
    let rows: Vec<Row> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut tracks: HashMap<String, Track> = HashMap::new();
    let mut segs: HashMap<&'static str, Vec<i64>> = HashMap::new();
    let mut cycle: Vec<i64> = Vec::new();
    let mut lead: Vec<i64> = Vec::new();
    let keep = |end: i64| since.is_none_or(|s| end >= s);
    for (at, issue, action, details) in rows {
        let d: Value = serde_json::from_str(&details).unwrap_or(Value::Null);
        let mut ship_members: Vec<String> = Vec::new();
        {
            let t = tracks.entry(issue.clone()).or_default();
            match action.as_str() {
                "add" => {
                    t.state = d["state"].as_str().unwrap_or("ready").to_string();
                    t.added = Some(at);
                    t.since = at;
                    t.col = column_of(&t.state, false);
                }
                "state" => {
                    if let Some(to) = d["to"].as_str() {
                        t.state = to.to_string();
                    }
                }
                "claim" | "take" => {
                    t.claimed = true;
                    t.first_claim.get_or_insert(at);
                }
                "release" => t.claimed = false,
                "done" => {
                    t.state = "done".to_string();
                    t.claimed = false;
                    if keep(at) {
                        if let Some(f) = t.first_claim {
                            cycle.push(at - f);
                        }
                        if let Some(a) = t.added {
                            lead.push(at - a);
                        }
                    }
                }
                "ship" => {
                    ship_members = d["released"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                "delete" => {
                    t.state = "dropped".to_string();
                }
                _ => {}
            }
            if action != "ship" {
                advance(t, at, keep(at), &mut segs);
            }
        }
        for m in ship_members {
            let t = tracks.entry(m.clone()).or_default();
            t.state = "released".to_string();
            t.claimed = false;
            advance(t, at, keep(at), &mut segs);
        }
    }
    let mut columns = serde_json::Map::new();
    for col in COLUMNS {
        let mut v = summarize(segs.remove(col).unwrap_or_default());
        let ages: Vec<i64> = tracks
            .values()
            .filter(|t| t.col == Some(col))
            .map(|t| env.now - t.since)
            .collect();
        if let Some(o) = v.as_object_mut() {
            o.insert("now".to_string(), json!(ages.len()));
            let cur = summarize(ages);
            o.insert("now_avg_hours".to_string(), cur["avg_hours"].clone());
            o.insert("now_buckets".to_string(), cur["buckets"].clone());
            o.insert("now_max_hours".to_string(), {
                let m = tracks
                    .values()
                    .filter(|t| t.col == Some(col))
                    .map(|t| env.now - t.since)
                    .max();
                json!(m.map(|ms| (ms as f64 / 3_600_000.0 * 10.0).round() / 10.0))
            });
        }
        columns.insert((*col).to_string(), v);
    }
    Ok(json!({
        "columns": columns,
        "cycle_time": summarize(cycle),
        "lead_time": summarize(lead),
        "since": since.map(crate::time::iso),
        "bucket_minutes": BUCKET_MINUTES,
    }))
}

pub fn cmd_stats(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&["since"], 0)?;
    let since = a
        .opt("since")
        .map(parse_duration)
        .transpose()?
        .map(|d| env.now - d);
    let v = db::read(c, |c| stats_json(c, env, since))?;
    let f = |x: &Value| match x.as_f64() {
        Some(h) if h >= 48.0 => format!("{:.1}d", h / 24.0),
        Some(h) => format!("{h:.1}h"),
        None => "-".to_string(),
    };
    let mut text = String::from("column        passed  median     p90   now  oldest\n");
    for col in COLUMNS {
        let x = &v["columns"][*col];
        let _ = writeln!(
            text,
            "{:<12} {:>7} {:>7} {:>7} {:>5} {:>7}",
            col,
            x["count"].to_string(),
            f(&x["median_hours"]),
            f(&x["p90_hours"]),
            x["now"].to_string(),
            f(&x["now_max_hours"])
        );
    }
    for (k, label) in [
        ("cycle_time", "cycle time (claim to done)"),
        ("lead_time", "lead time (added to done)"),
    ] {
        let x = &v[k];
        let _ = writeln!(
            text,
            "{label}: {} issues, median {}, p90 {}",
            x["count"],
            f(&x["median_hours"]),
            f(&x["p90_hours"])
        );
    }
    Ok(Output::new(v, text))
}
