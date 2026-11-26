//! Regression tests for the owner's test-drive report of 2026-11-26
//! (PenAndPaperPOC inventory). Each test names the repro it covers.

mod common;
use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Runs with `--config cfg` at the fixed clock `T0`.
fn at(t: &T, cfg: &Path, session: Option<&str>, args: &[&str]) -> Out {
    at_time(t, cfg, session, T0, args)
}

fn at_time(t: &T, cfg: &Path, session: Option<&str>, now: i64, args: &[&str]) -> Out {
    let mut c = t.cmd(session, &[]);
    c.arg("--config")
        .arg(cfg)
        .args(args)
        .env("AIT_NOW_MS", now.to_string());
    t.exec(c)
}

fn ok(o: Out) -> Value {
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    o.json
}

fn code(o: &Out) -> String {
    err_code(&o.json)
}

fn ids(v: &Value) -> Vec<String> {
    v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
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

fn plain_cfg(t: &T) -> PathBuf {
    t.write("plain.cfg", "backup_keep = 0\n")
}

// ------------------------------------------------------------ 1. done rules

#[test]
fn done_refuses_idea_decision_and_dropped_and_never_reopens() {
    let t = T::new("donerules");
    let cfg = plain_cfg(&t);
    ok(at(
        &t,
        &cfg,
        None,
        &["add", "CORE", "an idea", "--state", "idea"],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add", "CORE", "a choice", "--state", "decision", "--reason", "pick",
        ],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add", "CORE", "gone", "--state", "dropped", "--reason", "no",
        ],
    ));
    ok(at(&t, &cfg, None, &["add", "CORE", "real work"]));
    for id in ["CORE-1-00", "CORE-1-01", "CORE-1-02"] {
        let o = at(&t, &cfg, None, &["done", id, "--summary", "x"]);
        assert_eq!(code(&o), "INVALID_STATE", "{id}: {}", o.stdout);
    }
    ok(at(
        &t,
        &cfg,
        None,
        &["done", "CORE-1-01", "--summary", "decided", "--force"],
    ));
    // Done needs the live claim and its generation.
    let o = at(
        &t,
        &cfg,
        Some("a1"),
        &["done", "CORE-1-03", "--summary", "x"],
    );
    assert_eq!(code(&o), "NOT_CLAIMED");
    ok(at(&t, &cfg, Some("a1"), &["claim", "CORE-1-03"]));
    let o = at(
        &t,
        &cfg,
        Some("a1"),
        &["done", "CORE-1-03", "--summary", "x"],
    );
    assert_eq!(code(&o), "USAGE");
    let o = at(
        &t,
        &cfg,
        Some("a2"),
        &["done", "CORE-1-03", "--summary", "x", "--gen", "1"],
    );
    assert_eq!(code(&o), "NOT_CLAIMANT");
    // An expired claim cannot be closed until it is claimed again.
    let o = at_time(
        &t,
        &cfg,
        Some("a1"),
        T0 + 600 * MIN,
        &["done", "CORE-1-03", "--summary", "x", "--gen", "1"],
    );
    assert_eq!(code(&o), "CLAIM_STALE");
    let v = ok(at(
        &t,
        &cfg,
        Some("a1"),
        &[
            "done",
            "CORE-1-03",
            "--summary",
            "shipped",
            "--gen",
            "1",
            "--ran",
            "sonnet, effort high",
        ],
    ));
    assert_eq!(v["issue"]["ran"], "sonnet, effort high");
    // `state` never reopens a done issue without --force.
    let o = at(&t, &cfg, None, &["state", "CORE-1-03", "ready"]);
    assert_eq!(code(&o), "ALREADY_DONE");
    assert_eq!(o.code, 4);
    let o = at(&t, &cfg, None, &["state", "CORE-1-03", "done"]);
    assert_eq!(code(&o), "INVALID_STATE");
    ok(at(
        &t,
        &cfg,
        None,
        &["state", "CORE-1-03", "ready", "--force"],
    ));
}

// ------------------------------------------------------------ 2. fields

