//! Claims, leases, generations, takeover and done.

mod common;
use common::*;

fn setup(t: &T) {
    t.ok_at(None, T0, &["add", "CORE", "first"]);
    t.ok_at(None, T0, &["add", "CORE", "second"]);
}

#[test]
fn claim_conflicts_and_renewal() {
    let t = T::new("claim");
    setup(&t);
    let v = t.ok_at(Some("a1"), T0, &["claim", "CORE-1-00"]);
    assert_eq!(v["claim"]["generation"], 1);
    assert_eq!(v["claim"]["session"], "a1");
    assert_eq!(v["claim"]["status"], "live");
    assert_eq!(v["branch"], "task/CORE-1-00");
    let (code, e) = t.err_at(Some("a2"), T0, &["claim", "CORE-1-00"]);
    assert_eq!((code, e.as_str()), (4, "ALREADY_CLAIMED"));
    // Claiming again as the holder renews, same generation.
    let v = t.ok_at(Some("a1"), T0 + 10 * MIN, &["claim", "CORE-1-00"]);
    assert_eq!(v["renewed"], true);
    assert_eq!(v["claim"]["generation"], 1);
    // `--take` cannot steal a live claim.
    assert_eq!(
        t.err_at(Some("a2"), T0, &["claim", "CORE-1-00", "--take"])
            .1,
        "ALREADY_CLAIMED"
    );
}

#[test]
fn claim_commands_need_a_session() {
    let t = T::new("nosession");
    setup(&t);
    assert_eq!(
        t.err(None, &["next", "--claim"]),
        (2, "NO_SESSION".to_string())
    );
    assert_eq!(t.err(None, &["claim", "CORE-1-00"]).1, "NO_SESSION");
    assert_eq!(t.err(Some("bad name"), &["list"]).1, "USAGE");
}

#[test]
fn next_claim_respects_order_needs_and_reports_no_work() {
    let t = T::new("next");
    t.ok_at(None, T0, &["add", "CORE", "base"]);
    t.ok_at(
        None,
        T0,
        &["add", "CORE", "dependent", "--needs", "CORE-1-00"],
    );
    t.ok_at(None, T0, &["add", "CORE", "idea", "--state", "idea"]);
    let v = t.ok_at(Some("a1"), T0, &["next", "--claim"]);
    assert_eq!(id(&v), "CORE-1-00");
    let o = t.run_at(Some("a2"), T0, &["next", "--claim"]);
    assert_eq!(o.code, 6, "dependent is blocked by its need: {}", o.stdout);
    assert_eq!(err_code(&o.json), "NO_WORK");
    assert_eq!(o.json["error"]["retryable"], true);
    let ex = t.ok_at(None, T0, &["next", "--explain", "CORE-1-01"]);
    assert_eq!(ex["ready"], false);
    assert_eq!(ex["blockers"][0]["code"], "NEEDS");
    t.ok_at(
        Some("a1"),
        T0,
        &["done", "CORE-1-00", "--summary", "base built", "--gen", "1"],
    );
    assert_eq!(
        id(&t.ok_at(Some("a2"), T0, &["next", "--claim"])),
        "CORE-1-01"
    );
}

#[test]
fn a_dropped_need_keeps_blocking() {
    let t = T::new("dropped");
    t.ok_at(None, T0, &["add", "CORE", "base"]);
    t.ok_at(
        None,
        T0,
        &["add", "CORE", "dependent", "--needs", "CORE-1-00"],
    );
    t.ok_at(
        None,
        T0,
        &["state", "CORE-1-00", "dropped", "--reason", "not needed"],
    );
    assert_eq!(t.run_at(Some("a1"), T0, &["next", "--claim"]).code, 6);
}

