//! Structured errors: every failure has a stable `code`, a human `message`,
//! a `retryable` hint and a process exit code.

use std::io::Write;

#[derive(Debug)]
pub struct AitError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    /// Extra structured data (for example every problem an import found).
    pub details: Option<serde_json::Value>,
}

pub type Result<T> = std::result::Result<T, AitError>;

impl AitError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        AitError {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn usage(message: impl Into<String>) -> Self {
        AitError::new("USAGE", message)
    }

    pub fn not_found(what: &str) -> Self {
        AitError::new("NOT_FOUND", format!("no issue matches '{what}'"))
    }

    /// Exit codes (documented in README.md):
    /// 0 ok, 1 internal, 2 usage/invalid input, 3 not found, 4 conflict,
    /// 5 busy (retry), 6 no work available, 7 check found problems.
    pub fn exit_code(&self) -> i32 {
        match self.code {
            "USAGE" | "INVALID_ID" | "INVALID_INPUT" | "INVALID_STATE" | "UNKNOWN_PREFIX"
            | "CONFIG" | "NO_SESSION" | "NO_DB" | "INVALID_JSON" | "IMPORT_INVALID" => 2,
            "NOT_FOUND" => 3,
            "ALREADY_CLAIMED" | "CLAIM_STALE" | "STALE_GENERATION" | "NOT_CLAIMANT"
            | "NOT_CLAIMED" | "VERSION_CONFLICT" | "CYCLE" | "IN_USE" | "NOT_EMPTY"
            | "ID_TAKEN" | "GROUP_FULL" | "ALREADY_DONE" => 4,
            "BUSY" => 5,
            "NO_WORK" => 6,
            "CHECK_FAILED" => 7,
            _ => 1,
        }
    }

    pub fn with_details(mut self, d: serde_json::Value) -> Self {
        self.details = Some(d);
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut e = serde_json::json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        });
        if let (Some(d), Some(o)) = (&self.details, e.as_object_mut()) {
            o.insert("details".to_string(), d.clone());
        }
        serde_json::json!({ "error": e })
    }

    /// JSON mode prints the error object on stdout (one stream for agents to
    /// parse); text mode prints a single line on stderr.
    pub fn print(&self, json: bool) {
        if json {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{}", self.to_json());
        } else {
            let mut err = std::io::stderr().lock();
            let hint = if self.retryable { " (retryable)" } else { "" };
            let _ = writeln!(err, "ait: error [{}]: {}{}", self.code, self.message, hint);
        }
    }
}

impl From<rusqlite::Error> for AitError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e
            && (f.code == rusqlite::ErrorCode::DatabaseBusy
                || f.code == rusqlite::ErrorCode::DatabaseLocked)
        {
            return AitError::new("BUSY", format!("database is busy: {e}")).retryable();
        }
        AitError::new("DB_ERROR", format!("database error: {e}"))
    }
}

impl From<std::io::Error> for AitError {
    fn from(e: std::io::Error) -> Self {
        AitError::new("IO_ERROR", format!("i/o error: {e}"))
    }
}

impl From<serde_json::Error> for AitError {
    fn from(e: serde_json::Error) -> Self {
        AitError::new("INVALID_JSON", format!("invalid JSON: {e}"))
    }
}
