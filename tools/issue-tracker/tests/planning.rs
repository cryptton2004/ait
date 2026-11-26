//! Planning commands, ids, errors, config and database location.

mod common;
use common::*;
use serde_json::Value;

#[test]
fn add_allocates_ids_in_sequence() {
    let t = T::new("alloc");
    assert_eq!(id(&t.ok(None, &["add", "CORE", "First"])), "CORE-1-00");
    assert_eq!(id(&t.ok(None, &["add", "core", "Second"])), "CORE-1-01");
    assert_eq!(
        id(&t.ok(None, &["add", "CORE-2", "Group two"])),
        "CORE-2-00"
    );
    // A bare prefix continues in the highest group.
    assert_eq!(id(&t.ok(None, &["add", "CORE", "Third"])), "CORE-2-01");
    assert_eq!(
        id(&t.ok(None, &["add", "PAP-1-07", "Explicit"])),
        "PAP-1-07"
    );
    let (code, e) = t.err(None, &["add", "PAP-1-07", "Again"]);
    assert_eq!((code, e.as_str()), (4, "ID_TAKEN"));
}

#[test]
fn add_after_picks_next_free_gap_and_orders_after_it() {
    let t = T::new("after");
    t.ok(None, &["add", "CORE-1-00", "a"]);
    t.ok(None, &["add", "CORE-1-10", "b"]);
    t.ok(None, &["add", "CORE-1-11", "c"]);
    t.ok(None, &["add", "CORE-1-20", "d"]);
    let v = t.ok(None, &["add", "CORE", "inserted", "--after", "CORE-1-10"]);
    assert_eq!(id(&v), "CORE-1-12");
    let list = t.ok(None, &["list"]);
    let order: Vec<&str> = list["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "CORE-1-00",
            "CORE-1-10",
            "CORE-1-12",
            "CORE-1-11",
            "CORE-1-20"
        ]
    );
}

#[test]
fn invalid_input_is_reported_as_structured_errors() {
    let t = T::new("invalid");
    let o = t.run(None, &["add", "C", "bad prefix"]);
    assert_eq!(o.code, 2);
    assert_eq!(err_code(&o.json), "INVALID_ID");
    assert!(o.json["error"]["message"].is_string());
    assert_eq!(o.json["error"]["retryable"], Value::Bool(false));
    assert_eq!(t.err(None, &["add", "CORE", ""]).1, "INVALID_INPUT");
    assert_eq!(
        t.err(None, &["add", "CORE", "x", "--state", "nope"]).1,
        "INVALID_STATE"
    );
    assert_eq!(
        t.err(None, &["add", "CORE", "x", "--state", "blocked"]).1,
        "INVALID_INPUT"
    );
    assert_eq!(t.err(None, &["bogus"]), (2, "USAGE".to_string()));
    assert_eq!(
        t.err(None, &["show", "CORE-9-99"]),
        (3, "NOT_FOUND".to_string())
    );
    assert_eq!(t.err(None, &["add", "CORE", "x", "--wat", "1"]).1, "USAGE");
}

#[test]
fn text_errors_go_to_stderr() {
    let t = T::new("texterr");
    let mut c = t.bare();
    c.arg("--db").arg(t.db()).args(["show", "NOPE-1-00"]);
    let o = t.exec(c);
    assert_eq!(o.code, 3);
    assert!(o.stdout.is_empty());
    assert!(o.stderr.contains("NOT_FOUND"), "{}", o.stderr);
}

#[test]
fn show_returns_issue_links_claim_and_readiness() {
    let t = T::new("show");
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "Base",
            "--body",
            "## Goal\nDo it\n",
            "--size",
            "M",
            "--spec",
            "docs/x.md",
        ],
    );
    t.ok(
        None,
        &[
            "add",
            "CORE",
            "Child",
            "--needs",
            "CORE-1-00",
            "--parent",
            "CORE-1-00",
        ],
    );
    t.ok(
        None,
        &["add", "CORE", "Side", "--discovered-from", "CORE-1-01"],
    );
    t.ok(None, &["link", "CORE-1-02", "relates", "CORE-1-00"]);
    let v = t.ok(None, &["show", "CORE-1-00"]);
    assert_eq!(v["issue"]["body"], "## Goal\nDo it\n");
    assert_eq!(v["issue"]["size"], "M");
    assert_eq!(v["links"]["needed_by"][0]["id"], "CORE-1-01");
    assert_eq!(v["links"]["children"][0]["id"], "CORE-1-01");
    assert_eq!(v["links"]["relates"][0]["id"], "CORE-1-02");
    assert_eq!(v["ready"], true);
    let child = t.ok(None, &["show", "core-1-01"]);
    assert_eq!(child["ready"], false);
    assert_eq!(child["blockers"][0]["code"], "NEEDS");
    assert_eq!(child["links"]["discovered"][0]["id"], "CORE-1-02");
    assert_eq!(child["links"]["parent"]["id"], "CORE-1-00");
}

