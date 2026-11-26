//! `default.cfg`: plain `key = value` lines, `#` comments.
//!
//! Lookup order: `--config <file>`, then `AIT_CONFIG`, then `default.cfg`
//! next to the `ait` executable. Without any file, built-in defaults apply.
//! Relative paths inside the file are relative to the file's folder.

use crate::error::{AitError, Result};
use std::path::{Path, PathBuf};

/// Where an owner-controlled file is read from: a path on disk, or a file
/// committed in the repository (`git:<rev>:<path>`, e.g. `git:main:approvals.json`),
/// so an uncommitted or branch-local edit changes nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Git { rev: String, path: String },
}

impl Source {
    fn parse(v: &str, dir: &Path) -> std::result::Result<Source, String> {
        if let Some(rest) = v.strip_prefix("git:") {
            let (rev, path) = rest
                .split_once(':')
                .ok_or_else(|| "use git:<branch>:<path in repo>".to_string())?;
            let (rev, path) = (rev.trim(), path.trim().trim_start_matches('/'));
            if rev.is_empty() || rev.starts_with('-') || path.is_empty() {
                return Err("use git:<branch>:<path in repo>".to_string());
            }
            return Ok(Source::Git {
                rev: rev.to_string(),
                path: path.replace('\\', "/"),
            });
        }
        Ok(Source::File(resolve(dir, v)))
    }

    pub fn describe(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Git { rev, path } => format!("git:{rev}:{path}"),
        }
    }

    /// The file's text, or `None` if it does not exist (on disk or in `rev`).
    pub fn read(&self, repo: Option<&Path>) -> Result<Option<String>> {
        match self {
            Source::File(p) => {
                if !p.is_file() {
                    return Ok(None);
                }
                Ok(Some(std::fs::read_to_string(p)?))
            }
            Source::Git { rev, path } => {
                let repo = repo.ok_or_else(|| {
                    AitError::new(
                        "CONFIG",
                        format!(
                            "{} needs the repository: set repo = in default.cfg",
                            self.describe()
                        ),
                    )
                })?;
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["show", &format!("{rev}:{path}")])
                    .output()
                    .map_err(|e| AitError::new("CONFIG", format!("cannot run git: {e}")))?;
                if out.status.success() {
                    return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
                }
                let err = String::from_utf8_lossy(&out.stderr);
                if err.contains("does not exist") || err.contains("exists on disk, but not in") {
                    return Ok(None);
                }
                Err(AitError::new(
                    "CONFIG",
                    format!("cannot read {}: {}", self.describe(), err.trim()),
                ))
            }
        }
    }
}

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
    pub approvals: Option<Source>,
    /// Ids that were done before ait (count as merged for `check --git`).
    pub git_baseline: Option<Source>,
    /// The repository `check --git` and `git:` sources read; `None` = the
    /// repository that holds the database.
    pub repo: Option<PathBuf>,
    /// Agent tiers in order (cheapest first) with the model each maps to.
    pub agents: Vec<(String, String)>,
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
            git_baseline: None,
            repo: None,
            agents: Vec::new(),
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
                    Some(Source::parse(v, dir).map_err(|e| bad(&e))?)
                }
            }
            "git_baseline" => {
                cfg.git_baseline = if v.is_empty() {
                    None
                } else {
                    Some(Source::parse(v, dir).map_err(|e| bad(&e))?)
                }
            }
            "repo" => {
                cfg.repo = if v.is_empty() || v == "auto" {
                    None
                } else {
                    Some(resolve(dir, v))
                }
            }
            k if k.starts_with("agent.") => {
                let tier = k["agent.".len()..].trim().to_ascii_lowercase();
                if !valid_word(&tier) || v.is_empty() {
                    return Err(bad("use agent.<tier> = <model>, e.g. agent.deep = opus"));
                }
                if cfg.agents.iter().any(|(t, _)| *t == tier) {
                    return Err(bad(&format!("agent tier '{tier}' is set twice")));
                }
                cfg.agents.push((tier, v.to_string()));
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

/// Lowercase word: letters, digits, `-`, `_`, 1-32 characters.
pub fn valid_word(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl Config {
    /// The model configured for an agent tier.
    pub fn model_for(&self, tier: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|(t, _)| t == tier)
            .map(|(_, m)| m.as_str())
    }

    /// The repository to read git data from: `repo =`, else the working
    /// tree that owns the database's `.git` folder.
    pub fn repo_dir(&self, db_path: &Path) -> Option<PathBuf> {
        if let Some(r) = &self.repo {
            return Some(r.clone());
        }
        db_path
            .ancestors()
            .find(|d| d.file_name().is_some_and(|n| n == ".git"))
            .and_then(|g| g.parent())
            .map(Path::to_path_buf)
    }
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
    fn shipped_default_cfg_is_valid() {
        let c = parse(include_str!("../default.cfg"), Path::new("base")).unwrap();
        assert!(c.db.is_none());
        assert_eq!(c.lease_minutes, 60);
        assert_eq!(c.backup_keep, 14);
        // Approvals are opt-in: a configured but missing file approves
        // nothing, so the shipped config leaves the key unset.
        assert_eq!(c.approvals, None);
        assert_eq!(c.model_for("deep"), Some("opus"));
    }

    #[test]
    fn sources_tiers_and_repo() {
        let dir = Path::new("base");
        let c = parse(
            "approvals = git:main:docs/approvals.json\ngit_baseline = done.txt\nrepo = ..\nagent.light = haiku\nagent.deep = opus\n",
            dir,
        )
        .unwrap();
        assert_eq!(
            c.approvals,
            Some(Source::Git {
                rev: "main".into(),
                path: "docs/approvals.json".into()
            })
        );
        assert_eq!(c.git_baseline, Some(Source::File(dir.join("done.txt"))));
        assert_eq!(c.model_for("deep"), Some("opus"));
        assert_eq!(c.agents[0].0, "light");
        assert_eq!(c.repo_dir(Path::new("x/y.db")), Some(dir.join("..")));
        assert!(parse("approvals = git:main", dir).is_err());
        assert!(parse("agent.Deep Thought = x", dir).is_err());
        let d = Config::default();
        assert_eq!(
            d.repo_dir(Path::new("C:/r/.git/ait/tracker.db")),
            Some(PathBuf::from("C:/r"))
        );
        assert_eq!(d.repo_dir(Path::new("C:/tmp/t.db")), None);
    }

    #[test]
    fn shipped_approvals_example_is_valid() {
        let a = crate::approvals::parse(include_str!("../approvals.example.json")).unwrap();
        assert!(a.in_use());
    }

    #[test]
    fn auto_db_is_none() {
        assert!(parse("db = auto", Path::new(".")).unwrap().db.is_none());
    }
}
