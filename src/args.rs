//! Minimal hand-written argument parser (no third-party crate).
//!
//! `--name value`, `--name=value`, boolean `--flag`, positionals, and `--`
//! to end option parsing.

use crate::error::{AitError, Result};

/// Options that never take a value.
const BOOL_FLAGS: &[&str] = &[
    "json",
    "claim",
    "take",
    "take-stale",
    "force",
    "ready",
    "claimed",
    "stale",
    "all",
    "top",
    "bottom",
    "git",
    "frame",
    "issues",
    "tree",
    "read-only",
    "lan",
    "help",
];

/// Options accepted by every command.
const GLOBAL: &[&str] = &["json", "help", "db", "config", "as", "remote"];

#[derive(Debug, Default)]
pub struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
    /// Remote mode on the host: the files (`--body-file` ...) the client
    /// already read, by the path it was given. `Some` means this request
    /// must never read the host's disk or stdin.
    pub inline_files: Option<Vec<(String, String)>>,
}

/// Removes every `--name value` / `--name=value` from raw arguments (up to
/// a `--`) and returns the remaining arguments and the last value.
pub fn strip_opt(raw: &[String], name: &str) -> (Vec<String>, Option<String>) {
    let mut out = Vec::with_capacity(raw.len());
    let mut found = None;
    let mut it = raw.iter();
    let long = format!("--{name}");
    let eq = format!("--{name}=");
    while let Some(a) = it.next() {
        if a == "--" {
            out.push(a.clone());
            out.extend(it.cloned());
            break;
        }
        if *a == long {
            match it.next() {
                Some(v) => found = Some(v.clone()),
                // Leave it for the parser to report.
                None => out.push(a.clone()),
            }
            continue;
        }
        if let Some(v) = a.strip_prefix(&eq) {
            found = Some(v.to_string());
            continue;
        }
        if let Some(n) = a.strip_prefix("--")
            && !n.contains('=')
            && !BOOL_FLAGS.contains(&n)
            && !n.is_empty()
        {
            // Another option with a value: keep the value with it.
            out.push(a.clone());
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
            continue;
        }
        out.push(a.clone());
    }
    (out, found)
}

impl Args {
    pub fn parse(raw: Vec<String>) -> Result<Args> {
        let mut pos = Vec::new();
        let mut opts = Vec::new();
        let mut it = raw.into_iter();
        let mut only_pos = false;
        while let Some(a) = it.next() {
            if only_pos {
                pos.push(a);
                continue;
            }
            if a == "--" {
                only_pos = true;
                continue;
            }
            if a == "-h" {
                opts.push(("help".to_string(), None));
                continue;
            }
            if let Some(name) = a.strip_prefix("--") {
                if name.is_empty() {
                    return Err(AitError::usage("empty option name"));
                }
                if let Some((n, v)) = name.split_once('=') {
                    opts.push((n.to_string(), Some(v.to_string())));
                    continue;
                }
                if BOOL_FLAGS.contains(&name) {
                    opts.push((name.to_string(), None));
                    continue;
                }
                match it.next() {
                    Some(v) => opts.push((name.to_string(), Some(v))),
                    None => {
                        return Err(AitError::usage(format!("option --{name} needs a value")));
                    }
                }
            } else {
                pos.push(a);
            }
        }
        Ok(Args {
            pos,
            opts,
            inline_files: None,
        })
    }

