#![cfg(all(unix, feature = "central-real-db-tests"))]

use aes_gcm::{
    Aes256Gcm,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use codexctl::{central, store};
use serde_json::json;
use std::{path::PathBuf, process::Output};
use tokio::process::Command;

struct Fixture {
    root: tempfile::TempDir,
    database: String,
    control: tokio_postgres::Client,
    connection: tokio::task::JoinHandle<()>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let database = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let (control, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection = tokio::spawn(async move { connection.await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let schema = format!(
            "backfill_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        control
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let mut database = reqwest::Url::parse(&database).unwrap();
        database
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let fixture = Self {
            root,
            database: database.to_string(),
            control,
            connection,
            schema,
        };
        central::managed::setup(&fixture.state(), &fixture.root.path().join("key")).unwrap();
        fixture.write_users(true);
        fixture.write_devices(false);
        assert!(
            fixture.backfill().await.status.success(),
            "initial backfill"
        );
        fixture
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn write_users(&self, enabled: bool) {
        store::atomic_write(
            &self.state().join("users.json"),
            &serde_json::to_vec(
                &json!([{"id":"test","email":"test@example.invalid","enabled":enabled}]),
            )
            .unwrap(),
        )
        .unwrap();
    }

    fn write_devices(&self, revoked: bool) {
        store::atomic_write(&self.state().join("devices.json"),
            &serde_json::to_vec(&json!([{"id":"test-machine","tenant":"sawmills","user":"test","token_hash":"synthetic-hash-do-not-log","revoked":revoked}])).unwrap()).unwrap();
    }

    fn write_account(&self, label: &str, revision: i64) -> String {
        let id = central::managed::account_key("test", "seat");
        let state = self.state().join("accounts").join(&id);
        store::ensure_private_dir(&state).unwrap();
        let value = json!({"alias":"seat","tenant":"sawmills","user":"test",
            "auth":{"access_token":"synthetic-access-do-not-log","refresh_token":"synthetic-refresh-do-not-log","account_id":"synthetic-workspace"},
            "label":label,"verified":false,"import_rejected":false,"revision":revision});
        let key = std::fs::read(self.root.path().join("key")).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let encrypted = cipher
            .encrypt(&nonce, serde_json::to_vec(&value).unwrap().as_slice())
            .unwrap();
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        store::atomic_write(&state.join("vault.enc"), &bytes).unwrap();
        id
    }

    async fn seed_loan(&self) {
        let file = central::storage::CentralStore::from_mode(
            central::storage::StoreMode::File,
            &self.state(),
            &self.root.path().join("key"),
        )
        .await
        .unwrap();
        file.migrate().await.unwrap();
        let grant = central::loans::Grant {
            id: "test-loan".into(),
            account_id: "loan-account".into(),
            lender: "test".into(),
            borrower: "borrower".into(),
            lender_email: "test@example.invalid".into(),
            borrower_email: "borrower@example.invalid".into(),
            alias: "seat".into(),
            reference: "test/seat".into(),
            subject: central::loans::CredentialSubject {
                workspace: "synthetic-workspace".into(),
                uid: None,
                sub: None,
            },
            created_at: 1000,
            ends_at: 2_100_000_000,
            ended_at: None,
            ended_by: None,
            end_reason: None,
            deleted_at: None,
        };
        assert!(file.create_loan(&grant).await.unwrap());
        assert!(self.backfill().await.status.success());
    }

    async fn backfill(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .arg("backfill")
            .arg("--state")
            .arg(self.state())
            .arg("--key-file")
            .arg(self.root.path().join("key"))
            .env("DATABASE_URL", &self.database)
            .env("CODEXCTL_CENTRAL_STORE", "postgres")
            .env("CODEXCTL_CENTRAL_DB_TLS", "0")
            .env_remove("CODEXCTL_CENTRAL_DUAL_WRITE")
            .output()
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.control
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
        self.connection.abort();
    }
}

#[tokio::test]
async fn postgres_backfill_refuses_a_stale_same_id_company_user() {
    let fixture = Fixture::new().await;
    fixture.write_users(false);
    let result = fixture.backfill().await;
    let enabled: bool = fixture
        .control
        .query_one(
            &format!(
                "SELECT enabled FROM {}.central_users WHERE id='test'",
                fixture.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "stale same-ID user must fail the backfill Job"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("central_users") && error.contains("test"));
    assert!(enabled, "fail closed preserves the conflicting stored row");
    assert!(!error.contains("synthetic-hash-do-not-log"));
}

#[tokio::test]
async fn postgres_backfill_refuses_a_stale_same_id_machine() {
    let fixture = Fixture::new().await;
    fixture.write_devices(true);
    let result = fixture.backfill().await;
    let revoked: bool = fixture
        .control
        .query_one(
            &format!(
                "SELECT revoked FROM {}.central_devices WHERE id='test-machine'",
                fixture.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "stale same-ID machine must fail the backfill Job"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("central_devices") && error.contains("test-machine"));
    assert!(!revoked, "fail closed preserves the conflicting stored row");
    assert!(!error.contains("synthetic-hash-do-not-log"));
}

#[tokio::test]
async fn postgres_backfill_accepts_identical_registry_rows_on_rerun() {
    let fixture = Fixture::new().await;
    fixture.control.batch_execute(&format!(
        "UPDATE {}.central_users SET revision=17,updated_at='2001-01-01'; UPDATE {}.central_devices SET revision=23,updated_at='2001-01-01'",
        fixture.schema, fixture.schema
    )).await.unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        result.status.success(),
        "identical source rows must remain idempotent"
    );
    let counts: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(counts["users"], 1);
    assert_eq!(counts["devices"], 1);
}

#[tokio::test]
async fn postgres_backfill_refuses_same_revision_account_payload_drift() {
    let fixture = Fixture::new().await;
    let id = fixture.write_account("Original", 7);
    assert!(fixture.backfill().await.status.success());
    fixture.write_account("Changed", 7);
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "same revision cannot conceal a different account vault"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("central_accounts") && error.contains(&id));
    assert!(!error.contains("synthetic-access-do-not-log"));
    assert!(!error.contains("synthetic-refresh-do-not-log"));
}

#[tokio::test]
async fn postgres_backfill_accepts_an_identical_account_on_rerun() {
    let fixture = Fixture::new().await;
    fixture.write_account("Original", 7);
    assert!(fixture.backfill().await.status.success());
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(result.status.success());
    let counts: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(counts["accounts"], 0);
}

#[tokio::test]
async fn postgres_backfill_refuses_a_skipped_loan_with_different_payload() {
    let fixture = Fixture::new().await;
    fixture.seed_loan().await;
    fixture
        .control
        .batch_execute(&format!(
            "UPDATE {}.account_loans SET ends_at=ends_at+1 WHERE id='test-loan'",
            fixture.schema
        ))
        .await
        .unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "skipped loan must match the source grant"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("account_loans") && error.contains("test-loan"));
}

#[tokio::test]
async fn postgres_backfill_refuses_different_audit_for_an_identical_loan() {
    let fixture = Fixture::new().await;
    fixture.seed_loan().await;
    fixture
        .control
        .batch_execute(&format!(
            "UPDATE {}.account_loan_audit SET actor='changed-actor' WHERE grant_id='test-loan'",
            fixture.schema
        ))
        .await
        .unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "skipped grant must not conceal audit drift"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("account_loan_audit") && error.contains("test-loan"));
    assert!(!error.contains("changed-actor"));
}

#[tokio::test]
async fn postgres_backfill_refuses_an_inactive_source_alias_fence() {
    let fixture = Fixture::new().await;
    store::atomic_write(
        &fixture.state().join("renames.json"),
        &serde_json::to_vec(&json!([{"user":"test","from":"old-seat","to":"seat"}])).unwrap(),
    )
    .unwrap();
    assert!(fixture.backfill().await.status.success());
    fixture.control.batch_execute(&format!(
        "UPDATE {}.central_alias_tombstones SET deleted_at=now() WHERE user_id='test' AND alias='old-seat'", fixture.schema
    )).await.unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "the source rename fence must remain active after write"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("central_alias_tombstones") && error.contains("test/old-seat"));
}

#[tokio::test]
async fn postgres_backfill_accepts_identical_loans_and_audit_on_rerun() {
    let fixture = Fixture::new().await;
    fixture.seed_loan().await;
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        result.status.success(),
        "identical grant and audit must remain idempotent"
    );
    let counts: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(counts["loans"], 0);
}

#[tokio::test]
async fn postgres_backfill_refuses_a_source_account_older_than_postgres() {
    let fixture = Fixture::new().await;
    let id = fixture.write_account("Original", 7);
    assert!(fixture.backfill().await.status.success());
    fixture.write_account("Original", 6);
    let result = fixture.backfill().await;
    let revision: i64 = fixture
        .control
        .query_one(
            &format!(
                "SELECT revision FROM {}.central_accounts WHERE account_id=$1",
                fixture.schema
            ),
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "older source must not be silently skipped or overwrite newer state"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("central_accounts") && error.contains(&id));
    assert_eq!(revision, 7);
    assert!(!error.contains("synthetic-access-do-not-log"));
    assert!(!error.contains("synthetic-refresh-do-not-log"));
}

#[tokio::test]
async fn postgres_backfill_refuses_rows_removed_between_the_initial_and_final_pass() {
    let mut results = Vec::new();
    for table in [
        "central_users",
        "central_devices",
        "central_accounts",
        "account_loans",
        "central_alias_tombstones",
    ] {
        let fixture = Fixture::new().await;
        let identifier = match table {
            "central_users" => {
                store::atomic_write(&fixture.state().join("users.json"), b"[]").unwrap();
                "test".to_owned()
            }
            "central_devices" => {
                std::fs::remove_file(fixture.state().join("devices.json")).unwrap();
                "test-machine".to_owned()
            }
            "central_accounts" => {
                let id = fixture.write_account("Original", 7);
                assert!(fixture.backfill().await.status.success());
                std::fs::remove_dir_all(fixture.state().join("accounts").join(&id)).unwrap();
                id
            }
            "account_loans" => {
                fixture.seed_loan().await;
                std::fs::remove_file(fixture.state().join("central-storage.enc")).unwrap();
                "test-loan".to_owned()
            }
            _ => {
                store::atomic_write(
                    &fixture.state().join("renames.json"),
                    &serde_json::to_vec(&json!([{"user":"test","from":"old-seat","to":"seat"}]))
                        .unwrap(),
                )
                .unwrap();
                assert!(fixture.backfill().await.status.success());
                store::atomic_write(&fixture.state().join("renames.json"), b"[]").unwrap();
                "test/old-seat".to_owned()
            }
        };
        let result = fixture.backfill().await;
        fixture.finish().await;
        results.push((table, identifier, result));
    }
    for (table, identifier, result) in results {
        assert!(
            !result.status.success(),
            "target-only {table} row must fail the final backfill"
        );
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(
            error.contains(table) && error.contains(&identifier),
            "{error}"
        );
        assert!(!error.contains("synthetic-access-do-not-log"));
        assert!(!error.contains("synthetic-refresh-do-not-log"));
        assert!(!error.contains("synthetic-hash-do-not-log"));
    }
}
