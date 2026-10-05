use super::*;
use crate::central::server::TokenResponse;
use std::{sync::atomic::AtomicUsize, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    broker: Broker,
    headers: HeaderMap,
    requests: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    hold: Arc<AtomicBool>,
    modern: Arc<AtomicBool>,
    seen_account: Arc<StdMutex<Option<String>>>,
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
                revision: 0,
            },
        )
        .unwrap_or_else(|_| panic!("token response failed"));
        let owner = prepare_owner(&account, &key, true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let modern = Arc::new(AtomicBool::new(false));
        let seen_account = Arc::new(StdMutex::new(None));
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let app = Router::new().route("/usage", get({
            let (requests, fail, hold, modern, seen_account, started, release) = (requests.clone(), fail.clone(), hold.clone(), modern.clone(), seen_account.clone(), started.clone(), release.clone());
            move |headers: HeaderMap| {
                let (requests, fail, hold, modern, seen_account, started, release) = (requests.clone(), fail.clone(), hold.clone(), modern.clone(), seen_account.clone(), started.clone(), release.clone());
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    started.add_permits(1);
                    if hold.load(Ordering::SeqCst) { release.acquire().await.unwrap().forget(); }
                    if fail.load(Ordering::SeqCst) { return StatusCode::BAD_GATEWAY.into_response(); }
                    let account = headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
                    *seen_account.lock().unwrap() = account;
                    if modern.load(Ordering::SeqCst) {
                        if headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok()) != Some("synthetic-seat") { return StatusCode::BAD_REQUEST.into_response(); }
                        Json(json!({"plan_type":"promax","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":100,"limit_window_seconds":604800}},"rate_limit_reset_credits":{"available_count":2,"applicable_available_count":0}})).into_response()
                    } else {
                        Json(json!({"plan_type":"pro","credits":{"has_credits":false,"unlimited":false,"balance":"0","overage_limit_reached":false},"rate_limit_reset_credits":{"available_count":2,"applicable_available_count":0},"rate_limit":{"primary":{"used_percent":25,"window_minutes":300}}})).into_response()
                    }
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
            recovery_stop: Arc::new(tokio::sync::Notify::new()),
            background_recovery: false,
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
            central: None,
            holder_id: "test-holder".into(),
            registry: None,
        };
        Self {
            _root: root,
            broker,
            headers,
            requests,
            fail,
            hold,
            modern,
            seen_account,
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

#[tokio::test]
async fn deferred_recovery_does_not_reopen_a_newer_owner_fence() {
    let fixture = Fixture::new(Duration::from_secs(1)).await;
    let owner_ref = fixture.broker.owners.read().await["fixture"].1.clone();
    let before = owner_ref.lock().await.vault.clone();
    let permits = Arc::new(Semaphore::new(1));
    let permit = permits.clone().acquire_owned().await.unwrap();
    let renew_done = Arc::new(AtomicBool::new(false));
    {
        let mut owner = owner_ref.lock().await;
        owner.fence(false);
    }
    settle_background_recovery(
        owner_ref.clone(),
        None,
        None,
        before,
        true,
        0,
        permit,
        renew_done,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert!(!owner_ref.lock().await.available);
    assert!(permits.try_acquire().is_ok());
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
async fn legacy_usage_refresh_replaces_fields_and_failure_preserves_delivery() {
    let fixture = Fixture::new(Duration::from_secs(1)).await;
    fixture.modern.store(true, Ordering::SeqCst);
    let mut refreshed = TokenResponse {
        access_token: "token".into(),
        chatgpt_account_id: "synthetic-seat".into(),
        chatgpt_plan_type: Some("pro".into()),
        revision: "revision".into(),
        billing_class: Some(api::BillingClass::Unknown),
        native_routing_supported: false,
        statusline_usage: Some(crate::statusline::Usage {
            age_seconds: 0,
            weekly_used_percent: Some(100.0),
            weekly_resets_at: None,
            five_hour_used_percent: None,
            five_hour_resets_at: None,
            allowed: None,
            limit_reached: None,
        }),
        user_id: None,
        label: None,
    };
    super::refresh_legacy_usage(&fixture.broker, &mut refreshed).await;
    assert_eq!(
        refreshed.billing_class,
        Some(api::BillingClass::RateLimited)
    );
    assert_eq!(refreshed.chatgpt_plan_type.as_deref(), Some("promax"));
    assert_eq!(
        refreshed.statusline_usage.as_ref().unwrap().allowed,
        Some(true)
    );
    assert_eq!(
        refreshed.statusline_usage.as_ref().unwrap().limit_reached,
        Some(false)
    );
    assert_eq!(
        refreshed
            .statusline_usage
            .as_ref()
            .unwrap()
            .weekly_used_percent,
        Some(100.0)
    );
    assert_eq!(
        fixture.seen_account.lock().unwrap().as_deref(),
        Some("synthetic-seat")
    );
    fixture.fail.store(true, Ordering::SeqCst);
    let before = refreshed.clone();
    super::refresh_legacy_usage(&fixture.broker, &mut refreshed).await;
    assert_eq!(refreshed.billing_class, before.billing_class);
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
    assert_eq!(fixture.list().await["credits"]["balance"], "0");
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
    assert!(stale.get("credits").is_none());
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
        assert_eq!(data["accounts"][0]["credits"]["balance"], "0");
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
        assert!(data["accounts"][0].get("credits").is_none());
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
async fn dashboard_omits_credits_when_credentials_change_during_snapshot() {
    let mut f = Fixture::new(Duration::from_secs(2)).await;
    f.broker.sso = Some(Arc::new(enrollment::Sso::testing_session("test")));
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    f.broker.reset_reader = super::super::resets::Reader::with_base(&base);
    let app = super::super::dashboard::routes(&base)
        .route(
            "/credits",
            get({
                let (started, release) = (started.clone(), release.clone());
                move || {
                    let (started, release) = (started.clone(), release.clone());
                    async move {
                        started.add_permits(1);
                        release.acquire().await.unwrap().forget();
                        Json(json!({"available_count":0,"credits":[]}))
                    }
                }
            }),
        )
        .with_state(f.broker.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("{base}/accounts/data"))
            .header("cookie", "codexctl-session=synthetic-session")
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let owner = f.broker.owners.read().await["fixture"].1.clone();
    let mut current = owner.lock().await;
    current.vault.auth["changed"] = json!(true);
    release.add_permits(1);
    drop(current);
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let data: Value = response.json().await.unwrap();
    assert_eq!(data["accounts"][0]["usage_stale"], true);
    assert!(data["accounts"][0].get("credits").is_none());
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

impl Fixture {
    async fn attach_refresh_store(&mut self, mode: &str) -> CentralStore {
        self.attach_refresh_store_with_mode(super::super::storage::StoreMode::File, mode)
            .await
    }

    async fn attach_refresh_store_with_mode(
        &mut self,
        store_mode: super::super::storage::StoreMode,
        mode: &str,
    ) -> CentralStore {
        use std::os::unix::fs::PermissionsExt;
        let central = CentralStore::from_mode(store_mode, &self.broker.state, &self.broker.key)
            .await
            .unwrap();
        central.migrate().await.unwrap();
        self.broker.central = Some(central.clone());
        self.broker.read_only = false;
        if store_mode != super::super::storage::StoreMode::File {
            let user = users(&self.broker.state)
                .unwrap()
                .into_iter()
                .find(|user| user.id == "test")
                .unwrap();
            central
                .save_registry_entity_cas(
                    "users",
                    &user.id,
                    &serde_json::to_vec(&user).unwrap(),
                    None,
                )
                .await
                .unwrap();
            let device = vault::devices(&self.broker.state)
                .unwrap()
                .into_iter()
                .find(|device| device.user == "test")
                .unwrap();
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
        let owner_ref = self.broker.owners.read().await["fixture"].1.clone();
        let mut owner = owner_ref.lock().await;
        owner.retry_clock = Some(Arc::new(|| 60_000));
        owner.vault.revision = 1;
        vault::save(&owner.state, &owner.key, &owner.vault).unwrap();
        central
            .save_account(&Broker::owner_record(&owner).unwrap())
            .await
            .unwrap();
        let root = self._root.path();
        store::atomic_write(&root.join("mode"), mode.as_bytes()).unwrap();
        store::atomic_write(&root.join("count"), b"0").unwrap();
        let binary = root.join("codex");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        let script = format!(
            "#!/usr/bin/env python3\nimport os, runpy\nos.environ['CENTRAL_TEST_MODE_FILE'] = {}\nos.environ['CENTRAL_TEST_REFRESH_COUNTER'] = {}\nrunpy.run_path({}, run_name='__main__')\n",
            serde_json::to_string(&root.join("mode")).unwrap(),
            serde_json::to_string(&root.join("count")).unwrap(),
            serde_json::to_string(&fixture).unwrap(),
        );
        store::atomic_write(&binary, script.as_bytes()).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        self.broker.binary = binary.clone();
        owner.rpc = Some(Rpc::start(&binary, &owner.home).await.unwrap());
        owner.refresh_enabled = true;
        central
    }
}

#[cfg(feature = "central-real-db-tests")]
#[tokio::test]
async fn postgres_background_recovery_recovers_after_unhealthy_rpc() {
    if std::env::var("DATABASE_URL").is_err() {
        if std::env::var("CI").ok().as_deref() == Some("true") {
            panic!("DATABASE_URL must be set for PostgreSQL managed retry scenarios in CI");
        }
        return;
    }
    let mut fixture = Fixture::new(Duration::from_secs(2)).await;
    let central = fixture
        .attach_refresh_store_with_mode(super::super::storage::StoreMode::Postgres, "rpc-unhealthy")
        .await;

    assert_eq!(fixture.token().await, StatusCode::SERVICE_UNAVAILABLE);
    store::atomic_write(&fixture._root.path().join("mode"), b"").unwrap();
    let owner_ref = fixture.broker.owners.read().await["fixture"].1.clone();
    let mut owner = owner_ref.lock().await;
    let now = owner.retry_clock_now();
    owner.retry_started = Some(now.saturating_sub(60_000));
    drop(owner);
    recover_unhealthy_owners(&fixture.broker).await;
    assert_eq!(fixture.token().await, StatusCode::OK);
    // A central reconcile can stop a stale child while the owner remains
    // available. The pre-existing shared-store launch path must recreate it.
    let owner_ref = fixture.broker.owners.read().await["fixture"].1.clone();
    {
        let mut owner = owner_ref.lock().await;
        owner.rpc = None;
        owner.refresh_enabled = false;
        owner.available = true;
    }
    assert_eq!(fixture.token().await, StatusCode::OK);
    let committed = central
        .load_account(&account_key("test", "fixture"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.vault["revision"], committed.revision);
}

#[tokio::test]
async fn late_native_completion_is_published_before_another_holder_gets_the_lease() {
    let mut fixture = Fixture::new(Duration::from_secs(1)).await;
    let central = fixture.attach_refresh_store("late-error").await;
    let owner_ref = fixture.broker.owners.read().await["fixture"].1.clone();
    let revision = owner_ref.lock().await.snapshot().unwrap().revision;
    let response = token(
        State(fixture.broker.clone()),
        fixture.headers.clone(),
        Ok(Json(TokenRequest {
            alias: Some("fixture".into()),
            previous_revision: Some(revision),
            ..Default::default()
        })),
    )
    .await
    .unwrap_or_else(IntoResponse::into_response);
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let id = account_key("test", "fixture");
    let peer_lease = central
        .acquire_lease(&id, "peer", Duration::from_secs(60))
        .await
        .unwrap();
    let committed = central.load_account(&id).await.unwrap().unwrap();
    assert_eq!(committed.revision, 2);
    assert_eq!(committed.vault["revision"], 2);
    assert_eq!(
        committed.vault["auth"]["tokens"]["refresh_token"],
        "synthetic-rotated-refresh"
    );
    assert_eq!(
        std::fs::read_to_string(fixture._root.path().join("count")).unwrap(),
        "1"
    );
    assert!(owner_ref.lock().await.rpc.is_none());
    central.release_lease(&peer_lease).await.unwrap();
}

#[tokio::test]
async fn retry_keeps_recovery_epoch_after_the_native_process_stops() {
    let mut fixture = Fixture::new(Duration::from_secs(1)).await;
    let central = fixture.attach_refresh_store("").await;
    let id = account_key("test", "fixture");
    let lease = central
        .acquire_lease(&id, &fixture.broker.holder_id, Duration::from_secs(60))
        .await
        .unwrap();
    let owner_ref = fixture.broker.owners.read().await["fixture"].1.clone();
    {
        let mut owner = owner_ref.lock().await;
        owner.rpc.as_mut().unwrap().shutdown().await.unwrap();
        owner.rpc = None;
        owner.available = false;
        owner.routing_refused = true;
    }
    assert_eq!(fixture.token().await, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        central
            .renew(&lease, Duration::from_secs(60))
            .await
            .unwrap()
    );
    assert!(
        central
            .acquire_lease(&id, "peer", Duration::from_secs(60))
            .await
            .is_err()
    );
    central.release_lease(&lease).await.unwrap();
}