    /// Text of a file named on the command line (`-` = stdin). On the host
    /// of a remote call it comes only from what the client sent.
    pub fn read_file(&self, path: &str) -> Result<String> {
        if let Some(files) = &self.inline_files {
            return files
                .iter()
                .rev()
                .find(|(p, _)| p == path)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| {
                    AitError::new(
                        "INVALID_INPUT",
                        format!("cannot read {path}: the remote client did not send it"),
                    )
                });
        }
        if path == "-" {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            return Ok(s);
        }
        std::fs::read_to_string(path)
            .map_err(|e| AitError::new("INVALID_INPUT", format!("cannot read {path}: {e}")))
    }

    /// Removes and returns the first positional argument.
    pub fn shift(&mut self) -> Option<String> {
        if self.pos.is_empty() {
            None
        } else {
            Some(self.pos.remove(0))
        }
    }

    pub fn pos(&self, i: usize) -> Option<&str> {
        self.pos.get(i).map(String::as_str)
    }

    pub fn req_pos(&self, i: usize, what: &str) -> Result<&str> {
        self.pos(i)
            .ok_or_else(|| AitError::usage(format!("missing argument: {what}")))
    }

    pub fn flag(&self, name: &str) -> bool {
        self.opts.iter().any(|(k, _)| k == name)
    }

    /// Last value given for an option.
    pub fn opt(&self, name: &str) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }

    pub fn take_opt(&mut self, name: &str) -> Option<String> {
        let mut found = None;
        self.opts.retain(|(k, v)| {
            if k == name {
                found = v.clone();
                false
            } else {
                true
            }
        });
        found
    }

    pub fn opt_i64(&self, name: &str) -> Result<Option<i64>> {
        match self.opt(name) {
            None => Ok(None),
            Some(v) => v
                .trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| AitError::usage(format!("--{name} must be a whole number"))),
        }
    }

    /// Rejects unknown options and surplus positionals.
    pub fn expect(&self, allowed: &[&str], max_pos: usize) -> Result<()> {
        for (k, _) in &self.opts {
            if !allowed.contains(&k.as_str()) && !GLOBAL.contains(&k.as_str()) {
                return Err(AitError::usage(format!("unknown option --{k}")));
            }
        }
        if self.pos.len() > max_pos {
            return Err(AitError::usage(format!(
                "unexpected argument '{}' (quote text that contains spaces)",
                self.pos[max_pos]
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Args {
        Args::parse(v.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    #[test]
    fn parses_options_and_positionals() {
        let a = p(&[
            "add", "CORE", "--body", "x y", "--json", "--size=S", "Title",
        ]);
        assert_eq!(a.pos(0), Some("add"));
        assert_eq!(a.pos(2), Some("Title"));
        assert_eq!(a.opt("body"), Some("x y"));
        assert_eq!(a.opt("size"), Some("S"));
        assert!(a.flag("json"));
    }

    #[test]
    fn double_dash_ends_options() {
        let a = p(&["note", "X", "--", "--not-an-option"]);
        assert_eq!(a.pos(2), Some("--not-an-option"));
        assert!(!a.flag("not-an-option"));
    }

    #[test]
    fn missing_value_is_usage_error() {
        let r = Args::parse(vec!["--body".to_string()]);
        assert_eq!(r.unwrap_err().code, "USAGE");
    }

    #[test]
    fn strip_opt_removes_only_that_option() {
        let raw: Vec<String> = [
            "brief", "X", "--out", "b.md", "--notes", "--out", "--json", "--", "--out", "z",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (rest, v) = strip_opt(&raw, "out");
        // `--notes` takes the next word (`--out`) as its value.
        assert_eq!(v.as_deref(), Some("b.md"));
        assert_eq!(
            rest,
            vec![
                "brief", "X", "--notes", "--out", "--json", "--", "--out", "z"
            ]
        );
        let raw: Vec<String> = ["--remote=h:1", "list", "--remote", "h:2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (rest, v) = strip_opt(&raw, "remote");
        assert_eq!(v.as_deref(), Some("h:2"));
        assert_eq!(rest, vec!["list"]);
    }

    #[test]
    fn inline_files_never_touch_the_disk() {
        let mut a = p(&["add", "X", "t", "--body-file", "Cargo.toml"]);
        a.inline_files = Some(vec![("Cargo.toml".into(), "sent".into())]);
        assert_eq!(a.read_file("Cargo.toml").unwrap(), "sent");
        assert_eq!(a.read_file("-").unwrap_err().code, "INVALID_INPUT");
        assert_eq!(
            a.read_file("src/main.rs").unwrap_err().code,
            "INVALID_INPUT"
        );
    }

    #[test]
    fn expect_rejects_unknown() {
        let a = p(&["x", "--bogus", "1"]);
        assert!(a.expect(&["body"], 1).is_err());
        assert!(a.expect(&["bogus"], 1).is_ok());
        assert!(a.expect(&["bogus"], 0).is_err());
    }
}
