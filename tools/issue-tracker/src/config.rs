//! `default.cfg`: plain `key = value` lines, `#` comments.
//!
//! Lookup order: `--config <file>`, then `AIT_CONFIG`, then `default.cfg`
//! next to the `ait` executable. Without any file, built-in defaults apply.
//! Relative paths inside the file are relative to the file's folder.

use crate::error::{AitError, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit DB path; `None` means `<git-common-dir>/ait/tracker.db`.
    pub db: Option<PathBuf>,
    /// Allowed id prefixes; empty means any `[A-Z]{2,5}`.
    pub prefixes: Vec<String>,
    pub lease_minutes: i64,
    /// Backup folder; `None` means `<db folder>/backups`.
    pub backup_dir: Option<PathBuf>,
    /// Daily backups to keep; 0 disables automatic backups.
    pub backup_keep: usize,
    /// Owner-only approvals file; `None` or missing file = approvals not used.
    pub approvals: Option<PathBuf>,
    pub port: u16,
    pub main_branch: String,
    pub trailer: String,
    /// The config file that was loaded, if any.
    pub source: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db: None,
            prefixes: Vec::new(),
            lease_minutes: 60,
            backup_dir: None,
            backup_keep: 14,
            approvals: None,
            port: 7777,
            main_branch: "main".to_string(),
            trailer: "Issue-ID".to_string(),
            source: None,
        }
    }
}

pub fn load(explicit: Option<&str>) -> Result<Config> {
    let path: Option<PathBuf> = match explicit {
        Some(p) => Some(PathBuf::from(p)),
        None => match std::env::var("AIT_CONFIG") {
            Ok(p) if !p.trim().is_empty() => Some(PathBuf::from(p)),
            _ => {
                let beside_exe = std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|d| d.join("default.cfg")));
                beside_exe.filter(|p| p.is_file())
            }
        },
    };
    match path {
        None => Ok(Config::default()),
        Some(p) => {
            let text = std::fs::read_to_string(&p).map_err(|e| {
                AitError::new("CONFIG", format!("cannot read config {}: {e}", p.display()))
            })?;
            let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut cfg = parse(&text, &dir)?;
            cfg.source = Some(p);
            Ok(cfg)
        }
    }
}

fn resolve(dir: &Path, v: &str) -> PathBuf {
    let p = PathBuf::from(v);
    if p.is_absolute() { p } else { dir.join(p) }
}

pub fn parse(text: &str, dir: &Path) -> Result<Config> {
    let mut cfg = Config::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |m: &str| AitError::new("CONFIG", format!("default.cfg line {}: {m}", n + 1));
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| bad("expected key = value"))?;
        let (k, v) = (k.trim(), v.trim());
        match k {
            "db" => {
                cfg.db = if v.is_empty() || v == "auto" {
                    None
                } else {
                    Some(resolve(dir, v))
                }
            }
            "prefixes" => {
                cfg.prefixes = v
                    .split(',')
                    .map(|s| s.trim().to_ascii_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(p) = cfg.prefixes.iter().find(|p| !crate::ids::is_prefix(p)) {
                    return Err(bad(&format!("prefix '{p}' must be 2-5 letters A-Z")));
                }
            }
            "lease_minutes" => {
                cfg.lease_minutes = v
                    .parse()
                    .ok()
                    .filter(|m: &i64| (1..=10_080).contains(m))
                    .ok_or_else(|| bad("lease_minutes must be 1..10080"))?
            }
            "backup_dir" => {
                cfg.backup_dir = if v.is_empty() || v == "auto" {
                    None
                } else {
                    Some(resolve(dir, v))
                }
            }
            "backup_keep" => {
                cfg.backup_keep = v
                    .parse()
                    .ok()
                    .filter(|k: &usize| *k <= 3650)
                    .ok_or_else(|| bad("backup_keep must be 0..3650"))?
            }
            "approvals" => {
                cfg.approvals = if v.is_empty() {
                    None
                } else {
                    Some(resolve(dir, v))
                }
            }
            "port" => cfg.port = v.parse().ok().ok_or_else(|| bad("port must be 0..65535"))?,
            "main_branch" => {
                if v.is_empty() || v.starts_with('-') {
                    return Err(bad("main_branch must be a branch name"));
                }
                cfg.main_branch = v.to_string()
            }
            "trailer" => {
                if v.is_empty() || v.contains(':') {
                    return Err(bad("trailer must be a word such as Issue-ID"));
                }
                cfg.trailer = v.to_string()
            }
            _ => return Err(bad(&format!("unknown key '{k}'"))),
        }
    }
    Ok(cfg)
}

/// Finds the repository's common git dir from `start` by reading `.git`
/// and `commondir` files (no git process). All worktrees share it.
pub fn git_common_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(with_commondir(dotgit));
        }
        if dotgit.is_file() {
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim().to_string();
            return Some(with_commondir(resolve(dir, &target)));
        }
    }
    None
}

fn with_commondir(gitdir: PathBuf) -> PathBuf {
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) if !c.trim().is_empty() => resolve(&gitdir, c.trim()),
        _ => gitdir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys() {
        let dir = Path::new("base");
        let c = parse(
            "# comment\n db = data/t.db\nprefixes = core, PAP\nlease_minutes=30\nbackup_keep = 3\napprovals = approvals.json\nport = 8080\n",
            dir,
        )
        .unwrap();
        assert_eq!(c.db, Some(dir.join("data/t.db")));
        assert_eq!(c.prefixes, vec!["CORE", "PAP"]);
        assert_eq!(c.lease_minutes, 30);
        assert_eq!(c.backup_keep, 3);
        assert_eq!(c.port, 8080);
    }

    #[test]
    fn rejects_unknown_and_bad_values() {
        assert!(parse("nope = 1", Path::new(".")).is_err());
        assert!(parse("lease_minutes = 0", Path::new(".")).is_err());
        assert!(parse("prefixes = TOOLONG", Path::new(".")).is_err());
        assert!(parse("just text", Path::new(".")).is_err());
    }

    #[test]
    fn auto_db_is_none() {
        assert!(parse("db = auto", Path::new(".")).unwrap().db.is_none());
    }
}
