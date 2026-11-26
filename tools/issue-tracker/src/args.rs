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
    "help",
];

/// Options accepted by every command.
const GLOBAL: &[&str] = &["json", "help", "db", "config", "as"];

#[derive(Debug, Default)]
pub struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
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
        Ok(Args { pos, opts })
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
    fn expect_rejects_unknown() {
        let a = p(&["x", "--bogus", "1"]);
        assert!(a.expect(&["body"], 1).is_err());
        assert!(a.expect(&["bogus"], 1).is_ok());
        assert!(a.expect(&["bogus"], 0).is_err());
    }
}
