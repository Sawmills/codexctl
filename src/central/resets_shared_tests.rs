//! Shared resets are exercised through the existing HTTP handlers and real PostgreSQL.
use super::*;
use std::time::Duration;

async fn postgres_fixture(base: &str) -> (Fixture, Arc<tokio_postgres::Client>, String) {
    use crate::central::storage::{CentralStore, StoreMode, test_support::bootstrap};
    let mut f = Fixture::start(base).await;
    let initial = CentralStore::from_mode(StoreMode::Postgres, &f.broker.state, &f.broker.key)
        .await
        .unwrap();
    let (central, control, schema) = initial.isolated_test_schema().await.unwrap();
    bootstrap(&central).await.unwrap();
    for user in users(&f.broker.state).unwrap() {
        central
            .save_registry_entity_cas("users", &user.id, &serde_json::to_vec(&user).unwrap(), None)
            .await
            .unwrap();
    }
    for device in vault::devices(&f.broker.state).unwrap() {
        central
            .save_device_entity_cas(
                &device.tenant,
                &device.id,
                &serde_json::to_vec(&device).unwrap(),
                None,
            )
            .await
            .unwrap();
    }
    for (_, owner) in f.broker.owners.read().await.values() {
        let record = Broker::owner_record(&*owner.lock().await).unwrap();
        central.save_account(&record).await.unwrap();
        // The fixture's native import has already proved its workspace route.
        let revision = vault::digest(&serde_json::to_vec(&record.vault["auth"]).unwrap());
        control.execute("INSERT INTO central_token_evidence(account_id,account_revision,auth_revision,routing_supported,routing_observed_at) VALUES($1,$2,$3,true,clock_timestamp())",&[&record.account_id,&record.revision,&revision]).await.unwrap();
    }
    f.broker.central = Some(central);
    (f, control, schema)
}

