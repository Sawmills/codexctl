use super::*;
use axum::body::to_bytes;

struct Fixture {
    _root: tempfile::TempDir,
    broker: Broker,
    headers: HeaderMap,
}
impl Fixture {
    async fn start(base: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        setup(&state, &key).unwrap();
        record_user(&state, "test-user", "synthetic@sawmills.ai").unwrap();
        let credential = root.path().join("device");
        crate::central::register(&state, "device", "sawmills", "test-user", &credential).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                std::fs::read_to_string(credential).unwrap().trim()
            )
            .parse()
            .unwrap(),
        );
        let mode = root.path().join("mode");
        let counter = root.path().join("counter");
        store::atomic_write(&mode, b"").unwrap();
        store::atomic_write(&counter, b"0").unwrap();
        let binary = root.path().join("synthetic-codex");
        let quoted = |p: &Path| format!("'{}'", p.to_str().unwrap().replace('\'', "'\"'\"'"));
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        store::atomic_write(&binary,format!("#!/bin/sh\nexport CENTRAL_TEST_MODE_FILE={}\nexport CENTRAL_TEST_REFRESH_COUNTER={}\nexec {} \"$@\"\n",quoted(&mode),quoted(&counter),quoted(&fixture)).as_bytes()).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let broker = Broker {
            state,
            key,
            binary,
            read_only: false,
            ownership_unresolved: Arc::new(AtomicBool::new(false)),
            owners: Arc::new(RwLock::new(BTreeMap::new())),
            imports: Arc::new(Mutex::new(())),
            sso: None,
            catalog: Arc::new(catalog::Reader::new().unwrap()),
            reset_reader: crate::central::resets::Reader::with_base(base),
            failures: Arc::new(StdMutex::new(BTreeMap::new())),
            metrics_hash: None,
            work: Arc::new(Semaphore::new(128)),
            stopping: Arc::new(AtomicBool::new(false)),
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
        };
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = json!({"sub":"synthetic-login","iat":2000000000_u64,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat","chatgpt_plan_type":"pro"}});
        let auth = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":"synthetic-refresh","account_id":"synthetic-seat"}});

        broker
            .import_account(
                "test-user",
                Import {
                    alias: "personal".into(),
                    label: None,
                    auth,
                },
            )
            .await
            .unwrap_or_else(|_| panic!("fixture import failed"));
        Self {
            _root: root,
            broker,
            headers,
        }
    }
    async fn list(&self) -> (HeaderMap, Value) {
        let response =
            crate::central::resets::list(State(self.broker.clone()), self.headers.clone())
                .await
                .unwrap_or_else(|_| panic!("reset request failed"));
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        (headers, serde_json::from_slice(&body).unwrap())
    }
    async fn stop(&self) {
        for (_, owner) in self.broker.owners.read().await.values() {
            owner
                .lock()
                .await
                .rpc
                .as_mut()
                .unwrap()
                .shutdown()
                .await
                .unwrap();
        }
    }
}

