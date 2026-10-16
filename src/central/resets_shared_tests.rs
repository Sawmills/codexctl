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
        central
            .save_account(&Broker::owner_record(&*owner.lock().await).unwrap())
            .await
            .unwrap();
    }
    f.broker.central = Some(central);
    (f, control, schema)
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
    let audit:i64=control.query_one("SELECT count(*) FROM dashboard_audit WHERE operation='account_reset' AND target=$1 AND result='ok:reset'",&[&account_key("test-user","personal")]).await.unwrap().get(0);
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

async fn concurrent_upstream(
    answer: Value,
) -> (
    String,
    Arc<StdMutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let received = calls.clone();
    let ready = Arc::new(tokio::sync::Barrier::new(2));
    let app=Router::new()
        .route("/usage",get(||async {Json(json!({"rate_limit_reset_credits":{"available_count":1,"applicable_available_count":1}}))}))
        .route("/credits",get(||async {Json(json!({"available_count":1,"credits":[{"id":"only","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}))}))
        .route("/credits/consume",post(move |Json(body):Json<Value>| {
            let calls=calls.clone();let ready=ready.clone();let answer=answer.clone();
            async move {calls.lock().unwrap().push(body);ready.wait().await;Json(answer)}
        }));
    (
        base,
        received,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

#[tokio::test]
async fn postgres_reset_concurrent_unknown_result_is_terminal_for_both_callers() {
    let (base, received, upstream) =
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
        tokio::join!(fixture.redeem("same-intent"), other.redeem("same-intent"))
    })
    .await
    .expect("two reset callers must reach the fake provider");
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
            StatusCode::UNPROCESSABLE_ENTITY
        )
    );
    let calls = received.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["redeem_request_id"], calls[1]["redeem_request_id"]);
    assert_eq!(calls[0]["credit_id"], calls[1]["credit_id"]);
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
