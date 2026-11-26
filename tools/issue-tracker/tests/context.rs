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
    // A non-claimant's note is refused while the claim is live (owner rule,
    // 2026-11-26); --force records it with an empty generation.
    assert_eq!(
        t.err(Some("owner"), &["note", "CORE-1-00", "owner comment"])
            .1,
        "NOT_CLAIMANT"
    );
    let n = t.ok(
        Some("owner"),
        &["note", "CORE-1-00", "owner comment", "--force"],
    );
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
    let out = t.path("BRIEF.md");
    t.ok(None, &["brief", "CORE-1-01", "--out", &p(&out)]);
    let written = std::fs::read_to_string(&out).unwrap();
    assert!(written.starts_with("# Brief: CORE-1-01"));
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
    t.ok(
        Some("a1"),
        &["done", "CORE-1-00", "--summary", "shipped", "--gen", "1"],
    );
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
    // Garbage is refused with a line number; every problem is listed
    // (IMPORT_INVALID since the owner's 2026-11-26 report).
    let bad = t.write(
        "bad.jsonl",
        "{\"type\":\"ait-export\",\"format\":1,\"schema\":1}\n{\"type\":\"wat\"}\n",
    );
    let o = run(&t.path("other.db"), &["import", &p(&bad)]);
    assert_eq!(err_code(&o.json), "IMPORT_INVALID");
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

/// Merges a branch with one commit into main with `--no-ff` and `message`.
fn merge(repo: &std::path::Path, branch: &str, message: &str) {
    git(repo, &["checkout", "-q", "-b", branch]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", "work"]);
    git(repo, &["checkout", "-q", "main"]);
    git(repo, &["merge", "-q", "--no-ff", branch, "-m", message]);
}

/// Owner repro (2026-11-26): only real trailers on first-parent merges of
/// the configured repo count, and a baseline covers issues done before.
#[test]
fn check_git_reads_only_merge_trailers_of_the_configured_repo() {
    let t = T::new("checkgit");
    let repo = t.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join("baseline.txt"),
        "# done before the switch
CORE-1-03
",
    )
    .unwrap();
    git(&repo, &["add", "baseline.txt"]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);
    merge(
        &repo,
        "f0",
        "Add parser

Issue-ID: CORE-1-00",
    );
    // A plain commit on main is not a merge.
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Direct

Issue-ID: CORE-1-01",
        ],
    );
    // A key: value line that is not in the trailer block does not count.
    merge(
        &repo,
        "f2",
        "Mentions

Issue-ID: CORE-1-02
is how we tag

Signed-off-by: x",
    );
    merge(
        &repo,
        "f4",
        "Ready item

Issue-ID: CORE-1-04",
    );
    let cfg = t.write(
        "git.cfg",
        "repo = repo
git_baseline = git:main:baseline.txt
backup_keep = 0
",
    );
    // Run from a directory that is not the repo.
    let run = |args: &[&str]| {
        let mut c = t.cmd(None, &[]);
        c.arg("--config").arg(&cfg).args(args);
        t.exec(c)
    };
    for title in ["merged", "direct", "loose", "baseline", "ready"] {
        assert_eq!(run(&["add", "CORE", title]).code, 0);
    }
    for id in ["CORE-1-00", "CORE-1-01", "CORE-1-02", "CORE-1-03"] {
        let o = run(&["done", id, "--summary", "ok", "--force"]);
        assert_eq!(o.code, 0, "{}", o.stdout);
    }
    let o = run(&["check", "--git"]);
    assert_eq!(o.code, 7, "{}", o.stdout);
    let mut got: Vec<(String, String)> = o.json["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["code"].as_str().unwrap().to_string(),
                p["issue"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    got.sort();
    let want = [
        ("DONE_NOT_MERGED", "CORE-1-01"),
        ("DONE_NOT_MERGED", "CORE-1-02"),
        ("MERGED_NOT_DONE", "CORE-1-04"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want);
    // --repo overrides the configured repo; a folder with no repo is a problem.
    let o = run(&[
        "check",
        "--git",
        "--repo",
        t.path("nowhere").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 7);
    assert_eq!(o.json["problems"][0]["code"], "GIT");
}
