//! Real multi-process concurrency: many `ait` processes at once against one
//! database. Every process is waited on with a timeout.

mod common;
use common::*;
use std::collections::HashSet;

/// Starts all commands first, then collects them, so they really overlap.
fn all_at_once(t: &T, cmds: Vec<std::process::Command>) -> Vec<Out> {
    let children: Vec<_> = cmds.into_iter().map(|c| t.spawn(c)).collect();
    children.into_iter().map(wait).collect()
}

#[test]
fn ten_processes_one_issue_exactly_one_winner_every_round() {
    let t = T::new("race1");
    t.ok(None, &["init"]);
    for round in 0..5 {
        t.ok(None, &["add", "CORE", &format!("round {round}")]);
        let cmds = (0..10)
            .map(|i| t.cmd(Some(&format!("r{round}-agent{i}")), &["next", "--claim"]))
            .collect();
        let outs = all_at_once(&t, cmds);
        let winners: Vec<&Out> = outs.iter().filter(|o| o.code == 0).collect();
        assert_eq!(
            winners.len(),
            1,
            "round {round}: {:?}",
            outs.iter().map(|o| &o.stdout).collect::<Vec<_>>()
        );
        for o in outs.iter().filter(|o| o.code != 0) {
            assert_eq!(o.code, 6, "losers see NO_WORK, got {}", o.stdout);
            assert_eq!(err_code(&o.json), "NO_WORK");
        }
        assert_eq!(winners[0].json["claim"]["generation"], 1);
    }
    // The event log has exactly one claim per issue.
    let act = t.ok(None, &["activity", "--limit", "1000"]);
    let claims = act["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "claim")
        .count();
    assert_eq!(claims, 5);
}

#[test]
fn ten_processes_ten_issues_each_gets_a_different_one() {
    let t = T::new("race10");
    for i in 0..10 {
        t.ok(None, &["add", "CORE", &format!("task {i}")]);
    }
    let cmds = (0..10)
        .map(|i| t.cmd(Some(&format!("agent{i}")), &["next", "--claim"]))
        .collect();
    let outs = all_at_once(&t, cmds);
    let mut got = HashSet::new();
    for o in &outs {
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        assert!(
            got.insert(id(&o.json)),
            "issue handed out twice: {}",
            id(&o.json)
        );
    }
    assert_eq!(got.len(), 10);
    assert_eq!(t.run(Some("late"), &["next", "--claim"]).code, 6);
}

#[test]
fn ten_processes_claiming_the_same_id() {
    let t = T::new("raceid");
    t.ok(None, &["add", "CORE", "contested"]);
    let cmds = (0..10)
        .map(|i| t.cmd(Some(&format!("agent{i}")), &["claim", "CORE-1-00"]))
        .collect();
    let outs = all_at_once(&t, cmds);
    assert_eq!(outs.iter().filter(|o| o.code == 0).count(), 1);
    for o in outs.iter().filter(|o| o.code != 0) {
        assert_eq!(
            (o.code, err_code(&o.json).as_str()),
            (4, "ALREADY_CLAIMED"),
            "{}",
            o.stdout
        );
    }
}

#[test]
fn ten_processes_taking_over_the_same_stale_claim() {
    let t = T::new("racetake");
    t.ok_at(None, T0, &["add", "CORE", "stale one"]);
    t.ok_at(Some("dead"), T0, &["claim", "CORE-1-00"]);
    let later = (T0 + 90 * MIN).to_string();
    let cmds = (0..10)
        .map(|i| {
            let mut c = t.cmd(
                Some(&format!("agent{i}")),
                &["claim", "CORE-1-00", "--take"],
            );
            c.env("AIT_NOW_MS", &later);
            c
        })
        .collect();
    let outs = all_at_once(&t, cmds);
    let winners: Vec<&Out> = outs.iter().filter(|o| o.code == 0).collect();
    assert_eq!(winners.len(), 1);
    assert_eq!(winners[0].json["claim"]["generation"], 2);
    for o in outs.iter().filter(|o| o.code != 0) {
        assert_eq!(err_code(&o.json), "ALREADY_CLAIMED", "{}", o.stdout);
    }
}

#[test]
fn writes_during_reads_all_succeed() {
    let t = T::new("rw");
    t.ok(None, &["add", "CORE", "seed"]);
    let mut cmds = Vec::new();
    for w in 0..5 {
        for k in 0..6 {
            cmds.push(t.cmd(
                Some(&format!("w{w}")),
                &["add", "CORE", &format!("w{w} item {k}")],
            ));
        }
    }
    for _ in 0..10 {
        cmds.push(t.cmd(None, &["list"]));
        cmds.push(t.cmd(None, &["board"]));
    }
    let outs = all_at_once(&t, cmds);
    for o in &outs {
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    }
    assert_eq!(t.ok(None, &["list"])["count"], 31);
    let check = t.run(None, &["check"]);
    assert_eq!(check.code, 0, "{}", check.stdout);
}

#[test]
fn many_processes_creating_a_new_database_at_once() {
    let t = T::new("fresh");
    let cmds = (0..10)
        .map(|i| {
            t.cmd(
                Some(&format!("a{i}")),
                &["add", "CORE", &format!("first {i}")],
            )
        })
        .collect();
    let outs = all_at_once(&t, cmds);
    for o in &outs {
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    }
    let ids: HashSet<String> = outs.iter().map(|o| id(&o.json)).collect();
    assert_eq!(ids.len(), 10, "ids must be unique");
}
