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
    fail: bool,
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
                if fail {
                    return StatusCode::BAD_GATEWAY.into_response();
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
    let (base, upstream) = upstream(usage, credits.clone(), false).await;
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
    let (base, upstream) = upstream(json!({}), json!({}), true).await;
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
