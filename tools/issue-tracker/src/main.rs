//! `ait`: agent-first issue tracker. One binary, one local SQLite database.
//! See tools/issue-tracker/README.md and DesignIteration6.md.

#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod approvals;
mod args;
mod backup;
mod board;
mod check;
mod claims;
mod config;
mod db;
mod error;
mod exchange;
mod guide;
mod ids;
mod issues;
mod notes;
mod output;
mod roadmap;
mod serve;
mod time;

use args::Args;
use error::{AitError, Result};
use output::Output;
use serde_json::json;
use std::path::PathBuf;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let json_mode = raw.iter().take_while(|a| *a != "--").any(|a| a == "--json");
    let code = match run(raw) {
        Ok(out) => {
            out.print(json_mode);
            out.exit
        }
        Err(e) => {
            e.print(json_mode);
            e.exit_code()
        }
    };
    std::process::exit(code);
}

fn db_path(explicit: Option<String>, cfg: &config::Config) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("AIT_DB")
        && !p.trim().is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = &cfg.db {
        return Ok(p.clone());
    }
    let cwd = std::env::current_dir()?;
    config::git_common_dir(&cwd)
        .map(|d| d.join("ait").join("tracker.db"))
        .ok_or_else(|| {
            AitError::new(
                "NO_DB",
                "not inside a git repository and no database configured: pass --db <file>, set AIT_DB, or set db in default.cfg",
            )
        })
}

fn run(raw: Vec<String>) -> Result<Output> {
    let mut a = Args::parse(raw)?;
    let db_opt = a.take_opt("db");
    let cfg_opt = a.take_opt("config");
    let as_opt = a.take_opt("as");
    let Some(cmd) = a.shift() else {
        return Ok(guide::usage());
    };
    match cmd.as_str() {
        "help" => return Ok(guide::usage()),
        "guide" => return Ok(guide::guide()),
        "version" => {
            return Ok(Output::new(
                json!({"version": env!("CARGO_PKG_VERSION"), "schema": db::schema_version()}),
                format!(
                    "ait {} (schema {})",
                    env!("CARGO_PKG_VERSION"),
                    db::schema_version()
                ),
            ));
        }
        _ => {}
    }
    if a.flag("help") {
        return Ok(guide::usage());
    }
    let cfg = config::load(cfg_opt.as_deref())?;
    let session = as_opt
        .or_else(|| std::env::var("AIT_SESSION").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(s) = &session
        && !db::valid_session_name(s)
    {
        return Err(AitError::usage(format!(
            "session name '{s}' must be 1-64 characters of letters, digits and . _ : @ -"
        )));
    }
    let path = db_path(db_opt, &cfg)?;
    let env = db::Env {
        now: time::now_ms()?,
        session,
        cfg,
        db_path: path,
    };
    let conn = db::open(&env.db_path)?;
    db::touch_session(&conn, &env)?;
    if cmd != "serve" && cmd != "import" {
        // A failed automatic backup must never block work; `ait check`
        // and `ait backup` surface problems.
        let _ = backup::auto(&conn, &env);
    }
    dispatch(&cmd, &conn, &env, &a)
}

/// Runs one command. Also used by the dashboard, so its writes pass the
/// same checks as the command line.
pub fn dispatch(cmd: &str, c: &rusqlite::Connection, env: &db::Env, a: &Args) -> Result<Output> {
    match cmd {
        "init" => {
            a.expect(&[], 0)?;
            let p = env.db_path.display().to_string();
            Ok(Output::new(
                json!({"db": p, "schema": db::schema_version()}),
                format!("database ready: {p} (schema {})", db::schema_version()),
            ))
        }
        "add" | "new" => issues::cmd_add(c, env, a),
        "edit" => issues::cmd_edit(c, env, a),
        "state" => issues::cmd_state(c, env, a),
        "delete" => issues::cmd_delete(c, env, a),
        "move" => issues::cmd_move(c, env, a),
        "link" => issues::cmd_link(c, env, a, false),
        "unlink" => issues::cmd_link(c, env, a, true),
        "show" => issues::cmd_show(c, env, a),
        "list" | "ls" => issues::cmd_list(c, env, a),
        "next" => claims::cmd_next(c, env, a),
        "roadmap" => roadmap::cmd_roadmap(c, env, a),
        "claim" => claims::cmd_claim(c, env, a),
        "release" => claims::cmd_release(c, env, a),
        "done" => claims::cmd_done(c, env, a),
        "heartbeat" => claims::cmd_heartbeat(c, env, a),
        "verify" => claims::cmd_verify(c, env, a),
        "note" => notes::cmd_note(c, env, a),
        "handoff" => notes::cmd_handoff(c, env, a),
        "brief" => notes::cmd_brief(c, env, a),
        "board" => board::cmd_board(c, env, a),
        "activity" => board::cmd_activity(c, env, a),
        "check" => check::cmd_check(c, env, a),
        "backup" => backup::cmd_backup(c, env, a),
        "export" => exchange::cmd_export(c, env, a),
        "import" => exchange::cmd_import(c, env, a),
        "serve" => serve::cmd_serve(c, env, a),
        other => Err(AitError::usage(format!(
            "unknown command '{other}' (run `ait help`)"
        ))),
    }
}
