//! Opening the database, pragmas, numbered migrations, transactions, and the
//! shared per-call context.

use crate::config::Config;
use crate::error::{AitError, Result};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Numbered schema migrations. Never edit a released entry: append a new one.
/// `PRAGMA user_version` holds how many have been applied.
pub const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE issues (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL,
        reason TEXT,
        ord INTEGER NOT NULL,
        size TEXT,
        spec TEXT,
        parent TEXT,
        legacy TEXT UNIQUE,
        summary TEXT,
        version INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        done_at INTEGER
    );
    CREATE INDEX issues_state_ord ON issues(state, ord);
    CREATE INDEX issues_ord ON issues(ord);
    CREATE TABLE retired_ids (id TEXT PRIMARY KEY, at INTEGER NOT NULL);
    CREATE TABLE links (
        src TEXT NOT NULL,
        kind TEXT NOT NULL,
        dst TEXT NOT NULL,
        PRIMARY KEY (src, kind, dst)
    );
    CREATE INDEX links_dst ON links(dst, kind);
    CREATE TABLE claims (
        issue TEXT PRIMARY KEY,
        session TEXT,
        generation INTEGER NOT NULL,
        claimed_at INTEGER,
        lease_until INTEGER
    );
    CREATE INDEX claims_session ON claims(session);
    CREATE TABLE sessions (
        name TEXT PRIMARY KEY,
        machine TEXT,
        started INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE notes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue TEXT NOT NULL,
        session TEXT,
        kind TEXT NOT NULL,
        body TEXT NOT NULL,
        generation INTEGER,
        at INTEGER NOT NULL
    );
    CREATE INDEX notes_issue ON notes(issue, id);
    CREATE TABLE events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        session TEXT,
        issue TEXT,
        action TEXT NOT NULL,
        details TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX events_issue ON events(issue, seq);
    CREATE INDEX events_at ON events(at);
    ",
    // 2: issue type, priority, recommended agent, release membership, what ran
    "
    ALTER TABLE issues ADD COLUMN type TEXT NOT NULL DEFAULT 'task';
    ALTER TABLE issues ADD COLUMN priority TEXT NOT NULL DEFAULT 'P2';
    ALTER TABLE issues ADD COLUMN agent TEXT;
    ALTER TABLE issues ADD COLUMN effort TEXT;
    ALTER TABLE issues ADD COLUMN reviewer TEXT;
    ALTER TABLE issues ADD COLUMN release TEXT;
    ALTER TABLE issues ADD COLUMN ran TEXT;
    CREATE INDEX issues_parent ON issues(parent);
    CREATE INDEX issues_release ON issues(release);
    CREATE INDEX issues_prio ON issues(priority, ord);
    ",
    // 3: review and release workflow; dependency links with a target state
    "
    ALTER TABLE links ADD COLUMN until_state TEXT;
    ALTER TABLE issues ADD COLUMN released_at INTEGER;
    ALTER TABLE issues ADD COLUMN release_ref TEXT;
    ",
    // 4: acceptance criteria
    "
    CREATE TABLE criteria (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue TEXT NOT NULL,
        pos INTEGER NOT NULL,
        text TEXT NOT NULL,
        done_at INTEGER,
        done_by TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX criteria_issue ON criteria(issue, pos);
    ",
    // 5: a claim may carry its own lease length (NULL = lease_minutes)
    "
    ALTER TABLE claims ADD COLUMN lease_ms INTEGER;
    ",
    // 6: labels on issues (comma separated) and evidence on criteria
    "
    ALTER TABLE issues ADD COLUMN labels TEXT NOT NULL DEFAULT '';
    ALTER TABLE criteria ADD COLUMN evidence TEXT;
    ",
    // 7: an expected date (YYYY-MM-DD), mostly for releases and milestones
    "
    ALTER TABLE issues ADD COLUMN due TEXT;
    ",
];

pub fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Opens (creating if needed) and migrates the database.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(|e| {
            AitError::new(
                "NO_DB",
                format!("cannot create database folder {}: {e}", dir.display()),
            )
        })?;
    }
    let conn = Connection::open(path).map_err(|e| {
        AitError::new(
            "NO_DB",
            format!("cannot open database {}: {e}", path.display()),
        )
    })?;
    conn.busy_timeout(Duration::from_secs(5))?;
    // Switching to WAL needs a brief exclusive lock; retry if another
    // process is creating the database at the same moment.
    let mut tries = 0;
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            Ok(_) => break,
            Err(e) if tries < 50 => {
                let busy = AitError::from(e).code == "BUSY";
                if !busy {
                    return Err(AitError::new("DB_ERROR", "cannot enable WAL mode"));
                }
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF;")?;
    // Each `ait` call is a short-lived process. Checkpointing (and deleting)
    // the WAL on every close costs ~100 ms of disk syncs on Windows; leave it
    // to SQLite's automatic checkpoints instead. The data stays safe in the
    // WAL file next to the database.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    migrate(&conn)?;
    Ok(conn)
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    let latest = schema_version();
    let v = user_version(conn)?;
    if v == latest {
        return Ok(());
    }
    if v > latest {
        return Err(AitError::new(
            "SCHEMA_TOO_NEW",
            format!("database schema {v} is newer than this ait ({latest}); update ait"),
        ));
    }
    write(conn, |c| {
        // Re-check inside the lock: another process may have migrated.
        let v = user_version(c)?;
        if v > latest {
            return Err(AitError::new(
                "SCHEMA_TOO_NEW",
                "database schema is newer than this ait",
            ));
        }
        for (i, sql) in MIGRATIONS
            .iter()
            .enumerate()
            .skip(usize::try_from(v).unwrap_or(0))
        {
            c.execute_batch(sql)?;
            c.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        }
        Ok(())
    })
}