async fn upstream(
    usage: Value,
    credits: Value,
    status: StatusCode,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let validate = |headers: HeaderMap| {
        headers
            .get("chatgpt-account-id")
            .is_some_and(|v| v == "synthetic-seat")
            && headers.get("authorization").is_some_and(|v| {
                v.to_str()
                    .unwrap()
                    .starts_with("Bearer eyJhbGciOiJub25lIn0.")
            })
    };
    let app = Router::new()
        .route(
            "/usage",
            get(move |headers: HeaderMap| async move {
                if !validate(headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                if status != StatusCode::OK {
                    return status.into_response();
                }
                Json(usage).into_response()
            }),
        )
        .route(
            "/credits",
            get(move |headers: HeaderMap| async move {
                if !validate(headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(credits).into_response()
            }),
        );
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

#[tokio::test]
async fn reset_inventory_preserves_provider_counts_and_expiries_without_exporting_auth() {
    let usage =
        json!({"rate_limit_reset_credits":{"available_count":3,"applicable_available_count":0}});
    let credits = json!({"available_count":2,"credits":[{"id":"later","status":"available","expires_at":"2036-08-12T12:00:00Z"},{"id":"sooner","status":"available","expires_at":"2036-07-26T12:00:00Z"}]});
    let (base, upstream) = upstream(usage, credits.clone(), StatusCode::OK).await;
    let fixture = Fixture::start(&base).await;

    let (headers, result) = fixture.list().await;

    assert_eq!(
        result,
        json!({"userId":"test-user","accounts":[{"alias":"personal","available":3,"applicable":0,"credits":[{"id":"later","status":"available","expires_at":"2036-08-12T12:00:00Z","reset_type":null,"granted_at":null,"title":null,"description":null},{"id":"sooner","status":"available","expires_at":"2036-07-26T12:00:00Z","reset_type":null,"granted_at":null,"title":null,"description":null}]}]})
    );
    assert_eq!(headers["cache-control"], "no-store");
    fixture.stop().await;
    upstream.abort();
}

#[tokio::test]
async fn reset_inventory_failure_counts_once_and_keeps_totals_unknown() {
    let (base, upstream) = upstream(json!({}), json!({}), StatusCode::BAD_GATEWAY).await;
    let fixture = Fixture::start(&base).await;

    let (_, result) = fixture.list().await;

    assert_eq!(
        result["accounts"],
        json!([{"alias":"personal","error":"reset_read_failed"}])
    );
    assert_eq!(
        fixture.broker.failures.lock().unwrap()["reset_read_failed"].count,
        1
    );
    fixture.stop().await;
    upstream.abort();
}

#[tokio::test]
async fn reset_inventory_excludes_other_company_users() {
    let fixture = Fixture::start("http://127.0.0.1:1").await;
    fixture
        .broker
        .owners
        .write()
        .await
        .values_mut()
        .next()
        .unwrap()
        .0
        .user = "other-company-user".into();

    let (_, result) = fixture.list().await;

    assert_eq!(result, json!({"userId":"test-user","accounts":[]}));
    assert!(fixture.broker.failures.lock().unwrap().is_empty());
    fixture.stop().await;
}

#[tokio::test]
async fn slow_reset_reads_do_not_hold_the_refresh_owner_lock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::start(&format!("http://{}", listener.local_addr().unwrap())).await;
    let broker = fixture.broker.clone();
    let headers = fixture.headers.clone();
    let request =
        tokio::spawn(async move { crate::central::resets::list(State(broker), headers).await });
    let (connection, _) =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
    let owner = fixture
        .broker
        .owners
        .read()
        .await
        .values()
        .next()
        .unwrap()
        .1
        .clone();

    let owner_is_free = owner.try_lock().is_ok();

    drop(connection);
    let _ = request.await;
    fixture.stop().await;
    assert!(
        owner_is_free,
        "a slow reset endpoint blocks the refresh owner"
    );
}

#[tokio::test]
async fn reset_inventory_refreshes_a_rejected_token_once() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::start(&format!("http://{}", listener.local_addr().unwrap())).await;
    let app = Router::new().route("/usage", get(|headers: HeaderMap| async move {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let token = headers["authorization"].to_str().unwrap();
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        if claims["generation"] == 1 { return StatusCode::UNAUTHORIZED.into_response(); }
        Json(json!({"rate_limit_reset_credits":{"available_count":2,"applicable_available_count":1}})).into_response()
    })).route("/credits", get(|| async { Json(json!({"available_count":2,"credits":[]})) }));
    let upstream = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (_, result) = fixture.list().await;

    assert_eq!(
        result["accounts"],
        json!([{"alias":"personal","available":2,"applicable":1,"credits":[]}])
    );
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("counter")).unwrap(),
        "2"
    );
    fixture.stop().await;
    upstream.abort();
}

#[tokio::test]
async fn reset_inventory_never_reads_credentials_from_another_vault_user() {
    let (base, upstream) = upstream(json!({}), json!({}), StatusCode::OK).await;
    let fixture = Fixture::start(&base).await;
    fixture
        .broker
        .owners
        .read()
        .await
        .values()
        .next()
        .unwrap()
        .1
        .lock()
        .await
        .vault
        .user = "another-user".into();

    let (_, result) = fixture.list().await;

    assert_eq!(
        result["accounts"],
        json!([{"alias":"personal","error":"reset_read_failed"}])
    );
    fixture.stop().await;
    upstream.abort();
}

#[tokio::test]
async fn reset_inventory_stops_after_one_rejected_token_retry() {
    let (base, upstream) = upstream(json!({}), json!({}), StatusCode::UNAUTHORIZED).await;
    let fixture = Fixture::start(&base).await;

    let (_, result) = fixture.list().await;

    assert_eq!(
        result["accounts"],
        json!([{"alias":"personal","error":"reset_read_failed"}])
    );
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("counter")).unwrap(),
        "2"
    );
    assert_eq!(
        fixture.broker.failures.lock().unwrap()["reset_read_failed"].count,
        1
    );
    fixture.stop().await;
    upstream.abort();
}

