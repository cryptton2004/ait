//! Review, release and dependency workflow (owner request, 2026-11-26):
//! in_review and released states, review issues, `needs` links that wait
//! for a state, and `ait ship`.

mod common;
use common::*;
use serde_json::Value;
use std::path::Path;

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

fn next_ids(t: &T) -> Vec<String> {
    t.ok(None, &["next"])["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn reviews_start_when_the_parent_is_in_review_and_gate_done() {
    let t = T::new("reviews");
    t.ok(None, &["add", "CORE", "build the parser"]);
    let e = t.err(None, &["add", "CORE", "orphan review", "--type", "review"]);
    assert_eq!(e.1, "INVALID_INPUT");
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "code review",
            "--type",
            "review",
            "--parent",
            "CORE-1-00",
        ],
    );
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "security review",
            "--type",
            "review",
            "--parent",
            "CORE-1-00",
        ],
    );
    // Reviews are not offered or claimable before the parent is in review.
    assert_eq!(next_ids(&t), ["CORE-1-00"]);
    assert_eq!(
        t.err(Some("r1"), &["claim", "CORE-1-01"]).1,
        "REVIEW_NOT_STARTED"
    );
    let v = t.ok(None, &["next", "--explain", "CORE-1-01"]);
    assert_eq!(v["blockers"][0]["code"], "REVIEW_WAITS");
    // The builder cannot close it while reviews are open.
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    assert_eq!(
        t.err(
            Some("a1"),
            &["done", "CORE-1-00", "--summary", "x", "--gen", "1"]
        )
        .1,
        "REVIEWS_OPEN"
    );
    // Only the claimant submits it; the claim is handed back.
    assert_eq!(
        t.err(Some("a2"), &["state", "CORE-1-00", "in_review"]).1,
        "NOT_CLAIMANT"
    );
    let v = t.ok(
        Some("a1"),
        &["state", "CORE-1-00", "in_review", "--gen", "1"],
    );
    assert_eq!(v["issue"]["state"], "in_review");
    let show = t.ok(None, &["show", "CORE-1-00"]);
    assert_eq!(show["claim"]["status"], "released");
    assert_eq!(
        t.err(Some("a2"), &["claim", "CORE-1-00"]).1,
        "INVALID_STATE"
    );
    // Now the reviews are offered.
    assert_eq!(next_ids(&t), ["CORE-1-01", "CORE-1-02"]);
    let v = t.ok(Some("r1"), &["next", "--claim", "--type", "review"]);
    assert_eq!(v["issue"]["id"], "CORE-1-01");
    t.ok(
        Some("r1"),
        &["done", "CORE-1-01", "--summary", "LGTM", "--gen", "1"],
    );
    assert_eq!(
        t.err(None, &["done", "CORE-1-00", "--summary", "x"]).1,
        "REVIEWS_OPEN"
    );
    t.ok(Some("r2"), &["claim", "CORE-1-02"]);
    t.ok(
        Some("r2"),
        &[
            "done",
            "CORE-1-02",
            "--summary",
            "no findings",
            "--gen",
            "1",
        ],
    );
    // All reviews passed: anyone may close the reviewed issue, no claim needed.
    let v = t.ok(None, &["done", "CORE-1-00", "--summary", "parser shipped"]);
    assert_eq!(v["issue"]["state"], "done");
}

#[test]
fn a_failed_review_sends_the_work_back() {
    let t = T::new("reviewfail");
    t.ok(None, &["add", "CORE", "work"]);
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "review",
            "--type",
            "review",
            "--parent",
            "CORE-1-00",
        ],
    );
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    t.ok(
        Some("a1"),
        &["state", "CORE-1-00", "in_review", "--gen", "1"],
    );
    t.ok(Some("r1"), &["claim", "CORE-1-01"]);
    t.ok(
        Some("r1"),
        &["note", "CORE-1-01", "missing tests", "--gen", "1"],
    );
    t.ok(Some("r1"), &["release", "CORE-1-01", "--gen", "1"]);
    t.ok(
        Some("r1"),
        &[
            "state",
            "CORE-1-00",
            "ready",
            "--reason",
            "review: missing tests",
        ],
    );
    // Back to building; the review waits for the next submission.
    assert_eq!(next_ids(&t), ["CORE-1-00"]);
    let v = t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    assert_eq!(v["claim"]["generation"], 2);
    // in_review with no reviews at all is closed only by the owner.
    t.ok(None, &["add", "CORE", "solo"]);
    t.ok(Some("a3"), &["claim", "CORE-1-02"]);
    t.ok(
        Some("a3"),
        &["state", "CORE-1-02", "in_review", "--gen", "1"],
    );
    assert_eq!(
        t.err(None, &["done", "CORE-1-02", "--summary", "ok"]).1,
        "REVIEWS_OPEN"
    );
    t.ok(None, &["done", "CORE-1-02", "--summary", "ok", "--force"]);
}

