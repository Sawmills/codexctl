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
    async fn new() -> Option<Self> {
        let fixture = Self::in_database(true).await;
        let result = fixture.backfill().await;
        let deadline = chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        if chrono::Utc::now().date_naive() > deadline {
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("expired"));
            fixture.finish().await;
            eprintln!("B33 A5 authorization expired; payload backfill cases no longer apply");
            return None;
        }
        assert!(
            result.status.success(),
            "initial backfill: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Some(fixture)
    }

    async fn in_database(exception_database: bool) -> Self {
        Self::in_named_database(exception_database.then_some("codexctl_a5")).await
    }

    async fn in_named_database(name: Option<&str>) -> Self {
        let mut database = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let test_url = reqwest::Url::parse(&database).unwrap();
        assert!(
            matches!(
                test_url.host_str(),
                Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
            ),
            "database fixtures require a loopback test server"
        );
        if let Some(name) = name {
            assert!(matches!(name, "codexctl_a5" | "codexctl_a5_canary"));
            let (admin, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
                .await
                .unwrap();
            let connection = tokio::spawn(async move { connection.await.unwrap() });
            if let Err(error) = admin
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .await
            {
                // Concurrent same-name creation may lose the pg_database
                // unique-index race instead of the preliminary name check.
                assert!(
                    error.code() == Some(&tokio_postgres::error::SqlState::DUPLICATE_DATABASE)
                        || error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
                );
            }
            connection.abort();
            let mut url = reqwest::Url::parse(&database).unwrap();
            url.set_path(&format!("/{name}"));
            database = url.to_string();
        }
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
        self.write_loan().await;
        assert!(self.backfill().await.status.success());
    }

    async fn write_loan(&self) {
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
    }

    async fn backfill(&self) -> Output {
        self.backfill_with_scan_exception(true).await
    }

    async fn backfill_with_scan_exception(&self, scan_exception: bool) -> Output {
        self.backfill_with_exception(scan_exception.then_some("b33-a5"))
            .await
    }

    async fn backfill_with_exception(&self, scan_exception: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl-central"));
        command
            .arg("backfill")
            .arg("--state")
            .arg(self.state())
            .arg("--key-file")
            .arg(self.root.path().join("key"))
            .env("DATABASE_URL", &self.database)
            .env("CODEXCTL_CENTRAL_STORE", "postgres")
            .env("CODEXCTL_CENTRAL_DB_TLS", "0")
            .env_remove("CODEXCTL_CENTRAL_DUAL_WRITE");
        if let Some(scan_exception) = scan_exception {
            command.arg("--scan-exception").arg(scan_exception);
        }
        command.output().await.unwrap()
    }

    async fn finish(self) {
        self.control
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
        self.connection.abort();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires Linux user/mount namespaces and disposable loopback PostgreSQL"]
async fn postgres_backfill_preserves_a_readonly_source_with_and_without_locks() {
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt, path::Path};

    #[derive(PartialEq, Eq)]
    struct SourceEntry {
        mode: u32,
        uid: u32,
        gid: u32,
        modified: std::time::SystemTime,
        content: Vec<u8>,
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, SourceEntry> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            let content = if metadata.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                Vec::new()
            } else {
                std::fs::read(&path).unwrap()
            };
            entries.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                SourceEntry {
                    mode: metadata.mode(),
                    uid: metadata.uid(),
                    gid: metadata.gid(),
                    modified: metadata.modified().unwrap(),
                    content,
                },
            );
        }
        entries
    }

    // This manual proof must not silently pass after the scan authorization ends.
    assert!(
        chrono::Utc::now().date_naive() <= chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        "B33 scan authorization expired; a new proof requires separate authorization"
    );
    for existing_locks in [false, true] {
        let fixture = Fixture::in_database(true).await;
        fixture.write_account("Read-only source", 7);
        fixture.write_loan().await;
        store::atomic_write(
            &fixture.state().join("renames.json"),
            br#"[{"user":"test","from":"old-seat","to":"seat"}]"#,
        )
        .unwrap();
        // Loan setup creates this lock. Exercise both a retained lock and a
        // source with no lock at all, without allowing backfill to create one.
        std::fs::remove_file(fixture.state().join("central-storage.lock")).unwrap();
        if existing_locks {
            store::atomic_write(&fixture.state().join("central-storage.lock"), b"").unwrap();
            store::atomic_write(&fixture.state().join("import.lock"), b"").unwrap();
        }
        let before = snapshot(&fixture.state());
        for pass in 0..2 {
            // The mount exists only in this child's private user/mount namespace.
            // Both create and overwrite probes must fail on the read-only bind.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                Command::new("unshare")
                    .args(["--user", "--map-root-user", "--mount", "sh", "-ec"])
                    .arg(
                        r#"
                        mount --bind "$1" "$1"
                        mount -o remount,bind,ro "$1"
                        if touch "$1/.write-probe" 2>/dev/null; then exit 90; fi
                        if (printf x >> "$1/users.json") 2>/dev/null; then exit 91; fi
                        shift
                        exec "$@"
                    "#,
                    )
                    .arg("readonly-backfill")
                    .arg(fixture.state())
                    .arg(env!("CARGO_BIN_EXE_codexctl-central"))
                    .arg("backfill")
                    .arg("--state")
                    .arg(fixture.state())
                    .arg("--key-file")
                    .arg(fixture.root.path().join("key"))
                    .args(["--scan-exception", "b33-a5"])
                    .env("DATABASE_URL", &fixture.database)
                    .env("CODEXCTL_CENTRAL_STORE", "postgres")
                    .env("CODEXCTL_CENTRAL_DB_TLS", "0")
                    .env_remove("CODEXCTL_CENTRAL_DUAL_WRITE")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("read-only backfill deadline")
            .unwrap();
            assert!(
                result.status.success(),
                "read-only backfill (locks={existing_locks}, pass={pass}): {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
            for table in [
                "central_accounts",
                "central_users",
                "central_devices",
                "central_alias_tombstones",
                "account_loans",
                "account_loan_audit",
            ] {
                assert_eq!(
                    receipt["inventory"][table],
                    json!({
                        "source": 1, "target_before": pass, "target": 1,
                    }),
                    "{table}"
                );
            }
            assert_eq!(receipt["accounts"], usize::from(pass == 0));
            assert_eq!(receipt["loans"], usize::from(pass == 0));
            // Compare without Debug output: even synthetic credential bytes
            // should not appear in a failed assertion's diagnostic payload.
            assert!(
                before == snapshot(&fixture.state()),
                "source evidence changed"
            );
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn postgres_backfill_refuses_a_stale_same_id_company_user() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
    let Some(fixture) = Fixture::new().await else {
        return;
    };
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
        let Some(fixture) = Fixture::new().await else {
            return;
        };
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
        if table == "central_devices" {
            assert!(
                error.contains(table) && error.contains("count 1"),
                "{error}"
            );
            assert!(
                !error.contains(&identifier),
                "foreign tenants must be count-only: {error}"
            );
        } else {
            assert!(
                error.contains(table) && error.contains(&identifier),
                "{error}"
            );
        }
        assert!(!error.contains("synthetic-access-do-not-log"));
        assert!(!error.contains("synthetic-refresh-do-not-log"));
        assert!(!error.contains("synthetic-hash-do-not-log"));
    }
}

#[tokio::test]
async fn postgres_backfill_refuses_an_oversized_target_before_reading_source_credentials() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.control.batch_execute(&format!(
        "INSERT INTO {}.central_users(id,email,enabled) SELECT 'extra-'||n, 'synthetic@example.invalid', true FROM generate_series(1,50000) n",
        fixture.schema
    )).await.unwrap();
    let id = fixture.write_account("Unreadable", 7);
    store::atomic_write(
        &fixture.state().join("accounts").join(id).join("vault.enc"),
        b"not-an-encrypted-vault",
    )
    .unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "an oversized target must fail the Job"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("central_users") && error.contains("50001"),
        "{error}"
    );
    assert!(
        !error.contains("extra-") && !error.contains("synthetic@example.invalid"),
        "{error}"
    );
    assert!(
        !error.contains("vault"),
        "the cap must fail before source credential inventory: {error}"
    );
}

