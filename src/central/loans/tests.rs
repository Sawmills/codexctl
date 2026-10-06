use super::*;
use crate::{
    api,
    central::managed::{Account, User},
};

fn user(id: &str, email: &str) -> User {
    User {
        id: id.into(),
        email: email.into(),
        enabled: true,
        oidc_identity: None,
    }
}

fn request<'a>(lender: &'a User, borrower: Option<&'a User>) -> GrantRequest<'a> {
    GrantRequest {
        lender,
        borrower,
        alias: "Main",
        account_id: "key",
        subject: "subject",
        weekly_reset: Some(10_000),
        until: None,
        now: 1_000,
    }
}

#[test]
fn account_ref_keeps_owned_aliases_strict_and_parses_borrowed_references() {
    assert_eq!(
        AccountRef::parse("main").unwrap(),
        AccountRef::Owned("main".into())
    );
    assert_eq!(
        AccountRef::parse("Alice/Main").unwrap(),
        AccountRef::Borrowed {
            lender: "alice".into(),
            alias: "main".into()
        }
    );
    for bad in ["a/b/c", "/main", "alice/", "../x", "alice/..", "a\\b"] {
        assert!(AccountRef::parse(bad).is_err(), "{bad} must be refused");
    }
    assert!(crate::store::validate_alias("alice/main").is_err());
}

#[test]
fn borrowed_connection_files_stay_outside_the_owned_namespace() {
    let root = Path::new("/root");
    assert_eq!(
        AccountRef::parse("main").unwrap().connection_file(root),
        root.join("main.json")
    );
    assert_eq!(
        AccountRef::parse("alice/main")
            .unwrap()
            .connection_file(root),
        root.join("borrowed/alice/main.json")
    );
}

#[test]
fn grant_defaults_to_the_weekly_reset_and_stores_a_lowercase_reference() {
    let lender = user("lender", "Alice.Smith@sawmills.ai");
    let borrower = user("borrower", "bob@sawmills.ai");
    let grant = plan_grant(request(&lender, Some(&borrower)), "id".into()).unwrap();
    assert_eq!(grant.ends_at, 10_000);
    assert_eq!(grant.reference, "alice.smith/main");
    assert_eq!(grant.alias, "Main");
    assert!(grant.active(9_999));
    assert!(!grant.active(10_000));
}

#[test]
fn grant_rules_refuse_each_invalid_request() {
    let lender = user("lender", "alice@sawmills.ai");
    let borrower = user("borrower", "bob@sawmills.ai");
    let mut disabled = borrower.clone();
    disabled.enabled = false;
    let cases: Vec<(GrantRequest<'_>, &str)> = vec![
        (request(&lender, None), "borrower_not_found"),
        (request(&lender, Some(&lender)), "self_loan"),
        (request(&lender, Some(&disabled)), "borrower_disabled"),
        (
            GrantRequest {
                weekly_reset: None,
                ..request(&lender, Some(&borrower))
            },
            "weekly_reset_unknown",
        ),
        (
            GrantRequest {
                weekly_reset: Some(999),
                ..request(&lender, Some(&borrower))
            },
            "weekly_reset_unknown",
        ),
        (
            GrantRequest {
                until: Some(10_001),
                ..request(&lender, Some(&borrower))
            },
            "loan_end_after_weekly_reset",
        ),
        (
            GrantRequest {
                until: Some(1_000),
                ..request(&lender, Some(&borrower))
            },
            "loan_end_in_past",
        ),
    ];
    for (request, reason) in cases {
        assert_eq!(plan_grant(request, "id".into()).unwrap_err(), reason);
    }
    let early = plan_grant(
        GrantRequest {
            until: Some(5_000),
            ..request(&lender, Some(&borrower))
        },
        "id".into(),
    )
    .unwrap();
    assert_eq!(early.ends_at, 5_000);
}

