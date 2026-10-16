//! Removal is a company-user action; manage reads only scoped account metadata.
use super::*;

async fn form(
    f: &BrowserFixture,
    session: &str,
    id: &str,
    revision: i64,
    alias: &str,
    attempt: u8,
) -> Vec<(String, String)> {
    vec![
        ("alias".into(), alias.into()),
        ("confirmation".into(), alias.into()),
        ("account_id".into(), id.into()),
        ("revision".into(), revision.to_string()),
        ("attempt".into(), format!("{attempt:02x}").repeat(32)),
        ("form_token".into(), f.form_token(session).await),
    ]
}

async fn submit(
    f: &BrowserFixture,
    session: &str,
    input: &[(String, String)],
) -> reqwest::Response {
    f.http
        .post(format!("{}/accounts/remove", f.first.url))
        .header("cookie", session)
        .header("origin", "http://127.0.0.1:8787")
        .form(input)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn postgres_dashboard_remove_browser_refusal_is_safe_html() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    let (id, revision) = seed(&f, "HTML Seat").await;
    f.enable_browser_remove().await;
    let mut input = form(&f, &session, &id, revision, "HTML Seat", 51).await;
    input
        .iter_mut()
        .find(|(k, _)| k == "confirmation")
        .unwrap()
        .1 = "wrong".into();
    let response = f
        .http
        .post(format!("{}/accounts/remove", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .header("accept", "text/html")
        .form(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("Account could not be removed"),
        "browser refusals must explain the action in HTML"
    );
    assert!(!body.contains("synthetic-remove") && !body.contains(&session));
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_rechecks_session_and_freshness_after_audit_wait() {
    for (change, status) in [
        (
            "expires_at=clock_timestamp()+interval '50 milliseconds'",
            401,
        ),
        (
            "signed_in_at=clock_timestamp()-interval '629.95 seconds'",
            303,
        ),
    ] {
        let mut f = BrowserFixture::start().await;
        f.identity(Some(chrono::Utc::now().timestamp()));
        let (session, _) = f.sign_in().await;
        let (id, revision) = seed(&f, "Expiring Seat").await;
        f.enable_browser_remove().await;
        // Start with valid authority. At the actual final audit write, place its
        // deadline in the future and wait past it inside that same transaction.
        // This avoids racing request delivery or the test host's load.
        f.control.batch_execute(&format!("CREATE FUNCTION {s}.expire_remove_authority() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='account_remove' AND NEW.result='ok' THEN UPDATE {s}.browser_sessions SET {change}; PERFORM pg_sleep(0.1); END IF; RETURN NEW; END $$; CREATE TRIGGER expire_remove_authority BEFORE INSERT ON {s}.dashboard_audit FOR EACH ROW EXECUTE FUNCTION {s}.expire_remove_authority()",s=f.schema)).await.unwrap();
        let response = submit(
            &f,
            &session,
            &form(&f, &session, &id, revision, "Expiring Seat", 61).await,
        )
        .await;
        assert_eq!(
            response.status(),
            status,
            "expired authority must refuse the commit"
        );
        let row=f.control.query_one(&format!("SELECT deleted_at IS NOT NULL,(SELECT count(*) FROM {s}.dashboard_audit WHERE operation='account_remove' AND result='ok') FROM {s}.central_accounts WHERE account_id=$1",s=f.schema), &[&id]).await.unwrap();
        assert!(!row.get::<_, bool>(0));
        assert_eq!(row.get::<_, i64>(1), 0);
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_dashboard_remove_row_fences_queued_tokens_even_after_browser_disconnect() {
    for disconnect in [false, true] {
        let mut f = BrowserFixture::start().await;
        f.identity(Some(chrono::Utc::now().timestamp()));
        let (session, _) = f.sign_in().await;
        let (id, revision) = seed(&f, "Race Seat").await;
        f.enable_browser_remove().await;
        f.control.batch_execute(&format!("CREATE FUNCTION {s}.hold_tombstone() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.deleted_at IS NOT NULL AND OLD.deleted_at IS NULL THEN PERFORM pg_advisory_xact_lock(hashtextextended(current_schema(),13275)); END IF; RETURN NEW; END $$; CREATE TRIGGER hold_tombstone BEFORE UPDATE ON {s}.central_accounts FOR EACH ROW EXECUTE FUNCTION {s}.hold_tombstone()",s=f.schema)).await.unwrap();
        f.control
            .query_one(
                "SELECT pg_advisory_lock(hashtextextended($1,13275))",
                &[&f.schema],
            )
            .await
            .unwrap();
        let input = form(&f, &session, &id, revision, "Race Seat", 41).await;
        let post = tokio::spawn(
            f.http
                .post(format!("{}/accounts/remove", f.first.url))
                .header("cookie", &session)
                .header("origin", "http://127.0.0.1:8787")
                .form(&input)
                .send(),
        );
        timeout(Duration::from_secs(8),async {
            loop {
                let waiting:bool=f.control.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND query LIKE 'UPDATE central_accounts SET deleted_at=%' AND wait_event='advisory')", &[&f.schema]).await.unwrap().get(0);
                if waiting {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("Remove must reach its account-row-locked tombstone stage");
        let launches = f.second.launches();
        let mut token = Box::pin(
            f.http
                .post(format!("{}/v1/token", f.second.url))
                .bearer_auth("synthetic-remove-machine-token")
                .json(&json!({"alias":"Race Seat"}))
                .send(),
        );
        let pending = timeout(Duration::from_millis(200), token.as_mut())
            .await
            .is_err();
        let no_launch = f.second.launches() == launches;
        if disconnect {
            post.abort();
        }
        f.control
            .query_one(
                "SELECT pg_advisory_unlock(hashtextextended($1,13275))",
                &[&f.schema],
            )
            .await
            .unwrap();
        assert!(
            pending,
            "queued token must wait for the actual Remove writer"
        );
        assert!(
            no_launch,
            "no native work may start while Remove holds the account row"
        );
        let token = timeout(Duration::from_secs(8), token)
            .await
            .expect("queued token result deadline")
            .unwrap();
        assert_eq!(token.status(), 404);
        if !disconnect {
            assert_eq!(post.await.unwrap().unwrap().status(), 200);
        }
        assert_eq!(
            submit(&f, &session, &input).await.status(),
            200,
            "durable retry resolves even after client cancellation"
        );
        assert_eq!(f.second.launches(), launches);
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_dashboard_remove_refuses_loans_pending_logins_and_unreleased_foreign_custody() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    let (id, revision) = seed(&f, "Busy Seat").await;
    f.enable_browser_remove().await;
    for (n, expiry) in [(21, "interval '1 minute'"), (22, "interval '-1 minute'")] {
        f.control.batch_execute(&format!("INSERT INTO {s}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES('{id}','other-replica',77,clock_timestamp()+{expiry},false) ON CONFLICT(account_id) DO UPDATE SET holder_id='other-replica',epoch=77,expires_at=EXCLUDED.expires_at,released=false",s=f.schema)).await.unwrap();
        let response = submit(
            &f,
            &session,
            &form(&f, &session, &id, revision, "Busy Seat", n).await,
        )
        .await;
        assert_eq!(response.status(), 503);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "refresh_in_progress",
            "expiry alone is not stop proof"
        );
    }
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=true WHERE account_id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap();
    f.control.execute(&format!("INSERT INTO {}.account_loans(id,account_id,lender_id,borrower_id,lender_email,borrower_email,alias,reference,subject_workspace,created_at,ends_at) VALUES('blocking-loan',$1,'lender','borrower','lender@example.invalid','borrower@example.invalid','Busy Seat','lender/seat','remove-workspace',1,4102444800)",f.schema), &[&id]).await.unwrap();
    let response = submit(
        &f,
        &session,
        &form(&f, &session, &id, revision, "Busy Seat", 23).await,
    )
    .await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "account_lent"
    );
    f.control
        .batch_execute(&format!(
            "UPDATE {}.account_loans SET ended_at=2,end_reason='returned'",
            f.schema
        ))
        .await
        .unwrap();
    let login_id = "ee".repeat(32);
    let login = f
        .http
        .post(format!("{}/v1/relogin/start", f.first.url))
        .bearer_auth("synthetic-remove-machine-token")
        .json(&json!({"alias":"Busy Seat","id":login_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let response = submit(
        &f,
        &session,
        &form(&f, &session, &id, revision, "Busy Seat", 24).await,
    )
    .await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "login_pending"
    );
    let cancel = f
        .http
        .post(format!("{}/v1/relogin/cancel", f.first.url))
        .bearer_auth("synthetic-remove-machine-token")
        .json(&json!({"alias":"Busy Seat","id":login_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    assert!(
        !f.control
            .query_one(
                &format!(
                    "SELECT deleted_at IS NOT NULL FROM {}.central_accounts WHERE account_id=$1",
                    f.schema
                ),
                &[&id]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_audit_failure_rolls_back_tombstone_and_keeps_account_usable() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    let (id, revision) = seed(&f, "Audit Seat").await;
    f.enable_browser_remove().await;
    f.control.batch_execute(&format!("CREATE FUNCTION {s}.fail_remove_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='account_remove' AND NEW.result='ok' THEN RAISE EXCEPTION 'synthetic audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_remove_audit BEFORE INSERT ON {s}.dashboard_audit FOR EACH ROW EXECUTE FUNCTION {s}.fail_remove_audit()",s=f.schema)).await.unwrap();
    let response = submit(
        &f,
        &session,
        &form(&f, &session, &id, revision, "Audit Seat", 31).await,
    )
    .await;
    assert_eq!(response.status(), 503);
    let row=f.control.query_one(&format!("SELECT deleted_at IS NOT NULL,revision,(SELECT count(*) FROM {s}.dashboard_audit WHERE operation='account_remove' AND result='ok') FROM {s}.central_accounts WHERE account_id=$1",s=f.schema), &[&id]).await.unwrap();
    assert!(!row.get::<_, bool>(0));
    assert_eq!(row.get::<_, i64>(1), revision);
    assert_eq!(row.get::<_, i64>(2), 0);
    for pod in [&f.first, &f.second] {
        let token = f
            .http
            .post(format!("{}/v1/token", pod.url))
            .bearer_auth("synthetic-remove-machine-token")
            .json(&json!({"alias":"Audit Seat"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            token.status(),
            200,
            "a refused Remove must leave normal service usable"
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_manage_link_respects_its_own_gate() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    seed(&f, "Manage Seat").await;
    let page = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("/accounts/manage"));
    f.enable_browser_remove().await;
    let page = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("/accounts/manage"),
        "Remove must be discoverable independently of Add/Renew"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_guards_confirmation_session_and_origin() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    let (id, revision) = seed(&f, "Guard Seat").await;
    f.enable_browser_remove().await;
    for (n, key, value, status) in [
        (1, "confirmation", "guard seat", 409),
        (2, "revision", "999999", 409),
        (3, "account_id", "foreign-or-missing", 404),
        (4, "form_token", "invalid", 403),
    ] {
        let mut input = form(&f, &session, &id, revision, "Guard Seat", n).await;
        input.iter_mut().find(|(k, _)| k == key).unwrap().1 = value.into();
        let response = f
            .http
            .post(format!("{}/accounts/remove", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{key}");
    }
    for origin in [None, Some("null"), Some("https://foreign.invalid")] {
        let input = form(&f, &session, &id, revision, "Guard Seat", 5).await;
        let mut request = f
            .http
            .post(format!("{}/accounts/remove", f.first.url))
            .header("cookie", &session)
            .form(&input);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        assert_eq!(request.send().await.unwrap().status(), 403);
    }
    let input = form(&f, &session, &id, revision, "Guard Seat", 6).await;
    assert_eq!(
        f.http
            .post(format!("{}/accounts/remove", f.first.url))
            .bearer_auth("synthetic-remove-machine-token")
            .header("origin", "http://127.0.0.1:8787")
            .form(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403,
        "machine bearer cannot remove"
    );
    for freshness in [
        "auth_time_verified=false",
        "auth_time_verified=true,signed_in_at=clock_timestamp()-interval '20 minutes'",
    ] {
        f.control
            .batch_execute(&format!(
                "UPDATE {}.browser_sessions SET {freshness}",
                f.schema
            ))
            .await
            .unwrap();
        let response = f
            .http
            .post(format!("{}/accounts/remove", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&form(&f, &session, &id, revision, "Guard Seat", 7).await)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            303,
            "unverified or old auth_time requires fresh sign-in"
        );
        assert_eq!(
            response.headers()["location"],
            "/accounts/sign-in?fresh=true"
        );
    }
    assert!(
        !f.control
            .query_one(
                &format!(
                    "SELECT deleted_at IS NOT NULL FROM {}.central_accounts WHERE account_id=$1",
                    f.schema
                ),
                &[&id]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    f.finish().await;
}

async fn seed(f: &BrowserFixture, alias: &str) -> (String, i64) {
    f.machine("remove-machine", "synthetic-remove-machine-token")
        .await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let response = f
        .http
        .post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-remove-machine-token")
        .json(
            &json!({"alias":alias,"auth":add_grant("remove-workspace",Some("remove-login"),None)}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT account_id,revision FROM {}.central_accounts WHERE alias=$1",
                f.schema
            ),
            &[&alias],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn postgres_dashboard_remove_restart_stops_retained_old_runtime() {
    let mut f = BrowserFixture::start().await;
    let _ = f.sign_in().await;
    let (id, _) = seed(&f, "Old Runtime").await;
    f.first.stop().await;
    let home = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(&id)
        .join("runtime");
    let auth = std::fs::read(home.join("auth.json")).unwrap();
    let mut old = Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = old.id().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let incarnation = format!(
        "{}:{}:{}",
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim(),
        std::fs::read_link(format!("/proc/{pid}/ns/pid"))
            .unwrap()
            .display(),
        fields[19]
    );
    let marker = serde_json::to_vec(&json!({"pid":pid,"incarnation":incarnation})).unwrap();
    store::atomic_write(&home.join("pid"), &marker).unwrap();
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_accounts SET deleted_at=clock_timestamp() WHERE account_id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap();
    let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
    f.first = Pod::spawn(&f.database(), &f._root.path().join("key"), root, "postgres").await;
    let stopped = timeout(Duration::from_secs(2), old.wait()).await.is_ok();
    if !stopped {
        old.kill().await.unwrap();
    }
    assert!(
        stopped,
        "startup must stop the retained runtime of a removed account"
    );
    assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), auth);
    assert_eq!(std::fs::read(home.join("pid")).unwrap(), marker);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_tombstones_once_and_both_replicas_refuse_tokens() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    let (id, revision) = seed(&f, "Remove Seat").await;
    f.enable_browser_remove().await;
    let request = [
        ("alias", "Remove Seat".to_owned()),
        ("confirmation", "Remove Seat".to_owned()),
        ("account_id", id.clone()),
        ("revision", revision.to_string()),
        ("attempt", "ab".repeat(32)),
        ("form_token", token),
    ];
    for _ in 0..2 {
        let response = f
            .http
            .post(format!("{}/accounts/remove", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "confirmed removal and its exact retry succeed"
        );
    }
    for pod in [&f.first, &f.second] {
        let response = f
            .http
            .post(format!("{}/v1/token", pod.url))
            .bearer_auth("synthetic-remove-machine-token")
            .json(&json!({"alias":"Remove Seat"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "removed account is unavailable on every replica"
        );
    }
    let row=f.control.query_one(&format!("SELECT deleted_at IS NOT NULL,(SELECT count(*) FROM {s}.dashboard_audit WHERE operation='account_remove' AND result='ok') FROM {s}.central_accounts WHERE account_id=$1",s=f.schema), &[&id]).await.unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(
        row.get::<_, i64>(1),
        1,
        "tombstone and audit are idempotent"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_disabled_post_is_refused_and_audited() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    let response = f
        .http
        .post(format!("{}/accounts/remove", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "missing"),
            ("account_id", "synthetic-account"),
            ("revision", "1"),
            ("confirmation", "missing"),
            (
                "attempt",
                "acacacacacacacacacacacacacacacacacacacacacacacacacacacacacacacac",
            ),
            ("form_token", token.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "disabled Remove must refuse before account lookup"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT operation,result,target FROM {}.dashboard_audit",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "account_remove");
    assert_eq!(row.get::<_, String>(1), "refused:dashboard_remove_disabled");
    assert!(row.get::<_, String>(2).starts_with("unresolved:"));
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_remove_manage_is_scoped_and_metadata_only() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("remove-machine", "synthetic-remove-machine-token")
        .await;
    let alias = "Remove & Scoped";
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let seeded = f
        .http
        .post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-remove-machine-token")
        .json(
            &json!({"alias":alias,"auth":add_grant("remove-workspace",Some("remove-login"),None)}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(seeded.status(), 200);
    f.enable_browser_remove().await;
    assert_eq!(
        f.control
            .execute(
                &format!(
                    "UPDATE {}.central_accounts SET encrypted_vault=$2 WHERE alias=$1",
                    f.schema
                ),
                &[&alias, &vec![0u8]],
            )
            .await
            .unwrap(),
        1
    );
    let page = f
        .http
        .get(format!("{}/accounts/manage", f.first.url))
        .query(&[("alias", "remove & scoped")])
        .header("cookie", &session)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        200,
        "manage must read metadata without decrypting credentials"
    );
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = page.text().await.unwrap();
    assert!(html.contains("Remove &amp; Scoped"));
    for field in [
        "form_token",
        "attempt",
        "account_id",
        "revision",
        "confirmation",
    ] {
        assert!(
            html.contains(&format!("name=\"{field}\"")),
            "missing {field}"
        );
    }
    assert!(html.contains("does not revoke"));
    f.control.batch_execute(&format!(
        "INSERT INTO {s}.central_users(id,email,enabled) VALUES('foreign-remove','foreign@example.invalid',true); \
         UPDATE {s}.central_accounts SET user_id='foreign-remove' WHERE alias='Remove & Scoped'",s=f.schema
    )).await.unwrap();
    let foreign = f
        .http
        .get(format!("{}/accounts/manage", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 404);
    f.control.batch_execute(&format!(
        "UPDATE {s}.central_accounts SET user_id=(SELECT id FROM {s}.central_users WHERE email='dashboard@example.invalid'), \
         deleted_at=clock_timestamp() WHERE alias='Remove & Scoped'",s=f.schema
    )).await.unwrap();
    let removed = f
        .http
        .get(format!("{}/accounts/manage", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 404);
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}