#[tokio::test]
async fn postgres_backfill_refuses_an_oversized_source_before_reading_credentials() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let rows = vec![json!({"id":"test","email":"synthetic@example.invalid","enabled":true}); 50001];
    store::atomic_write(
        &fixture.state().join("users.json"),
        &serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    let id = fixture.write_account("Unreadable", 7);
    store::atomic_write(
        &fixture.state().join("accounts").join(id).join("vault.enc"),
        b"not-an-encrypted-vault",
    )
    .unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("central_users") && error.contains("50001"),
        "{error}"
    );
    assert!(
        !error.contains("vault") && !error.contains("synthetic@example.invalid"),
        "{error}"
    );
}

#[tokio::test]
async fn postgres_backfill_stops_the_target_probe_at_the_first_row_over_the_cap() {
    let fixture = Fixture::in_database(true).await;
    fixture.control.batch_execute(&format!(
        "CREATE TABLE {0}.probe_rows(n INTEGER);\n\
         INSERT INTO {0}.probe_rows SELECT n FROM generate_series(1,50002) n;\n\
         CREATE SEQUENCE {0}.probe_visits;\n\
         CREATE FUNCTION {0}.allow_probe_row() RETURNS BOOLEAN LANGUAGE plpgsql VOLATILE AS $$\n\
         BEGIN\n\
           IF nextval('{0}.probe_visits') > 50001 THEN RAISE EXCEPTION 'read past row probe limit'; END IF;\n\
           RETURN true;\n\
         END $$;\n\
         CREATE VIEW {0}.central_users AS SELECT n FROM {0}.probe_rows WHERE {0}.allow_probe_row()",
        fixture.schema
    )).await.unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    if chrono::Utc::now().date_naive() > chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap() {
        assert!(error.contains("expired"), "{error}");
    } else {
        assert!(
            error.contains("central_users") && error.contains("count 50001"),
            "{error}"
        );
        assert!(!error.contains("read past row probe limit"), "{error}");
    }
}