#[test]
fn priority_agent_effort_reviewer_and_ran() {
    let t = T::new("fields");
    let cfg = t.write(
        "f.cfg",
        "backup_keep = 0\nagent.standard = sonnet\nagent.deep = opus\n",
    );
    ok(at(
        &t,
        &cfg,
        None,
        &["add", "CORE", "later", "--priority", "p3"],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add",
            "CORE",
            "hard",
            "--priority",
            "P1",
            "--agent",
            "deep",
            "--effort",
            "high",
            "--reviewer",
            "codex",
        ],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add",
            "CORE",
            "urgent",
            "--priority",
            "P0",
            "--agent",
            "standard",
        ],
    ));
    ok(at(&t, &cfg, None, &["add", "CORE", "default"]));
    let o = at(&t, &cfg, None, &["add", "CORE", "bad", "--agent", "huge"]);
    assert_eq!(code(&o), "INVALID_INPUT");
    let o = at(&t, &cfg, None, &["add", "CORE", "bad", "--priority", "P9"]);
    assert_eq!(code(&o), "INVALID_INPUT");
    let o = at(
        &t,
        &cfg,
        None,
        &["add", "CORE", "bad", "--effort", "extreme"],
    );
    assert_eq!(code(&o), "INVALID_INPUT");
    // next sorts by priority, then order.
    let v = ok(at(&t, &cfg, None, &["next"]));
    assert_eq!(
        ids(&v),
        ["CORE-1-02", "CORE-1-01", "CORE-1-03", "CORE-1-00"]
    );
    assert_eq!(v["issues"][2]["priority"], "P2");
    let v = ok(at(&t, &cfg, None, &["next", "--agent", "deep"]));
    assert_eq!(ids(&v), ["CORE-1-01"]);
    let v = ok(at(&t, &cfg, None, &["list", "--priority", "P0,P1"]));
    assert_eq!(ids(&v), ["CORE-1-02", "CORE-1-01"]);
    let v = ok(at(&t, &cfg, None, &["list", "--agent", "standard"]));
    assert_eq!(ids(&v), ["CORE-1-02"]);
    // The claim returns the fields and the model the tier maps to.
    let v = ok(at(
        &t,
        &cfg,
        Some("a1"),
        &["next", "--claim", "--agent", "deep"],
    ));
    assert_eq!(v["issue"]["id"], "CORE-1-01");
    assert_eq!(v["issue"]["agent"], "deep");
    assert_eq!(v["issue"]["agent_model"], "opus");
    assert_eq!(v["issue"]["effort"], "high");
    assert_eq!(v["issue"]["reviewer"], "codex");
    assert_eq!(v["issue"]["priority"], "P1");
    // edit changes them (the claimant, or --force).
    let o = at(&t, &cfg, None, &["edit", "CORE-1-01", "--priority", "P0"]);
    assert_eq!(code(&o), "NOT_CLAIMANT");
    ok(at(
        &t,
        &cfg,
        Some("a1"),
        &[
            "edit",
            "CORE-1-01",
            "--effort",
            "max",
            "--agent",
            "standard",
        ],
    ));
    let v = ok(at(&t, &cfg, None, &["show", "CORE-1-01"]));
    assert_eq!(v["issue"]["effort"], "max");
    assert_eq!(v["issue"]["agent_model"], "sonnet");
    ok(at(
        &t,
        &cfg,
        Some("a1"),
        &[
            "done",
            "CORE-1-01",
            "--summary",
            "ok",
            "--gen",
            "1",
            "--ran",
            "opus/max",
        ],
    ));
    let v = ok(at(&t, &cfg, None, &["show", "CORE-1-01"]));
    assert_eq!(v["issue"]["ran"], "opus/max");
}

// ------------------------------------------------------------ 3. roadmap

