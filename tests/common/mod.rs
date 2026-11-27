//! Shared helpers: run the real `ait` binary against a temporary database
//! under `target/tmp/`, with a timeout on every process.

#![allow(dead_code)]

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_ait");
pub const TIMEOUT: Duration = Duration::from_secs(60);
static N: AtomicUsize = AtomicUsize::new(0);

pub struct T {
    pub dir: PathBuf,
}

pub struct Out {
    pub code: i32,
    pub json: Value,
    pub stdout: String,
    pub stderr: String,
}

/// Waits for `child` with a hard timeout; kills it and panics on timeout.
pub fn wait(mut child: Child) -> Out {
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t1 = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = so.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let t2 = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = se.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ait process timed out after {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = t1.join().unwrap();
    let stderr = t2.join().unwrap();
    let json = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    Out {
        code: status.code().unwrap_or(-1),
        json,
        stdout,
        stderr,
    }
}

impl T {
    pub fn new(name: &str) -> T {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let dir = base.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        T { dir }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.join("tracker.db")
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// A command with a clean environment (no AIT_* variables leak in).
    pub fn bare(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.dir)
            .env_remove("AIT_SESSION")
            .env_remove("AIT_CONFIG")
            .env_remove("AIT_DB")
            .env_remove("AIT_NOW_MS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    pub fn cmd(&self, session: Option<&str>, args: &[&str]) -> Command {
        let mut c = self.bare();
        c.arg("--db").arg(self.db()).arg("--json").args(args);
        if let Some(s) = session {
            c.env("AIT_SESSION", s);
        }
        c
    }

    pub fn spawn(&self, mut c: Command) -> Child {
        c.spawn().unwrap()
    }

    pub fn exec(&self, c: Command) -> Out {
        wait(self.spawn(c))
    }

    pub fn run(&self, session: Option<&str>, args: &[&str]) -> Out {
        self.exec(self.cmd(session, args))
    }

    /// Runs with the clock set to `now` (ms).
    pub fn run_at(&self, session: Option<&str>, now: i64, args: &[&str]) -> Out {
        let mut c = self.cmd(session, args);
        c.env("AIT_NOW_MS", now.to_string());
        self.exec(c)
    }

    pub fn ok(&self, session: Option<&str>, args: &[&str]) -> Value {
        let o = self.run(session, args);
        assert_eq!(o.code, 0, "ait {args:?} failed: {}{}", o.stdout, o.stderr);
        o.json
    }

    pub fn ok_at(&self, session: Option<&str>, now: i64, args: &[&str]) -> Value {
        let o = self.run_at(session, now, args);
        assert_eq!(o.code, 0, "ait {args:?} failed: {}{}", o.stdout, o.stderr);
        o.json
    }

    /// Expects failure; returns (exit code, error code).
    pub fn err(&self, session: Option<&str>, args: &[&str]) -> (i32, String) {
        let o = self.run(session, args);
        assert_ne!(
            o.code, 0,
            "ait {args:?} unexpectedly succeeded: {}",
            o.stdout
        );
        (o.code, err_code(&o.json))
    }

    pub fn err_at(&self, session: Option<&str>, now: i64, args: &[&str]) -> (i32, String) {
        let o = self.run_at(session, now, args);
        assert_ne!(
            o.code, 0,
            "ait {args:?} unexpectedly succeeded: {}",
            o.stdout
        );
        (o.code, err_code(&o.json))
    }

    pub fn write(&self, name: &str, text: &str) -> PathBuf {
        let p = self.path(name);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&p, text).unwrap();
        p
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn err_code(v: &Value) -> String {
    v["error"]["code"].as_str().unwrap_or("").to_string()
}

pub fn id(v: &Value) -> String {
    v["issue"]["id"].as_str().unwrap_or("").to_string()
}

pub fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A fixed, realistic clock for lease tests: 2026-11-26T08:00:00Z.
pub const T0: i64 = 1_795_680_000_000;
pub const MIN: i64 = 60_000;
