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

#[test]
fn acceptance_criteria_gate_review_and_done() {
    let t = T::new("criteria");
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "login",
            "--criteria",
            "- [ ] accepts a password\n- [ ] locks after 5 tries\n\n",
        ],
    );
    let v = t.ok(None, &["criteria", "CORE-1-00"]);
    assert_eq!(v["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(v["criteria"][1]["text"], "locks after 5 tries");
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    assert_eq!(
        t.err(
            Some("a1"),
            &["state", "CORE-1-00", "in_review", "--gen", "1"]
        )
        .1,
        "CRITERIA_OPEN"
    );
    assert_eq!(
        t.err(
            Some("a1"),
            &["done", "CORE-1-00", "--summary", "x", "--gen", "1"]
        )
        .1,
        "CRITERIA_OPEN"
    );
    // Only the claimant ticks them while the issue is claimed.
    assert_eq!(
        t.err(Some("a2"), &["criteria", "CORE-1-00", "--check", "1"])
            .1,
        "NOT_CLAIMANT"
    );
    t.ok(
        Some("a1"),
        &["criteria", "CORE-1-00", "--check", "1", "--gen", "1"],
    );
    t.ok(
        Some("a1"),
        &[
            "criteria",
            "CORE-1-00",
            "--add",
            "logs failures",
            "--gen",
            "1",
        ],
    );
    assert_eq!(
        t.err(
            Some("a1"),
            &["criteria", "CORE-1-00", "--check", "9", "--gen", "1"]
        )
        .1,
        "NOT_FOUND"
    );
    t.ok(
        Some("a1"),
        &["criteria", "CORE-1-00", "--remove", "2", "--gen", "1"],
    );
    let v = t.ok(None, &["show", "CORE-1-00"]);
    let texts: Vec<&str> = v["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["accepts a password", "logs failures"]);
    assert_eq!(v["criteria"][1]["n"], 2);
    assert_eq!(v["criteria"][0]["done_by"], "a1");
    t.ok(
        Some("a1"),
        &["criteria", "CORE-1-00", "--check", "2", "--gen", "1"],
    );
    let v = t.ok(
        Some("a1"),
        &["state", "CORE-1-00", "in_review", "--gen", "1"],
    );
    assert_eq!(v["issue"]["state"], "in_review");
    // Criteria survive export and import.
    let out = t.path("e.jsonl");
    t.ok(None, &["export", "--out", &p(&out)]);
    let mut c = t.bare();
    c.arg("--db")
        .arg(t.path("copy.db"))
        .args(["--json", "import", &p(&out)]);
    assert_eq!(t.exec(c).code, 0);
    let mut c = t.bare();
    c.arg("--db")
        .arg(t.path("copy.db"))
        .args(["--json", "criteria", "CORE-1-00"]);
    assert_eq!(t.exec(c).json["criteria"][1]["done"], true);
}

#[test]
fn epics_roll_up_to_in_review_and_done() {
    let t = T::new("rollup");
    t.ok(
        None,
        &[
            "add", "EPIC", "world", "--type", "feature", "--state", "planned",
        ],
    );
    t.ok(None, &["add", "WLD", "a", "--parent", "EPIC-1-00"]);
    t.ok(None, &["add", "WLD", "b", "--parent", "EPIC-1-00"]);
    t.ok(
        None,
        &[
            "add",
            "EPIC",
            "epic review",
            "--type",
            "review",
            "--parent",
            "EPIC-1-00",
        ],
    );
    let state = |id: &str| {
        t.ok(None, &["show", id])["issue"]["state"]
            .as_str()
            .unwrap()
            .to_string()
    };
    t.ok(Some("a1"), &["claim", "WLD-1-00"]);
    t.ok(
        Some("a1"),
        &["state", "WLD-1-00", "in_review", "--gen", "1"],
    );
    assert_eq!(state("EPIC-1-00"), "planned");
    t.ok(Some("a2"), &["claim", "WLD-1-01"]);
    let v = t.ok(
        Some("a2"),
        &["state", "WLD-1-01", "in_review", "--gen", "1"],
    );
    assert_eq!(v["rollup"][0]["to"], "in_review");
    assert_eq!(state("EPIC-1-00"), "in_review");
    // A child sent back takes the epic back out of review.
    t.ok(
        None,
        &[
            "state",
            "WLD-1-01",
            "ready",
            "--reason",
            "review found a bug",
        ],
    );
    assert_eq!(state("EPIC-1-00"), "planned");
    t.ok(Some("a2"), &["claim", "WLD-1-01"]);
    t.ok(
        Some("a2"),
        &["state", "WLD-1-01", "in_review", "--gen", "2"],
    );
    assert_eq!(state("EPIC-1-00"), "in_review");
    // Children finished, but the epic's own review is open: it stays in review.
    t.ok(None, &["done", "WLD-1-00", "--summary", "ok", "--force"]);
    t.ok(None, &["done", "WLD-1-01", "--summary", "ok", "--force"]);
    assert_eq!(state("EPIC-1-00"), "in_review");
    t.ok(Some("r1"), &["claim", "EPIC-1-01"]);
    let v = t.ok(
        Some("r1"),
        &["done", "EPIC-1-01", "--summary", "epic ok", "--gen", "1"],
    );
    assert_eq!(v["rollup"][0]["to"], "done");
    let v = t.ok(None, &["show", "EPIC-1-00"]);
    assert_eq!(v["issue"]["state"], "done");
    assert!(
        v["issue"]["summary"]
            .as_str()
            .unwrap()
            .contains("automatic")
    );
}

#[test]
fn stats_measure_time_in_each_column() {
    let t = T::new("stats");
    t.ok_at(None, T0, &["add", "CORE", "timed"]);
    t.ok_at(None, T0, &["add", "CORE", "still ready"]);
    t.ok_at(Some("a1"), T0 + 60 * MIN, &["claim", "CORE-1-00"]);
    t.ok_at(
        Some("a1"),
        T0 + 180 * MIN,
        &["state", "CORE-1-00", "in_review", "--gen", "1"],
    );
    t.ok_at(
        None,
        T0 + 240 * MIN,
        &["done", "CORE-1-00", "--summary", "ok", "--force"],
    );
    let v = t.ok_at(None, T0 + 300 * MIN, &["stats"]);
    let col = |c: &str, k: &str| v["columns"][c][k].clone();
    assert_eq!(col("ready", "count"), 1);
    assert_eq!(col("ready", "median_hours"), 1.0);
    assert_eq!(col("in_progress", "median_hours"), 2.0);
    assert_eq!(col("in_review", "median_hours"), 1.0);
    assert_eq!(col("ready", "now"), 1);
    assert_eq!(col("ready", "now_max_hours"), 5.0);
    assert_eq!(col("done", "now"), 1);
    assert_eq!(v["cycle_time"]["median_hours"], 3.0);
    assert_eq!(v["lead_time"]["median_hours"], 4.0);
    // --since keeps only time that ended inside the window.
    let v = t.ok_at(None, T0 + 300 * MIN, &["stats", "--since", "90m"]);
    assert_eq!(v["columns"]["ready"]["count"], 0);
    assert_eq!(v["columns"]["in_review"]["count"], 1);
}

/// Ids follow the hierarchy: a feature opens a group (`CORE-2-00`), the
/// issues under it take the numbers after `-00`, and loose tasks stay out
/// of feature groups.
#[test]
fn features_open_groups_and_children_number_under_them() {
    let t = T::new("idgroups");
    let id = |v: serde_json::Value| v["issue"]["id"].as_str().unwrap().to_string();
    let f1 = id(t.ok(None, &["add", "CORE", "first feature", "--type", "feature"]));
    let f2 = id(t.ok(
        None,
        &["add", "CORE", "second feature", "--type", "feature"],
    ));
    assert_eq!((f1.as_str(), f2.as_str()), ("CORE-1-00", "CORE-2-00"));
    let a = id(t.ok(None, &["add", "CORE", "task of 2", "--parent", "CORE-2-00"]));
    let b = id(t.ok(None, &["add", "CORE", "task of 1", "--parent", "CORE-1-00"]));
    let c = id(t.ok(
        None,
        &["add", "CORE", "another of 2", "--parent", "CORE-2-00"],
    ));
    assert_eq!(
        (a.as_str(), b.as_str(), c.as_str()),
        ("CORE-2-01", "CORE-1-01", "CORE-2-02")
    );
    // A loose task opens its own group instead of joining a feature's.
    let loose = id(t.ok(None, &["add", "CORE", "loose"]));
    assert_eq!(loose, "CORE-3-00");
    assert_eq!(id(t.ok(None, &["add", "CORE", "loose 2"])), "CORE-3-01");
    // An explicit group or id still wins.
    assert_eq!(
        id(t.ok(
            None,
            &["add", "CORE-9", "explicit", "--parent", "CORE-1-00"]
        )),
        "CORE-9-00"
    );
}

/// A released feature shows 100% on the roadmap, with or without members.
#[test]
fn released_features_show_complete_on_the_roadmap() {
    let t = T::new("rmreleased");
    t.ok(None, &["add", "CORE", "empty feature", "--type", "feature"]);
    t.ok(None, &["add", "CORE", "full feature", "--type", "feature"]);
    t.ok(None, &["add", "CORE", "its task", "--parent", "CORE-2-00"]);
    let pct = |id: &str| {
        let v = t.ok(None, &["roadmap", "--all"]);
        v["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .map(|f| f["rollup"]["percent"].clone())
            .unwrap()
    };
    assert_eq!(pct("CORE-1-00"), 0);
    t.ok(
        None,
        &[
            "done",
            "CORE-1-00",
            "--summary",
            "nothing to build",
            "--force",
        ],
    );
    assert_eq!(pct("CORE-1-00"), 100);
    t.ok(None, &["ship", "CORE-1-00", "--force"]);
    assert_eq!(pct("CORE-1-00"), 100);
    t.ok(
        None,
        &["done", "CORE-2-01", "--summary", "built", "--force"],
    );
    t.ok(None, &["ship", "CORE-2-00", "--force"]);
    assert_eq!(pct("CORE-2-00"), 100);
}

/// Labels: set on add/edit, normalized, filterable, kept by export/import.
#[test]
fn labels_are_set_filtered_and_validated() {
    let t = T::new("labels");
    let v = t.ok(
        None,
        &["add", "CORE", "login form", "--labels", "UI, Bug,ui"],
    );
    assert_eq!(v["issue"]["labels"], serde_json::json!(["ui", "bug"]));
    t.ok(None, &["add", "CORE", "schema", "--labels", "db"]);
    t.ok(None, &["add", "CORE", "plain"]);
    let ids = |label: &str| -> Vec<String> {
        t.ok(None, &["list", "--label", label])["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("ui"), ["CORE-1-00"]);
    assert_eq!(ids("db"), ["CORE-1-01"]);
    assert_eq!(ids("ui,bug"), ["CORE-1-00"]);
    let v = t.ok(None, &["edit", "CORE-1-01", "--labels", "db,perf"]);
    assert_eq!(v["issue"]["labels"], serde_json::json!(["db", "perf"]));
    let v = t.ok(None, &["edit", "CORE-1-01", "--labels", ""]);
    assert_eq!(v["issue"]["labels"], serde_json::json!([]));
    for bad in ["has space", "émoji", &"x".repeat(33)] {
        assert_eq!(
            t.err(None, &["add", "CORE", "x", "--labels", bad]).1,
            "INVALID_INPUT",
            "{bad}"
        );
    }
}

/// Checking a criterion can record how it was met.
#[test]
fn criteria_can_carry_evidence() {
    let t = T::new("evidence");
    t.ok(
        None,
        &["add", "CORE", "login", "--criteria", "rejects bad password"],
    );
    t.ok(Some("a1"), &["claim", "CORE-1-00"]);
    let v = t.ok(
        Some("a1"),
        &[
            "criteria",
            "CORE-1-00",
            "--check",
            "1",
            "--evidence",
            "test login_rejects_bad_password passes",
            "--gen",
            "1",
        ],
    );
    assert_eq!(
        v["criteria"][0]["evidence"],
        "test login_rejects_bad_password passes"
    );
    let v = t.ok(None, &["show", "CORE-1-00"]);
    assert_eq!(
        v["criteria"][0]["evidence"],
        "test login_rejects_bad_password passes"
    );
    // Unchecking clears it; evidence needs --check.
    let v = t.ok(
        Some("a1"),
        &["criteria", "CORE-1-00", "--uncheck", "1", "--gen", "1"],
    );
    assert!(v["criteria"][0]["evidence"].is_null(), "{v}");
    assert_eq!(
        t.err(
            Some("a1"),
            &["criteria", "CORE-1-00", "--evidence", "x", "--gen", "1"]
        )
        .1,
        "USAGE"
    );
}

/// `add` warns about open issues with nearly the same title, and adds anyway.
#[test]
fn add_warns_about_likely_duplicates() {
    let t = T::new("dups");
    t.ok(
        None,
        &["add", "CORE", "Login form rejects a wrong password"],
    );
    t.ok(None, &["add", "CORE", "Database schema migration"]);
    let v = t.ok(None, &["add", "CORE", "login form: reject wrong password"]);
    assert_eq!(v["issue"]["id"], "CORE-1-02");
    let d = v["possible_duplicates"].as_array().unwrap();
    assert_eq!(d.len(), 1, "{v}");
    assert_eq!(d[0]["id"], "CORE-1-00");
    // Unrelated titles and finished issues are not reported.
    let v = t.ok(None, &["add", "CORE", "Dashboard dark mode"]);
    assert_eq!(v["possible_duplicates"], serde_json::json!([]));
    t.ok(None, &["done", "CORE-1-01", "--summary", "x", "--force"]);
    let v = t.ok(None, &["add", "CORE", "database schema migration"]);
    assert_eq!(v["possible_duplicates"], serde_json::json!([]));
}

/// Releases carry an expected date; moving it is recorded with both dates.
#[test]
fn due_dates_are_validated_and_moves_recorded() {
    let t = T::new("due");
    let v = t.ok(
        None,
        &[
            "add",
            "REL",
            "v1",
            "--type",
            "release",
            "--due",
            "2026-12-15",
        ],
    );
    assert_eq!(v["issue"]["due"], "2026-12-15");
    for bad in ["2026-13-01", "2026-02-30", "15/12/2026", "2026-1-5"] {
        assert_eq!(
            t.err(None, &["edit", "REL-1-00", "--due", bad]).1,
            "INVALID_INPUT",
            "{bad}"
        );
    }
    assert_eq!(
        t.ok(None, &["edit", "REL-1-00", "--due", "2024-02-29"])["issue"]["due"],
        "2024-02-29"
    );
    t.ok(None, &["edit", "REL-1-00", "--due", "2027-01-10"]);
    let v = t.ok(None, &["activity", "--issue", "REL-1-00"]);
    let moved = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e["details"]["due"].is_object())
        .unwrap();
    assert_eq!(moved["details"]["due"]["from"], "2024-02-29");
    assert_eq!(moved["details"]["due"]["to"], "2027-01-10");
    let v = t.ok(None, &["edit", "REL-1-00", "--due", ""]);
    assert!(v["issue"]["due"].is_null(), "{v}");
}