/// Runs `f` inside `BEGIN IMMEDIATE ... COMMIT`; rolls back on error.
pub fn write<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(v) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(v),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Runs `f` inside one read transaction, for a consistent multi-query view.
pub fn read<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN DEFERRED")?;
    let r = f(conn);
    let _ = conn.execute_batch("COMMIT");
    r
}

/// Everything a command needs besides the connection.
pub struct Env {
    pub now: i64,
    pub session: Option<String>,
    pub cfg: Config,
    pub db_path: PathBuf,
}

impl Env {
    pub fn lease_ms(&self) -> i64 {
        self.cfg.lease_minutes * 60_000
    }

    pub fn require_session(&self) -> Result<&str> {
        self.session.as_deref().ok_or_else(|| {
            AitError::new(
                "NO_SESSION",
                "this command needs a session name: set AIT_SESSION or pass --as <name>",
            )
        })
    }
}

pub fn valid_session_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@-".contains(&b))
}

/// Appends one row to the append-only event log.
pub fn event(
    c: &Connection,
    env: &Env,
    issue: Option<&str>,
    action: &str,
    details: Value,
) -> Result<i64> {
    c.execute(
        "INSERT INTO events(at, session, issue, action, details) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![env.now, env.session, issue, action, details.to_string()],
    )?;
    Ok(c.last_insert_rowid())
}

/// How often a session's activity renews its leases.
pub const RENEW_EVERY_MS: i64 = 60_000;

/// Records the session and renews every lease it holds ("any call by the
/// claimant renews it").
pub fn touch_session(conn: &Connection, env: &Env) -> Result<()> {
    let Some(s) = env.session.as_deref() else {
        return Ok(());
    };
    // Skip the write when this session was seen less than a minute ago:
    // leases are renewed at most once a minute, which keeps reads fast.
    let last: Option<i64> = {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT last_seen FROM sessions WHERE name = ?1", [s], |r| {
            r.get(0)
        })
        .optional()?
    };
    if last.is_some_and(|l| env.now >= l && env.now - l < RENEW_EVERY_MS) {
        return Ok(());
    }
    let machine = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok();
    write(conn, |c| {
        c.execute(
            "INSERT INTO sessions(name, machine, started, last_seen) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET last_seen = excluded.last_seen,
                 machine = COALESCE(excluded.machine, sessions.machine)",
            params![s, machine, env.now],
        )?;
        c.execute(
            // Only live leases: an expired claim stays stale until the
            // session claims it again (or another session takes it over).
            // A claim with its own lease length keeps it on renewal.
            "UPDATE claims SET lease_until = MAX(lease_until, ?3 + COALESCE(lease_ms, ?2))
             WHERE session = ?1 AND lease_until > ?3",
            params![s, env.lease_ms(), env.now],
        )?;
        Ok(())
    })
}

pub fn meta_get(c: &Connection, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(
        c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_gets_latest_schema() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        assert_eq!(user_version(&c).unwrap(), schema_version());
        // Running again is a no-op.
        migrate(&c).unwrap();
        assert_eq!(user_version(&c).unwrap(), schema_version());
    }

    #[test]
    fn refuses_newer_schema() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA user_version = 999").unwrap();
        assert_eq!(migrate(&c).unwrap_err().code, "SCHEMA_TOO_NEW");
    }

    #[test]
    fn write_rolls_back_on_error() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let r: Result<()> = write(&c, |c| {
            c.execute("INSERT INTO meta VALUES ('k','v')", [])?;
            Err(AitError::usage("boom"))
        });
        assert!(r.is_err());
        assert_eq!(meta_get(&c, "k").unwrap(), None);
    }

    #[test]
    fn session_names() {
        assert!(valid_session_name("agent-1"));
        assert!(valid_session_name("cloud-1@pc"));
        assert!(!valid_session_name(""));
        assert!(!valid_session_name("has space"));
    }
}