#[test]
fn releases_milestones_features_and_roadmap() {
    let t = T::new("roadmap");
    let cfg = plain_cfg(&t);
    ok(at(
        &t,
        &cfg,
        None,
        &["add", "REL", "v1", "--type", "release"],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add",
            "MS",
            "alpha",
            "--type",
            "milestone",
            "--release",
            "REL-1-00",
        ],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add",
            "FEAT",
            "world",
            "--type",
            "feature",
            "--release",
            "MS-1-00",
        ],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &["add", "WLD", "w1", "--parent", "FEAT-1-00"],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &["add", "WLD", "w2", "--parent", "FEAT-1-00"],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &[
            "add",
            "WLD",
            "w3",
            "--parent",
            "FEAT-1-00",
            "--state",
            "decision",
            "--reason",
            "x",
        ],
    ));
    ok(at(
        &t,
        &cfg,
        None,
        &["done", "WLD-1-00", "--summary", "ok", "--force"],
    ));
    // release must point at a release or milestone.
    let o = at(
        &t,
        &cfg,
        None,
        &["add", "WLD", "bad", "--release", "WLD-1-01"],
    );
    assert_eq!(code(&o), "INVALID_INPUT");
    let v = ok(at(&t, &cfg, None, &["list", "--type", "feature"]));
    assert_eq!(ids(&v), ["FEAT-1-00"]);
    let v = ok(at(
        &t,
        &cfg,
        None,
        &["list", "--all", "--parent", "FEAT-1-00"],
    ));
    assert_eq!(ids(&v), ["WLD-1-00", "WLD-1-01", "WLD-1-02"]);
    let v = ok(at(&t, &cfg, None, &["list", "--release", "MS-1-00"]));
    assert_eq!(ids(&v), ["FEAT-1-00"]);
    let v = ok(at(&t, &cfg, None, &["list", "--tree"]));
    let depth: Vec<(String, i64)> = v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_string(),
                i["depth"].as_i64().unwrap(),
            )
        })
        .collect();
    assert!(depth.contains(&("WLD-1-01".to_string(), 1)), "{depth:?}");
    assert!(depth.contains(&("FEAT-1-00".to_string(), 0)), "{depth:?}");
    let v = ok(at(&t, &cfg, None, &["roadmap"]));
    let rel = &v["releases"][0];
    assert_eq!(rel["id"], "REL-1-00");
    // REL holds MS, FEAT and the three tasks.
    assert_eq!(rel["rollup"]["total"], 5);
    assert_eq!(rel["rollup"]["done"], 1);
    let feat = &v["features"][0];
    assert_eq!(feat["rollup"]["total"], 3);
    assert_eq!(feat["rollup"]["percent"], 33);
    assert_eq!(feat["rollup"]["next"], "WLD-1-01");
    assert_eq!(feat["rollup"]["decisions"][0], "WLD-1-02");
    let v = ok(at(&t, &cfg, None, &["show", "FEAT-1-00"]));
    assert_eq!(v["rollup"]["done"], 1);
    // Containers are not offered as build work.
    let v = ok(at(&t, &cfg, None, &["next"]));
    assert_eq!(ids(&v), ["WLD-1-01"]);
    // A release with members keeps its type.
    let o = at(&t, &cfg, None, &["edit", "REL-1-00", "--type", "task"]);
    assert_eq!(code(&o), "IN_USE");
}

// ------------------------------------------------------------ 4. generations

#[test]
fn an_evicted_session_cannot_write() {
    let t = T::new("evicted");
    let cfg = plain_cfg(&t);
    ok(at(&t, &cfg, None, &["add", "CORE", "work"]));
    ok(at(&t, &cfg, Some("a1"), &["claim", "CORE-1-00"]));
    let later = T0 + 600 * MIN;
    let v = ok(at_time(
        &t,
        &cfg,
        Some("a2"),
        later,
        &["claim", "CORE-1-00", "--take"],
    ));
    assert_eq!(v["claim"]["generation"], 2);
    let tries: [&[&str]; 5] = [
        &["note", "CORE-1-00", "late"],
        &["handoff", "CORE-1-00", "late"],
        &["state", "CORE-1-00", "blocked", "--reason", "x"],
        &["edit", "CORE-1-00", "--title", "renamed"],
        &["done", "CORE-1-00", "--summary", "x", "--gen", "1"],
    ];
    for args in tries {
        let o = at_time(&t, &cfg, Some("a1"), later, args);
        assert_eq!(o.code, 4, "{args:?}: {}", o.stdout);
        let c = code(&o);
        assert!(
            c == "NOT_CLAIMANT" || c == "STALE_GENERATION",
            "{args:?}: {c}"
        );
    }
    // A stale generation is refused even from the new holder's session.
    let o = at_time(
        &t,
        &cfg,
        Some("a2"),
        later,
        &["note", "CORE-1-00", "x", "--gen", "1"],
    );
    assert_eq!(code(&o), "STALE_GENERATION");
    ok(at_time(
        &t,
        &cfg,
        Some("a2"),
        later,
        &["note", "CORE-1-00", "mine", "--gen", "2"],
    ));
    ok(at_time(
        &t,
        &cfg,
        Some("a1"),
        later,
        &["note", "CORE-1-00", "owner", "--force"],
    ));
    let v = ok(at_time(&t, &cfg, None, later, &["show", "CORE-1-00"]));
    assert_eq!(v["issue"]["title"], "work");
}

