//! Owner-only approvals, read from a protected JSON file next to
//! `default.cfg`. `ait` only ever reads this file.
//!
//! ```json
//! {"approvals": [
//!   {"match": "CORE-1", "scope": "build", "from": "2026-11-01",
//!    "until": "2026-12-31", "exclude": ["CORE-1-05"]}
//! ]}
//! ```
//! `match` is `*`, a full id, or an id prefix ending at a `-` boundary
//! (`CORE` covers `CORE-1-00`; `CORE-1` covers `CORE-1-xx`). If the file does
//! not exist, approvals are not in use and every ready issue is offered.

use crate::error::{AitError, Result};
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
}

fn bad(m: impl Into<String>) -> AitError {
    AitError::new("CONFIG", format!("approvals file: {}", m.into()))
}

pub fn load(path: Option<&Path>) -> Result<Approvals> {
    let Some(p) = path else {
        return Ok(Approvals::default());
    };
    if !p.is_file() {
        return Ok(Approvals::default());
    }
    let text = std::fs::read_to_string(p)?;
    parse(&text)
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
    Ok(Approvals { list: Some(list) })
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

    /// Whether an approval with scope `build` covers `id` on `today`.
    pub fn covers(&self, id: &str, today: i64) -> bool {
        let Some(list) = &self.list else {
            return true;
        };
        list.iter().any(|a| {
            a.scope == "build"
                && matches(&a.pattern, id)
                && !a.exclude.iter().any(|x| matches(x, id))
                && a.from.is_none_or(|f| today >= f)
                && a.until.is_none_or(|u| today <= u)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let today = crate::time::parse_date("2026-11-26").unwrap();
        assert!(a.covers("CORE-1-00", today));
        assert!(!a.covers("CORE-1-05", today));
        assert!(!a.covers("CORE-10-00", today));
        assert!(!a.covers("CORE-2-00", today));
        assert!(a.covers("PAP-2-01", today));
        let later = crate::time::parse_date("2027-01-01").unwrap();
        assert!(!a.covers("CORE-1-00", later));
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("[{\"scope\":\"build\"}]").is_err());
        assert!(parse("[{\"match\":\"X\",\"until\":\"soon\"}]").is_err());
        assert!(parse("[{\"match\":\"X\",\"scope\":\"ship\"}]").is_err());
        assert!(parse("nope").is_err());
    }
}
