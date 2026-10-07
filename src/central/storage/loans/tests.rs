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
        subject: Default::default(),
        created_at: 1_000,
        ends_at,
        ended_at: None,
        ended_by: None,
        end_reason: None,
        deleted_at: None,
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
    let active: Vec<_> = store
        .active_loans_for_borrower("borrower")
        .await
        .unwrap()
        .into_iter()
        .filter(|g| g.id.starts_with(prefix))
        .map(|g| g.id)
        .collect();
    assert_eq!(active, [id("two")], "an ended grant is not active");
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
    let mut audit = store
        .loan_audit(&[id("one"), id("two")], None, 100)
        .await
        .unwrap();
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

    store.retire_loans(3_000 + RETENTION_SECONDS).await.unwrap();
    let audit = store.loan_audit(&[id("two")], None, 100).await.unwrap();
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
    let raw = FileStore {
        state: root.path().into(),
        key: key.clone(),
    }
    .read_state()
    .unwrap();
    assert!(
        raw.loans["file-one"].deleted_at.is_some(),
        "retention marks a grant deleted and keeps it"
    );
    assert!(raw.loan_audit.iter().any(|e| e.deleted_at.is_some()));
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
    ended.end(2_000, Some("lender"), EndReason::Revoked);
    file.mirror_loan(&ended, &AuditEvent::ended(&ended))
        .unwrap();
    file.mirror_loan(&active, &AuditEvent::granted(&active))
        .unwrap();
    let mirrored = file.read_state().unwrap();
    assert_eq!(mirrored.loans["race"].end_reason, Some(EndReason::Revoked));
    let mut kinds: Vec<_> = mirrored.loan_audit.iter().map(|e| e.kind).collect();
    kinds.sort_by_key(|kind| *kind as u8);
    assert_eq!(
        kinds,
        [AuditKind::Granted, AuditKind::Ended],
        "each event once"
    );
    file.mirror_loan(&ended, &AuditEvent::ended(&ended))
        .unwrap();
    assert_eq!(file.read_state().unwrap().loan_audit.len(), 2);
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

#[cfg(feature = "central-real-db-tests")]
#[tokio::test]
async fn backfill_copies_file_loans_with_their_audit_once() {
    if std::env::var("DATABASE_URL").is_err() {
        if std::env::var("CI").ok().as_deref() == Some("true") {
            panic!("DATABASE_URL must be set for PostgreSQL scenarios in CI");
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    crate::central::vault::create_secret(&key, &[9; 32]).unwrap();
    let file = CentralStore::file(root.path(), &key);
    file.migrate().await.unwrap();
    assert!(
        file.create_loan(&grant("active", "a", i64::MAX / 2))
            .await
            .unwrap()
    );
    assert!(
        file.create_loan(&grant("ended", "b", i64::MAX / 2))
            .await
            .unwrap()
    );
    file.end_loan("ended", 2_000, "lender", EndReason::Revoked)
        .await
        .unwrap();

    let shared = CentralStore::from_mode(super::super::StoreMode::Postgres, root.path(), &key)
        .await
        .unwrap();
    let (db, control, schema) = shared.isolated_test_schema().await.unwrap();
    db.migrate().await.unwrap();
    assert_eq!(db.backfill(root.path(), &key).await.unwrap().loans, 2);
    assert_eq!(
        db.backfill(root.path(), &key).await.unwrap().loans,
        0,
        "idempotent"
    );
    assert!(
        db.load_loan("active")
            .await
            .unwrap()
            .unwrap()
            .ended_at
            .is_none()
    );
    assert_eq!(
        db.load_loan("ended").await.unwrap().unwrap().end_reason,
        Some(EndReason::Revoked)
    );
    let kinds: Vec<_> = db
        .loan_audit(&["ended".into()], None, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, [AuditKind::Ended, AuditKind::Granted]);
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[cfg(feature = "central-real-db-tests")]
#[tokio::test]
async fn a_retried_end_repairs_a_missed_dual_mirror() {
    if std::env::var("DATABASE_URL").is_err() {
        if std::env::var("CI").ok().as_deref() == Some("true") {
            panic!("DATABASE_URL must be set for PostgreSQL scenarios in CI");
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    crate::central::vault::create_secret(&key, &[9; 32]).unwrap();
    let shared = CentralStore::from_mode(super::super::StoreMode::Dual, root.path(), &key)
        .await
        .unwrap();
    let (dual, control, schema) = shared.isolated_test_schema().await.unwrap();
    dual.migrate().await.unwrap();
    assert!(
        dual.create_loan(&grant("missed", "a", i64::MAX / 2))
            .await
            .unwrap()
    );
    let CentralStore::Dual { file, postgres, .. } = &dual else {
        unreachable!()
    };
    // The PostgreSQL end commits; its file mirror is lost.
    postgres
        .end_loan("missed", 2_000, "lender", EndReason::Revoked)
        .await
        .unwrap()
        .unwrap();
    assert!(
        file.read_state().unwrap().loans["missed"]
            .ended_at
            .is_none()
    );
    assert!(
        dual.end_loan("missed", 3_000, "lender", EndReason::Revoked)
            .await
            .unwrap()
            .is_none()
    );
    let mirrored = file.read_state().unwrap();
    assert_eq!(mirrored.loans["missed"].ended_at, Some(2_000));
    assert_eq!(
        mirrored
            .loan_audit
            .iter()
            .filter(|e| e.kind == AuditKind::Ended)
            .count(),
        1
    );
    // Another retry changes nothing.
    dual.end_loan("missed", 4_000, "lender", EndReason::Revoked)
        .await
        .unwrap();
    assert_eq!(
        file.read_state().unwrap().loan_audit.len(),
        mirrored.loan_audit.len()
    );
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
