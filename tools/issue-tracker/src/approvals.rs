//! Owner-only approvals. `ait` only ever reads this file. It can live on disk
//! (`approvals = approvals.json`) or be read from a committed blob
//! (`approvals = git:main:docs/planning/approvals.json`).
//!
//! ```json
//! {"approvals": [
//!   {"match": "CORE-1", "scope": "build", "from": "2026-11-01",
//!    "until": "2026-12-31", "exclude": ["CORE-1-05"], "note": "ignored"}
//! ]}
//! ```
//! A top-level array works too. `match` is `*`, a full id, or an id prefix
//! ending at a `-` boundary (`CORE` covers `CORE-1-00`; `CORE-1` covers
//! `CORE-1-xx`), case-insensitive.
//!
//! Scopes: `build` lets agents implement `ready` issues (`ait next`);
//! `frame` lets agents write `idea`/`planned` issues up to `ready`
//! (`ait next --frame`). A build approval also allows framing.
//! If the file does not exist, approvals are not in use and everything is offered.

use crate::config::Source;
use crate::error::{AitError, Result};
use crate::time::day;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Approval {
    pub pattern: String,
    pub scope: String,
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Approvals {
    /// `None` = approvals not in use.
    pub list: Option<Vec<Approval>>,
    /// The configured source that does not exist: nothing is approved.
    pub missing: Option<String>,
}

/// Whether an approval covers an issue, and if not, the most useful reason.
#[derive(Debug, Clone, PartialEq)]
pub enum Cover {
    NotInUse,
    Covered { pattern: String },
    FrameOnly { pattern: String, until: Option<i64> },
    Excluded { pattern: String, by: String },
    Expired { pattern: String, until: i64 },
    NotStarted { pattern: String, from: i64 },
    NotCovered,
    Missing { source: String },
}

impl Cover {
    pub fn ok(&self) -> bool {
        matches!(self, Cover::NotInUse | Cover::Covered { .. })
    }

    /// Short machine code for `--explain` output.
    pub fn code(&self) -> &'static str {
        match self {
            Cover::NotInUse | Cover::Covered { .. } => "APPROVED",
            Cover::FrameOnly { .. } => "FRAME_ONLY",
            Cover::Excluded { .. } => "EXCLUDED",
            Cover::Expired { .. } => "EXPIRED",
            Cover::NotStarted { .. } => "NOT_STARTED",
            Cover::NotCovered => "NOT_APPROVED",
            Cover::Missing { .. } => "NO_APPROVALS_FILE",
        }
    }

    pub fn message(&self) -> String {
        let d = |n: i64| day(n * 86_400_000);
        match self {
            Cover::NotInUse => "approvals are not in use".to_string(),
            Cover::Covered { pattern } => format!("approved ({pattern})"),
            Cover::FrameOnly { pattern, until } => match until {
                Some(u) => format!(
                    "frame only ({pattern}, until {}): approved to write up, not to build",
                    d(*u)
                ),
                None => format!("frame only ({pattern}): approved to write up, not to build"),
            },
            Cover::Excluded { pattern, by } => {
                format!("excluded by {by} in the {pattern} approval")
            }
            Cover::Expired { pattern, until } => {
                format!("approval expired {} ({pattern})", d(*until))
            }
            Cover::NotStarted { pattern, from } => {
                format!("approval starts {} ({pattern})", d(*from))
            }
            Cover::NotCovered => "no owner approval covers it".to_string(),
            Cover::Missing { source } => {
                format!("the approvals file {source} does not exist, so nothing is approved")
            }
        }
    }
}

fn bad(m: impl Into<String>) -> AitError {
    AitError::new("CONFIG", format!("approvals file: {}", m.into()))
}

pub fn load(src: Option<&Source>, repo: Option<&Path>) -> Result<Approvals> {
    let Some(src) = src else {
        return Ok(Approvals::default());
    };
    match src.read(repo)? {
        // Fail closed: a configured but missing file approves nothing.
        None => Ok(Approvals {
            list: Some(Vec::new()),
            missing: Some(src.describe()),
        }),
        Some(text) => parse(&text),
    }
}

pub fn parse(text: &str) -> Result<Approvals> {
    let v: Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    let arr = match &v {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("approvals")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("expected {\"approvals\": [...]}"))?,
        _ => return Err(bad("expected an object or array")),
    };
    let mut list = Vec::new();
    for (i, a) in arr.iter().enumerate() {
        let o = a
            .as_object()
            .ok_or_else(|| bad(format!("entry {i} is not an object")))?;
        let pattern = o
            .get("match")
            .and_then(Value::as_str)
            .ok_or_else(|| bad(format!("entry {i} needs \"match\"")))?
            .trim()
            .to_ascii_uppercase();
        if pattern.is_empty() {
            return Err(bad(format!("entry {i} has an empty \"match\"")));
        }
        let scope = o
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("build")
            .to_string();
        if scope != "build" && scope != "frame" {
            return Err(bad(format!("entry {i}: scope must be build or frame")));
        }
        let date = |k: &str| -> Result<Option<i64>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => crate::time::parse_date(s)
                    .map(Some)
                    .ok_or_else(|| bad(format!("entry {i}: {k} must be YYYY-MM-DD"))),
                Some(_) => Err(bad(format!("entry {i}: {k} must be a date string"))),
            }
        };
        let from = date("from")?;
        let until = date("until")?;
        let exclude = match o.get("exclude") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(x)) => x
                .iter()
                .map(|e| {
                    e.as_str()
                        .map(|s| s.trim().to_ascii_uppercase())
                        .ok_or_else(|| bad(format!("entry {i}: exclude must list strings")))
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => return Err(bad(format!("entry {i}: exclude must be a list"))),
        };
        list.push(Approval {
            pattern,
            scope,
            from,
            until,
            exclude,
        });
    }
    Ok(Approvals {
        list: Some(list),
        missing: None,
    })
}

