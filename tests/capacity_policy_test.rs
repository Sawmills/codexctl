#![cfg(feature = "central-prototype")]
use codexctl::central::{
    capacity::{CapacityAccount, CapacityRecovery, Evidence, Outcome, PlanRequest},
    managed::Account,
    storage::{CentralStore, StoreMode},
};
const MODEL: &str = "gpt-6.1-sol";
const SESSION: &str = "019e8489-aa28-7071-ab90-16b81c7cfd1d";
fn accounts() -> Vec<CapacityAccount> {
    ["a", "b", "c", "d", "e", "paid", "excluded", "stale"]
        .into_iter()
        .enumerate()
        .map(|(i, alias)| {
            let account: Account = serde_json::from_value(serde_json::json!({
        "userId":"test", "alias":alias, "accountId":format!("seat-{alias}"), "plan":"pro",
        "billingClass": if alias == "paid" {"usage_based"} else {"rate_limited"},
        "primaryUsed":i, "secondaryUsed":0, "resetsAt":9999999,
        "available":alias!="excluded", "usageScore":i, "usageStale":alias=="stale", "liveSessions":0
    })).unwrap();
            CapacityAccount {
                account,
                identity: identity(alias),
            }
        })
        .collect()
}
fn identity(alias: &str) -> codexctl::central::loans::CredentialSubject {
    codexctl::central::loans::CredentialSubject {
        workspace: "shared-workspace".into(),
        uid: None,
        sub: Some(alias.into()),
    }
}
fn evidence(alias: &str, model: &str, id: &str, outcome: Outcome) -> Evidence {
    Evidence {
        alias: alias.into(),
        model: model.into(),
        event_id: id.into(),
        outcome,
        session_id: Some(SESSION.into()),
        observed_at: None,
        identity: Some(identity(alias)),
    }
}
fn request(model: &str, failed: Option<&str>) -> PlanRequest {
    PlanRequest {
        model: model.into(),
        session_id: failed.map(|_| SESSION.into()),
        failed_alias: failed.map(str::to_owned),
        tried: failed.into_iter().map(str::to_owned).collect(),
        recent_429_rates: Default::default(),
    }
}
async fn fixture() -> (tempfile::TempDir, CentralStore) {
    let root = tempfile::tempdir().unwrap();
    let store = CentralStore::from_mode(StoreMode::File, root.path(), &root.path().join("key"))
        .await
        .unwrap();
    (root, store)
}
async fn overload(policy: &CapacityRecovery, alias: &str, at: i64) {
    for n in 0..3 {
        policy
            .observe(
                evidence(
                    alias,
                    MODEL,
                    &format!("{alias}-{at}-{n}"),
                    Outcome::Overloaded,
                ),
                at + n,
            )
            .await
            .unwrap();
    }
    policy
        .observe(
            evidence(
                alias,
                MODEL,
                &format!("{alias}-terminal-{at}"),
                Outcome::Terminal,
            ),
            at + 3,
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn cooldown_counts_distinct_requests_and_is_exactly_model_scoped() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    let one = evidence("a", MODEL, "one", Outcome::Overloaded);
    for _ in 0..4 {
        p.observe(one.clone(), 1000).await.unwrap();
    }
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1001)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    p.observe(evidence("a", MODEL, "two", Outcome::Overloaded), 1001)
        .await
        .unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1002)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    p.observe(evidence("a", MODEL, "three", Outcome::Overloaded), 1002)
        .await
        .unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1003)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        p.plan(request("gpt-6-sol", None), accounts(), 1003)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    p.observe(one, 1299).await.unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1301)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1302)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
}
#[tokio::test]
async fn success_clears_streak_and_requests_outside_window_do_not_cool() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    for at in [1000, 1121, 1242] {
        p.observe(
            evidence("a", MODEL, &format!("request-{at}"), Outcome::Overloaded),
            at,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1243)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    overload(&p, "a", 1300).await;
    p.observe(evidence("a", MODEL, "success", Outcome::Completed), 1305)
        .await
        .unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1306)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        p.plan(request(MODEL, Some("a")), accounts(), 1306)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("evidence_pending")
    );
}
#[tokio::test]
async fn only_correlated_terminal_capacity_can_move_and_billed_accounts_never_enter() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    for i in 0..3 {
        p.observe(
            evidence("a", MODEL, &format!("request-{i}"), Outcome::Overloaded),
            1000 + i,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        p.plan(request(MODEL, Some("a")), accounts(), 1003)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("evidence_pending")
    );
    p.observe(evidence("a", MODEL, "terminal", Outcome::Terminal), 1004)
        .await
        .unwrap();
    let mut r = request(MODEL, Some("a"));
    r.session_id = Some("119e8489-aa28-7071-ab90-16b81c7cfd1d".into());
    assert!(p.plan(r, accounts(), 1004).await.unwrap().alias.is_none());
    let only_ineligible = accounts()
        .into_iter()
        .filter(|a| {
            matches!(
                a.account.alias.as_str(),
                "a" | "paid" | "excluded" | "stale"
            )
        })
        .collect();
    assert_eq!(
        p.plan(request(MODEL, Some("a")), only_ineligible, 1005)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("no_included_alternate")
    );
    assert_eq!(
        p.plan(request(MODEL, Some("a")), accounts(), 1005)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
}
#[tokio::test]
async fn session_budget_and_tried_accounts_survive_new_launcher_and_duplicate_plans() {
    let (_root, store) = fixture().await;
    for (i, alias) in ["a", "b", "c", "d"].into_iter().enumerate() {
        let p = CapacityRecovery::new(store.clone(), "test".into());
        let now = 1000 + (i as i64) * 10;
        overload(&p, alias, now).await;
        for _ in 0..2 {
            let plan = p
                .plan(request(MODEL, Some(alias)), accounts(), now + 4)
                .await
                .unwrap();
            if i < 3 {
                assert_eq!(plan.alias.as_deref(), Some(["b", "c", "d"][i]));
            } else {
                assert_eq!(plan.reason.as_deref(), Some("recovery_budget"));
            }
        }
    }
}
#[tokio::test]
async fn all_accounts_cooling_stops_and_the_same_accounts_share_cooldowns_across_users() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store.clone(), "test".into());
    for alias in ["a", "b", "c", "d", "e"] {
        overload(&p, alias, 1000).await;
    }
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1004)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("no_included_alternate")
    );
    let other = CapacityRecovery::new(store, "other".into());
    assert_eq!(
        other
            .plan(request(MODEL, None), accounts(), 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        None
    );
    assert!(
        p.observe(evidence("a", "other", "bad", Outcome::Overloaded), 1004)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn account_rename_and_borrowed_reference_keep_the_same_cooldown() {
    let (_root, store) = fixture().await;
    let owner = CapacityRecovery::new(store.clone(), "owner".into());
    overload(&owner, "a", 1000).await;
    let mut renamed = accounts();
    renamed[0].account.alias = "renamed".into();
    assert_eq!(
        owner
            .plan(request(MODEL, None), renamed, 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    let borrower = CapacityRecovery::new(store, "borrower".into());
    let mut borrowed = accounts();
    borrowed[0].account.alias = "owner/a".into();
    assert_eq!(
        borrower
            .plan(request(MODEL, None), borrowed, 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
}

#[tokio::test]
async fn selected_identity_survives_uid_upgrade_on_an_idempotent_plan() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    overload(&p, "a", 1000).await;
    assert_eq!(
        p.plan(request(MODEL, Some("a")), accounts(), 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    let mut upgraded = accounts();
    upgraded[1].identity.uid = Some("uid-added-to-b".into());
    assert_eq!(
        p.plan(request(MODEL, Some("a")), upgraded.clone(), 1005)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    upgraded[1].identity.sub = None;
    assert_eq!(
        p.plan(request(MODEL, Some("a")), upgraded, 1006)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
}

#[tokio::test]
async fn success_from_another_machine_clears_interleaved_shared_failures() {
    let (_root, store) = fixture().await;
    let a = CapacityRecovery::new(store.clone(), "test".into());
    let b = CapacityRecovery::new(store, "test".into());
    a.observe(
        evidence("a", MODEL, "success-before-failure", Outcome::Completed),
        1000,
    )
    .await
    .unwrap();
    for n in 1..=2 {
        b.observe(
            evidence("a", MODEL, &format!("failure-{n}"), Outcome::Overloaded),
            1000 + n,
        )
        .await
        .unwrap();
    }
    a.observe(
        evidence("a", MODEL, "success-between-failures", Outcome::Completed),
        1003,
    )
    .await
    .unwrap();
    b.observe(
        evidence("a", MODEL, "third-failure", Outcome::Overloaded),
        1004,
    )
    .await
    .unwrap();
    assert_eq!(
        a.plan(request(MODEL, None), accounts(), 1005)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
}

#[tokio::test]
async fn success_clears_failure_streak_even_when_deduplication_is_full() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    for n in 0..253 {
        p.observe(
            evidence("a", MODEL, &format!("failure-{n}"), Outcome::Overloaded),
            1000,
        )
        .await
        .unwrap();
        p.observe(
            evidence("a", MODEL, &format!("completed-{n}"), Outcome::Completed),
            1000,
        )
        .await
        .unwrap();
    }
    for n in 253..256 {
        p.observe(
            evidence("a", MODEL, &format!("failure-{n}"), Outcome::Overloaded),
            1000,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1001)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    p.observe(
        evidence("a", MODEL, "completed-after-bound", Outcome::Completed),
        1002,
    )
    .await
    .unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1003)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
}

#[tokio::test]
async fn expired_duplicate_reports_cannot_restart_a_cooldown_and_new_requests_can() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    overload(&p, "a", 1000).await;
    let mut old = evidence("a", MODEL, "a-1000-0", Outcome::Overloaded);
    old.observed_at = Some(1000);
    assert!(p.observe(old, 1304).await.is_err());
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1304)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    overload(&p, "a", 1305).await;
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1309)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
}

#[tokio::test]
async fn a_duplicate_plan_respects_accounts_excluded_by_earlier_usage_recovery() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    overload(&p, "a", 1000).await;
    assert_eq!(
        p.plan(request(MODEL, Some("a")), accounts(), 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    let mut retry = request(MODEL, Some("a"));
    retry.tried.insert(0, "b".into());
    assert_eq!(
        p.plan(retry, accounts(), 1005)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("alternate_unavailable")
    );
}

#[tokio::test]
async fn reordered_reports_preserve_failures_after_the_latest_success() {
    let (_root, store) = fixture().await;
    let a = CapacityRecovery::new(store.clone(), "test".into());
    let b = CapacityRecovery::new(store, "test".into());
    let observed = |id: &str, outcome, at| {
        let mut report = evidence("a", MODEL, id, outcome);
        report.observed_at = Some(at);
        report
    };
    // A success that arrives after a newer streak cannot clear that streak.
    for at in 1001..=1003 {
        b.observe(
            observed(&format!("failure-{at}"), Outcome::Overloaded, at),
            at,
        )
        .await
        .unwrap();
    }
    b.observe(observed("terminal", Outcome::Terminal, 1003), 1003)
        .await
        .unwrap();
    a.observe(observed("delayed-success", Outcome::Completed, 1000), 1004)
        .await
        .unwrap();
    assert_eq!(
        a.plan(request(MODEL, None), accounts(), 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        a.plan(request(MODEL, Some("a")), accounts(), 1004)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    a.observe(observed("new-success", Outcome::Completed, 1005), 1005)
        .await
        .unwrap();
    // Older failures and terminal reports must not re-enter after a success.
    for at in 1001..=1003 {
        b.observe(
            observed(&format!("delayed-failure-{at}"), Outcome::Overloaded, at),
            1006,
        )
        .await
        .unwrap();
    }
    b.observe(observed("delayed-terminal", Outcome::Terminal, 1003), 1006)
        .await
        .unwrap();
    assert_eq!(
        a.plan(request(MODEL, None), accounts(), 1006)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        a.plan(request(MODEL, Some("a")), accounts(), 1006)
            .await
            .unwrap()
            .reason
            .as_deref(),
        Some("evidence_pending")
    );
    // A duplicated older completion cannot erase the next cooldown either.
    for at in 1007..=1009 {
        b.observe(
            observed(&format!("new-failure-{at}"), Outcome::Overloaded, at),
            at,
        )
        .await
        .unwrap();
    }
    a.observe(observed("delayed-success", Outcome::Completed, 1000), 1010)
        .await
        .unwrap();
    assert_eq!(
        a.plan(request(MODEL, None), accounts(), 1010)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
}

#[tokio::test]
async fn success_watermark_exists_even_before_the_first_failure() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    p.observe(
        evidence("a", MODEL, "first-success", Outcome::Completed),
        1004,
    )
    .await
    .unwrap();
    for at in 1000..=1002 {
        let mut report = evidence("a", MODEL, &format!("old-{at}"), Outcome::Overloaded);
        report.observed_at = Some(at);
        p.observe(report, 1005).await.unwrap();
    }
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1006)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
}

#[tokio::test]
async fn delayed_success_preserves_later_failures_observed_during_cooling() {
    let (_root, store) = fixture().await;
    let p = CapacityRecovery::new(store, "test".into());
    overload(&p, "a", 1000).await;
    for at in 1010..=1012 {
        p.observe(
            evidence(
                "a",
                MODEL,
                &format!("cooling-failure-{at}"),
                Outcome::Overloaded,
            ),
            at,
        )
        .await
        .unwrap();
    }
    let mut success = evidence("a", MODEL, "delayed-success", Outcome::Completed);
    success.observed_at = Some(1005);
    p.observe(success, 1013).await.unwrap();
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1013)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("b")
    );
    // Retaining these failures must not extend the original 300-second deadline.
    assert_eq!(
        p.plan(request(MODEL, None), accounts(), 1302)
            .await
            .unwrap()
            .alias
            .as_deref(),
        Some("a")
    );
}