#[test]
fn edit_bumps_version_and_detects_conflicts() {
    let t = T::new("edit");
    t.ok(None, &["add", "CORE", "Old title"]);
    let v = t.ok(
        None,
        &[
            "edit",
            "CORE-1-00",
            "--title",
            "New title",
            "--version",
            "1",
        ],
    );
    assert_eq!(v["issue"]["title"], "New title");
    assert_eq!(v["issue"]["version"], 2);
    let (code, e) = t.err(
        None,
        &["edit", "CORE-1-00", "--body", "x", "--version", "1"],
    );
    assert_eq!((code, e.as_str()), (4, "VERSION_CONFLICT"));
    assert_eq!(t.err(None, &["edit", "CORE-1-00"]).1, "USAGE");
    // Parent loops are refused.
    t.ok(None, &["add", "CORE", "Kid", "--parent", "CORE-1-00"]);
    assert_eq!(
        t.err(None, &["edit", "CORE-1-00", "--parent", "CORE-1-01"])
            .1,
        "CYCLE"
    );
    // Body from a file.
    let f = t.write("body.md", "from file\n");
    let v = t.ok(None, &["edit", "CORE-1-00", "--body-file", &p(&f)]);
    assert_eq!(v["issue"]["body"], "from file\n");
}

#[test]
fn state_changes_need_reasons_and_done_needs_the_done_command() {
    let t = T::new("state");
    t.ok(None, &["add", "CORE", "x"]);
    assert_eq!(
        t.err(None, &["state", "CORE-1-00", "blocked"]).1,
        "INVALID_INPUT"
    );
    let v = t.ok(
        None,
        &[
            "state",
            "CORE-1-00",
            "blocked",
            "--reason",
            "waiting for API key",
        ],
    );
    assert_eq!(v["issue"]["state"], "blocked");
    assert_eq!(v["issue"]["reason"], "waiting for API key");
    assert_eq!(
        t.err(None, &["state", "CORE-1-00", "done"]).1,
        "INVALID_STATE"
    );
    let v = t.ok(None, &["state", "CORE-1-00", "ready"]);
    assert_eq!(v["issue"]["reason"], Value::Null);
}

#[test]
fn needs_cycles_are_refused() {
    let t = T::new("cycle");
    for n in ["a", "b", "c"] {
        t.ok(None, &["add", "CORE", n]);
    }
    t.ok(None, &["link", "CORE-1-00", "needs", "CORE-1-01"]);
    t.ok(None, &["link", "CORE-1-01", "needs", "CORE-1-02"]);
    let (code, e) = t.err(None, &["link", "CORE-1-02", "needs", "CORE-1-00"]);
    assert_eq!((code, e.as_str()), (4, "CYCLE"));
    assert_eq!(
        t.err(None, &["add", "CORE", "self", "--needs", "CORE-1-03"])
            .1,
        "NOT_FOUND"
    );
    let v = t.ok(None, &["unlink", "CORE-1-01", "needs", "CORE-1-02"]);
    assert_eq!(v["changed"], true);
    t.ok(None, &["link", "CORE-1-02", "needs", "CORE-1-00"]);
}

#[test]
fn delete_protects_referenced_issues_and_retires_the_id() {
    let t = T::new("delete");
    t.ok(None, &["add", "CORE", "base"]);
    t.ok(None, &["add", "CORE", "dependent", "--needs", "CORE-1-00"]);
    assert_eq!(
        t.err(None, &["delete", "CORE-1-00"]),
        (4, "IN_USE".to_string())
    );
    t.ok(None, &["delete", "CORE-1-00", "--force"]);
    assert_eq!(t.err(None, &["show", "CORE-1-00"]).1, "NOT_FOUND");
    let dep = t.ok(None, &["show", "CORE-1-01"]);
    assert_eq!(dep["links"]["needs"].as_array().unwrap().len(), 0);
    // The deleted id is never handed out again.
    assert_eq!(t.err(None, &["add", "CORE-1-00", "reuse"]).1, "ID_TAKEN");
    assert_eq!(id(&t.ok(None, &["add", "CORE", "next"])), "CORE-1-02");
}

#[test]
fn legacy_aliases_resolve_everywhere() {
    let t = T::new("legacy");
    t.ok(None, &["add", "CORE", "old one", "--legacy", "core-17"]);
    assert_eq!(id(&t.ok(None, &["show", "core-17"])), "CORE-1-00");
    assert_eq!(id(&t.ok(None, &["show", "CORE-17"])), "CORE-1-00");
    assert_eq!(
        t.err(None, &["add", "CORE", "dup", "--legacy", "CORE-17"])
            .1,
        "ID_TAKEN"
    );
    t.ok(None, &["state", "core-17", "planned"]);
}