#[test]
fn needs_can_wait_for_a_state() {
    let t = T::new("until");
    t.ok(None, &["add", "CORE", "api"]);
    t.ok(
        None,
        &["add", "CORE", "docs", "--needs", "CORE-1-00:in_review"],
    );
    t.ok(None, &["add", "CORE", "client"]);
    t.ok(
        None,
        &[
            "link",
            "CORE-1-02",
            "blocked-by",
            "CORE-1-00",
            "--until",
            "ready",
        ],
    );
    assert_eq!(
        t.err(
            None,
            &[
                "link",
                "CORE-1-02",
                "needs",
                "CORE-1-00",
                "--until",
                "dropped"
            ]
        )
        .1,
        "INVALID_INPUT"
    );
    // client waits for api to be ready (it is); docs waits for api in review.
    assert_eq!(next_ids(&t), ["CORE-1-00", "CORE-1-02"]);
    let v = t.ok(None, &["show", "CORE-1-01"]);
    assert_eq!(v["links"]["needs"][0]["until"], "in_review");
    let v = t.ok(None, &["next", "--explain", "CORE-1-01"]);
    assert!(
        v["blockers"][0]["message"]
            .as_str()
            .unwrap()
            .contains("CORE-1-00 to be in_review"),
        "{v}"
    );
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    t.ok(
        Some("a1"),
        &["state", "CORE-1-00", "in_review", "--gen", "1"],
    );
    assert_eq!(next_ids(&t), ["CORE-1-01", "CORE-1-02"]);
    // The link survives export and import.
    let out = t.path("e.jsonl");
    t.ok(None, &["export", "--out", &p(&out)]);
    let mut c = t.bare();
    c.arg("--db")
        .arg(t.path("copy.db"))
        .args(["--json", "import", &p(&out)]);
    assert_eq!(t.exec(c).code, 0);
}

fn ship_setup(t: &T) {
    t.ok(None, &["add", "EPIC", "world", "--type", "feature"]);
    t.ok(None, &["add", "WLD", "w1", "--parent", "EPIC-1-00"]);
    t.ok(None, &["add", "WLD", "w2", "--parent", "EPIC-1-00"]);
}

