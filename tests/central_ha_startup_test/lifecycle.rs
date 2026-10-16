//! Tombstones are authoritative across warm owners, leases and restarts.
use super::*;

#[tokio::test]
async fn postgres_removed_account_with_unidentified_login_blocks_replacements_after_restart() {
    let mut f = login_fixture().await;
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    f.first.stop().await;
    let account = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "seat"));
    let runtime = std::fs::read(account.join("runtime/pid")).unwrap();
    let operation = "de".repeat(32);
    let journal = account.join("relogin").join(&operation);
    store::ensure_private_dir(&journal).unwrap();
    // A real live broker identity with durable spawn intent but no child PID
    // cannot prove which login identity the unfinished child may hold.
    let pid = std::process::id();
    #[cfg(target_os = "linux")]
    let incarnation = {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        format!(
            "{}:{}:{}",
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim(),
            std::fs::read_link(format!("/proc/{pid}/ns/pid"))
                .unwrap()
                .display(),
            fields[19]
        )
    };
    #[cfg(not(target_os = "linux"))]
    let incarnation = String::from_utf8(
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .await
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let evidence = serde_json::to_vec(&json!({
        "sequence":1,"id":operation,"user":"test","device":"test-machine","alias":"seat",
        "broker":{"pid":pid,"incarnation":incarnation},"child":{"status":"spawning"},
        "original_revision":"synthetic","candidate_revision":null,"candidate":null,
        "phase":"pending","code":null,"error":null,"retired":false
    }))
    .unwrap();
    store::atomic_write(&journal.join("record.json"), &evidence).unwrap();
    // The latest receipt is terminal, but complete inventory must still inspect
    // an older non-retired child. The shared-mode preflight reads the latest.
    let terminal_id = "df".repeat(32);
    let terminal_home = account.join("relogin").join(&terminal_id);
    store::ensure_private_dir(&terminal_home).unwrap();
    let mut terminal: Value = serde_json::from_slice(&evidence).unwrap();
    terminal["sequence"] = json!(2);
    terminal["id"] = json!(terminal_id);
    terminal["child"] = json!({"status":"exited"});
    terminal["phase"] = json!("canceled");
    terminal["retired"] = json!(true);
    store::atomic_write(
        &terminal_home.join("record.json"),
        &serde_json::to_vec(&terminal).unwrap(),
    )
    .unwrap();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
    f.first = Pod::spawn(&f.database, &f.key, root, "postgres").await;
    let response = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let metrics = response.text().await.unwrap();
    assert!(
        metrics
            .lines()
            .any(|line| line
                == "codexctl_central_ownership_unresolved{reason=\"recovery_failed\"} 1"),
        "removed account's unidentified login must retain the startup ownership fence: {metrics}"
    );
    let response = f
        .http
        .get(format!("{}/v1/accounts", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let accounts: Vec<Value> = response.json().await.unwrap();
    assert!(!accounts.iter().any(|a| a["alias"] == "seat"));
    assert_eq!(
        accounts.iter().find(|a| a["alias"] == "other").unwrap()["available"],
        false
    );
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"other","billing":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        f.first.launches(),
        0,
        "startup/request must not launch a replacement against unresolved inventory"
    );
    assert_eq!(
        std::fs::read(journal.join("record.json")).unwrap(),
        evidence
    );
    assert_eq!(std::fs::read(account.join("runtime/pid")).unwrap(), runtime);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_warm_catalog_batches_lifecycle_reads_and_refuses_database_errors() {
    let f = login_fixture().await;
    // Listing and the cached physical alias may differ only in ASCII case.
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET alias='SeAt' WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    // Count evaluated deletion metadata through the real PostgreSQL projection,
    // without coupling the assertion to SQL text or private storage methods.
    f.control.batch_execute(&format!(r#"
        CREATE SEQUENCE {s}.lifecycle_reads;
        CREATE FUNCTION {s}.count_lifecycle(d TIMESTAMPTZ) RETURNS TIMESTAMPTZ LANGUAGE plpgsql AS $$
        BEGIN
            PERFORM nextval('{s}.lifecycle_reads');
            RETURN d;
        END $$;
        ALTER TABLE {s}.central_accounts RENAME TO stored_accounts;
        CREATE VIEW {s}.central_accounts AS SELECT account_id,user_id,alias,workspace,login,
            encrypted_vault,revision,{s}.count_lifecycle(deleted_at) AS deleted_at,updated_at
            FROM {s}.stored_accounts;
    "#,s=f.schema)).await.unwrap();
    let response = f
        .http
        .get(format!("{}/v1/accounts", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let accounts: Vec<Value> = response.json().await.unwrap();
    assert_eq!(accounts.len(), 2);
    let reads: i64 = f
        .control
        .query_one(
            &format!("SELECT last_value FROM {}.lifecycle_reads", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        reads, 4,
        "two accounts need one active-alias scan and one batched lifecycle scan"
    );

    f.control.batch_execute(&format!(r#"
        CREATE OR REPLACE FUNCTION {s}.count_lifecycle(d TIMESTAMPTZ) RETURNS TIMESTAMPTZ LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'synthetic lifecycle read outage'; END $$;
    "#,s=f.schema)).await.unwrap();
    let response = f
        .http
        .get(format!("{}/v1/accounts", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "warm caches cannot hide lifecycle database errors"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "persistence_failed"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_removal_after_renewal_start_stops_native_polling_without_a_client_poll() {
    let f = login_fixture().await;
    let id = "bf".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    let pid: i32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stage: removal after admission must stop the native login child");
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 404);
    let other = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"other","billing":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_catalog_keeps_other_accounts_when_removal_commits_after_alias_listing() {
    let f = login_fixture().await;
    // Preserve the list statement's MVCC snapshot while removal commits. A
    // one-shot alias projection gates only the first listing of this account.
    f.control
        .batch_execute(&format!(
            r#"
        CREATE SEQUENCE {s}.catalog_selected;
        CREATE FUNCTION {s}.hold_first_alias(a TEXT) RETURNS TEXT LANGUAGE plpgsql AS $$
        BEGIN
            IF a='seat' AND (SELECT NOT is_called FROM {s}.catalog_selected) THEN
                PERFORM nextval('{s}.catalog_selected');
                PERFORM pg_advisory_xact_lock(hashtextextended('{s}',13275));
            END IF;
            RETURN a;
        END $$;
        ALTER TABLE {s}.central_accounts RENAME TO stored_accounts;
        CREATE VIEW {s}.central_accounts AS SELECT account_id,user_id,
            {s}.hold_first_alias(alias) AS alias,workspace,login,encrypted_vault,
            revision,deleted_at,updated_at FROM {s}.stored_accounts;
    "#,
            s = f.schema
        ))
        .await
        .unwrap();
    f.control
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock(hashtextextended('{}',13275))",
            f.schema
        ))
        .await
        .unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let bearer = f.token.clone();
    let pending = tokio::spawn(async move {
        http.get(format!("{url}/v1/accounts"))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(5), async {
        loop {
            let selected: bool = f
                .control
                .query_one(
                    &format!("SELECT is_called FROM {}.catalog_selected", f.schema),
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if selected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stage: active alias list must select seat before removal");
    f.control
        .batch_execute(&format!(
            "UPDATE {}.stored_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'; COMMIT",
            f.schema
        ))
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "removal must not fail the whole catalog"
    );
    let accounts: Vec<Value> = response.json().await.unwrap();
    assert!(!accounts.iter().any(|account| account["alias"] == "seat"));
    assert!(accounts.iter().any(|account| account["alias"] == "other"));
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_background_recovery_still_stops_an_unsettled_child_during_lifecycle_outage() {
    let mut f = login_fixture_with_expiry(false, false, false, 1).await;
    f.first.stop().await;
    store::atomic_write(&f.first.root.path().join("retry-clock"), b"100000").unwrap();
    f.first = Pod::spawn_with_recovery(&f.database, &f.key, f.first.root, "postgres", true).await;
    store::atomic_write(&f.first.root.path().join("mode"), b"billing-error-marked").unwrap();
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 503);
    store::atomic_write(
        &f.first.root.path().join("mode"),
        b"owner-native-hung-journal",
    )
    .unwrap();
    store::atomic_write(&f.first.root.path().join("retry-clock"), b"1000000").unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("native-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stage: background recovery must start its native probe");
    let pid: i32 = std::fs::read_to_string(f.first.root.path().join("native-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {}.central_accounts IN ACCESS EXCLUSIVE MODE",
            f.schema
        ))
        .await
        .unwrap();
    let stopped = timeout(Duration::from_secs(55), async {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    f.control.batch_execute("ROLLBACK").await.unwrap();
    assert!(
        stopped.is_ok(),
        "stage: unavailable lifecycle reads must not bypass bounded recovery settlement"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_removal_during_migration_verification_refuses_publication_and_keeps_evidence() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"owner-native-hold").unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let bearer = f.token.clone();
    let pending = tokio::spawn(async move {
        http.post(format!("{url}/v1/accounts"))
            .bearer_auth(bearer)
            .json(&json!({"alias":"seat","auth":renewal_grant()}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(5), async {
        while !f.first.root.path().join("native-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stage: migration verification must reach the native read");
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("native-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    store::atomic_write(&f.first.root.path().join("native-release"), b"1").unwrap();
    let response = timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.status(),
        404,
        "removed verification must preserve the lifecycle result"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "account_removed"
    );
    wait_dead(pid).await;
    let retained = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "seat"));
    assert!(retained.join("vault.enc").exists());
    assert!(retained.join("runtime/auth.json").exists());
    assert_eq!(f.first.launches(), 1);
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 404);
    assert_eq!(f.second.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_lifecycle_outage_cannot_keep_an_unsettled_child_past_lease_custody() {
    let f = login_fixture().await;
    store::atomic_write(
        &f.first.root.path().join("mode"),
        b"owner-native-hung-journal",
    )
    .unwrap();
    let response = timeout(
        Duration::from_secs(13),
        request(&f.http, &f.first, &f.token),
    )
    .await
    .expect("stage: foreground native deadline");
    assert_eq!(response.status(), 503);
    let pid: i32 = std::fs::read_to_string(f.first.root.path().join("native-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "unsettled child must still be live"
    );
    // Block both authority reads and lease renewal. No provider response or
    // token request can perform cleanup while the local custody clock expires.
    f.control
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {}.central_accounts IN ACCESS EXCLUSIVE MODE",
            f.schema
        ))
        .await
        .unwrap();
    let stopped = timeout(Duration::from_secs(125), async {
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    f.control.batch_execute("ROLLBACK").await.unwrap();
    assert!(
        stopped.is_ok(),
        "stage: metadata outage must not bypass custody expiry"
    );
    assert!(
        f.first
            .root
            .path()
            .join("state/accounts")
            .join(account_key("test", "seat"))
            .join("vault.enc")
            .exists()
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_removal_during_admission_refuses_token_and_renewal_without_starting_a_child() {
    for renewal in [false, true] {
        let f = login_fixture().await;
        // Exercise replacement of an existing lease too: an INSERT's foreign
        // key check alone does not protect the ON CONFLICT update path.
        f.control.batch_execute(&format!(
            "INSERT INTO {s}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) SELECT account_id,'prior-refresh',1,clock_timestamp()-interval '1 second',true FROM {s}.central_accounts WHERE alias='seat' ON CONFLICT(account_id) DO UPDATE SET expires_at=clock_timestamp()-interval '1 second',released=true",
            s=f.schema
        )).await.unwrap();
        f.control
        .batch_execute(&format!(
            "BEGIN; SELECT account_id FROM {s}.central_accounts WHERE alias='seat' FOR UPDATE; UPDATE {s}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
            s=f.schema
        ))
        .await
        .unwrap();
        let blocker: i32 = f
            .control
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let http = f.http.clone();
        let url = f.first.url.clone();
        let bearer = f.token.clone();
        let pending = tokio::spawn(async move {
            let (path, body) = if renewal {
                (
                    "/v1/relogin/start",
                    json!({"alias":"seat","id":"bc".repeat(32)}),
                )
            } else {
                ("/v1/token", json!({"alias":"seat","billing":false}))
            };
            http.post(format!("{url}{path}"))
                .bearer_auth(bearer)
                .json(&body)
                .send()
                .await
                .unwrap()
        });
        timeout(Duration::from_secs(4), async {
            loop {
                assert_eq!(f.first.launches(), 0, "native refresh started before removal committed, renewal={renewal}");
                assert!(!f.first.root.path().join("login-pid").exists(), "native login started before removal committed");
                let waiting: bool = f.control.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted AND $1=ANY(pg_blocking_pids(pid)))", &[&blocker]
                ).await.unwrap().get(0);
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("stage: request must wait for the same-account removal row before native work");
        assert_eq!(
            f.first.launches(),
            0,
            "pending removal must fence native refresh, renewal={renewal}"
        );
        assert!(!f.first.root.path().join("login-pid").exists());
        f.control.batch_execute("COMMIT").await.unwrap();
        let response = timeout(Duration::from_secs(4), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "removal must win admission, renewal={renewal}"
        );
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "account_removed"
        );
        assert_eq!(f.first.launches(), 0);
        assert!(!f.first.root.path().join("login-pid").exists());
        stop_fixture(f).await;
    }
}

#[tokio::test]
async fn postgres_removed_account_stops_deferred_settlement_and_releases_stopped_custody() {
    let f = login_fixture().await;
    f.control
        .batch_execute(&format!(
            r#"
        CREATE SEQUENCE {s}.publication_attempt;
        CREATE FUNCTION {s}.reject_publication() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.revision > OLD.revision THEN
                PERFORM nextval('{s}.publication_attempt');
                RAISE EXCEPTION 'synthetic publication failure';
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER reject_publication BEFORE UPDATE ON {s}.central_accounts
        FOR EACH ROW EXECUTE FUNCTION {s}.reject_publication();
    "#,
            s = f.schema
        ))
        .await
        .unwrap();
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 503);
    timeout(Duration::from_secs(8), async {
        loop {
            let attempts: i64 = f
                .control
                .query_one(
                    &format!("SELECT last_value FROM {}.publication_attempt", f.schema),
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if attempts >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stage: deferred publication must enter retry");
    let launches = f.first.launches();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    // Observe settlement without another token request performing cleanup.
    timeout(Duration::from_secs(5), async {
        loop {
            let released: bool = f
                .control
                .query_one(
                    &format!(
                        "SELECT released FROM {}.account_refresh_leases WHERE account_id=$1",
                        f.schema
                    ),
                    &[&account_key("test", "seat")],
                )
                .await
                .unwrap()
                .get(0);
            if released {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stage: removed settlement must stop and release proved-stopped custody");
    for pod in [&f.first, &f.second] {
        let response = request(&f.http, pod, &f.token).await;
        assert_eq!(response.status(), 404);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "account_removed"
        );
    }
    assert_eq!(f.first.launches(), launches);
    assert_eq!(f.second.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_removed_account_cannot_start_renewal_or_be_imported_from_stale_local_state() {
    let f = login_fixture().await;
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let renew = f
        .http
        .post(format!("{}/v1/relogin/start", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","id":"ae".repeat(32)}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        renew.status(),
        404,
        "renewal must refuse before device polling starts"
    );
    assert_eq!(
        renew.json::<Value>().await.unwrap()["error"],
        "account_removed"
    );
    let imported = f
        .http
        .post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","auth":renewal_grant()}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        imported.status(),
        404,
        "a stale local owner cannot re-import the removed account"
    );
    assert_eq!(
        imported.json::<Value>().await.unwrap()["error"],
        "account_removed"
    );
    assert_eq!(f.first.launches(), 0);
    assert!(!f.first.root.path().join("login-pid").exists());
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_removed_account_ends_a_lease_wait_without_waiting_for_expiry() {
    let f = enable_fast_path(login_fixture().await).await;
    let warm = request(&f.http, &f.first, &f.token).await;
    assert_eq!(warm.status(), 200);
    let warm = warm.json::<Value>().await.unwrap();
    let revision = warm["revision"].as_str().unwrap().to_owned();
    let account = account_key("test", "seat");
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET holder_id='foreign',epoch=epoch+1,expires_at=clock_timestamp()+interval '1 hour',released=false WHERE account_id=$1",f.schema),&[&account]).await.unwrap();
    f.control.batch_execute(&format!(
        "CREATE TABLE {s}.claim_attempts(account_id TEXT); \
         CREATE FUNCTION {s}.mark_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {s}.claim_attempts VALUES(NEW.account_id); RETURN NEW; END $$; \
         CREATE TRIGGER mark_claim BEFORE INSERT ON {s}.account_refresh_leases FOR EACH ROW EXECUTE FUNCTION {s}.mark_claim()",s=f.schema
    )).await.unwrap();
    let http = f.http.clone();
    let url = f.second.url.clone();
    let token = f.token.clone();
    let pending = tokio::spawn(async move {
        http.post(format!("{url}/v1/token"))
            .bearer_auth(token)
            .json(&json!({"alias":"seat","previousRevision":revision,"billing":true}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8), async {
        loop {
            let count: i64 = f
                .control
                .query_one(
                    &format!("SELECT count(*) FROM {}.claim_attempts", f.schema),
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if count > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stage: waiting caller must attempt the lease claim");
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(5), pending)
        .await
        .expect("stage: Removed must end the foreign-lease wait promptly")
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "account_removed"
    );
    assert_eq!(
        f.second.launches(),
        0,
        "a waiter cannot replace the foreign refresh child"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_removed_account_refuses_both_committed_token_fast_paths() {
    for vault in [false, true] {
        let f = if vault {
            login_fixture_in_database(false, false, false, 4102444800, true).await
        } else {
            login_fixture().await
        };
        let mut f = enable_fast_path(f).await;
        if vault {
            f.first.stop().await;
            store::atomic_write(&f.first.root.path().join("vault-fast-path"), b"1").unwrap();
            let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
            f.first = Pod::spawn(&f.database, &f.key, root, "postgres").await;
        }
        let warm = f
            .http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"seat","billing":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            warm.status(),
            200,
            "valid active fast-path account, vault={vault}"
        );
        let launches = f.first.launches();
        f.control.execute(&format!("UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",f.schema),&[]).await.unwrap();
        let response = f
            .http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"seat","billing":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "fast path must preserve Removed, vault={vault}"
        );
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "account_removed"
        );
        assert_eq!(f.first.launches(), launches);
        stop_fixture(f).await;
    }
}

#[tokio::test]
async fn postgres_removed_account_is_hidden_from_warm_catalog_and_stale_local_restart() {
    let mut f = login_fixture().await;
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    let stopped_evidence = std::fs::read(
        f.first
            .root
            .path()
            .join("state/accounts")
            .join(account_key("test", "seat"))
            .join("runtime/pid"),
    )
    .unwrap();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    for stage in [
        "warm",
        "warm unproven",
        "stopped restart",
        "unproven restart",
    ] {
        if stage == "warm unproven" {
            let home = f
                .first
                .root
                .path()
                .join("state/accounts")
                .join(account_key("test", "seat"))
                .join("runtime");
            store::atomic_write(&home.join("pid"), b"unreadable process evidence").unwrap();
        }
        if stage.ends_with("restart") {
            f.first.stop().await;
            if stage == "unproven restart" {
                let home = f
                    .first
                    .root
                    .path()
                    .join("state/accounts")
                    .join(account_key("test", "seat"))
                    .join("runtime");
                store::ensure_private_dir(&home).unwrap();
                store::atomic_write(&home.join("pid"), b"unreadable process evidence").unwrap();
            }
            let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
            f.first = Pod::spawn(&f.database, &f.key, root, "postgres").await;
            assert_eq!(
                f.first.launches(),
                0,
                "stale local vault cannot launch a removed account"
            );
        }
        let response = f
            .http
            .get(format!("{}/v1/accounts", f.first.url))
            .bearer_auth(&f.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let accounts: Vec<Value> = response.json().await.unwrap();
        assert!(
            !accounts.iter().any(|a| a["alias"] == "seat"),
            "removed account leaked from catalog, stage={stage}"
        );
        assert!(accounts.iter().any(|a| a["alias"] == "other"));
        if matches!(stage, "warm unproven" | "unproven restart") {
            let pid = f
                .first
                .root
                .path()
                .join("state/accounts")
                .join(account_key("test", "seat"))
                .join("runtime/pid");
            assert_eq!(std::fs::read(pid).unwrap(), b"unreadable process evidence");
            if stage == "warm unproven" {
                // Remove this stage's injected corruption so the next stage
                // still proves a restart with clean stopped-process evidence.
                let pid = f
                    .first
                    .root
                    .path()
                    .join("state/accounts")
                    .join(account_key("test", "seat"))
                    .join("runtime/pid");
                store::atomic_write(&pid, &stopped_evidence).unwrap();
            }
        }
    }
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT deleted_at IS NOT NULL FROM {}.central_accounts WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0), "startup must preserve the tombstone");
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_removed_account_refuses_a_warm_owner_without_disabling_other_accounts() {
    let f = login_fixture().await;
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    let launches = f.first.launches();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE alias='seat'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let response = request(&f.http, &f.first, &f.token).await;
    assert_eq!(
        response.status(),
        404,
        "a removed account is a definitive refusal"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "account_removed"
    );
    assert_eq!(
        f.first.launches(),
        launches,
        "removal cannot launch a refresh child"
    );
    let other = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"other","billing":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 200, "removal is scoped to one account");
    stop_fixture(f).await;
}