#[tokio::test]
async fn postgres_reset_requires_current_workspace_routing_before_any_provider_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let app=Router::new()
        .route("/usage",get(||async {Json(json!({"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}}))}))
        .route("/credits",get(||async {Json(json!({"credits":[{"id":"only","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}))}))
        .route("/credits/consume",post(||async {Json(json!({"code":"reset","windows_reset":1}))}))
        .layer(axum::middleware::from_fn(move |request:axum::extract::Request,next:axum::middleware::Next|{
            counted.fetch_add(1,Ordering::SeqCst); async move {next.run(request).await}
        }));
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for mutation in [
        "DELETE FROM central_token_evidence",
        "UPDATE central_token_evidence SET routing_supported=false",
        "UPDATE central_token_evidence SET routing_observed_at=clock_timestamp()-interval '61 seconds'",
        "UPDATE central_token_evidence SET auth_revision='another-credential'",
        "UPDATE central_token_evidence SET account_revision=account_revision+1",
        "UPDATE central_token_evidence SET deleted_at=clock_timestamp()",
    ] {
        let (fixture, control, schema) = postgres_fixture(&base).await;
        control.batch_execute(mutation).await.unwrap();
        calls.store(0, Ordering::SeqCst);
        let reset = fixture.redeem("unproved-workspace").await;
        let (_, inventory) = fixture.list().await;
        let count = calls.load(Ordering::SeqCst);
        fixture.stop().await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        assert_eq!(
            reset.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace routing must be positively proved ({mutation}): {}",
            reset.1
        );
        assert_eq!(reset.1["error"], "reset_routing_unverified");
        assert_eq!(inventory["accounts"][0]["error"], "reset_read_failed");
        assert_eq!(
            count, 0,
            "unproved routing must not read usage, credits, or consume"
        );
    }
    upstream.abort();
}

#[tokio::test]
async fn postgres_reset_migration_keeps_the_existing_dashboard_audit_contract() {
    let (base, _, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    control.batch_execute("ALTER TABLE dashboard_audit DROP CONSTRAINT dashboard_audit_operation_check; ALTER TABLE dashboard_audit ADD CONSTRAINT dashboard_audit_operation_check CHECK(operation IN ('account_add','account_renew','account_remove','machine_revoke'))").await.unwrap();
    let constraint = "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='dashboard_audit'::regclass AND conname='dashboard_audit_operation_check'";
    let before: String = control.query_one(constraint, &[]).await.unwrap().get(0);
    for _ in 0..2 {
        crate::central::storage::test_support::bootstrap(fixture.broker.central.as_ref().unwrap())
            .await
            .unwrap();
    }
    let after: String = control.query_one(constraint, &[]).await.unwrap().get(0);
    let additive: bool = control
        .query_one("SELECT to_regclass('reset_audit') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        after, before,
        "repeated migration must preserve the established audit constraint"
    );
    assert!(additive, "reset audit uses a new additive table");
}

#[tokio::test]
async fn postgres_reset_inventory_lists_owned_accounts_without_spending() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let response =
        crate::central::resets::list(State(fixture.broker.clone()), fixture.headers.clone())
            .await
            .unwrap_or_else(IntoResponse::into_response);
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(status, StatusCode::OK, "shared reset inventory: {body}");
    assert_eq!(body["accounts"][0]["alias"], "personal");
    assert_eq!(body["accounts"][0]["applicable"], 1);
    assert!(
        received.lock().unwrap().is_empty(),
        "listing must never spend a reset"
    );
}

#[tokio::test]
async fn postgres_reset_redeem_keeps_its_receipt_across_replicas() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let first = fixture.redeem("shared-retry").await;
    for _ in 0..2 {
        crate::central::storage::test_support::bootstrap(fixture.broker.central.as_ref().unwrap())
            .await
            .unwrap();
    }
    let mut broker = fixture.broker.clone();
    broker.owners = Arc::default();
    broker.reset_reader = crate::central::resets::Reader::with_base(&base);
    broker.holder_id = "second-reset-replica".into();
    let second = Fixture {
        _root: tempfile::tempdir().unwrap(),
        broker,
        headers: fixture.headers.clone(),
    };
    let retry = second.redeem("shared-retry").await;
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(first.0, StatusCode::OK, "shared redemption: {}", first.1);
    assert_eq!(first.1, json!({"code":"reset","windows_reset":1}));
    assert_eq!(retry.0, StatusCode::OK);
    assert_eq!(retry.1["code"], "already_redeemed");
    let calls = received.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "another replica must return the durable receipt"
    );
    assert_eq!(calls[0]["credit_id"], "soon");
}

#[tokio::test]
async fn postgres_reset_completed_receipt_survives_saved_token_expiry() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let (base, received, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let first = fixture.redeem("receipt-after-expiry").await;
    let central = fixture.broker.central.as_ref().unwrap();
    let mut record = central
        .load_account_by_alias("test-user", "personal")
        .await
        .unwrap()
        .unwrap();
    let token = record.vault["auth"]["tokens"]["access_token"]
        .as_str()
        .unwrap();
    let mut claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    claims["exp"] = json!(1);
    record.vault["auth"]["tokens"]["access_token"] = json!(format!(
        "header.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    ));
    record.revision += 1;
    central.save_account(&record).await.unwrap();
    let replay = fixture.redeem("receipt-after-expiry").await;
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(
        replay.0,
        StatusCode::OK,
        "a stored receipt needs caller authority, not a live provider token: {}",
        replay.1
    );
    assert_eq!(replay.1["code"], "already_redeemed");
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn postgres_reset_ambiguous_retry_joins_requests_across_replicas() {
    let (base, received, upstream) = redemption_upstream(1).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let first = fixture.redeem("uncertain-original").await;
    let central = fixture.broker.central.as_ref().unwrap();
    // An unknown outcome retains the key, not active custody. Refresh or Renew
    // must still be able to repair an expired credential before a later retry.
    let lease = central
        .acquire_lease(
            &account_key("test-user", "personal"),
            "repair-after-unknown",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    central.release_lease(&lease).await.unwrap();
    let mut broker = fixture.broker.clone();
    broker.owners = Arc::default();
    broker.holder_id = "retry-reset-replica".into();
    let other = Fixture {
        _root: tempfile::tempdir().unwrap(),
        broker,
        headers: fixture.headers.clone(),
    };
    let retry = other.redeem("uncertain-joined").await;
    let replay = other.redeem("uncertain-original").await;
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(first.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        retry.0,
        StatusCode::OK,
        "retry must keep the ambiguous intent: {}",
        retry.1
    );
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["code"], "already_redeemed");
    let calls = received.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["credit_id"], calls[1]["credit_id"]);
    assert_eq!(calls[0]["redeem_request_id"], calls[1]["redeem_request_id"]);
}

#[tokio::test]
async fn postgres_reset_refuses_foreign_unreleased_refresh_custody() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let central = fixture.broker.central.as_ref().unwrap();
    let id = account_key("test-user", "personal");
    let lease = central
        .acquire_lease(&id, "foreign-holder", Duration::from_secs(60))
        .await
        .unwrap();
    control.execute("UPDATE account_refresh_leases SET expires_at=now()-interval '1 minute' WHERE account_id=$1",&[&id]).await.unwrap();
    let result = fixture.redeem("foreign-custody").await;
    central.release_lease(&lease).await.unwrap();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        result.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "unreleased custody: {}",
        result.1
    );
    assert_eq!(result.1["error"], "refresh_in_progress");
    assert!(received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn postgres_reset_refuses_pending_renewal_before_refresh_custody() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let account = account_key("test-user", "personal");
    control.execute(
        "INSERT INTO central_login_operations(user_id,id,account_id,alias,device_id,phase,holder_id,expires_at,encrypted_payload,kind) VALUES('test-user','pending-renewal',$1,'personal','machine','pending','renewal-holder',clock_timestamp()+interval '30 seconds','\\x00','renewal')",
        &[&account],
    ).await.unwrap();
    let result = fixture.redeem("reset-during-renewal").await;
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        result.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "pending renewal must exclude redemption: {}",
        result.1
    );
    assert_eq!(result.1["error"], "login_pending");
    assert!(
        received.lock().unwrap().is_empty(),
        "a pending renewal already owns this account workflow"
    );
}

async fn browser_server(f: &mut Fixture, base: &str) -> (String, tokio::task::JoinHandle<()>) {
    f.broker.sso = Some(Arc::new(crate::central::enrollment::Sso::testing_session(
        "test-user",
    )));
    f.broker.catalog = Arc::new(catalog::Reader::testing(
        format!("{base}/usage"),
        Duration::from_secs(1),
    ));
    assert!(
        f.broker
            .central
            .as_ref()
            .unwrap()
            .create_browser_session(
                "synthetic-session",
                "test-user",
                Some(chrono::Utc::now().timestamp()),
                Duration::from_secs(3600)
            )
            .await
            .unwrap()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::central::dashboard::routes("http://127.0.0.1").with_state(f.broker.clone());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

#[tokio::test]
async fn postgres_dashboard_reset_gate_is_off_for_get_and_post() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::new();
    let get = http
        .get(format!("{url}/accounts/reset?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap();
    let post = http
        .post(format!("{url}/accounts/reset"))
        .header("cookie", "codexctl-session=synthetic-session")
        .header("origin", "http://127.0.0.1")
        .form(&[
            ("alias", "personal"),
            ("id", "off-attempt"),
            ("form_token", "bad"),
        ])
        .send()
        .await
        .unwrap();
    let statuses = (get.status(), post.status());
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        statuses,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE
        )
    );
    assert!(received.lock().unwrap().is_empty());
}

fn hidden(html: &str, name: &str) -> String {
    html.split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .unwrap_or("")
        .split('"')
        .next()
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn postgres_dashboard_reset_confirms_and_applies_without_javascript() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    fixture.broker.browser_resets_enabled = true;
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let get = http
        .get(format!("{url}/accounts/reset?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap();
    let status = get.status();
    let html = get.text().await.unwrap();
    let mut results = Vec::new();
    if status == StatusCode::OK {
        let input = [
            ("alias", "personal".to_owned()),
            ("id", hidden(&html, "id")),
            ("form_token", hidden(&html, "form_token")),
        ];
        for _ in 0..2 {
            let response = http
                .post(format!("{url}/accounts/reset"))
                .header("cookie", "codexctl-session=synthetic-session")
                .header("origin", "http://127.0.0.1")
                .form(&input)
                .send()
                .await
                .unwrap();
            results.push((response.status(), response.text().await.unwrap()));
        }
    }
    let audit:i64=control.query_one("SELECT count(*) FROM reset_audit WHERE operation='account_reset' AND user_id='test-user' AND target=$1 AND result='ok:reset'",&[&account_key("test-user","personal")]).await.unwrap().get(0);
    let central = fixture.broker.central.as_ref().unwrap();
    for _ in 0..2 {
        central
            .dashboard_audit(
                crate::central::storage::DashboardAudit {
                    operation: crate::central::storage::DashboardOperation::MachineRevoke,
                    actor: "synthetic@sawmills.ai",
                    attempt: "existing-dashboard-event",
                },
                "fixture-machine",
                "ok",
            )
            .await
            .unwrap();
    }
    let history: Vec<String> = control.query("SELECT operation FROM dashboard_audit_history WHERE deleted_at IS NULL ORDER BY operation", &[])
        .await.unwrap().into_iter().map(|row| row.get(0)).collect();
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(status, StatusCode::OK, "confirmation: {html}");
    assert!(html.contains("Apply a banked reset"));
    assert!(!html.contains("synthetic-session") && !html.contains("synthetic-refresh"));
    for (status, body) in results {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Reset applied"));
    }
    assert_eq!(received.lock().unwrap().len(), 1);
    assert_eq!(
        history,
        ["account_reset", "machine_revoke"],
        "combined history includes each retained operation once"
    );
    assert_eq!(
        audit, 1,
        "reset receipt and one audit must persist together"
    );
}

#[tokio::test]
async fn postgres_dashboard_reset_rechecks_authority_after_journal_wait() {
    for (change, want) in [
        (
            "signed_in_at=clock_timestamp()-interval '631 seconds'",
            StatusCode::SEE_OTHER,
        ),
        (
            "expires_at=clock_timestamp()-interval '1 second'",
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let (base, received, upstream) = redemption_upstream(0).await;
        let (mut fixture, control, schema) = postgres_fixture(&base).await;
        fixture.broker.browser_resets_enabled = true;
        let (url, server) = browser_server(&mut fixture, &base).await;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let html = http
            .get(format!("{url}/accounts/reset?alias=personal"))
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        control.batch_execute(&format!("CREATE FUNCTION expire_reset_authority() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE browser_sessions SET {change}; RETURN NEW; END $$; CREATE TRIGGER expire_reset_authority BEFORE INSERT ON account_reset_journals FOR EACH ROW EXECUTE FUNCTION expire_reset_authority()")).await.unwrap();
        let response = http
            .post(format!("{url}/accounts/reset"))
            .header("cookie", "codexctl-session=synthetic-session")
            .header("origin", "http://127.0.0.1")
            .form(&[
                ("alias", "personal".to_owned()),
                ("id", hidden(&html, "id")),
                ("form_token", hidden(&html, "form_token")),
            ])
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        server.abort();
        fixture.stop().await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        upstream.abort();
        assert_eq!(status, want, "authority expired at reservation: {body}");
        assert!(
            received.lock().unwrap().is_empty(),
            "expired authority must not spend"
        );
    }
}

#[tokio::test]
async fn postgres_dashboard_reset_audit_failure_retains_the_pending_key() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    fixture.broker.browser_resets_enabled = true;
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::new();
    let html = http
        .get(format!("{url}/accounts/reset?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let input = [
        ("alias", "personal".to_owned()),
        ("id", hidden(&html, "id")),
        ("form_token", hidden(&html, "form_token")),
    ];
    control.batch_execute("CREATE FUNCTION reject_reset_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='account_reset' THEN RAISE EXCEPTION 'synthetic reset audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_reset_audit BEFORE INSERT ON reset_audit FOR EACH ROW EXECUTE FUNCTION reject_reset_audit()").await.unwrap();
    let first = http
        .post(format!("{url}/accounts/reset"))
        .header("cookie", "codexctl-session=synthetic-session")
        .header("origin", "http://127.0.0.1")
        .form(&input)
        .send()
        .await
        .unwrap();
    let first_status = first.status();
    control
        .batch_execute("DROP TRIGGER reject_reset_audit ON reset_audit")
        .await
        .unwrap();
    let mut retries = Vec::new();
    for _ in 0..2 {
        let response = http
            .post(format!("{url}/accounts/reset"))
            .header("cookie", "codexctl-session=synthetic-session")
            .header("origin", "http://127.0.0.1")
            .form(&input)
            .send()
            .await
            .unwrap();
        retries.push((response.status(), response.text().await.unwrap()));
    }
    let audits: i64 = control
        .query_one(
            "SELECT count(*) FROM reset_audit WHERE operation='account_reset'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(first_status, StatusCode::BAD_GATEWAY);
    for (status, body) in retries {
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Reset applied"));
    }
    assert_eq!(audits, 1);
    let calls = received.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "failed settlement retries once, then the receipt replays"
    );
    assert_eq!(calls[0]["credit_id"], calls[1]["credit_id"]);
    assert_eq!(calls[0]["redeem_request_id"], calls[1]["redeem_request_id"]);
}

#[tokio::test]
async fn postgres_dashboard_reset_button_requires_gate_and_exhaustion() {
    for (gate, used) in [
        (false, Some(100.0)),
        (true, Some(42.0)),
        (true, Some(100.0)),
        (true, None),
    ] {
        let usage = json!({"plan_type":"pro","rate_limit":used.map(|used|json!({"primary_window":{"used_percent":used,"limit_window_seconds":18000,"reset_at":4102444800_i64}})),"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":if used==Some(100.0){1}else{0}}});
        let (base,upstream)=upstream(usage,json!({"available_count":1,"credits":[{"id":"soon","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}),StatusCode::OK).await;
        let (mut fixture, control, schema) = postgres_fixture(&base).await;
        fixture.broker.browser_resets_enabled = gate;
        let (url, server) = browser_server(&mut fixture, &base).await;
        let response = reqwest::Client::new()
            .get(format!("{url}/accounts"))
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap();
        let status = response.status();
        let html = response.text().await.unwrap();
        let observed: Value = reqwest::Client::new()
            .get(format!("{url}/accounts/data"))
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        server.abort();
        fixture.stop().await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        upstream.abort();
        assert_eq!(status, StatusCode::OK, "{html}");
        let form = html
            .split("action=\"/accounts/reset\"")
            .nth(1)
            .map(|s| s.split("</form>").next().unwrap());
        assert_eq!(
            form.is_some(),
            gate,
            "per-account reset form; gate={gate}, used={used:?}"
        );
        if let Some(form) = form {
            assert!(form.contains("Apply reset for personal"));
            assert_eq!(
                form.contains("disabled"),
                used != Some(100.0),
                "{form}; observed={}",
                observed["accounts"]
            );
        }
    }
}

#[tokio::test]
async fn postgres_dashboard_reset_reuses_one_usage_observation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let usage_calls = Arc::new(AtomicUsize::new(0));
    let counted = usage_calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app=Router::new().route("/usage",get(move || {
        counted.fetch_add(1,Ordering::SeqCst);
        async {Json(json!({"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_at":4102444800_i64}},"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}}))}
    })).route("/credits",get(||async {Json(json!({"available_count":1,"credits":[{"id":"only","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}))}));
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for clear_native in [false, true] {
        let (mut fixture, control, schema) = postgres_fixture(&base).await;
        fixture.broker.browser_resets_enabled = true;
        if clear_native {
            for (_, owner) in fixture.broker.owners.read().await.values() {
                let mut owner = owner.lock().await;
                owner.limits = None;
                owner.limits_observed = None;
            }
        }
        let (url, server) = browser_server(&mut fixture, &base).await;
        usage_calls.store(0, Ordering::SeqCst);
        let http = reqwest::Client::new();
        let mut snapshots = Vec::new();
        for _ in 0..2 {
            let response = http
                .get(format!("{url}/accounts/data"))
                .header("cookie", "codexctl-session=synthetic-session")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            snapshots.push(response.json::<Value>().await.unwrap());
        }
        let calls = usage_calls.load(Ordering::SeqCst);
        server.abort();
        fixture.stop().await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        for snapshot in snapshots {
            assert_eq!(
                snapshot["accounts"][0]["banked_resets"]["redeemable_now"], 1,
                "one shared observation still provides applicability: {snapshot}"
            );
        }
        assert_eq!(
            calls, 1,
            "dashboard and reset inventory must share usage (clear_native={clear_native})"
        );
    }
    upstream.abort();
}

async fn concurrent_upstream(
    answer: Value,
) -> (
    String,
    Arc<StdMutex<Vec<Value>>>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let received = calls.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let ready = started.clone();
    let finish = release.clone();
    let app=Router::new()
        .route("/usage",get(||async {Json(json!({"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}}))}))
        .route("/credits",get(||async {Json(json!({"available_count":1,"credits":[{"id":"only","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}))}))
        .route("/credits/consume",post(move |Json(body):Json<Value>| {
            let calls=calls.clone();let ready=ready.clone();let finish=finish.clone();let answer=answer.clone();
            async move {calls.lock().unwrap().push(body);ready.notify_one();finish.notified().await;Json(answer)}
        }));
    (
        base,
        received,
        started,
        release,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

#[tokio::test]
async fn postgres_reset_excludes_removal_until_provider_work_finishes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered = started.clone();
    let finish = release.clone();
    let app = Router::new()
        .route("/usage", get(|| async { Json(json!({"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}})) }))
        .route("/credits", get(|| async { Json(json!({"credits":[{"id":"only","status":"available","expires_at":"2036-07-26T00:00:00Z"}]})) }))
        .route("/credits/consume", post(move || {
            let entered=entered.clone(); let finish=finish.clone();
            async move { entered.notify_one(); finish.notified().await; Json(json!({"code":"reset","windows_reset":1})) }
        }));
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    fixture.broker.browser_remove_enabled = true;
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::new();
    let html = http
        .get(format!("{url}/accounts/manage?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let (reset, removal) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(fixture.redeem("held-provider"), async {
            started.notified().await;
            let claim = tokio::time::timeout(
                Duration::from_secs(1),
                fixture.broker.central.as_ref().unwrap().acquire_lease(
                    &account_key("test-user", "personal"),
                    "competing-refresh",
                    Duration::from_secs(60),
                ),
            )
            .await
            .expect("stage: refresh admission refuses promptly during reset provider work");
            assert!(
                claim.is_err(),
                "a refresh claim cannot overlap active reset execution"
            );
            let response = http
                .post(format!("{url}/accounts/remove"))
                .header("cookie", "codexctl-session=synthetic-session")
                .header("origin", "http://127.0.0.1")
                .form(&[
                    ("alias", "personal".to_owned()),
                    ("confirmation", "personal".to_owned()),
                    ("account_id", hidden(&html, "account_id")),
                    ("revision", hidden(&html, "revision")),
                    ("attempt", hidden(&html, "attempt")),
                    ("form_token", hidden(&html, "form_token")),
                ])
                .send()
                .await
                .unwrap();
            let result = (response.status(), response.text().await.unwrap());
            release.notify_one();
            result
        })
    })
    .await
    .expect("stage: competing removal returns while the reset provider is held");
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        removal.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "active reset must retain custody: {}",
        removal.1
    );
    assert!(removal.1.contains("reset_in_progress"));
    assert_eq!(reset.0, StatusCode::OK);
}

#[tokio::test]
async fn postgres_reset_journal_refuses_a_foreign_company_user() {
    use crate::central::storage::resets::ResetWrite;
    let (base, _, upstream) = redemption_upstream(1).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    assert_eq!(
        fixture.redeem("tenant-boundary").await.0,
        StatusCode::BAD_GATEWAY
    );
    let central = fixture.broker.central.as_ref().unwrap();
    let id = account_key("test-user", "personal");
    let (version, journal) = central.reset_journal("test-user", &id).await.unwrap();
    let read = central.reset_journal("another-user", &id).await;
    let write = central
        .write_reset_journal(
            "another-user",
            &id,
            version,
            &journal,
            ResetWrite::Settle {
                audit: None,
                code: crate::api::ConsumeResetCode::Unknown,
            },
        )
        .await;
    let after = central.reset_journal("test-user", &id).await.unwrap();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert!(
        read.is_err(),
        "a foreign company user cannot read a reset journal"
    );
    assert!(
        write.is_err(),
        "a foreign company user cannot settle a reset journal"
    );
    assert_eq!(
        after,
        (version, journal),
        "foreign settlement cannot change history"
    );
}

#[tokio::test]
async fn postgres_reset_removal_retains_and_tombstones_its_journal() {
    use crate::central::storage::resets::ResetWrite;
    let (base, _, upstream) = redemption_upstream(1).await;
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    fixture.broker.browser_remove_enabled = true;
    assert_eq!(
        fixture.redeem("unknown-before-remove").await.0,
        StatusCode::BAD_GATEWAY
    );
    let id = account_key("test-user", "personal");
    let central = fixture.broker.central.as_ref().unwrap().clone();
    let (version, journal) = central.reset_journal("test-user", &id).await.unwrap();
    assert!(journal["pending"].is_object());
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::new();
    let html = http
        .get(format!("{url}/accounts/manage?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let response = http
        .post(format!("{url}/accounts/remove"))
        .header("cookie", "codexctl-session=synthetic-session")
        .header("origin", "http://127.0.0.1")
        .form(&[
            ("alias", "personal".to_owned()),
            ("confirmation", "personal".to_owned()),
            ("account_id", hidden(&html, "account_id")),
            ("revision", hidden(&html, "revision")),
            ("attempt", hidden(&html, "attempt")),
            ("form_token", hidden(&html, "form_token")),
        ])
        .send()
        .await
        .unwrap();
    let removed = response.status();
    let read = central.reset_journal("test-user", &id).await;
    let write = central
        .write_reset_journal(
            "test-user",
            &id,
            version,
            &journal,
            ResetWrite::Settle {
                audit: None,
                code: crate::api::ConsumeResetCode::Unknown,
            },
        )
        .await;
    let retained=control.query_one("SELECT journal::text,(to_jsonb(j)->>'deleted_at') IS NOT NULL FROM account_reset_journals j WHERE account_id=$1",&[&id]).await.unwrap();
    let stored: Value = serde_json::from_str(&retained.get::<_, String>(0)).unwrap();
    let tombstoned: bool = retained.get(1);
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(removed, StatusCode::OK);
    assert!(
        read.is_err(),
        "a removed account's journal must not be replayable"
    );
    assert!(
        write.is_err(),
        "a removed account's journal must not settle"
    );
    assert!(tombstoned, "removal must tombstone the operational row");
    assert_eq!(
        stored, journal,
        "pending key and dedupe history must be retained"
    );
}

#[tokio::test]
async fn postgres_reset_concurrent_unknown_result_stays_terminal_on_retry() {
    let (base, received, started, release, upstream) =
        concurrent_upstream(json!({"code":"new_unrecognized_code","windows_reset":0})).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let mut broker = fixture.broker.clone();
    broker.owners = Arc::default();
    broker.holder_id = "other-reset-replica".into();
    let other = Fixture {
        _root: tempfile::tempdir().unwrap(),
        broker,
        headers: fixture.headers.clone(),
    };
    let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(fixture.redeem("same-intent"), async {
            started.notified().await;
            let response = other.redeem("same-intent").await;
            release.notify_one();
            response
        })
    })
    .await
    .expect("stage: concurrent reset caller is refused without waiting on the provider");
    let replay = other.redeem("same-intent").await;
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        (first.0, second.0),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::SERVICE_UNAVAILABLE
        )
    );
    let calls = received.lock().unwrap();
    assert_eq!(second.1["error"], "reset_in_progress");
    assert_eq!(
        replay.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a terminal rejection must never replay as success"
    );
    assert_eq!(calls.len(), 1);
}

#[tokio::test]
async fn postgres_reset_nonconsumption_preserves_token_evidence() {
    for (code, consumed) in [
        ("nothing_to_reset", false),
        ("no_credit", false),
        ("new_unrecognized_code", false),
        ("reset", true),
        ("already_redeemed", true),
    ] {
        let (base, _, _, release, upstream) =
            concurrent_upstream(json!({"code":code,"windows_reset":i64::from(consumed)})).await;
        let (fixture, control, schema) = postgres_fixture(&base).await;
        let central = fixture.broker.central.as_ref().unwrap();
        let id = account_key("test-user", "personal");
        assert!(
            central
                .fast_token_read(&id)
                .await
                .unwrap()
                .unwrap()
                .evidence
                .is_some()
        );
        release.notify_one();
        let response = tokio::time::timeout(Duration::from_secs(10), fixture.redeem("evidence"))
            .await
            .expect("stage: provider settlement returns");
        let evidence = central
            .fast_token_read(&id)
            .await
            .unwrap()
            .unwrap()
            .evidence;
        fixture.stop().await;
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        upstream.abort();
        assert_eq!(
            response.0,
            if code == "new_unrecognized_code" {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::OK
            },
            "provider result {code}: {}",
            response.1
        );
        assert_eq!(
            evidence.is_none(),
            consumed,
            "only actual consumption invalidates token evidence ({code})"
        );
    }
}

#[tokio::test]
async fn postgres_reset_completed_receipts_are_bounded_and_keep_the_newest_retry() {
    let (base, received, _, release, upstream) =
        concurrent_upstream(json!({"code":"nothing_to_reset","windows_reset":0})).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    let central = fixture.broker.central.as_ref().unwrap();
    let id = account_key("test-user", "personal");
    // Reverse lexical order catches pruning by request ID instead of completion.
    let latest = "request-0000";
    let (journal, replay) = tokio::time::timeout(Duration::from_secs(45), async {
        for number in (0..260).rev() {
            release.notify_one();
            let result = fixture.redeem(&format!("request-{number:04}")).await;
            assert_eq!(
                result.0,
                StatusCode::OK,
                "settlement {number}: {}",
                result.1
            );
        }
        let (_, journal) = central.reset_journal("test-user", &id).await.unwrap();
        let replay = fixture.redeem(latest).await;
        (journal, replay)
    })
    .await
    .expect("stage: bounded completed history and latest receipt replay");
    let calls = received.lock().unwrap().len();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    let completed = journal["completed"].as_object().unwrap();
    assert_eq!(
        completed.len(),
        256,
        "completed request receipts must be bounded"
    );
    assert!(journal["pending"].is_null());
    assert!(
        completed.contains_key(latest),
        "the newest receipt survives regardless of ID order"
    );
    assert!(
        !completed.contains_key("request-0259"),
        "oldest receipt expires first"
    );
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["code"], "nothing_to_reset");
    assert_eq!(
        calls, 260,
        "latest retry replays without another provider call"
    );
}

#[tokio::test]
async fn postgres_reset_retains_every_joined_receipt_within_the_history_budget() {
    use crate::central::storage::resets::ResetWrite;
    let (base, received, upstream) = redemption_upstream(1).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    assert_eq!(fixture.redeem("original").await.0, StatusCode::BAD_GATEWAY);
    let central = fixture.broker.central.as_ref().unwrap();
    let id = account_key("test-user", "personal");
    let (version, mut journal) = central.reset_journal("test-user", &id).await.unwrap();
    let joined: Vec<_> = (0..255).map(|n| format!("joined-{n:03}")).collect();
    journal["pending"]["joined_requests"] = json!(joined);
    // Prepare an ambiguous operation at its receipt budget through the DAL seam.
    assert!(
        central
            .write_reset_journal(
                "test-user",
                &id,
                version,
                &journal,
                ResetWrite::Settle {
                    audit: None,
                    code: crate::api::ConsumeResetCode::NothingToReset
                }
            )
            .await
            .unwrap()
    );
    let overflow = fixture.redeem("one-too-many").await;
    let original = fixture.redeem("original").await;
    let joined_retry = fixture.redeem("joined-000").await;
    let (_, final_journal) = central.reset_journal("test-user", &id).await.unwrap();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(
        overflow.0,
        StatusCode::CONFLICT,
        "a full pending operation refuses new joins: {}",
        overflow.1
    );
    assert_eq!(overflow.1["error"], "reset_busy");
    assert_eq!(original.0, StatusCode::OK);
    assert_eq!(joined_retry.0, StatusCode::OK);
    assert_eq!(joined_retry.1["code"], "already_redeemed");
    assert_eq!(final_journal["completed"].as_object().unwrap().len(), 256);
    assert!(
        joined
            .iter()
            .all(|key| final_journal["completed"].get(key).is_some())
    );
    assert_eq!(
        received.lock().unwrap().len(),
        2,
        "original provider key is retried exactly once"
    );
}

#[tokio::test]
async fn postgres_reset_contention_reports_busy_to_renewal() {
    let (base, _, started, release, upstream) =
        concurrent_upstream(json!({"code":"reset","windows_reset":1})).await;
    let (fixture, control, schema) = postgres_fixture(&base).await;
    fixture
        .broker
        .central
        .as_ref()
        .unwrap()
        .login_register_holder(&fixture.broker.login_holder())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::central::managed::api_routes().with_state(fixture.broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (reset, renewal) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(fixture.redeem("held-during-renew"), async {
            started.notified().await;
            let response = reqwest::Client::new()
                .post(format!("{url}/v1/relogin/start"))
                .headers(fixture.headers.clone())
                .json(&json!({"alias":"personal","id":"a".repeat(64)}))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            release.notify_one();
            (status, body)
        })
    })
    .await
    .expect("stage: renewal refuses promptly while reset provider work is held");
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    assert_eq!(reset.0, StatusCode::OK);
    assert_eq!(renewal.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        renewal.1["error"], "reset_in_progress",
        "Renew must preserve the actual busy reason"
    );
}

#[tokio::test]
async fn postgres_dashboard_reset_refusals_are_guarded_safe_html() {
    let (base, received, upstream) = redemption_upstream(0).await;
    let (mut fixture, control, schema) = postgres_fixture(&base).await;
    fixture.broker.browser_resets_enabled = true;
    let (url, server) = browser_server(&mut fixture, &base).await;
    let http = reqwest::Client::new();
    let html = http
        .get(format!("{url}/accounts/reset?alias=personal"))
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let token = hidden(&html, "form_token");
    let mut results = Vec::new();
    for (origin, form_token, cookie) in [
        ("https://other.invalid", token.clone(), true),
        ("http://127.0.0.1", "bad".into(), true),
        ("http://127.0.0.1", token.clone(), false),
    ] {
        let mut request = http
            .post(format!("{url}/accounts/reset"))
            .headers(fixture.headers.clone())
            .header("origin", origin)
            .header("accept", "text/html");
        if cookie {
            request = request.header("cookie", "codexctl-session=synthetic-session");
        }
        let response = request
            .form(&[
                ("alias", "personal".to_owned()),
                ("id", hidden(&html, "id")),
                ("form_token", form_token),
            ])
            .send()
            .await
            .unwrap();
        results.push((
            response.status(),
            response
                .headers()
                .get("cache-control")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned),
            response.text().await.unwrap(),
        ));
    }
    server.abort();
    fixture.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    upstream.abort();
    for (status, cache, body) in results {
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(cache.as_deref(), Some("no-store"));
        assert!(body.contains("Reset could not be applied"));
        assert!(
            !body.contains("synthetic-session")
                && !body.contains("synthetic-refresh")
                && !body.contains(&token)
        );
    }
    assert!(received.lock().unwrap().is_empty());
}
