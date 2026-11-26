//! Notes, handoffs, the resume view, briefs, backups, export/import, check.

mod common;
use common::*;

#[test]
fn notes_are_append_only_and_handoffs_are_versioned() {
    let t = T::new("notes");
    t.ok(None, &["add", "CORE", "work"]);
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    t.ok(Some("a1"), &["note", "CORE-1-00", "tried X, dead end"]);
    t.ok(Some("a1"), &["handoff", "CORE-1-00", "v1: parser done"]);
    t.ok(Some("a1"), &["note", "CORE-1-00", "decided on Y"]);
    let f = t.write("HANDOFF.md", "v2: parser + tests done\nremaining: docs\n");
    t.ok(Some("a1"), &["handoff", "CORE-1-00", "--from-file", &p(&f)]);
    let v = t.ok(None, &["show", "CORE-1-00"]);
    assert_eq!(v["note_count"], 4);
    assert_eq!(
        v["latest_handoff"]["body"],
        "v2: parser + tests done\nremaining: docs\n"
    );
    assert_eq!(v["latest_handoff"]["generation"], 1);
    let kinds: Vec<&str> = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["note", "handoff", "note", "handoff"]);
    assert_eq!(v["notes"][0]["body"], "tried X, dead end");
    // Earlier handoffs stay in history; `handoff <id>` shows the latest.
    let h = t.ok(None, &["handoff", "CORE-1-00"]);
    assert!(h["handoff"]["body"].as_str().unwrap().starts_with("v2"));
    let v = t.ok(None, &["show", "CORE-1-00", "--notes", "1"]);
    assert_eq!(v["notes"].as_array().unwrap().len(), 1);
    // Non-claimants can still note (generation is left empty).
    let n = t.ok(Some("owner"), &["note", "CORE-1-00", "owner comment"]);
    assert_eq!(n["note"]["generation"], serde_json::Value::Null);
    assert_eq!(t.err(None, &["note", "CORE-1-00", "  "]).1, "INVALID_INPUT");
    assert_eq!(t.err(None, &["note", "CORE-1-00"]).1, "USAGE");
}

#[test]
fn brief_is_self_contained_markdown() {
    let t = T::new("brief");
    t.ok(None, &["add", "CORE", "base"]);
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "Cloud task",
            "--body",
            "Build the parser.",
            "--needs",
            "CORE-1-00",
        ],
    );
    t.ok(Some("cloud-1"), &["claim", "CORE-1-01"]);
    t.ok(
        Some("cloud-1"),
        &["handoff", "CORE-1-01", "started on lexer"],
    );
    let v = t.ok(Some("cloud-1"), &["brief", "CORE-1-01"]);
    let md = v["markdown"].as_str().unwrap();
    for want in [
        "# Brief: CORE-1-01 Cloud task",
        "Build the parser.",
        "**cloud-1**",
        "needs CORE-1-00",
        "started on lexer",
        "ait handoff CORE-1-01 --from-file HANDOFF.md",
        "Issue-ID: CORE-1-01",
    ] {
        assert!(md.contains(want), "brief misses {want:?}:\n{md}");
    }
}

