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
    /// WIP limits: `in_progress`, `in_review` and `agent.<tier>`.
    pub wip: Vec<(String, i64)>,
    pub port: u16,
    /// Local web origins (a factory UI) allowed to read and write through
    /// the dashboard API, e.g. http://localhost:5173.
    pub allowed_origins: Vec<String>,
    pub main_branch: String,
    pub trailer: String,
    /// `ait serve` also starts the TLS listener for other PCs.
    pub lan: bool,
    /// Address the LAN listener binds to; `None` = 0.0.0.0.
    pub bind: Option<std::net::IpAddr>,
    /// Port of the LAN listener; `None` = dashboard port + 1.
    pub lan_port: Option<u16>,
    /// Addresses allowed to connect to the LAN listener; `None` = private
    /// ranges and loopback.
    pub lan_allow: Option<Vec<crate::lan::Cidr>>,
    /// TLS certificate and key; `None` = `<db folder>/tls/cert.pem`, `key.pem`.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Remote mode: `host:port` of an `ait serve --lan` host.
    pub remote: Option<String>,
    pub remote_token_file: Option<PathBuf>,
    /// Pinned host certificate, 64 lowercase hex characters.
    pub remote_fingerprint: Option<String>,
    /// Time to connect and finish the TLS handshake.
    pub remote_timeout_ms: u64,
    /// Time to wait for the answer once the command was sent.
    pub remote_reply_timeout_ms: u64,
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
            wip: Vec::new(),
            allowed_origins: Vec::new(),
            port: 7777,
            main_branch: "main".to_string(),
            trailer: "Issue-ID".to_string(),
            lan: false,
            bind: None,
            lan_port: None,
            lan_allow: None,
            tls_cert: None,
            tls_key: None,
            remote: None,
            remote_token_file: None,
            remote_fingerprint: None,
            remote_timeout_ms: 3000,
            remote_reply_timeout_ms: 60_000,
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

/// A path setting: empty or `auto` means the built-in default.
fn opt_path(dir: &Path, v: &str) -> Option<PathBuf> {
    if v.is_empty() || v == "auto" {
        None
    } else {
        Some(resolve(dir, v))
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
            k if k.starts_with("wip.") => {
                let what = k["wip.".len()..].trim().to_ascii_lowercase();
                let ok_key = what == "in_progress"
                    || what == "in_review"
                    || what.strip_prefix("agent.").is_some_and(valid_word);
                let n: i64 = v.parse().unwrap_or(-1);
                if !ok_key || n < 1 {
                    return Err(bad(
                        "use wip.in_progress = N, wip.in_review = N or wip.agent.<tier> = N (N >= 1)",
                    ));
                }
                cfg.wip.retain(|(w, _)| *w != what);
                cfg.wip.push((what, n));
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
            "allowed_origins" => {
                cfg.allowed_origins.clear();
                for o in v.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                    if !local_origin(o) {
                        return Err(bad(&format!(
                            "allowed_origins takes local origins such as http://localhost:5173 (not '{o}')"
                        )));
                    }
                    cfg.allowed_origins.push(o.to_ascii_lowercase());
                }
            }
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
            "lan" => {
                cfg.lan = match v {
                    "true" | "yes" | "on" | "1" => true,
                    "false" | "no" | "off" | "0" | "" => false,
                    _ => return Err(bad("lan must be true or false")),
                }
            }
            "bind" => {
                cfg.bind = if v.is_empty() {
                    None
                } else {
                    Some(v.parse().map_err(|_| {
                        bad("bind must be an IP address such as 0.0.0.0 or 192.168.1.10")
                    })?)
                }
            }
            "lan_port" => {
                cfg.lan_port = if v.is_empty() || v == "auto" {
                    None
                } else {
                    Some(
                        v.parse()
                            .ok()
                            .ok_or_else(|| bad("lan_port must be 0..65535"))?,
                    )
                }
            }
            "lan_allow" => {
                let mut list = Vec::new();
                for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    list.push(crate::lan::Cidr::parse(part).map_err(|e| bad(&e))?);
                }
                if list.is_empty() {
                    return Err(bad(
                        "lan_allow needs at least one address range, e.g. 192.168.1.0/24",
                    ));
                }
                cfg.lan_allow = Some(list);
            }
            "tls_cert" => cfg.tls_cert = opt_path(dir, v),
            "tls_key" => cfg.tls_key = opt_path(dir, v),
            "remote" => {
                cfg.remote = if v.is_empty() || v == "off" {
                    None
                } else {
                    Some(crate::remote::check_target(v).map_err(|e| bad(&e))?)
                }
            }
            "remote_token_file" => cfg.remote_token_file = opt_path(dir, v),
            "remote_fingerprint" => {
                cfg.remote_fingerprint = if v.is_empty() {
                    None
                } else {
                    Some(crate::tls::parse_fingerprint(v).map_err(|e| bad(&e))?)
                }
            }
            "remote_timeout_ms" => {
                cfg.remote_timeout_ms = v
                    .parse()
                    .ok()
                    .filter(|m: &u64| (1..=600_000).contains(m))
                    .ok_or_else(|| bad("remote_timeout_ms must be 1..600000"))?
            }
            "remote_reply_timeout_ms" => {
                cfg.remote_reply_timeout_ms = v
                    .parse()
                    .ok()
                    .filter(|m: &u64| (1..=3_600_000).contains(m))
                    .ok_or_else(|| bad("remote_reply_timeout_ms must be 1..3600000"))?
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

/// `http(s)://localhost|127.0.0.1|[::1][:port]`, nothing else.
pub fn local_origin(o: &str) -> bool {
    let rest = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"));
    let Some(rest) = rest else {
        return false;
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && h != "[" && !p.contains(']') => (h, Some(p)),
        _ => (rest, None),
    };
    let host_ok = matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1]"
    );
    let port_ok =
        port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    host_ok && port_ok
}

