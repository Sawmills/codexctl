//! Loans through the real API routes with synthetic users and accounts.
use crate::central::{
    catalog,
    managed::{AccountIndex, Broker, account_key, prepare_owner, record_user, users},
    vault::{self, Vault},
};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

const LENDER: &str = "lender";
const BORROWER: &str = "borrower";
const STRANGER: &str = "stranger";

fn auth(login: &str, seat: &str) -> Value {
    auth_with_uid(login, None, seat)
}

fn auth_with_uid(login: &str, uid: Option<&str>, seat: &str) -> Value {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let claims = json!({"sub":login,"iat":2_000_000_000_i64,"https://api.openai.com/auth":{"chatgpt_account_id":seat,"chatgpt_user_id":uid}});
    json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":"synthetic-refresh","account_id":seat}})
}

struct Fixture {
    _root: tempfile::TempDir,
    state: PathBuf,
    key: PathBuf,
    broker: Broker,
    base: String,
    weekly_reset: Arc<AtomicI64>,
    long_window_seconds: Arc<AtomicI64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    lender: HeaderMap,
    borrower: HeaderMap,
    stranger: HeaderMap,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        crate::central::managed::setup(&state, &key).unwrap();
        let mut headers = Vec::new();
        for (user, email) in [
            (LENDER, "Alice@sawmills.ai"),
            (BORROWER, "bob@sawmills.ai"),
            (STRANGER, "carol@sawmills.ai"),
        ] {
            record_user(&state, user, email).unwrap();
            let credential = root.path().join(format!("{user}-machine"));
            crate::central::register(
                &state,
                &format!("{user}-machine"),
                "sawmills",
                user,
                &credential,
            )
            .unwrap();
            let mut map = HeaderMap::new();
            map.insert(
                "authorization",
                format!("Bearer {}", std::fs::read_to_string(credential).unwrap())
                    .parse()
                    .unwrap(),
            );
            headers.push(map);
        }
        let weekly_reset = Arc::new(AtomicI64::new(now() + 3 * 24 * 3600));
        let long_window_seconds = Arc::new(AtomicI64::new(604_800));
        let usage = axum::Router::new().route(
            "/usage",
            axum::routing::get({
                let weekly_reset = weekly_reset.clone();
                let long_window_seconds = long_window_seconds.clone();
                move || {
                    let reset = weekly_reset.load(Ordering::SeqCst);
                    let long = long_window_seconds.load(Ordering::SeqCst);
                    async move {
                        axum::Json(json!({
                            "plan_type":"pro",
                            "credits":{"has_credits":false,"unlimited":false,"balance":"0","overage_limit_reached":false},
                            "rate_limit":{"allowed":true,"limit_reached":false,
                                "primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_at":reset - 1000},
                                "secondary_window":{"used_percent":20,"limit_window_seconds":long,"reset_at":reset}}
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let mut tasks = vec![tokio::spawn(async move {
            axum::serve(listener, usage).await.unwrap();
        })];
        let broker = Broker::testing(
            state.clone(),
            key.clone(),
            Vec::new(),
            catalog::Reader::testing(endpoint, Duration::from_secs(2)),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::central::managed::api_routes().with_state(broker.clone());
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let [lender, borrower, stranger] = headers.try_into().unwrap();
        let fixture = Self {
            _root: root,
            state,
            key,
            broker,
            base,
            weekly_reset,
            long_window_seconds,
            tasks,
            lender,
            borrower,
            stranger,
        };
        fixture.add_owner(LENDER, "main", "synthetic-login").await;
        fixture
    }

    async fn add_owner(&self, user: &str, alias: &str, login: &str) {
        self.add_owner_with(
            user,
            alias,
            login,
            auth(login, &format!("seat-{user}-{alias}")),
        )
        .await;
    }

    async fn add_owner_with(&self, user: &str, alias: &str, tag: &str, credential: Value) {
        let key = account_key(user, alias);
        // A fresh state directory per login, as a renewed owner would get.
        let account = self.state.join("accounts").join(format!("{key}-{tag}"));
        crate::store::ensure_private_dir(&account).unwrap();
        vault::save(
            &account,
            &self.key,
            &Vault {
                user: user.into(),
                tenant: "sawmills".into(),
                alias: alias.into(),
                label: None,
                auth: credential,
                verified: true,
                import_rejected: false,
                revision: 0,
            },
        )
        .unwrap();
        let owner = prepare_owner(&account, &self.key, true).unwrap();
        self.broker.owners.write().await.insert(
            key,
            (
                AccountIndex {
                    user: user.into(),
                    alias: alias.into(),
                },
                Arc::new(tokio::sync::Mutex::new(owner)),
            ),
        );
    }

    async fn owner(
        &self,
        user: &str,
        alias: &str,
    ) -> Arc<tokio::sync::Mutex<crate::central::server::Owner>> {
        self.broker.owners.read().await[&account_key(user, alias)]
            .1
            .clone()
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        body: Value,
    ) -> (StatusCode, Value) {
        let client = reqwest::Client::new();
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => client.get(url),
            _ => client.post(url).json(&body),
        };
        let response = request.headers(headers.clone()).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn lend(&self, body: Value) -> (StatusCode, Value) {
        self.call("POST", "/v1/loans", &self.lender, body).await
    }

    async fn lend_main(&self) -> Value {
        let (status, grant) = self
            .lend(json!({"alias":"main","borrowerEmail":"BOB@sawmills.ai"}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{grant}");
        grant
    }

    async fn token(&self, headers: &HeaderMap, alias: &str) -> (StatusCode, Value) {
        self.call("POST", "/v1/token", headers, json!({"alias":alias}))
            .await
    }

    async fn catalog(&self, headers: &HeaderMap) -> Vec<Value> {
        let (status, body) = self.call("GET", "/v1/accounts", headers, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().unwrap().clone()
    }

    async fn end(&self, headers: &HeaderMap, id: &str) -> (StatusCode, Value) {
        self.call("POST", "/v1/loans/end", headers, json!({"id":id}))
            .await
    }

    /// The whole audit, oldest first.
    async fn audit(&self, headers: &HeaderMap) -> Vec<Value> {
        let (status, body) = self
            .call("GET", "/v1/loans/audit", headers, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["before"].is_null(), "{body}");
        let mut events = body["events"].as_array().unwrap().clone();
        events.reverse();
        events
    }

    fn set_user(&self, id: &str, change: impl FnOnce(&mut crate::central::managed::User)) {
        let mut all = users(&self.state).unwrap();
        change(all.iter_mut().find(|user| user.id == id).unwrap());
        crate::store::atomic_write(
            &self.state.join("users.json"),
            &serde_json::to_vec(&all).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_borrower_sees_and_uses_a_loaned_account_until_the_lender_ends_it() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    assert_eq!(grant["reference"], "alice/main");
    assert_eq!(grant["endsAt"], fixture.weekly_reset.load(Ordering::SeqCst));

    let borrowed = fixture.catalog(&fixture.borrower).await;
    assert_eq!(borrowed.len(), 1);
    assert_eq!(borrowed[0]["alias"], "alice/main");
    assert_eq!(borrowed[0]["userId"], BORROWER);
    assert_eq!(borrowed[0]["loan"]["lenderEmail"], "Alice@sawmills.ai");
    assert_eq!(borrowed[0]["loan"]["endsAt"], grant["endsAt"]);
    let lent = fixture.catalog(&fixture.lender).await;
    assert_eq!(lent[0]["alias"], "main");
    assert!(lent[0].get("loan").is_none());
    assert!(fixture.catalog(&fixture.stranger).await.is_empty());

    let (status, token) = fixture.token(&fixture.borrower, "ALICE/Main").await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(token["userId"], BORROWER);
    assert_eq!(token["chatgptAccountId"], "seat-lender-main");
    assert_eq!(
        fixture.token(&fixture.stranger, "alice/main").await.0,
        StatusCode::NOT_FOUND
    );

    // Both users count on the one shared account.
    assert_eq!(
        fixture.token(&fixture.lender, "main").await.0,
        StatusCode::OK
    );
    assert_eq!(fixture.catalog(&fixture.lender).await[0]["liveSessions"], 2);
    assert_eq!(
        fixture.catalog(&fixture.borrower).await[0]["liveSessions"],
        2
    );

    let (status, ended) = fixture
        .end(&fixture.lender, grant["id"].as_str().unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{ended}");
    assert_eq!(ended["endReason"], "revoked");
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("loan_ended"))
    );
    assert!(fixture.catalog(&fixture.borrower).await.is_empty());
    assert_eq!(
        fixture
            .end(&fixture.borrower, grant["id"].as_str().unwrap())
            .await
            .0,
        StatusCode::OK,
        "ending an ended loan is idempotent"
    );
    assert_eq!(
        fixture
            .end(&fixture.stranger, grant["id"].as_str().unwrap())
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    for headers in [&fixture.lender, &fixture.borrower] {
        assert_eq!(
            kinds(&fixture.audit(headers).await),
            ["granted", "token_issued", "ended"]
        );
    }
    assert!(fixture.audit(&fixture.stranger).await.is_empty());
    let (status, loans) = fixture
        .call("GET", "/v1/loans", &fixture.borrower, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(loans[0]["id"], grant["id"]);
}

#[tokio::test]
async fn the_borrower_can_return_a_loan_and_a_renewal_is_a_new_grant() {
    let fixture = Fixture::new().await;
    let first = fixture.lend_main().await;
    let (status, returned) = fixture
        .end(&fixture.borrower, first["id"].as_str().unwrap())
        .await;
    assert_eq!(
        (status, returned["endReason"].clone()),
        (StatusCode::OK, json!("returned"))
    );
    let second = fixture.lend_main().await;
    assert_ne!(first["id"], second["id"]);
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn grant_rules_are_enforced_by_the_server() {
    let fixture = Fixture::new().await;
    let reset = fixture.weekly_reset.load(Ordering::SeqCst);
    let cases = [
        (
            json!({"alias":"main","borrowerEmail":"nobody@sawmills.ai"}),
            StatusCode::NOT_FOUND,
            "borrower_not_found",
        ),
        (
            json!({"alias":"main","borrowerEmail":"alice@sawmills.ai"}),
            StatusCode::BAD_REQUEST,
            "self_loan",
        ),
        (
            json!({"alias":"main","borrowerEmail":"bob@sawmills.ai","until":reset + 1}),
            StatusCode::BAD_REQUEST,
            "loan_end_after_weekly_reset",
        ),
        (
            json!({"alias":"main","borrowerEmail":"bob@sawmills.ai","until":now() - 1}),
            StatusCode::BAD_REQUEST,
            "loan_end_in_past",
        ),
        (
            json!({"alias":"other","borrowerEmail":"bob@sawmills.ai"}),
            StatusCode::NOT_FOUND,
            "account_not_found",
        ),
        (
            json!({"alias":"x/main","borrowerEmail":"bob@sawmills.ai"}),
            StatusCode::BAD_REQUEST,
            "invalid_alias",
        ),
    ];
    for (body, status, reason) in cases {
        let (got, answer) = fixture.lend(body.clone()).await;
        assert_eq!(
            (got, answer["error"].clone()),
            (status, json!(reason)),
            "{body}"
        );
    }
    // Only the lender can lend the account.
    let (status, _) = fixture
        .call(
            "POST",
            "/v1/loans",
            &fixture.borrower,
            json!({"alias":"main","borrowerEmail":"carol@sawmills.ai"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    fixture.lend_main().await;
    let (status, answer) = fixture
        .lend(json!({"alias":"main","borrowerEmail":"carol@sawmills.ai"}))
        .await;
    assert_eq!(
        (status, answer["error"].clone()),
        (StatusCode::CONFLICT, json!("loan_exists"))
    );
}

#[tokio::test]
async fn a_past_weekly_reset_refuses_the_grant() {
    let fixture = Fixture::new().await;
    fixture.weekly_reset.store(now() - 60, Ordering::SeqCst);
    let (status, answer) = fixture
        .lend(json!({"alias":"main","borrowerEmail":"bob@sawmills.ai"}))
        .await;
    assert_eq!(
        (status, answer["error"].clone()),
        (StatusCode::CONFLICT, json!("weekly_reset_unknown"))
    );
}

#[tokio::test]
async fn a_long_window_other_than_a_week_refuses_the_grant() {
    let fixture = Fixture::new().await;
    for seconds in [86_400, 30 * 86_400] {
        fixture.long_window_seconds.store(seconds, Ordering::SeqCst);
        let (status, answer) = fixture
            .lend(json!({"alias":"main","borrowerEmail":"bob@sawmills.ai"}))
            .await;
        assert_eq!(
            (status, answer["error"].clone()),
            (StatusCode::CONFLICT, json!("weekly_reset_unknown")),
            "{seconds}"
        );
    }
}

#[tokio::test]
async fn an_expired_loan_ends_itself_at_the_next_token_request() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let store = fixture.broker.loan_store();
    let id = grant["id"].as_str().unwrap();
    let mut stored = store.load_loan(id).await.unwrap().unwrap();
    // Move the end into the past through the store, as time would.
    store
        .end_loan(id, 0, "test", crate::central::loans::EndReason::Revoked)
        .await
        .unwrap();
    stored.id = "past-end".into();
    stored.ends_at = now() - 1;
    assert!(store.create_loan(&stored).await.unwrap());
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("loan_ended"))
    );
    let expired = store.load_loan("past-end").await.unwrap().unwrap();
    assert_eq!(
        (expired.end_reason, expired.ended_by),
        (Some(crate::central::loans::EndReason::Expired), None),
        "an automatic expiry has no user actor"
    );
    let audit = fixture.audit(&fixture.lender).await;
    let expiry = audit
        .iter()
        .find(|e| e["grantId"] == "past-end" && e["kind"] == "ended")
        .unwrap();
    assert_eq!(
        (expiry["actor"].clone(), expiry["reason"].clone()),
        (Value::Null, json!("expired"))
    );
}

#[tokio::test]
async fn a_disabled_lender_or_a_changed_login_pauses_the_loan() {
    let fixture = Fixture::new().await;
    fixture.lend_main().await;
    fixture.set_user(LENDER, |user| user.enabled = false);
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("loan_paused"))
    );
    let paused = fixture.catalog(&fixture.borrower).await;
    assert_eq!(
        (
            paused[0]["available"].clone(),
            paused[0]["loan"]["paused"].clone()
        ),
        (json!(false), json!("lender_disabled"))
    );
    fixture.set_user(LENDER, |user| user.enabled = true);
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );

    // A login renewal to another login replaces the lender's owner.
    fixture.add_owner(LENDER, "main", "another-login").await;
    assert_eq!(
        fixture.token(&fixture.lender, "main").await.0,
        StatusCode::OK
    );
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("loan_paused"))
    );
    assert!(kinds(&fixture.audit(&fixture.borrower).await).contains(&"paused".to_owned()));
}

#[tokio::test]
async fn a_removed_lender_alias_ends_the_loan() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    fixture.broker.owners.write().await.clear();
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("loan_ended"))
    );
    let stored = fixture
        .broker
        .loan_store()
        .load_loan(grant["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.end_reason,
        Some(crate::central::loans::EndReason::AccountRemoved)
    );
}

#[tokio::test]
async fn lender_only_paths_refuse_a_borrowed_reference() {
    let fixture = Fixture::new().await;
    fixture.lend_main().await;
    let id = "0".repeat(64);
    for (path, body) in [
        (
            "/v1/resets/redeem",
            json!({"alias":"alice/main","redeem_request_id":"r"}),
        ),
        ("/v1/relogin/start", json!({"alias":"alice/main","id":id})),
        ("/v1/relogin/status", json!({"alias":"alice/main","id":id})),
        ("/v1/relogin/cancel", json!({"alias":"alice/main","id":id})),
        (
            "/v1/accounts",
            json!({"alias":"alice/main","label":null,"auth":auth("x","y")}),
        ),
    ] {
        let (status, answer) = fixture.call("POST", path, &fixture.borrower, body).await;
        assert!(
            matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND),
            "{path}: {status} {answer}"
        );
    }
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_loan_that_ends_during_the_refresh_delivers_no_token() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let owner = fixture.owner(LENDER, "main").await;
    let held = owner.lock().await;
    let request = {
        let base = fixture.base.clone();
        let headers = fixture.borrower.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/token"))
                .headers(headers)
                .json(&json!({"alias":"alice/main"}))
                .send()
                .await
                .unwrap()
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.broker.work.available_permits() == 128 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("token request reached the owner");
    assert_eq!(
        fixture
            .end(&fixture.lender, grant["id"].as_str().unwrap())
            .await
            .0,
        StatusCode::OK
    );
    drop(held);
    let response = request.await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "loan_ended"
    );
    assert!(
        !kinds(&fixture.audit(&fixture.lender).await).contains(&"token_issued".to_owned()),
        "a refused request records no token issue"
    );
}

#[tokio::test]
async fn a_revoked_borrower_machine_gets_no_token() {
    let fixture = Fixture::new().await;
    fixture.lend_main().await;
    let mut devices = vault::devices(&fixture.state).unwrap();
    devices
        .iter_mut()
        .filter(|d| d.user == BORROWER)
        .for_each(|d| d.revoked = true);
    vault::save_devices(&fixture.state, &devices).unwrap();
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_stored_reference_survives_an_email_change_and_a_twin_is_ambiguous() {
    let fixture = Fixture::new().await;
    fixture.lend_main().await;
    fixture.set_user(LENDER, |user| user.email = "a.smith@sawmills.ai".into());
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );
    assert_eq!(
        fixture.token(&fixture.borrower, "a.smith/main").await.0,
        StatusCode::NOT_FOUND
    );

    // A new user with the old local part cannot take over the reference
    // silently: two grants with one reference are refused.
    record_user(&fixture.state, "twin", "alice@other.example").unwrap();
    let credential = fixture.state.join("../twin-machine");
    crate::central::register(
        &fixture.state,
        "twin-machine",
        "sawmills",
        "twin",
        &credential,
    )
    .unwrap();
    let mut twin = HeaderMap::new();
    twin.insert(
        "authorization",
        format!("Bearer {}", std::fs::read_to_string(credential).unwrap())
            .parse()
            .unwrap(),
    );
    fixture.add_owner("twin", "main", "twin-login").await;
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/loans",
            &twin,
            json!({"alias":"main","borrowerEmail":"bob@sawmills.ai"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("ambiguous_loan"))
    );
    // Automatic selection must not pick a name that cannot resolve.
    let listed = fixture.catalog(&fixture.borrower).await;
    assert_eq!(listed.len(), 2);
    for entry in listed {
        assert_eq!(
            (entry["available"].clone(), entry["loan"]["paused"].clone()),
            (json!(false), json!("ambiguous_loan"))
        );
    }
}

#[tokio::test]
async fn the_borrowers_dashboard_keeps_working_and_leaves_out_borrowed_accounts() {
    let fixture = Fixture::new().await;
    fixture.add_owner(BORROWER, "own", "borrower-login").await;
    fixture.lend_main().await;
    let mut broker = fixture.broker.clone();
    broker.sso = Some(Arc::new(crate::central::enrollment::Sso::testing_session(
        BORROWER,
    )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/accounts/data", listener.local_addr().unwrap());
    let app = crate::central::dashboard::routes("https://accounts.example").with_state(broker);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .get(&url)
        .header("cookie", "codexctl-session=synthetic-session")
        .send()
        .await
        .unwrap();
    server.abort();
    assert_eq!(response.status(), StatusCode::OK);
    let data: Value = response.json().await.unwrap();
    let aliases: Vec<_> = data["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|account| account["alias"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(aliases, ["own"]);
}

#[tokio::test]
async fn an_automatic_expiry_applies_the_retention() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let store = fixture.broker.loan_store();
    let id = grant["id"].as_str().unwrap();
    let mut stored = store.load_loan(id).await.unwrap().unwrap();
    let old = now() - crate::central::loans::RETENTION_SECONDS - 60;
    store
        .end_loan(id, old, "test", crate::central::loans::EndReason::Revoked)
        .await
        .unwrap();
    stored.id = "expiring".into();
    stored.ends_at = now() - 1;
    assert!(store.create_loan(&stored).await.unwrap());
    // The next request expires the second grant and prunes the old one.
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(store.load_loan(id).await.unwrap().is_none());
    assert!(store.load_loan("expiring").await.unwrap().is_some());
}

#[tokio::test]
async fn the_audit_pages_by_time_without_losing_events_at_a_boundary() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let id = grant["id"].as_str().unwrap();
    let store = fixture.broker.loan_store();
    // Three machines at one time straddle the page boundary.
    // Recent times, inside the retention window.
    let base = now() - 1_000;
    for (at, machine) in [(100, "a"), (200, "b"), (200, "c"), (200, "d"), (300, "e")] {
        let at = base + at;
        store
            .append_loan_audit(&crate::central::loans::AuditEvent::token_issued(
                at, id, machine,
            ))
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    let mut before: Option<i64> = None;
    for _ in 0..10 {
        let path = match before {
            Some(before) => format!("/v1/loans/audit?limit=2&before={before}"),
            None => "/v1/loans/audit?limit=2".to_owned(),
        };
        let (status, page) = fixture
            .call("GET", &path, &fixture.lender, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let events = page["events"].as_array().unwrap();
        assert!(!events.is_empty(), "{page}");
        seen.extend(events.iter().map(|e| e["at"].as_i64().unwrap()));
        match page["before"].as_i64() {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    let granted = grant["createdAt"].as_i64().unwrap();
    assert_eq!(
        seen,
        [
            granted,
            base + 300,
            base + 200,
            base + 200,
            base + 200,
            base + 100
        ]
    );
    let (status, _) = fixture
        .call(
            "GET",
            "/v1/loans/audit?id=unknown",
            &fixture.lender,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_lane_pinned_to_an_ended_loan_never_moves_to_a_new_grant_of_that_name() {
    let fixture = Fixture::new().await;
    let first = fixture.lend_main().await;
    let first_id = first["id"].as_str().unwrap();
    let pinned = json!({"alias":"alice/main","loanId":first_id});
    let (status, body) = fixture
        .call("POST", "/v1/token", &fixture.borrower, pinned.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    fixture.end(&fixture.lender, first_id).await;
    fixture.lend_main().await;
    let (status, body) = fixture
        .call("POST", "/v1/token", &fixture.borrower, pinned)
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("loan_ended"))
    );
    // A new selection of the name reaches the new grant.
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_conflicting_login_uid_pauses_and_a_gained_uid_does_not() {
    let fixture = Fixture::new().await;
    // The grant records a login without a UID; the token then gains one.
    fixture.lend_main().await;
    fixture
        .add_owner_with(
            LENDER,
            "main",
            "gained",
            auth_with_uid("synthetic-login", Some("u1"), "seat-lender-main"),
        )
        .await;
    assert_eq!(
        fixture.token(&fixture.borrower, "alice/main").await.0,
        StatusCode::OK
    );

    // A new grant records the UID; the same subject with another UID differs.
    let fixture = Fixture::new().await;
    fixture
        .add_owner_with(
            LENDER,
            "main",
            "first",
            auth_with_uid("synthetic-login", Some("u1"), "seat-lender-main"),
        )
        .await;
    fixture.lend_main().await;
    fixture
        .add_owner_with(
            LENDER,
            "main",
            "other",
            auth_with_uid("synthetic-login", Some("u2"), "seat-lender-main"),
        )
        .await;
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("loan_paused"))
    );
}

#[tokio::test]
async fn an_email_shared_by_two_company_users_refuses_the_grant() {
    let fixture = Fixture::new().await;
    record_user(&fixture.state, "previous-holder", "Bob@sawmills.ai").unwrap();
    let (status, answer) = fixture
        .lend(json!({"alias":"main","borrowerEmail":"bob@sawmills.ai"}))
        .await;
    assert_eq!(
        (status, answer["error"].clone()),
        (StatusCode::CONFLICT, json!("borrower_ambiguous"))
    );
}

#[tokio::test]
async fn a_loan_store_failure_keeps_owned_accounts_listed_and_refuses_borrowed_tokens() {
    let fixture = Fixture::new().await;
    fixture.lend_main().await;
    std::fs::write(
        fixture.state.join("central-storage.enc"),
        b"not encrypted state",
    )
    .unwrap();
    let listed = fixture.catalog(&fixture.lender).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["alias"], "main");
    assert!(fixture.catalog(&fixture.borrower).await.is_empty());
    let (status, body) = fixture.token(&fixture.borrower, "alice/main").await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("loan_store_failed"))
    );
}

#[tokio::test]
async fn a_lender_credential_in_another_workspace_pauses_before_the_account_check() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    fixture
        .add_owner_with(
            LENDER,
            "main",
            "moved",
            auth("synthetic-login", "another-seat"),
        )
        .await;
    let (status, body) = fixture
        .call(
            "POST",
            "/v1/token",
            &fixture.borrower,
            json!({"alias":"alice/main","accountId":"seat-lender-main","loanId":grant["id"]}),
        )
        .await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::CONFLICT, json!("loan_paused"))
    );
    assert!(kinds(&fixture.audit(&fixture.borrower).await).contains(&"paused".to_owned()));
}

#[tokio::test]
async fn a_read_hides_a_loan_that_ended_more_than_90_days_ago() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let id = grant["id"].as_str().unwrap();
    let old = now() - crate::central::loans::RETENTION_SECONDS - 60;
    fixture
        .broker
        .loan_store()
        .end_loan(id, old, "test", crate::central::loans::EndReason::Revoked)
        .await
        .unwrap();
    // No lifecycle activity happens; the read itself applies the retention.
    let (status, loans) = fixture
        .call("GET", "/v1/loans", &fixture.lender, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(loans.as_array().unwrap().is_empty(), "{loans}");
}

#[tokio::test]
async fn a_history_read_fails_when_the_retention_cannot_be_applied() {
    let fixture = Fixture::new().await;
    let grant = fixture.lend_main().await;
    let old = now() - crate::central::loans::RETENTION_SECONDS - 60;
    fixture
        .broker
        .loan_store()
        .end_loan(
            grant["id"].as_str().unwrap(),
            old,
            "test",
            crate::central::loans::EndReason::Revoked,
        )
        .await
        .unwrap();
    // Hold the store lock so the retirement write cannot run.
    let held = vault::registry_lock(&fixture.state, "central-storage.lock").unwrap();
    let (status, body) = fixture
        .call("GET", "/v1/loans", &fixture.lender, Value::Null)
        .await;
    drop(held);
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("loan_store_failed"))
    );
}
