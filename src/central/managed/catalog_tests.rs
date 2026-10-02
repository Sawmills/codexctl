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
                    Json(json!({"plan_type":"pro","rate_limit":{"primary":{"used_percent":25,"window_minutes":300}}})).into_response()
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
        (stale["billingClass"].clone(), stale["usageScore"].clone()),
        (json!("unknown"), Value::Null)
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
            summary["billingClass"].clone()
        ),
        (json!(true), json!("credentials_changed"), json!("unknown"))
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