impl Config {
    /// The WIP limit for `in_progress`, `in_review` or `agent.<tier>`.
    pub fn wip_limit(&self, what: &str) -> Option<i64> {
        self.wip.iter().find(|(w, _)| w == what).map(|(_, n)| *n)
    }

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
    fn local_origins_only() {
        for ok in [
            "http://localhost:5173",
            "http://127.0.0.1",
            "https://[::1]:8443",
        ] {
            assert!(local_origin(ok), "{ok}");
        }
        for bad in [
            "http://evil.example",
            "http://localhost.evil.example",
            "localhost:5173",
            "http://127.0.0.1:99999x",
            "file://localhost",
            "http://localhost:",
        ] {
            assert!(!local_origin(bad), "{bad}");
        }
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
    fn lan_and_remote_keys() {
        let dir = Path::new("base");
        let c = parse(
            "lan = true
bind = 127.0.0.1
lan_port = 9000
lan_allow = 192.168.1.0/24, 10.1.2.3
tls_cert = tls/c.pem
tls_key = tls/k.pem
remote = host-a:7778
remote_token_file = pc2.token
remote_fingerprint = sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD
remote_timeout_ms = 500
",
            dir,
        )
        .unwrap();
        assert!(c.lan);
        assert_eq!(c.bind, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(c.lan_port, Some(9000));
        assert_eq!(c.lan_allow.as_ref().map(Vec::len), Some(2));
        assert_eq!(c.tls_cert, Some(dir.join("tls/c.pem")));
        assert_eq!(c.remote.as_deref(), Some("host-a:7778"));
        assert_eq!(c.remote_token_file, Some(dir.join("pc2.token")));
        assert_eq!(
            c.remote_fingerprint.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(c.remote_timeout_ms, 500);
        for bad in [
            "lan = maybe",
            "bind = everywhere",
            "lan_allow = 10.0.0.0/33",
            "lan_allow = ,",
            "remote = no-port",
            "remote = host:99999",
            "remote_fingerprint = sha256:1234",
            "remote_timeout_ms = 0",
        ] {
            assert!(parse(bad, dir).is_err(), "{bad}");
        }
    }

    #[test]
    fn auto_db_is_none() {
        assert!(parse("db = auto", Path::new(".")).unwrap().db.is_none());
    }
}