// ------------------------------------------------------------ 5. approvals

/// The drive's approvals file, verbatim.
const DRIVE_APPROVALS: &str = r#"[
  {"match": "core", "scope": "build", "from": "2026-11-22", "until": "2026-12-31", "exclude": ["core-3-11"], "note": "core sign-off"},
  {"match": "fw", "scope": "frame", "from": "2026-11-12", "until": null, "note": "frame only"},
  {"match": "bug", "scope": "build", "from": "2026-10-16", "until": "2026-11-01", "note": "expired"},
  {"match": "ms", "scope": "build", "from": "2026-12-01", "until": null, "note": "future"}
]"#;

fn approval_issues(t: &T, cfg: &Path) {
    for args in [
        &["add", "CORE", "core work"][..],
        &["add", "CORE-3-11", "excluded"],
        &["add", "FW", "framework ready"],
        &["add", "FW", "framework idea", "--state", "idea"],
        &["add", "FW", "framework plan", "--state", "planned"],
        &["add", "BUG", "old bug"],
        &["add", "MS", "future"],
        &["add", "PAP", "nobody approved"],
    ] {
        ok(at(t, cfg, None, args));
    }
}

#[test]
fn approvals_explain_each_cause_and_frame_mode() {
    let t = T::new("approvals2");
    t.write("drive-approvals.json", DRIVE_APPROVALS);
    let cfg = t.write(
        "a.cfg",
        "backup_keep = 0\napprovals = drive-approvals.json\n",
    );
    approval_issues(&t, &cfg);
    let v = ok(at(&t, &cfg, None, &["next"]));
    assert_eq!(ids(&v), ["CORE-1-00"]);
    let v = ok(at(&t, &cfg, None, &["next", "--frame"]));
    assert_eq!(v["mode"], "frame");
    assert_eq!(ids(&v), ["FW-1-01", "FW-1-02"]);
    for (id, want) in [
        ("CORE-3-11", "EXCLUDED"),
        ("FW-1-00", "FRAME_ONLY"),
        ("BUG-1-00", "EXPIRED"),
        ("MS-1-00", "NOT_STARTED"),
        ("PAP-1-00", "NOT_APPROVED"),
    ] {
        let v = ok(at(&t, &cfg, None, &["next", "--explain", id]));
        let codes: Vec<&str> = v["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, [want], "{id}");
    }
    let v = ok(at(&t, &cfg, None, &["next", "--explain", "FW-1-01"]));
    assert_eq!(v["frame_approval"]["code"], "APPROVED");
    let v = ok(at(&t, &cfg, None, &["next", "--explain", "CORE-1-00"]));
    assert_eq!(v["ready"], true);
    // Framing claims an idea; build claims stay with ready items.
    let v = ok(at(&t, &cfg, Some("a1"), &["next", "--claim", "--frame"]));
    assert_eq!(v["issue"]["id"], "FW-1-01");
}

#[test]
fn approvals_are_read_from_mains_committed_blob() {
    let t = T::new("approvalsgit");
    let repo = t.path("repo");
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("docs/approvals.json"), DRIVE_APPROVALS).unwrap();
    git(&repo, &["add", "docs/approvals.json"]);
    git(&repo, &["commit", "-q", "-m", "approvals"]);
    // An uncommitted edit that would approve PAP does not count.
    std::fs::write(
        repo.join("docs/approvals.json"),
        r#"[{"match": "pap", "scope": "build"}]"#,
    )
    .unwrap();
    let cfg = t.write(
        "g.cfg",
        "backup_keep = 0\nrepo = repo\napprovals = git:main:docs/approvals.json\n",
    );
    approval_issues(&t, &cfg);
    let v = ok(at(&t, &cfg, None, &["next"]));
    assert_eq!(ids(&v), ["CORE-1-00"]);
    // A missing blob approves nothing (fail closed), and says so.
    let cfg2 = t.write(
        "g2.cfg",
        "backup_keep = 0\nrepo = repo\napprovals = git:main:docs/missing.json\n",
    );
    let o = at(&t, &cfg2, None, &["next"]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert_eq!(ids(&o.json), Vec::<String>::new());
    let v = ok(at(&t, &cfg2, None, &["next", "--explain", "CORE-1-00"]));
    assert_eq!(v["blockers"][0]["code"], "NO_APPROVALS_FILE");
}

// ------------------------------------------------------------ 6. import validation

/// The drive's bad.jsonl, verbatim.
const BAD_EXPORT: &str = r#"{"format":1,"schema":1,"type":"ait-export"}
{"type":"issue","id":"core-3-00","title":"lowercase id","body":"","state":"bogus","ord":1,"created_at":0,"updated_at":0,"version":1}
{"type":"issue","id":"CORE-3-01","title":"cycle a","body":"","state":"ready","ord":2,"created_at":0,"updated_at":0,"version":1}
{"type":"issue","id":"CORE-3-02","title":"cycle b","body":"","state":"ready","ord":3,"created_at":0,"updated_at":0,"version":1}
{"type":"link","src":"CORE-3-01","kind":"needs","dst":"CORE-3-02"}
{"type":"link","src":"CORE-3-02","kind":"needs","dst":"CORE-3-01"}
{"type":"link","src":"CORE-3-02","kind":"needs","dst":"NOPE-1-00"}
"#;

fn problem_codes(o: &Out) -> Vec<String> {
    o.json["error"]["details"]["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn import_refuses_what_check_would_reject_and_writes_nothing() {
    let t = T::new("badimport");
    let cfg = plain_cfg(&t);
    let bad = t.write("bad.jsonl", BAD_EXPORT);
    let o = at(&t, &cfg, None, &["import", &p(&bad)]);
    assert_eq!(
        (o.code, code(&o).as_str()),
        (2, "IMPORT_INVALID"),
        "{}",
        o.stdout
    );
    let codes = problem_codes(&o);
    for want in ["BAD_ID", "BAD_STATE", "CYCLE", "DANGLING_LINK"] {
        assert!(
            codes.iter().any(|c| c == want),
            "missing {want} in {codes:?}"
        );
    }
    let v = ok(at(&t, &cfg, None, &["list", "--all"]));
    assert_eq!(v["count"], 0);
    // The database is still empty, so a good import works afterwards.
    let good = t.write(
        "good.jsonl",
        "{\"record\":\"ait-export\",\"format\":2,\"schema\":2}\n{\"record\":\"issue\",\"id\":\"CORE-1-00\",\"title\":\"ok\",\"body\":\"\",\"state\":\"ready\",\"ord\":1,\"created_at\":0,\"updated_at\":0,\"version\":1,\"type\":\"feature\"}\n",
    );
    let v = ok(at(&t, &cfg, None, &["import", &p(&good)]));
    assert_eq!(v["imported"], 1);
    let v = ok(at(&t, &cfg, None, &["show", "CORE-1-00"]));
    assert_eq!(v["issue"]["type"], "feature");
    assert_eq!(v["issue"]["priority"], "P2");
}

const INVENTORY: &str = r#"{"legacy": "docs/q.md#epic-w", "kind": "feature", "title": "Epic W", "body": "world", "state": "planned", "labels": ["wld"], "order": 90000, "source": "docs/q.md"}
{"legacy": "docs/rel.md", "kind": "release", "title": "v1", "state": "planned", "due": "2027-03-01"}
{"legacy": "wld-1-00", "title": "W1", "parent": "docs/q.md#epic-w", "release": "docs/rel.md", "state": "ready", "priority": "P1", "agent": "deep", "order": 1}
{"legacy": "wld-1-01", "title": "W2", "parent": "docs/q.md#epic-w", "needs": ["WLD-1-00"], "state": "done", "done_summary": "shipped", "order": 2}
{"legacy": "wld-1-02", "title": "W3", "parent": "docs/q.md#epic-w", "needs": ["wld-1-01"], "relates": ["docs/rel.md"], "state": "blocked", "reason": "waits", "order": 3}
"#;

#[test]
fn import_issues_loads_an_inventory() {
    let t = T::new("inventory");
    let cfg = t.write("i.cfg", "backup_keep = 0\nagent.deep = opus\n");
    let inv = t.write("inv.jsonl", INVENTORY);
    let v = ok(at(
        &t,
        &cfg,
        None,
        &[
            "import",
            "--issues",
            &p(&inv),
            "--prefix-for",
            "feature=EPIC",
        ],
    ));
    assert_eq!(v["imported"], 5);
    assert_eq!(v["links"], 3);
    let g = v["generated_ids"].as_array().unwrap();
    assert_eq!(g[0]["id"], "EPIC-1-00");
    assert_eq!(g[1]["id"], "REL-1-00");
    let w1 = ok(at(&t, &cfg, None, &["show", "WLD-1-00"]));
    assert_eq!(w1["issue"]["legacy"], Value::Null);
    assert_eq!(w1["issue"]["parent"], "EPIC-1-00");
    assert_eq!(w1["issue"]["release"], "REL-1-00");
    let rel = ok(at(&t, &cfg, None, &["show", "REL-1-00"]));
    assert_eq!(rel["issue"]["due"], "2027-03-01");
    assert_eq!(w1["issue"]["due"], Value::Null);
    assert_eq!(w1["issue"]["priority"], "P1");
    assert_eq!(w1["issue"]["agent_model"], "opus");
    let w2 = ok(at(&t, &cfg, None, &["show", "WLD-1-01"]));
    assert_eq!(w2["issue"]["state"], "done");
    assert_eq!(w2["issue"]["summary"], "shipped");
    assert_eq!(w2["issue"]["priority"], "P2");
    let epic = ok(at(&t, &cfg, None, &["show", "docs/q.md#epic-w"]));
    assert_eq!(epic["issue"]["id"], "EPIC-1-00");
    assert_eq!(epic["issue"]["type"], "feature");
    let body = epic["issue"]["body"].as_str().unwrap();
    assert!(
        body.contains("Labels: wld") && body.contains("Source: docs/q.md"),
        "{body}"
    );
    assert_eq!(ok(at(&t, &cfg, None, &["check"]))["ok"], true);
    let v = ok(at(&t, &cfg, None, &["next"]));
    assert_eq!(ids(&v), ["WLD-1-00"]);
}

#[test]
fn import_issues_lists_every_problem_before_writing() {
    let t = T::new("badinventory");
    let cfg = plain_cfg(&t);
    let inv = t.write(
        "inv.jsonl",
        concat!(
            "{\"legacy\": \"a\", \"title\": \"A\", \"needs\": [\"b\"]}\n",
            "{\"legacy\": \"b\", \"title\": \"B\", \"needs\": [\"a\"]}\n",
            "{\"legacy\": \"c\", \"state\": \"bogus\", \"title\": \"C\"}\n",
            "{\"legacy\": \"d\", \"parent\": \"nowhere\", \"title\": \"D\", \"due\": \"soon\"}\n",
            "{\"legacy\": \"A\", \"title\": \"dup legacy\"}\n",
            "{\"legacy\": \"e\"}\n",
            "{\"legacy\": \"f\", \"title\": \"F\", \"state\": \"blocked\"}\n",
            "not json\n",
        ),
    );
    let o = at(&t, &cfg, None, &["import", "--issues", &p(&inv)]);
    assert_eq!(
        (o.code, code(&o).as_str()),
        (2, "IMPORT_INVALID"),
        "{}",
        o.stdout
    );
    let msgs: Vec<String> = o.json["error"]["details"]["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["message"].as_str().unwrap().to_string())
        .collect();
    let all = msgs.join("\n");
    for want in [
        "line 3",
        "nowhere",
        "due date 'soon'",
        "legacy 'A'",
        "title is missing",
        "needs --reason",
        "line 8",
    ] {
        assert!(all.contains(want), "missing {want} in:\n{all}");
    }
    assert!(problem_codes(&o).iter().any(|c| c == "CYCLE"), "{all}");
    let v = ok(at(&t, &cfg, None, &["list", "--all"]));
    assert_eq!(v["count"], 0);
}