#[test]
fn export_import_round_trip_is_byte_for_byte() {
    let t = T::new("roundtrip");
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "one",
            "--body",
            "multi\nline \"quoted\" ünïcode ✓",
            "--legacy",
            "core-1",
        ],
    );
    t.ok(
        None,
        &["add", "CORE", "two", "--needs", "CORE-1-00", "--size", "S"],
    );
    t.ok(
        None,
        &[
            "add", "PAP", "three", "--state", "blocked", "--reason", "waiting",
        ],
    );
    t.ok(None, &["link", "PAP-1-00", "relates", "CORE-1-01"]);
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    t.ok(Some("a1"), &["note", "CORE-1-00", "a note"]);
    t.ok(Some("a1"), &["handoff", "CORE-1-00", "a handoff"]);
    t.ok(Some("a1"), &["done", "CORE-1-00", "--summary", "shipped"]);
    t.ok(None, &["add", "CORE", "deleted"]);
    t.ok(None, &["delete", "CORE-1-02"]);
    let a = t.path("a.jsonl");
    let b = t.path("b.jsonl");
    t.ok(None, &["export", "--out", &p(&a)]);

    let restored = t.path("restored.db");
    let run = |db: &std::path::Path, args: &[&str]| {
        let mut c = t.bare();
        c.arg("--db").arg(db).arg("--json").args(args);
        t.exec(c)
    };
    let o = run(&restored, &["import", &p(&a)]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    let o = run(&restored, &["export", "--out", &p(&b)]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    let ea = std::fs::read(&a).unwrap();
    let eb = std::fs::read(&b).unwrap();
    assert!(!ea.is_empty());
    assert_eq!(ea, eb, "export -> import -> export must be identical");
    // The restored DB behaves like the original.
    let v = run(&restored, &["show", "core-1"]).json;
    assert_eq!(v["issue"]["state"], "done");
    assert_eq!(v["latest_handoff"]["body"], "a handoff");
    assert_eq!(
        err_code(&run(&restored, &["add", "CORE-1-02", "reuse"]).json),
        "ID_TAKEN"
    );
    // Importing into a used database is refused.
    let o = run(&restored, &["import", &p(&a)]);
    assert_eq!((o.code, err_code(&o.json).as_str()), (4, "NOT_EMPTY"));
    // Garbage is refused with a line number.
    let bad = t.write(
        "bad.jsonl",
        "{\"type\":\"ait-export\",\"format\":1,\"schema\":1}\n{\"type\":\"wat\"}\n",
    );
    let o = run(&t.path("other.db"), &["import", &p(&bad)]);
    assert_eq!(err_code(&o.json), "INVALID_INPUT");
    assert!(
        o.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line 2")
    );
}

#[test]
fn daily_backup_and_manual_backups_with_pruning() {
    let t = T::new("backup");
    let cfg = t.write("default.cfg", "backup_dir = bk\nbackup_keep = 3\n");
    let run = |args: &[&str]| {
        let mut c = t.cmd(None, &[]);
        c.args(["--config", &p(&cfg)]).args(args);
        t.exec(c)
    };
    assert_eq!(run(&["add", "CORE", "x"]).code, 0);
    let names = || -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(t.path("bk"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    };
    assert_eq!(names().len(), 1, "first call of the day makes a backup");
    assert_eq!(run(&["list"]).code, 0);
    assert_eq!(names().len(), 1, "only one automatic backup per day");
    // Unrelated files in the backup folder are never touched.
    t.write("bk/keep-me.txt", "mine");
    for _ in 0..4 {
        let o = run(&["backup"]);
        assert_eq!(o.code, 0, "{}", o.stdout);
    }
    let n = names();
    assert_eq!(
        n.iter().filter(|x| x.starts_with("tracker-")).count(),
        3,
        "{n:?}"
    );
    assert!(n.contains(&"keep-me.txt".to_string()));
    // A backup is a working database.
    let newest = n
        .iter()
        .filter(|x| x.starts_with("tracker-"))
        .max()
        .unwrap()
        .clone();
    let mut c = t.bare();
    c.arg("--db")
        .arg(t.path("bk").join(newest))
        .args(["--json", "list"]);
    let o = t.exec(c);
    assert_eq!(o.json["count"], 1, "{}", o.stdout);
}

#[test]
fn check_passes_on_clean_data_and_reports_corruption() {
    let t = T::new("check");
    t.ok(None, &["add", "CORE", "a"]);
    t.ok(None, &["add", "CORE", "b", "--needs", "CORE-1-00"]);
    let o = t.run(None, &["check"]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert_eq!(o.json["ok"], true);
    // Damage the data behind ait's back.
    {
        let c = rusqlite::Connection::open(t.db()).unwrap();
        c.execute_batch(
            "INSERT INTO links VALUES ('CORE-1-00', 'needs', 'CORE-1-01');
             INSERT INTO links VALUES ('CORE-1-00', 'relates', 'GONE-1-00');
             UPDATE issues SET state = 'blocked', reason = NULL WHERE id = 'CORE-1-01';",
        )
        .unwrap();
    }
    let o = t.run(None, &["check"]);
    assert_eq!(o.code, 7, "{}", o.stdout);
    let codes: Vec<&str> = o.json["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    for want in ["CYCLE", "DANGLING_LINK", "NO_REASON"] {
        assert!(codes.contains(&want), "missing {want} in {codes:?}");
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let o = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn check_git_flags_done_issues_without_a_merge_on_main() {
    let t = T::new("checkgit");
    let repo = t.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Add parser\n\nIssue-ID: CORE-1-00",
        ],
    );
    let db = t.path("t.db");
    let run = |args: &[&str]| {
        let mut c = t.bare();
        c.current_dir(&repo)
            .arg("--db")
            .arg(&db)
            .arg("--json")
            .args(args);
        t.exec(c)
    };
    run(&["add", "CORE", "merged"]);
    run(&["add", "CORE", "not merged"]);
    run(&["done", "CORE-1-00", "--summary", "ok"]);
    run(&["done", "CORE-1-01", "--summary", "claimed done"]);
    let o = run(&["check", "--git"]);
    assert_eq!(o.code, 7, "{}", o.stdout);
    let probs = o.json["problems"].as_array().unwrap();
    assert_eq!(probs.len(), 1, "{probs:?}");
    assert_eq!(probs[0]["code"], "DONE_NOT_MERGED");
    assert_eq!(probs[0]["issue"], "CORE-1-01");
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Finish\n\nIssue-ID: CORE-1-01",
        ],
    );
    assert_eq!(run(&["check", "--git"]).code, 0);
}