#[test]
fn expired_lease_takeover_and_stale_generation_refusal() {
    let t = T::new("takeover");
    setup(&t);
    t.ok_at(Some("old"), T0, &["claim", "CORE-1-00"]);
    t.ok_at(
        Some("old"),
        T0 + MIN,
        &[
            "handoff",
            "CORE-1-00",
            "half done; next: tests",
            "--gen",
            "1",
        ],
    );
    let later = T0 + 120 * MIN;
    // The claim is stale now: not offered, and a plain claim is refused.
    let show = t.ok_at(None, later, &["show", "CORE-1-00"]);
    assert_eq!(show["claim"]["status"], "stale");
    assert_eq!(show["blockers"][0]["code"], "STALE_CLAIM");
    assert_eq!(
        id(&t.ok_at(Some("new"), later, &["next", "--claim"])),
        "CORE-1-01"
    );
    assert_eq!(
        t.err_at(Some("new2"), later, &["claim", "CORE-1-00"]).1,
        "CLAIM_STALE"
    );
    // Takeover bumps the generation and hands over the previous handoff.
    let v = t.ok_at(Some("new2"), later, &["claim", "CORE-1-00", "--take"]);
    assert_eq!(v["claim"]["generation"], 2);
    assert_eq!(v["taken_from"]["session"], "old");
    assert_eq!(v["latest_handoff"]["body"], "half done; next: tests");
    // The old agent wakes up: every write with its old generation is refused.
    let (code, e) = t.err_at(
        Some("old"),
        later,
        &["done", "CORE-1-00", "--summary", "x", "--gen", "1"],
    );
    assert_eq!((code, e.as_str()), (4, "STALE_GENERATION"));
    assert_eq!(
        t.err_at(Some("old"), later, &["release", "CORE-1-00", "--gen", "1"])
            .1,
        "STALE_GENERATION"
    );
    assert_eq!(
        t.err_at(
            Some("old"),
            later,
            &["note", "CORE-1-00", "mine!", "--gen", "1"]
        )
        .1,
        "STALE_GENERATION"
    );
    assert_eq!(
        t.err_at(
            Some("old"),
            later,
            &["handoff", "CORE-1-00", "x", "--gen", "1"]
        )
        .1,
        "STALE_GENERATION"
    );
    assert_eq!(
        t.err_at(Some("old"), later, &["verify", "CORE-1-00", "--gen", "1"])
            .1,
        "STALE_GENERATION"
    );
    // Without --gen it is still not the claimant.
    assert_eq!(
        t.err_at(Some("old"), later, &["done", "CORE-1-00", "--summary", "x"])
            .1,
        "NOT_CLAIMANT"
    );
    assert_eq!(
        t.err_at(Some("old"), later, &["release", "CORE-1-00"]).1,
        "NOT_CLAIMANT"
    );
    // The new holder is unaffected.
    let v = t.ok_at(Some("new2"), later, &["verify", "CORE-1-00", "--gen", "2"]);
    assert_eq!(v["ok"], true);
    t.ok_at(
        Some("new2"),
        later,
        &["done", "CORE-1-00", "--summary", "finished", "--gen", "2"],
    );
}

#[test]
fn next_take_stale_claims_expired_work() {
    let t = T::new("takestale");
    t.ok_at(None, T0, &["add", "CORE", "only"]);
    t.ok_at(Some("old"), T0, &["next", "--claim"]);
    let later = T0 + 61 * MIN;
    assert_eq!(t.run_at(Some("new"), later, &["next", "--claim"]).code, 6);
    let v = t.ok_at(Some("new"), later, &["next", "--claim", "--take-stale"]);
    assert_eq!(v["claim"]["generation"], 2);
    assert_eq!(v["taken_from"]["session"], "old");
}

#[test]
fn any_call_by_the_claimant_renews_the_lease() {
    let t = T::new("renew");
    setup(&t);
    t.ok_at(Some("a1"), T0, &["claim", "CORE-1-00"]);
    // At +50 min a read-only call by the claimant renews the 60 min lease.
    t.ok_at(Some("a1"), T0 + 50 * MIN, &["board"]);
    let v = t.ok_at(None, T0 + 100 * MIN, &["show", "CORE-1-00"]);
    assert_eq!(v["claim"]["status"], "live");
    // Heartbeat lists the session's claims.
    let hb = t.ok_at(Some("a1"), T0 + 105 * MIN, &["heartbeat"]);
    assert_eq!(hb["claims"][0]["issue"], "CORE-1-00");
    assert_eq!(
        t.err_at(Some("a2"), T0 + 105 * MIN, &["heartbeat", "CORE-1-00"])
            .1,
        "NOT_CLAIMANT"
    );
    // Without calls the lease runs out.
    let v = t.ok_at(None, T0 + 200 * MIN, &["show", "CORE-1-00"]);
    assert_eq!(v["claim"]["status"], "stale");
}

#[test]
fn release_and_done_rules() {
    let t = T::new("release");
    setup(&t);
    t.ok_at(Some("a1"), T0, &["claim", "CORE-1-00"]);
    assert_eq!(
        t.err_at(Some("a2"), T0, &["release", "CORE-1-00"]).1,
        "NOT_CLAIMANT"
    );
    assert_eq!(
        t.err_at(None, T0, &["release", "CORE-1-00"]).1,
        "NOT_CLAIMANT"
    );
    t.ok_at(None, T0, &["release", "CORE-1-00", "--force"]);
    assert_eq!(
        t.err_at(Some("a1"), T0, &["release", "CORE-1-00"]).1,
        "NOT_CLAIMED"
    );
    // Released issues are offered again, with a new generation.
    let v = t.ok_at(Some("a2"), T0, &["next", "--claim"]);
    assert_eq!(id(&v), "CORE-1-00");
    assert_eq!(v["claim"]["generation"], 2);
    assert_eq!(t.err_at(Some("a2"), T0, &["done", "CORE-1-00"]).1, "USAGE");
    // Done needs the generation of the live claim (owner rule, 2026-11-26).
    assert_eq!(
        t.err_at(Some("a2"), T0, &["done", "CORE-1-00", "--summary", "x"])
            .1,
        "USAGE"
    );
    assert_eq!(
        t.err_at(
            Some("a2"),
            T0,
            &["done", "CORE-1-00", "--summary", "x", "--gen", "1"]
        )
        .1,
        "STALE_GENERATION"
    );
    let v = t.ok_at(
        Some("a2"),
        T0,
        &[
            "done",
            "CORE-1-00",
            "--summary",
            "delivered X",
            "--gen",
            "2",
        ],
    );
    assert_eq!(v["issue"]["state"], "done");
    assert_eq!(v["issue"]["summary"], "delivered X");
    assert_eq!(
        t.err_at(Some("a2"), T0, &["done", "CORE-1-00", "--summary", "again"])
            .1,
        "ALREADY_DONE"
    );
    assert_eq!(
        t.err_at(Some("a2"), T0, &["claim", "CORE-1-00"]).1,
        "INVALID_STATE"
    );
    let show = t.ok_at(None, T0, &["show", "CORE-1-00"]);
    assert_eq!(show["claim"]["status"], "released");
    // Closing an unclaimed issue needs --force (owner rule, 2026-11-26).
    assert_eq!(
        t.err_at(None, T0, &["done", "CORE-1-01", "--summary", "closed"])
            .1,
        "NOT_CLAIMED"
    );
    t.ok_at(
        None,
        T0,
        &[
            "done",
            "CORE-1-01",
            "--summary",
            "closed by owner",
            "--force",
        ],
    );
}