#[test]
fn references_resolve_only_through_the_borrowers_active_grants() {
    let lender = user("lender", "alice@sawmills.ai");
    let borrower = user("borrower", "bob@sawmills.ai");
    let grant = plan_grant(request(&lender, Some(&borrower)), "one".into()).unwrap();
    let mut ended = grant.clone();
    ended.id = "old".into();
    ended.end(1_500, "lender", EndReason::Revoked);
    let grants = vec![ended, grant.clone()];
    assert_eq!(
        match_reference(&grants, "borrower", "ALICE/main", 2_000).unwrap(),
        Some(&grant)
    );
    assert_eq!(
        match_reference(&grants, "other", "alice/main", 2_000).unwrap(),
        None
    );
    assert_eq!(
        match_reference(&grants, "borrower", "alice/main", 10_000).unwrap(),
        None
    );
    let mut twin = grant.clone();
    twin.id = "two".into();
    twin.account_id = "other-key".into();
    let grants = vec![grant, twin];
    assert_eq!(
        match_reference(&grants, "borrower", "alice/main", 2_000).unwrap_err(),
        "ambiguous_loan"
    );
}

#[test]
fn token_issue_events_coalesce_per_machine_and_hour() {
    let first = AuditEvent::token_issued(7_200, "grant", "machine");
    let later = AuditEvent::token_issued(10_799, "grant", "machine");
    let next_hour = AuditEvent::token_issued(10_800, "grant", "machine");
    let other = AuditEvent::token_issued(7_200, "grant", "other");
    assert_eq!(first.coalesce_key, later.coalesce_key);
    assert_ne!(first.coalesce_key, next_hour.coalesce_key);
    assert_ne!(first.coalesce_key, other.coalesce_key);
}

fn account(alias: &str, used: f64, loan: bool) -> Account {
    serde_json::from_value(serde_json::json!({
        "userId": "borrower",
        "alias": alias,
        "label": null,
        "accountId": "seat",
        "plan": "pro",
        "billingClass": "rate_limited",
        "primaryUsed": used,
        "secondaryUsed": null,
        "primaryWindowSeconds": 18000,
        "secondaryWindowSeconds": null,
        "primaryResetsAt": null,
        "resetsAt": null,
        "available": true,
        "usageScore": used,
        "loan": loan.then(|| serde_json::json!({"id":"g","lenderEmail":"alice@sawmills.ai","endsAt":1})),
    }))
    .unwrap()
}

fn first(accounts: &[Account]) -> Result<String> {
    accounts
        .iter()
        .find(|a| a.billing_class == api::BillingClass::RateLimited)
        .map(|a| a.alias.clone())
        .ok_or_else(|| anyhow::anyhow!("none"))
}

#[test]
fn selection_ranks_owned_accounts_before_borrowed_ones() {
    let accounts = vec![
        account("alice/main", 10.0, true),
        account("own", 90.0, false),
    ];
    assert_eq!(owned_then_borrowed(&accounts, first).unwrap(), "own");
    let only_borrowed = vec![account("alice/main", 10.0, true)];
    assert_eq!(
        owned_then_borrowed(&only_borrowed, first).unwrap(),
        "alice/main"
    );
}

#[test]
fn selection_skips_a_borrowed_account_at_the_backoff() {
    let accounts = vec![account("alice/main", 95.0, true)];
    assert_eq!(
        owned_then_borrowed(&accounts, first)
            .unwrap_err()
            .to_string(),
        "none"
    );
    assert!(below_borrower_backoff(&account("alice/main", 94.9, true)));
    assert!(!below_borrower_backoff(&account("alice/main", 95.0, true)));
}

#[test]
fn pause_events_coalesce_per_reason_and_hour() {
    let first = AuditEvent::paused(7_200, "grant", "lender_disabled");
    assert_eq!(first.kind, AuditKind::Paused);
    assert_eq!(
        first.coalesce_key,
        AuditEvent::paused(10_799, "grant", "lender_disabled").coalesce_key
    );
    assert_ne!(
        first.coalesce_key,
        AuditEvent::paused(7_200, "grant", "subject_changed").coalesce_key
    );
}