#[tokio::test]
async fn postgres_backfill_stops_source_parsing_at_the_first_row_over_the_cap() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut source = String::from("[");
    source.push_str(&vec!["null"; 50001].join(","));
    source.push_str(",invalid-json-after-the-cap]");
    store::atomic_write(&fixture.state().join("users.json"), source.as_bytes()).unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("central_users") && error.contains("count 50001"),
        "{error}"
    );
    assert!(!error.contains("invalid-json-after-the-cap"), "{error}");
}

#[tokio::test]
async fn postgres_backfill_stops_encrypted_loan_map_parsing_at_the_row_cap() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let rows = (0..50001)
        .map(|n| format!("\"{n}\":null"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!("{{\"loans\":{{{rows},invalid-json-after-the-cap}}}}");
    let key = std::fs::read(fixture.root.path().join("key")).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let mut bytes = nonce.to_vec();
    bytes.extend(cipher.encrypt(&nonce, source.as_bytes()).unwrap());
    store::atomic_write(&fixture.state().join("central-storage.enc"), &bytes).unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("account_loans") && error.contains("count 50001"),
        "{error}"
    );
    assert!(!error.contains("invalid-json-after-the-cap"), "{error}");
}

#[tokio::test]
async fn postgres_backfill_refuses_foreign_tenant_machines_with_counts_only() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.control.batch_execute(&format!(
        "INSERT INTO {}.central_devices(id,tenant,user_id,token_hash,revoked) VALUES('foreign-private-machine','foreign-private-tenant','foreign-user','foreign-hash-do-not-log',false)",
        fixture.schema
    )).await.unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "a foreign tenant must fail the Job"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("central_devices") && error.contains("count 1"),
        "{error}"
    );
    for private in [
        "foreign-private-machine",
        "foreign-private-tenant",
        "foreign-user",
        "foreign-hash-do-not-log",
    ] {
        assert!(
            !error.contains(private),
            "foreign tenant inventory must be count-only: {error}"
        );
    }
}