fn matches(pattern: &str, id: &str) -> bool {
    pattern == "*"
        || id == pattern
        || (id.starts_with(pattern) && id[pattern.len()..].starts_with('-'))
}

impl Approvals {
    pub fn in_use(&self) -> bool {
        self.list.is_some()
    }

    /// Checks `id` for `scope` ("build" or "frame") on day number `today`.
    /// A build approval also allows framing.
    pub fn check(&self, id: &str, today: i64, scope: &str) -> Cover {
        let Some(list) = &self.list else {
            return Cover::NotInUse;
        };
        if let Some(source) = &self.missing {
            return Cover::Missing {
                source: source.clone(),
            };
        }
        let id = id.to_ascii_uppercase();
        let scope_ok =
            |a: &Approval| a.scope == "build" || (scope == "frame" && a.scope == "frame");
        let relevant: Vec<&Approval> = list.iter().filter(|a| matches(&a.pattern, &id)).collect();
        let excluded = |a: &Approval| a.exclude.iter().find(|x| matches(x, &id)).cloned();
        let active =
            |a: &Approval| a.from.is_none_or(|f| today >= f) && a.until.is_none_or(|u| today <= u);
        if let Some(a) = relevant
            .iter()
            .find(|a| scope_ok(a) && active(a) && excluded(a).is_none())
        {
            return Cover::Covered {
                pattern: a.pattern.clone(),
            };
        }
        // Explain, most specific cause first.
        for a in relevant.iter().filter(|a| scope_ok(a)) {
            if let Some(by) = excluded(a) {
                return Cover::Excluded {
                    pattern: a.pattern.clone(),
                    by,
                };
            }
        }
        if scope == "build"
            && let Some(a) = relevant
                .iter()
                .find(|a| a.scope == "frame" && active(a) && excluded(a).is_none())
        {
            return Cover::FrameOnly {
                pattern: a.pattern.clone(),
                until: a.until,
            };
        }
        for a in relevant.iter().filter(|a| scope_ok(a)) {
            if let Some(u) = a.until
                && today > u
            {
                return Cover::Expired {
                    pattern: a.pattern.clone(),
                    until: u,
                };
            }
            if let Some(f) = a.from
                && today < f
            {
                return Cover::NotStarted {
                    pattern: a.pattern.clone(),
                    from: f,
                };
            }
        }
        Cover::NotCovered
    }

    /// Whether a build approval covers `id` on `today`.
    #[cfg(test)]
    pub fn covers(&self, id: &str, today: i64) -> bool {
        self.check(id, today, "build").ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day_of(s: &str) -> i64 {
        crate::time::parse_date(s).unwrap()
    }

    #[test]
    fn absent_means_everything_approved() {
        let a = Approvals::default();
        assert!(!a.in_use());
        assert!(a.covers("CORE-1-00", 0));
    }

    #[test]
    fn prefix_group_exclude_and_dates() {
        let a = parse(
            r#"{"approvals":[{"match":"CORE-1","exclude":["CORE-1-05"],"until":"2026-12-31"},{"match":"PAP-2-01"}]}"#,
        )
        .unwrap();
        let today = day_of("2026-11-26");
        assert!(a.covers("CORE-1-00", today));
        assert!(!a.covers("CORE-1-05", today));
        assert!(!a.covers("CORE-10-00", today));
        assert!(!a.covers("CORE-2-00", today));
        assert!(a.covers("PAP-2-01", today));
        assert!(!a.covers("CORE-1-00", day_of("2027-01-01")));
    }

    #[test]
    fn explains_each_cause() {
        // The owner's drive file, unchanged: lowercase, top-level array, notes.
        let a = parse(
            r#"[
  {"match": "core", "scope": "build", "from": "2026-11-22", "until": "2026-12-31", "exclude": ["core-3-11"], "note": "x"},
  {"match": "fw", "scope": "frame", "from": "2026-11-12", "until": null},
  {"match": "bug", "scope": "build", "from": "2026-10-16", "until": "2026-11-01"},
  {"match": "ms", "scope": "build", "from": "2026-12-01", "until": null}
]"#,
        )
        .unwrap();
        let t = day_of("2026-11-26");
        assert_eq!(a.check("CORE-3-00", t, "build").code(), "APPROVED");
        assert_eq!(a.check("CORE-3-11", t, "build").code(), "EXCLUDED");
        assert_eq!(a.check("FW-1-00", t, "build").code(), "FRAME_ONLY");
        assert_eq!(a.check("FW-1-00", t, "frame").code(), "APPROVED");
        assert_eq!(a.check("CORE-3-00", t, "frame").code(), "APPROVED");
        assert_eq!(a.check("BUG-40-00", t, "build").code(), "EXPIRED");
        assert_eq!(a.check("MS-2-00", t, "build").code(), "NOT_STARTED");
        assert_eq!(a.check("LOR-1-00", t, "build").code(), "NOT_APPROVED");
        assert!(
            a.check("BUG-40-00", t, "build")
                .message()
                .contains("2026-11-01")
        );
        assert!(
            a.check("CORE-3-11", t, "build")
                .message()
                .contains("CORE-3-11")
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("[{\"scope\":\"build\"}]").is_err());
        assert!(parse("[{\"match\":\"X\",\"until\":\"soon\"}]").is_err());
        assert!(parse("[{\"match\":\"X\",\"scope\":\"ship\"}]").is_err());
        assert!(parse("nope").is_err());
    }
}