#[test]
fn list_filters_and_move_reorders() {
    let t = T::new("list");
    t.ok(None, &["add", "CORE", "one"]);
    t.ok(None, &["add", "CORE", "two", "--state", "planned"]);
    t.ok(None, &["add", "PAP", "three"]);
    t.ok(
        None,
        &[
            "add", "CORE", "gone", "--state", "dropped", "--reason", "obsolete",
        ],
    );
    assert_eq!(t.ok(None, &["list"])["count"], 3);
    assert_eq!(t.ok(None, &["list", "--all"])["count"], 4);
    assert_eq!(t.ok(None, &["list", "--prefix", "PAP"])["count"], 1);
    assert_eq!(
        t.ok(None, &["list", "--state", "planned,dropped"])["count"],
        2
    );
    assert_eq!(t.ok(None, &["list", "--ready"])["count"], 2);
    t.ok(None, &["move", "PAP-1-00", "--top"]);
    let l = t.ok(None, &["list"]);
    assert_eq!(l["issues"][0]["id"], "PAP-1-00");
    t.ok(None, &["move", "PAP-1-00", "--after", "CORE-1-01"]);
    let l = t.ok(None, &["list"]);
    assert_eq!(l["issues"][2]["id"], "PAP-1-00");
    t.ok(None, &["move", "PAP-1-00", "--before", "CORE-1-00"]);
    assert_eq!(t.ok(None, &["list"])["issues"][0]["id"], "PAP-1-00");
    assert_eq!(t.err(None, &["move", "PAP-1-00"]).1, "USAGE");
    // `next` without --claim follows the order.
    assert_eq!(t.ok(None, &["next"])["issues"][0]["id"], "PAP-1-00");
}

#[test]
fn config_file_sets_prefixes_lease_and_db() {
    let t = T::new("config");
    let cfg = t.write(
        "cfg/default.cfg",
        "# test config\ndb = data/tracker.db\nprefixes = CORE, PAP\nlease_minutes = 5\nbackup_keep = 0\n",
    );
    let mut c = t.bare();
    c.args(["--config", &p(&cfg), "--json", "add", "WLD", "x"]);
    let o = t.exec(c);
    assert_eq!((o.code, err_code(&o.json).as_str()), (2, "UNKNOWN_PREFIX"));
    let mut c = t.bare();
    c.args([
        "--config",
        &p(&cfg),
        "--json",
        "--as",
        "a1",
        "add",
        "CORE",
        "x",
    ]);
    assert_eq!(t.exec(c).code, 0);
    assert!(
        t.path("cfg/data/tracker.db").is_file(),
        "db path is relative to the cfg file"
    );
    let mut c = t.bare();
    c.args([
        "--config",
        &p(&cfg),
        "--json",
        "--as",
        "a1",
        "claim",
        "CORE-1-00",
    ]);
    let v = t.exec(c).json;
    let claimed = v["claim"]["claimed_at"].as_str().unwrap().to_string();
    let lease = v["claim"]["lease_until"].as_str().unwrap().to_string();
    // 5 minute lease: same hour, minutes differ by 5 (or wrap).
    assert_ne!(claimed, lease);
    assert!(
        !t.path("cfg/data/backups").exists(),
        "backup_keep = 0 disables backups"
    );
    let bad = t.write("bad.cfg", "colour = blue\n");
    let mut c = t.bare();
    c.args(["--config", &p(&bad), "--json", "list"]);
    assert_eq!(err_code(&t.exec(c).json), "CONFIG");
}

#[test]
fn database_is_found_in_the_git_common_dir_from_any_worktree() {
    let t = T::new("gitdir");
    // Main checkout: repo/.git is a folder.
    std::fs::create_dir_all(t.path("repo/.git/worktrees/wt1")).unwrap();
    std::fs::create_dir_all(t.path("repo/src")).unwrap();
    // Linked worktree: wt1/.git is a file pointing into repo/.git/worktrees/wt1.
    std::fs::create_dir_all(t.path("wt1")).unwrap();
    let gitdir = t.path("repo/.git/worktrees/wt1");
    t.write("wt1/.git", &format!("gitdir: {}\n", p(&gitdir)));
    t.write("repo/.git/worktrees/wt1/commondir", "../..\n");

    let mut c = t.bare();
    c.current_dir(t.path("repo/src"))
        .args(["--json", "add", "CORE", "from main"]);
    assert_eq!(t.exec(c).code, 0);
    assert!(t.path("repo/.git/ait/tracker.db").is_file());

    let mut c = t.bare();
    c.current_dir(t.path("wt1")).args(["--json", "list"]);
    let o = t.exec(c);
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert_eq!(o.json["issues"][0]["title"], "from main");
}

#[test]
fn help_version_and_guide_work_without_a_database() {
    let t = T::new("help");
    for args in [["help"], ["version"], ["guide"]] {
        let mut c = t.bare();
        c.args(args);
        let o = t.exec(c);
        assert_eq!(o.code, 0);
        assert!(!o.stdout.is_empty());
    }
    assert!(!t.db().exists());
}
