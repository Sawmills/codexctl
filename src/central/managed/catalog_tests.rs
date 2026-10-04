use super::*;
use std::{sync::atomic::AtomicUsize, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    broker: Broker,
    headers: HeaderMap,
    requests: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    hold: Arc<AtomicBool>,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
    http: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(timeout: Duration) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        setup(&state, &key).unwrap();
        record_user(&state, "test", "test@example.invalid").unwrap();
        let device = root.path().join("device");
        crate::central::register(&state, "fixture", "sawmills", "test", &device).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", std::fs::read_to_string(device).unwrap())
                .parse()
                .unwrap(),
        );
        let account = state.join("accounts/fixture");
        store::ensure_private_dir(&account).unwrap();
        let auth = super::tests::recovery_auth(Some(2000000000), "synthetic-refresh");
        vault::save(
            &account,
            &key,
            &Vault {
                user: "test".into(),
                tenant: "sawmills".into(),
                alias: "fixture".into(),
                label: None,
                auth,
                verified: true,
                import_rejected: false,
            },
        )
        .unwrap();
        let owner = prepare_owner(&account, &key, true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let app = Router::new().route("/usage", get({
            let (requests, fail, hold, started, release) = (requests.clone(), fail.clone(), hold.clone(), started.clone(), release.clone());
            move || {
                let (requests, fail, hold, started, release) = (requests.clone(), fail.clone(), hold.clone(), started.clone(), release.clone());
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    started.add_permits(1);
                    if hold.load(Ordering::SeqCst) { release.acquire().await.unwrap().forget(); }
                    if fail.load(Ordering::SeqCst) { return StatusCode::BAD_GATEWAY.into_response(); }
                    Json(json!({"plan_type":"pro","rate_limit_reset_credits":{"available_count":2,"applicable_available_count":0},"rate_limit":{"primary":{"used_percent":25,"window_minutes":300}}})).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let http = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let broker = Broker {
            state,
            key,
            binary: "unused".into(),
            read_only: true,
            ownership_unresolved: Arc::new(AtomicBool::new(false)),
            owners: Arc::new(RwLock::new(BTreeMap::from([(
                "fixture".into(),
                (
                    AccountIndex {
                        user: "test".into(),
                        alias: "fixture".into(),
                    },
                    Arc::new(Mutex::new(owner)),
                ),
            )]))),
            imports: Arc::new(Mutex::new(())),
            sso: None,
            activity: Arc::default(),
            reset_reader: crate::central::resets::Reader::new().unwrap(),
            catalog: Arc::new(catalog::Reader::testing(endpoint, timeout)),
            failures: Arc::new(StdMutex::new(BTreeMap::new())),
            metrics_hash: None,
            work: Arc::new(Semaphore::new(128)),
            stopping: Arc::new(AtomicBool::new(false)),
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
        };
        Self {
            _root: root,
            broker,
            headers,
            requests,
            fail,
            hold,
            started,
            release,
            http,
        }
    }

    async fn list(&self) -> Value {
        list(self.broker.clone(), self.headers.clone()).await
    }

    async fn token(&self) -> StatusCode {
        token(
            State(self.broker.clone()),
            self.headers.clone(),
            Ok(Json(TokenRequest {
                alias: Some("fixture".into()),
                billing: true,
                ..Default::default()
            })),
        )
        .await
        .unwrap_or_else(IntoResponse::into_response)
        .status()
    }

    fn spawn_list(&self) -> tokio::task::JoinHandle<Value> {
        tokio::spawn(list(self.broker.clone(), self.headers.clone()))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.http.abort();
    }
}

async fn list(broker: Broker, headers: HeaderMap) -> Value {
    let response = accounts(State(broker), headers)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&body).unwrap()[0].clone()
}

#[tokio::test]
async fn b8_usage_5xx_keeps_the_owner_available_and_counts_one_attempt() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.fail.store(true, Ordering::SeqCst);

    let failed = fixture.list().await;
    let repeated = fixture.list().await;
    let token = fixture.token().await;

    assert_eq!(
        (
            failed["available"].clone(),
            failed["usageStale"].clone(),
            failed["usageError"].clone()
        ),
        (json!(true), json!(true), json!("catalog_usage_failed"))
    );
    assert_eq!(
        (
            repeated["usageAgeSeconds"].clone(),
            token,
            fixture.requests.load(Ordering::SeqCst)
        ),
        (Value::Null, StatusCode::OK, 1)
    );
    assert_eq!(
        fixture.broker.failures.lock().unwrap()["catalog_usage_failed"].count,
        1
    );
}

#[tokio::test]
async fn b8_usage_timeout_keeps_token_delivery_working() {
    let fixture = Fixture::new(Duration::from_millis(100)).await;
    fixture.hold.store(true, Ordering::SeqCst);

    let failed = fixture.list().await;
    let token = fixture.token().await;

    assert_eq!(
        (
            failed["available"].clone(),
            failed["usageError"].clone(),
            token
        ),
        (json!(true), json!("catalog_usage_timeout"), StatusCode::OK)
    );
    assert_eq!(
        fixture.broker.failures.lock().unwrap()["catalog_usage_timeout"].count,
        1
    );
}

#[tokio::test]
async fn b8_slow_usage_does_not_hold_the_token_lock() {
    let fixture = Fixture::new(Duration::from_secs(5)).await;
    fixture.hold.store(true, Ordering::SeqCst);
    let listing = fixture.spawn_list();
    tokio::time::timeout(Duration::from_secs(2), fixture.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();

    let token = tokio::time::timeout(Duration::from_millis(500), fixture.token()).await;
    fixture.release.add_permits(1);
    let summary = listing.await.unwrap();

    assert_eq!(token.unwrap(), StatusCode::OK);
    assert_eq!(
        (summary["available"].clone(), summary["usageStale"].clone()),
        (json!(true), json!(false))
    );
}

#[tokio::test]
async fn b8_concurrent_polls_share_one_usage_request() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;

    let (first, second, third) = tokio::join!(fixture.list(), fixture.list(), fixture.list());

    assert_eq!(
        (
            first["primaryUsed"].clone(),
            second["primaryUsed"].clone(),
            third["primaryUsed"].clone()
        ),
        (json!(25.0), json!(25.0), json!(25.0))
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn b8_expired_cache_refreshes_once_and_reports_age() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.list().await;
    fixture.broker.catalog.expire().await;

    let (first, second) = tokio::join!(fixture.list(), fixture.list());

    assert_eq!(
        (
            first["usageStale"].clone(),
            second["usageAgeSeconds"].clone()
        ),
        (json!(false), json!(0))
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        first["statuslineUsage"]["five_hour_used_percent"],
        json!(25.0)
    );
}

#[tokio::test]
async fn b8_failed_refresh_retains_stale_usage_without_selection_permission() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.list().await;
    fixture.broker.catalog.expire().await;
    fixture.fail.store(true, Ordering::SeqCst);

    let stale = fixture.list().await;

    assert_eq!(
        (
            stale["primaryUsed"].clone(),
            stale["usageStale"].clone(),
            stale["available"].clone()
        ),
        (json!(25.0), json!(true), json!(true))
    );
    assert!(stale["usageAgeSeconds"].as_u64().unwrap() >= 60);
    assert_eq!(
        (
            stale["billingClass"].clone(),
            stale["usageScore"].clone(),
            stale["statuslineUsage"].clone()
        ),
        (json!("unknown"), Value::Null, Value::Null)
    );
}

#[tokio::test]
async fn b8_usage_recovers_after_the_failed_attempt_cooldown() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.fail.store(true, Ordering::SeqCst);
    fixture.list().await;
    fixture.broker.catalog.expire().await;
    fixture.fail.store(false, Ordering::SeqCst);

    let recovered = fixture.list().await;

    assert_eq!(
        (
            recovered["available"].clone(),
            recovered["usageStale"].clone(),
            recovered["usageError"].clone()
        ),
        (json!(true), json!(false), Value::Null)
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn b8_credential_rotation_discards_inflight_usage_evidence() {
    let fixture = Fixture::new(Duration::from_secs(5)).await;
    fixture.hold.store(true, Ordering::SeqCst);
    let listing = fixture.spawn_list();
    tokio::time::timeout(Duration::from_secs(2), fixture.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    fixture.broker.owners.read().await["fixture"]
        .1
        .lock()
        .await
        .vault
        .auth = super::tests::recovery_auth(Some(2000000001), "synthetic-new-refresh");

    fixture.release.add_permits(1);
    let summary = listing.await.unwrap();

    assert_eq!(
        (
            summary["usageStale"].clone(),
            summary["usageError"].clone(),
            summary["billingClass"].clone(),
            summary["statuslineUsage"].clone()
        ),
        (
            json!(true),
            json!("credentials_changed"),
            json!("unknown"),
            Value::Null
        )
    );
}

#[tokio::test]
async fn b8_listing_never_recovers_an_unavailable_owner() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.broker.owners.read().await["fixture"]
        .1
        .lock()
        .await
        .available = false;

    let summary = fixture.list().await;
    let token = fixture.token().await;

    assert_eq!(
        (
            summary["available"].clone(),
            summary["usageStale"].clone(),
            token
        ),
        (json!(false), json!(true), StatusCode::SERVICE_UNAVAILABLE)
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn b8_known_routing_refusal_prevents_selection_with_fresh_usage() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    fixture.list().await;
    fixture.broker.owners.read().await["fixture"]
        .1
        .lock()
        .await
        .routing_refused = true;

    let summary = fixture.list().await;
    let account: Account = serde_json::from_value(summary.clone()).unwrap();

    assert_eq!(
        (summary["available"].clone(), summary["usageStale"].clone()),
        (json!(false), json!(false))
    );
    assert!(super::super::remote::select(&[account]).is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn b8_expired_access_does_not_fetch_or_alert() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    let claims = json!({"sub":"synthetic-login","exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat"}});
    fixture.broker.owners.read().await["fixture"]
        .1
        .lock()
        .await
        .vault
        .auth["tokens"]["access_token"] = json!(format!(
        "header.{}.",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    ));

    let summary = fixture.list().await;

    assert_eq!(
        (
            summary["available"].clone(),
            summary["usageStale"].clone(),
            summary["usageError"].clone()
        ),
        (json!(true), json!(true), json!("access_expired"))
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
    assert!(fixture.broker.failures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn catalog_carries_declared_duration_through_fresh_and_stale_samples() {
    let fixture = Fixture::new(Duration::from_secs(2)).await;
    let fresh = fixture.list().await;
    assert_eq!(fresh["primaryWindowSeconds"], 18000);
    assert_eq!(fresh.get("secondaryWindowSeconds"), Some(&Value::Null));
    assert_eq!(fresh.get("primaryResetsAt"), Some(&Value::Null));

    fixture.broker.catalog.expire().await;
    fixture.fail.store(true, Ordering::SeqCst);
    let stale = fixture.list().await;
    assert_eq!(stale["primaryWindowSeconds"], 18000);
    assert_eq!(stale["usageStale"], true);
}

#[tokio::test]
async fn dashboard_caches_usage_and_reset_expiry_without_refreshing_credentials() {
    let mut f = Fixture::new(Duration::from_secs(2)).await;
    f.broker.sso = Some(Arc::new(enrollment::Sso::testing_session("test")));
    let credits_requests = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    f.broker.reset_reader = super::super::resets::Reader::with_base(&format!(
        "http://{}",
        listener.local_addr().unwrap()
    ));
    let credits = Router::new().route("/credits", get({ let requests = credits_requests.clone(); move || {
        requests.fetch_add(1, Ordering::SeqCst);
        async { Json(json!({"available_count":2,"credits":[
            {"id":"private-reset-id","status":"available","expires_at":"2099-12-01T00:00:00Z"},
            {"id":"spent-id","status":"consumed","expires_at":"2099-01-01T00:00:00Z"},
            {"id":"second-id","status":"available","expires_at":"2099-11-01T00:00:00Z"}
        ]})) }
    }}));
    let credits_task = tokio::spawn(async move {
        axum::serve(listener, credits).await.unwrap();
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/accounts/data", listener.local_addr().unwrap());
    let app =
        super::super::dashboard::routes("https://accounts.example").with_state(f.broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let response = client
            .get(&url)
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(!body.contains("private-reset-id"));
        let data: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(data["accounts"][0]["banked_resets"]["count"], 2);
        assert_eq!(data["accounts"][0]["banked_resets"]["redeemable_now"], 0);
        assert_eq!(
            data["accounts"][0]["banked_resets"]["nearest_expiry"],
            4097174400_i64
        );
    }
    assert_eq!(f.requests.load(Ordering::SeqCst), 1);
    assert_eq!(credits_requests.load(Ordering::SeqCst), 1);
    // A CLI catalog read retains its 60-second cache, while the browser must
    // refresh the same observation with enough validity left for the response.
    f.broker.catalog.age(Duration::from_secs(45)).await;
    f.list().await;
    assert_eq!(f.requests.load(Ordering::SeqCst), 1);
    let read = || {
        client
            .get(&url)
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
    };
    let (first, second) = tokio::join!(read(), read());
    for response in [first.unwrap(), second.unwrap()] {
        let data: Value = response.json().await.unwrap();
        assert_eq!(data["accounts"][0]["usage_stale"], false);
        assert!(data["accounts"][0]["usage_age_seconds"].as_u64().unwrap() < 5);
    }
    assert_eq!(
        f.requests.load(Ordering::SeqCst),
        2,
        "early refresh is deduplicated"
    );
    f.broker.catalog.age(Duration::from_secs(45)).await;
    f.fail.store(true, Ordering::SeqCst);
    for _ in 0..3 {
        let data: Value = read().await.unwrap().json().await.unwrap();
        assert_eq!(data["accounts"][0]["usage_stale"], true);
    }
    assert_eq!(
        f.requests.load(Ordering::SeqCst),
        3,
        "failure cooldown survives early browser requests"
    );
    credits_task.abort();
    server.abort();
}

#[tokio::test]
async fn dashboard_redirects_if_session_ends_during_snapshot() {
    let mut f = Fixture::new(Duration::from_secs(5)).await;
    f.broker.sso = Some(Arc::new(enrollment::Sso::testing_session("test")));
    f.hold.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    f.broker.reset_reader = super::super::resets::Reader::with_base(&base);
    let app = super::super::dashboard::routes(&base)
        .route(
            "/credits",
            get(|| async { Json(json!({"available_count":0,"credits":[]})) }),
        )
        .with_state(f.broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = tokio::spawn(async move {
        client
            .get(format!("{base}/accounts"))
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap()
    });
    f.started.acquire().await.unwrap().forget();
    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        "codexctl-session=synthetic-session".parse().unwrap(),
    );
    headers.insert("origin", "http://127.0.0.1".parse().unwrap());
    let signed_out = enrollment::accounts_sign_out(State(f.broker.clone()), headers)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    assert_eq!(signed_out.status(), StatusCode::SEE_OTHER);
    f.release.add_permits(1);
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/accounts/sign-in");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        !response
            .text()
            .await
            .unwrap()
            .contains("browser_sign_in_required")
    );
    server.abort();
}
