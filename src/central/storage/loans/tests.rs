use super::*;
use crate::central::loans::{AuditKind, RETENTION_SECONDS};

fn grant(id: &str, account: &str, ends_at: i64) -> Grant {
    Grant {
        id: id.into(),
        account_id: account.into(),
        lender: "lender".into(),
        borrower: "borrower".into(),
        lender_email: "alice@sawmills.ai".into(),
        borrower_email: "bob@sawmills.ai".into(),
        alias: "main".into(),
        reference: "alice/main".into(),
        subject: "subject".into(),
        created_at: 1_000,
        ends_at,
        ended_at: None,
        ended_by: None,
        end_reason: None,
    }
}

/// One scenario for every backend: one active grant per account, a single
/// end, expiry, coalescing, and the 90-day prune.
async fn scenario(store: &CentralStore, prefix: &str) {
    let id = |name: &str| format!("{prefix}-{name}");
    let account = id("account");
    assert!(
        store
            .create_loan(&grant(&id("one"), &account, 5_000))
            .await
            .unwrap()
    );
    assert!(
        !store
            .create_loan(&grant(&id("two"), &account, 5_000))
            .await
            .unwrap(),
        "an account has at most one active grant"
    );
    let ended = store
        .end_loan(&id("one"), 2_000, "borrower", EndReason::Returned)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (ended.ended_at, ended.end_reason, ended.ended_by.as_deref()),
        (Some(2_000), Some(EndReason::Returned), Some("borrower"))
    );
    assert!(
        store
            .end_loan(&id("one"), 2_100, "lender", EndReason::Revoked)
            .await
            .unwrap()
            .is_none(),
        "an end is final"
    );
    assert!(
        store
            .create_loan(&grant(&id("two"), &account, 5_000))
            .await
            .unwrap()
    );
    let expired = store.expire_loans(5_000).await.unwrap();
    assert!(
        expired
            .iter()
            .any(|g| g.id == id("two") && g.end_reason == Some(EndReason::Expired))
    );
    assert!(
        store
            .expire_loans(5_000)
            .await
            .unwrap()
            .iter()
            .all(|g| g.id != id("two"))
    );
    let mine = store.loans_for_user("borrower").await.unwrap();
    assert!(mine.iter().any(|g| g.id == id("one")) && mine.iter().any(|g| g.id == id("two")));
    assert!(
        store
            .loans_for_user("stranger")
            .await
            .unwrap()
            .iter()
            .all(|g| !g.id.starts_with(prefix))
    );

    store
        .append_loan_audit(&AuditEvent::token_issued(2_500, &id("two"), "machine"))
        .await
        .unwrap();
    store
        .append_loan_audit(&AuditEvent::token_issued(2_600, &id("two"), "machine"))
        .await
        .unwrap();
    let mut audit = store.loan_audit(&[id("one"), id("two")]).await.unwrap();
    audit.sort_by_key(|event| (event.at, event.grant_id.clone()));
    assert_eq!(
        audit
            .iter()
            .map(|e| (e.at, e.kind, e.reason.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (1_000, AuditKind::Granted, None),
            (1_000, AuditKind::Granted, None),
            (2_000, AuditKind::Ended, Some("returned")),
            (2_500, AuditKind::TokenIssued, None),
            (5_000, AuditKind::Ended, Some("expired")),
        ],
        "each transition stores its event; token issue events coalesce per machine and hour"
    );

    store.prune_loans(3_000 + RETENTION_SECONDS).await.unwrap();
    let audit = store.loan_audit(&[id("two")]).await.unwrap();
    assert_eq!(audit.iter().map(|e| e.at).collect::<Vec<_>>(), vec![5_000]);
    assert!(
        store.load_loan(&id("one")).await.unwrap().is_none(),
        "ended 90 days ago"
    );
    assert!(
        store.load_loan(&id("two")).await.unwrap().is_some(),
        "ended within 90 days"
    );
}

#[tokio::test]
async fn file_store_keeps_loans_in_the_encrypted_state() {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    crate::central::vault::create_secret(&key, &[9; 32]).unwrap();
    let store = CentralStore::file(root.path(), &key);
    store.migrate().await.unwrap();
    scenario(&store, "file").await;
    let raw = std::fs::read(root.path().join("central-storage.enc")).unwrap();
    assert!(
        !String::from_utf8_lossy(&raw).contains("alice@sawmills.ai"),
        "grants are encrypted at rest"
    );
    assert!(!root.path().join("loans.json").exists());
}

#[test]
fn a_delayed_mirror_never_reopens_an_ended_grant() {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    crate::central::vault::create_secret(&key, &[9; 32]).unwrap();
    let file = FileStore {
        state: root.path().into(),
        key,
    };
    let active = grant("race", "account", 5_000);
    let mut ended = active.clone();
    ended.end(2_000, "lender", EndReason::Revoked);
    file.mirror_loan(&ended, &AuditEvent::ended(&ended))
        .unwrap();
    file.mirror_loan(&active, &AuditEvent::granted(&active))
        .unwrap();
    assert_eq!(
        file.read_state().unwrap().loans["race"].end_reason,
        Some(EndReason::Revoked)
    );
}

#[cfg(feature = "central-real-db-tests")]
#[tokio::test]
async fn postgres_and_dual_stores_keep_loans_in_tables() {
    if std::env::var("DATABASE_URL").is_err() {
        if std::env::var("CI").ok().as_deref() == Some("true") {
            panic!("DATABASE_URL must be set for PostgreSQL scenarios in CI");
        }
        return;
    }
    for mode in [
        super::super::StoreMode::Postgres,
        super::super::StoreMode::Dual,
    ] {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        crate::central::vault::create_secret(&key, &[9; 32]).unwrap();
        let shared = CentralStore::from_mode(mode, root.path(), &key)
            .await
            .unwrap();
        let (store, control, schema) = shared.isolated_test_schema().await.unwrap();
        store.migrate().await.unwrap();
        scenario(&store, &mode.to_string()).await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