#[test]
fn approvals_file_gates_what_next_offers() {
    let t = T::new("approvals");
    let cfg = t.write(
        "default.cfg",
        "approvals = approvals.json\nbackup_keep = 0\n",
    );
    t.write(
        "approvals.json",
        r#"{"approvals": [{"match": "CORE-1", "scope": "build", "exclude": ["CORE-1-00"]}]}"#,
    );
    let run = |session: Option<&str>, args: &[&str]| {
        let mut c = t.cmd(session, &[]);
        c.args(["--config", &p(&cfg)]).args(args);
        t.exec(c)
    };
    assert_eq!(run(None, &["add", "CORE", "excluded"]).code, 0);
    assert_eq!(run(None, &["add", "CORE", "approved"]).code, 0);
    assert_eq!(run(None, &["add", "CORE-2", "other group"]).code, 0);
    let o = run(Some("a1"), &["next", "--claim"]);
    assert_eq!(o.json["issue"]["id"], "CORE-1-01");
    assert_eq!(run(Some("a2"), &["next", "--claim"]).code, 6);
    let ex = run(None, &["next", "--explain", "CORE-2-00"]);
    assert_eq!(ex.json["blockers"][0]["code"], "NOT_APPROVED");
    // A broken approvals file is an error, never "everything approved".
    t.write("approvals.json", "{ not json");
    assert_eq!(
        err_code(&run(Some("a3"), &["next", "--claim"]).json),
        "CONFIG"
    );
}

/// `claim --lease` sets this claim's own lease length; renewals (any call by
/// the session, heartbeat) keep it instead of falling back to lease_minutes.
#[test]
fn a_claim_can_carry_its_own_lease() {
    let t = T::new("lease");
    t.ok_at(None, T0, &["add", "CORE", "long decision"]);
    t.ok_at(None, T0, &["add", "CORE", "normal"]);
    const DAY: i64 = 24 * 60 * MIN;
    let v = t.ok_at(Some("owner"), T0, &["claim", "CORE-1-00", "--lease", "2d"]);
    assert_eq!(v["claim"]["lease_until"], "2026-11-28T08:00:00Z", "{v}");
    assert_eq!(v["claim"]["lease_minutes"], 2880);
    // Another claim by the same session uses the default. Renewing the
    // session's leases keeps the first one at two days from now.
    let v = t.ok_at(Some("owner"), T0 + 5 * MIN, &["claim", "CORE-1-01"]);
    assert!(v["claim"]["lease_minutes"].is_null(), "{v}");
    let v = t.ok_at(Some("owner"), T0 + 10 * MIN, &["show", "CORE-1-00"]);
    assert_eq!(v["claim"]["lease_until"], "2026-11-28T08:10:00Z", "{v}");
    // A heartbeat a day later renews to two days from then.
    let v = t.ok_at(Some("owner"), T0 + DAY, &["heartbeat"]);
    let c0 = v["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["issue"] == "CORE-1-00")
        .unwrap();
    assert_eq!(c0["lease_until"], "2026-11-29T08:00:00Z", "{v}");
    // The other claim (default 60 minutes) had already expired and stays so.
    let c1 = v["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["issue"] == "CORE-1-01")
        .unwrap();
    assert_eq!(c1["status"], "stale", "{v}");
    // Others still cannot take a live long claim.
    assert_eq!(
        t.err_at(Some("a1"), T0 + DAY, &["claim", "CORE-1-00"]).1,
        "ALREADY_CLAIMED"
    );
    // Limits: 1 minute to 30 days, in a duration format.
    for bad in ["0m", "31d", "soon"] {
        let (code, _) = t.err_at(
            Some("owner"),
            T0 + DAY,
            &["claim", "CORE-1-01", "--lease", bad],
        );
        assert_eq!(code, 2, "{bad}");
    }
}