#[tokio::test]
async fn postgres_backfill_receipt_reports_source_and_target_table_counts() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let id = fixture.write_account("Copied", 7);
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let inventory = &receipt["inventory"];
    assert_eq!(inventory.as_object().map(|o| o.len()), Some(6));
    assert_eq!(
        inventory["central_accounts"],
        json!({"source":1,"target_before":0,"target":1})
    );
    for table in ["central_users", "central_devices"] {
        assert_eq!(
            inventory[table],
            json!({"source":1,"target_before":1,"target":1})
        );
    }
    for table in [
        "central_alias_tombstones",
        "account_loans",
        "account_loan_audit",
    ] {
        assert_eq!(
            inventory[table],
            json!({"source":0,"target_before":0,"target":0})
        );
    }
    assert!(
        !String::from_utf8(result.stdout).unwrap().contains(&id),
        "receipt must contain counts only"
    );
}

#[tokio::test]
async fn postgres_backfill_refuses_alias_growth_over_the_cap_before_inventory() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.control.batch_execute(&format!(
        "INSERT INTO {}.central_alias_tombstones(user_id,alias,target) SELECT 'test','old-'||n,'seat' FROM generate_series(1,49999) n",
        fixture.schema
    )).await.unwrap();
    store::atomic_write(
        &fixture.state().join("renames.json"),
        &serde_json::to_vec(&json!([
            {"user":"test","from":"new-a","to":"seat"},
            {"user":"test","from":"new-b","to":"seat"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let result = fixture.backfill().await;
    let retained: i64 = fixture
        .control
        .query_one(
            &format!(
                "SELECT count(*) FROM {}.central_alias_tombstones",
                fixture.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("central_alias_tombstones") && error.contains("50001"),
        "{error}"
    );
    assert!(
        !error.contains("old-"),
        "cap failure must precede inventory: {error}"
    );
    assert_eq!(
        retained, 49999,
        "the failing alias transaction must roll back"
    );
}

#[tokio::test]
async fn postgres_backfill_refuses_projected_audit_growth_before_reading_history() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.seed_loan().await;
    fixture.control.batch_execute(&format!(
        "INSERT INTO {}.account_loan_audit(grant_id,at,kind,deleted_at) SELECT 'test-loan',n,'token_issued',1000 FROM generate_series(1,49999) n",
        fixture.schema
    )).await.unwrap();
    let path = fixture.state().join("central-storage.enc");
    let bytes = std::fs::read(&path).unwrap();
    let key = std::fs::read(fixture.root.path().join("key")).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let plain = cipher
        .decrypt(aes_gcm::Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .unwrap();
    let mut ledger: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    let mut grant = ledger["loans"]["test-loan"].clone();
    grant["id"] = json!("second-loan");
    grant["accountId"] = json!("second-account");
    ledger["loans"]["second-loan"] = grant;
    let mut event = ledger["loan_audit"][0].clone();
    event["grantId"] = json!("second-loan");
    ledger["loan_audit"].as_array_mut().unwrap().push(event);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let encrypted = cipher
        .encrypt(&nonce, serde_json::to_vec(&ledger).unwrap().as_slice())
        .unwrap();
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    store::atomic_write(&path, &bytes).unwrap();
    let result = fixture.backfill().await;
    let grants: i64 = fixture
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.account_loans", fixture.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("account_loan_audit") && error.contains("50001"),
        "{error}"
    );
    assert!(
        !error.contains("test-loan"),
        "cap must precede historical inventory: {error}"
    );
    assert_eq!(
        grants, 1,
        "projected cap must fail before copying another loan"
    );
}

#[tokio::test]
async fn postgres_backfill_accepts_the_cap_and_counts_retained_target_rows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.control.batch_execute(&format!(
        "INSERT INTO {}.central_users(id,email,enabled,deleted_at) SELECT 'retired-'||n,'synthetic@example.invalid',false,now() FROM generate_series(1,49999) n",
        fixture.schema
    )).await.unwrap();
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        receipt["inventory"]["central_users"],
        json!({"source":1,"target_before":50000,"target":50000})
    );
}

#[tokio::test]
async fn postgres_backfill_requires_explicit_scan_authorization() {
    let fixture = Fixture::in_database(false).await;
    let result = fixture.backfill_with_scan_exception(false).await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "no flag must refuse before any inventory"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("--scan-exception"), "{error}");
}

#[tokio::test]
async fn postgres_backfill_refuses_scan_authorization_for_a_different_database() {
    let fixture = Fixture::in_database(false).await;
    fixture.write_users(false);
    let result = fixture.backfill().await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("scan exception") && error.contains("codexctl_a5"),
        "{error}"
    );
    assert!(
        !error.contains("central_users"),
        "scope must be checked before registry inventory: {error}"
    );
}