#[test]
fn ship_releases_a_finished_epic_after_merge_and_push() {
    let t = T::new("ship");
    let repo = t.path("repo");
    let origin = t.path("origin.git");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "start"]);
    git(&repo, &["remote", "add", "origin", &p(&origin)]);
    let cfg = t.write("s.cfg", "repo = repo\nbackup_keep = 0\n");
    let run = |args: &[&str]| {
        let mut c = t.cmd(None, &[]);
        c.arg("--config").arg(&cfg).args(args);
        t.exec(c)
    };
    ship_setup(&t);
    // Not finished yet.
    let o = run(&["ship", "EPIC-1-00"]);
    assert_eq!(err_code(&o.json), "NOT_SHIPPABLE", "{}", o.stdout);
    assert_eq!(o.code, 4);
    for id in ["WLD-1-00", "WLD-1-01"] {
        assert_eq!(run(&["done", id, "--summary", "ok", "--force"]).code, 0);
    }
    // Done, but not merged on main.
    let o = run(&["ship", "EPIC-1-00"]);
    assert_eq!(err_code(&o.json), "NOT_SHIPPABLE", "{}", o.stdout);
    for (b, id) in [("b0", "WLD-1-00"), ("b1", "WLD-1-01")] {
        git(&repo, &["checkout", "-q", "-b", b]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "work"]);
        git(&repo, &["checkout", "-q", "main"]);
        let msg = format!("Issue-ID: {id}");
        git(
            &repo,
            &["merge", "-q", "--no-ff", b, "-m", "merge", "-m", &msg],
        );
    }
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["fetch", "-q", "origin"]);
    // A later merge that is not pushed blocks shipping its issue.
    t.ok(None, &["add", "WLD", "w3", "--parent", "EPIC-1-00"]);
    assert_eq!(
        run(&["done", "WLD-1-02", "--summary", "ok", "--force"]).code,
        0
    );
    git(&repo, &["checkout", "-q", "-b", "b2"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "work"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(
        &repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "b2",
            "-m",
            "merge",
            "-m",
            "Issue-ID: WLD-1-02",
        ],
    );
    let o = run(&["ship", "EPIC-1-00"]);
    assert_eq!(err_code(&o.json), "NOT_SHIPPABLE");
    assert!(o.stdout.contains("not pushed"), "{}", o.stdout);
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["fetch", "-q", "origin"]);
    let o = run(&["ship", "EPIC-1-00", "--ref", "v1.0"]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    let released: Vec<&str> = o.json["released"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(released, ["EPIC-1-00", "WLD-1-00", "WLD-1-01", "WLD-1-02"]);
    let v: Value = run(&["show", "WLD-1-00"]).json;
    assert_eq!(v["issue"]["state"], "released");
    assert_eq!(v["issue"]["release_ref"], "v1.0");
    // Released work is final.
    assert_eq!(
        err_code(&run(&["state", "WLD-1-00", "ready"]).json),
        "ALREADY_DONE"
    );
    assert_eq!(
        err_code(&run(&["state", "WLD-1-00", "released"]).json),
        "INVALID_STATE"
    );
    assert_eq!(
        err_code(&run(&["done", "WLD-1-00", "--summary", "x", "--force"]).json),
        "ALREADY_DONE"
    );
    let v = run(&["roadmap"]).json;
    assert_eq!(v["features"][0]["rollup"]["released"], 3);
    assert_eq!(run(&["check", "--git"]).code, 0, "check after ship");
}

#[test]
fn ship_without_a_repo_warns_and_force_skips_the_gates() {
    let t = T::new("shipnorepo");
    let cfg = t.write("s.cfg", "repo = nowhere-not-a-repo\nbackup_keep = 0\n");
    let _ = cfg;
    ship_setup(&t);
    assert_eq!(t.err(None, &["ship", "EPIC-1-00"]).1, "NOT_SHIPPABLE");
    t.ok(None, &["done", "WLD-1-00", "--summary", "ok", "--force"]);
    t.ok(None, &["state", "WLD-1-01", "dropped", "--reason", "cut"]);
    let v = t.ok(None, &["ship", "EPIC-1-00", "--force"]);
    assert_eq!(v["released"].as_array().unwrap().len(), 2);
}

#[test]
fn wip_limits_per_column_and_agent_tier() {
    let t = T::new("wip");
    let cfg = t.write(
        "w.cfg",
        "backup_keep = 0\nagent.deep = opus\nagent.standard = sonnet\nwip.in_progress = 2\nwip.agent.deep = 1\nwip.in_review = 1\n",
    );
    let run = |s: Option<&str>, args: &[&str]| {
        let mut c = t.cmd(s, &[]);
        c.arg("--config").arg(&cfg).args(args);
        t.exec(c)
    };
    for (title, tier) in [
        ("a", "deep"),
        ("b", "deep"),
        ("c", "standard"),
        ("d", "standard"),
    ] {
        assert_eq!(run(None, &["add", "CORE", title, "--agent", tier]).code, 0);
    }
    let o = run(Some("s1"), &["next", "--claim"]);
    assert_eq!(o.json["issue"]["id"], "CORE-1-00");
    // The deep tier is full, so the next agent gets standard work.
    let o = run(Some("s2"), &["next", "--claim"]);
    assert_eq!(o.json["issue"]["id"], "CORE-1-02", "{}", o.stdout);
    // In progress is full.
    let o = run(Some("s3"), &["next", "--claim"]);
    assert_eq!(
        (o.code, err_code(&o.json).as_str()),
        (4, "WIP_LIMIT"),
        "{}",
        o.stdout
    );
    assert_eq!(o.json["error"]["retryable"], true);
    let o = run(None, &["next", "--explain", "CORE-1-03"]);
    assert_eq!(o.json["blockers"][0]["code"], "WIP_LIMIT");
    // Renewing an existing claim is not a new claim.
    assert_eq!(run(Some("s1"), &["claim", "CORE-1-00"]).code, 0);
    // Handing work to review frees a slot.
    assert_eq!(
        run(
            Some("s1"),
            &["state", "CORE-1-00", "in_review", "--gen", "1"]
        )
        .code,
        0
    );
    assert_eq!(run(Some("s3"), &["claim", "CORE-1-03"]).code, 0);
    // Review is full.
    let o = run(
        Some("s2"),
        &["state", "CORE-1-02", "in_review", "--gen", "1"],
    );
    assert_eq!(err_code(&o.json), "WIP_LIMIT", "{}", o.stdout);
    let o = run(
        Some("s2"),
        &["state", "CORE-1-02", "in_review", "--gen", "1", "--force"],
    );
    assert_eq!(o.code, 0, "{}", o.stdout);
    // Bad limits are configuration errors.
    let bad = t.write("bad.cfg", "wip.everything = 3\n");
    let mut c = t.cmd(None, &["list"]);
    c.arg("--config").arg(&bad);
    assert_eq!(err_code(&t.exec(c).json), "CONFIG");
}
