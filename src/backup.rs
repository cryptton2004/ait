//! Backups with `VACUUM INTO`: one automatic backup per day (the first `ait`
//! call of the day makes it), plus `ait backup` on demand. Keeps the newest
//! `backup_keep` files.

use crate::args::Args;
use crate::db::{self, Env};
use crate::error::Result;
use crate::output::Output;
use crate::time::{day, iso};
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn backup_dir(env: &Env) -> PathBuf {
    env.cfg.backup_dir.clone().unwrap_or_else(|| {
        env.db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
            .join("backups")
    })
}

fn vacuum_into(c: &Connection, path: &Path) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    c.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
    Ok(())
}

/// Backup files this tool made: `tracker-<date>[T<time>][-n].db`.
fn is_backup_name(name: &str) -> bool {
    name.strip_prefix("tracker-")
        .and_then(|r| r.strip_suffix(".db"))
        .is_some_and(|mid| {
            mid.len() >= 10
                && mid.as_bytes()[..10].iter().enumerate().all(|(i, b)| {
                    if i == 4 || i == 7 {
                        *b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                })
                && mid[10..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'T' || b == b'-')
        })
}

/// Deletes the oldest backups beyond `keep`. Only touches files whose names
/// match the backup pattern inside the backup folder.
pub fn prune(dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_backup_name(n))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    names.sort();
    let mut removed = Vec::new();
    if names.len() > keep {
        let n = names.len() - keep;
        for name in names.into_iter().take(n) {
            let p = dir.join(&name);
            if std::fs::remove_file(&p).is_ok() {
                removed.push(p);
            }
        }
    }
    Ok(removed)
}

/// Makes today's backup if nobody has yet. Exactly one process wins the
/// right to back up each day (a conditional update of `meta`).
pub fn auto(c: &Connection, env: &Env) -> Result<Option<PathBuf>> {
    if env.cfg.backup_keep == 0 {
        return Ok(None);
    }
    let today = day(env.now);
    if db::meta_get(c, "last_backup_day")?.as_deref() == Some(today.as_str()) {
        return Ok(None);
    }
    let won = db::write(c, |c| {
        let n = c.execute(
            "INSERT INTO meta(key, value) VALUES ('last_backup_day', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE meta.value <> excluded.value",
            [&today],
        )?;
        Ok(n > 0)
    })?;
    if !won {
        return Ok(None);
    }
    let dir = backup_dir(env);
    let path = dir.join(format!("tracker-{today}.db"));
    if path.exists() {
        return Ok(None);
    }
    vacuum_into(c, &path)?;
    prune(&dir, env.cfg.backup_keep)?;
    Ok(Some(path))
}

pub fn cmd_backup(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&[], 0)?;
    let dir = backup_dir(env);
    let stamp: String = iso(env.now)[..19].chars().filter(|ch| *ch != ':').collect();
    let mut path = dir.join(format!("tracker-{stamp}.db"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("tracker-{stamp}-{n}.db"));
        n += 1;
    }
    vacuum_into(c, &path)?;
    let removed = if env.cfg.backup_keep > 0 {
        prune(&dir, env.cfg.backup_keep)?
    } else {
        Vec::new()
    };
    let p = path.display().to_string();
    Ok(Output::new(
        json!({"backup": p, "pruned": removed.iter().map(|r| r.display().to_string()).collect::<Vec<_>>()}),
        format!("backup written to {p}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names() {
        assert!(is_backup_name("tracker-2026-11-26.db"));
        assert!(is_backup_name("tracker-2026-11-26T053340.db"));
        assert!(is_backup_name("tracker-2026-11-26T053340-2.db"));
        assert!(!is_backup_name("tracker.db"));
        assert!(!is_backup_name("tracker-notes.db"));
        assert!(!is_backup_name("other-2026-11-26.db"));
        assert!(!is_backup_name("tracker-2026-11-26.db-wal"));
    }
}
