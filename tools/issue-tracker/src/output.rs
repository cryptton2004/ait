//! Command results: a JSON value (for `--json`) plus a short text rendering.

use serde_json::Value;
use std::io::Write;

pub struct Output {
    pub json: Value,
    pub text: String,
    pub exit: i32,
}

impl Output {
    pub fn new(json: Value, text: impl Into<String>) -> Self {
        Output {
            json,
            text: text.into(),
            exit: 0,
        }
    }

    pub fn print(&self, json_mode: bool) {
        let mut out = std::io::stdout().lock();
        if json_mode {
            let _ = writeln!(out, "{}", self.json);
        } else {
            let t = self.text.trim_end_matches('\n');
            if !t.is_empty() {
                let _ = writeln!(out, "{t}");
            }
        }
    }
}

/// Strips terminal control characters from untrusted text before printing
/// it as text (keeps newlines and tabs).
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// One-line version of untrusted text, cut to `max` characters.
pub fn line(s: &str, max: usize) -> String {
    let one: String = clean(s)
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    if one.chars().count() > max {
        let cut: String = one.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        one
    }
}

pub fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(clean("a\u{1b}[31mb\nc"), "a[31mb\nc");
        assert_eq!(line("a\nb", 10), "a b");
        assert_eq!(line("abcdef", 4), "abc…");
    }
}