impl Fixture {
    async fn redeem(&self, id: &str) -> (StatusCode, Value) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/resets/redeem", listener.local_addr().unwrap());
        let app = crate::central::resets::routes().with_state(self.broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::new()
            .post(url)
            .headers(self.headers.clone())
            .json(&json!({"alias":"personal","redeem_request_id":id}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        server.abort();
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn reset_redemption_refuses_an_account_without_an_exhausted_window() {
    let (base, upstream) = upstream(
        json!({"rate_limit_reset_credits":{"available_count":2,"applicable_available_count":0}}),
        json!({}),
        StatusCode::OK,
    )
    .await;
    let fixture = Fixture::start(&base).await;
    let (status, body) = fixture.redeem("not-exhausted").await;
    fixture.stop().await;
    upstream.abort();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, json!({"error":"nothing_to_reset"}));
}

async fn redemption_upstream(
    ambiguous: bool,
) -> (
    String,
    Arc<StdMutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(StdMutex::new(Vec::new()));
    let requests = received.clone();
    let app = Router::new()
        .route("/usage", get(|| async { Json(json!({"rate_limit_reset_credits":{"available_count":3,"applicable_available_count":1}})) }))
        .route("/credits", get(|| async { Json(json!({"credits":[
            {"id":"late","status":"available","expires_at":"2036-08-12T00:00:00Z"},
            {"id":"spent","status":"redeemed","expires_at":"2036-06-01T00:00:00Z"},
            {"id":"expired","status":"available","expires_at":"2000-01-01T00:00:00Z"},
            {"id":"soon","status":"available","expires_at":"2036-07-26T00:00:00Z"}
        ]})) }))
        .route("/credits/consume", post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["chatgpt-account-id"], "synthetic-seat");
            assert!(headers.contains_key("authorization"));
            let mut requests = requests.lock().unwrap();
            requests.push(body);
            if ambiguous && requests.len() == 1 {
                return StatusCode::BAD_GATEWAY.into_response();
            }
            Json(json!({"code":if ambiguous { "already_redeemed" } else { "reset" },"windows_reset":1})).into_response()
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
async fn reset_redemption_spends_the_closest_expiry_on_an_exhausted_account() {
    let (base, received, upstream) = redemption_upstream(false).await;
    let fixture = Fixture::start(&base).await;
    let (status, body) = fixture.redeem("closest-expiry").await;
    fixture.stop().await;
    upstream.abort();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"code":"reset","windows_reset":1}));
    let calls = received.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["credit_id"], "soon");
}

#[tokio::test]
async fn reset_redemption_retry_returns_already_redeemed_without_a_second_spend() {
    let (base, received, upstream) = redemption_upstream(false).await;
    let fixture = Fixture::start(&base).await;
    assert_eq!(fixture.redeem("durable-retry").await.0, StatusCode::OK);
    // A new reader has no in-memory request cache; the receipt must be durable.
    let mut broker = fixture.broker.clone();
    broker.reset_reader = crate::central::resets::Reader::with_base(&base);
    let retry = Fixture {
        _root: tempfile::tempdir().unwrap(),
        broker,
        headers: fixture.headers.clone(),
    };
    let (status, body) = retry.redeem("durable-retry").await;
    fixture.stop().await;
    upstream.abort();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "already_redeemed");
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn reset_redemption_ambiguous_retry_keeps_the_credit_and_key_and_blocks_new_spends() {
    let (base, received, upstream) = redemption_upstream(true).await;
    let fixture = Fixture::start(&base).await;
    assert_eq!(fixture.redeem("uncertain").await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        fixture.redeem("new-operation").await,
        (StatusCode::CONFLICT, json!({"error":"reset_pending"}))
    );
    let mut broker = fixture.broker.clone();
    broker.reset_reader = crate::central::resets::Reader::with_base(&base);
    let retry = Fixture {
        _root: tempfile::tempdir().unwrap(),
        broker,
        headers: fixture.headers.clone(),
    };
    let (status, body) = retry.redeem("uncertain").await;
    fixture.stop().await;
    upstream.abort();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], "already_redeemed");
    let calls = received.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    assert_eq!(calls[0]["credit_id"], "soon");
    assert_eq!(
        fixture.broker.failures.lock().unwrap()["reset_redeem_failed"].count,
        1
    );
}

#[tokio::test]
async fn concurrent_reset_retries_spend_only_once() {
    let (base, received, upstream) = redemption_upstream(false).await;
    let fixture = Fixture::start(&base).await;
    let (first, second) = tokio::join!(fixture.redeem("concurrent"), fixture.redeem("concurrent"));
    fixture.stop().await;
    upstream.abort();
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let mut codes = [
        first.1["code"].as_str().unwrap(),
        second.1["code"].as_str().unwrap(),
    ];
    codes.sort();
    assert_eq!(codes, ["already_redeemed", "reset"]);
    assert_eq!(received.lock().unwrap().len(), 1);
}