#[tokio::test]
async fn postgres_canary_exception_cannot_scan_the_real_a5_database() {
    let fixture = Fixture::in_database(true).await;
    let result = fixture.backfill_with_exception(Some("b33-a5-canary")).await;
    fixture.finish().await;
    assert!(
        !result.status.success(),
        "the canary flag must not authorize the H target"
    );
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("codexctl_a5_canary"), "{error}");
}

#[tokio::test]
async fn postgres_canary_exception_allows_only_its_throwaway_named_database() {
    let fixture = Fixture::in_named_database(Some("codexctl_a5_canary")).await;
    let result = fixture.backfill_with_exception(Some("b33-a5-canary")).await;
    fixture.finish().await;
    let deadline = chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
    if chrono::Utc::now().date_naive() > deadline {
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("expired"));
    } else {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            receipt["inventory"]["central_users"],
            json!({"source":1,"target_before":0,"target":1})
        );
    }
}

#[tokio::test]
async fn postgres_h_exception_refuses_canary_before_even_the_count_probe() {
    let fixture = Fixture::in_named_database(Some("codexctl_a5_canary")).await;
    fixture.control.batch_execute(&format!(
        "CREATE TABLE {}.central_users(id TEXT); INSERT INTO {}.central_users SELECT n::text FROM generate_series(1,50001) n",
        fixture.schema, fixture.schema
    )).await.unwrap();
    let result = fixture.backfill_with_exception(Some("b33-a5")).await;
    fixture.finish().await;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("requires database codexctl_a5"), "{error}");
    assert!(
        !error.contains("central_users") && !error.contains("50001"),
        "scope must precede the count probe: {error}"
    );
}

#[tokio::test]
async fn postgres_backfill_scope_is_independent_of_session_date_style() {
    let mut fixture = Fixture::in_database(true).await;
    fixture.control.batch_execute(&format!(
        "CREATE TABLE {}.central_users(id TEXT); INSERT INTO {}.central_users SELECT n::text FROM generate_series(1,50001) n",
        fixture.schema, fixture.schema
    )).await.unwrap();
    let mut database = reqwest::Url::parse(&fixture.database).unwrap();
    database.set_query(None);
    database
        .query_pairs_mut()
        .append_pair("sslmode", "disable")
        .append_pair(
            "options",
            &format!("-csearch_path={} -cDateStyle=SQL,DMY", fixture.schema),
        );
    fixture.database = database.to_string().replace('+', "%20");
    let result = fixture.backfill().await;
    fixture.finish().await;
    if chrono::Utc::now().date_naive() > chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap() {
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("expired"));
    } else {
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("central_users") && error.contains("50001"),
            "{error}"
        );
    }
}
