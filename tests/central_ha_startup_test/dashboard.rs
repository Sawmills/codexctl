use super::*;

struct BrowserFixture {
    first: Pod,
    second: Pod,
    issuer: Child,
    _root: tempfile::TempDir,
    control: tokio_postgres::Client,
    schema: String,
    http: reqwest::Client,
}

impl BrowserFixture {
    async fn start() -> Self {
        let database = std::env::var("DATABASE_URL").expect("DATABASE_URL for dashboard HA tests");
        let (control, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move { connection.await.unwrap() });
        let schema = format!(
            "browser_{}",
            central::enrollment::random_bytes()[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        control
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let mut database = reqwest::Url::parse(&database).unwrap();
        database
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        database
            .query_pairs_mut()
            .append_pair("application_name", &schema);
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        central::managed::setup(&root.path().join("state"), &key).unwrap();
        store::atomic_write(
            &root.path().join("identity.json"),
            &serde_json::to_vec(
                &json!({"sub":"dashboard-company-user", "email":"dashboard@example.invalid"}),
            )
            .unwrap(),
        )
        .unwrap();
        let mut issuer = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/company_oidc.py"))
            .arg(root.path())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut line = String::new();
        timeout(
            Duration::from_secs(10),
            BufReader::new(issuer.stdout.take().unwrap()).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        let secret = root.path().join("oidc.secret");
        store::atomic_write(&secret, b"synthetic-company-client-secret").unwrap();
        let config = serde_json::to_vec(&json!({"issuer":ready["issuer"],"client_id":"codexctl-test","client_secret_file":secret,"allowed_domains":["example.invalid"]})).unwrap();
        let output = command(
            database.as_str(),
            &root.path().join("state"),
            &key,
            "migrate",
        )
        .output()
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "migrate: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let first = Self::pod(database.as_str(), &key, &config).await;
        let second = Self::pod(database.as_str(), &key, &config).await;
        Self {
            first,
            second,
            issuer,
            _root: root,
            control,
            schema,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    async fn pod(database: &str, key: &Path, config: &[u8]) -> Pod {
        let root = tempfile::tempdir().unwrap();
        central::managed::setup(&root.path().join("state"), &root.path().join("key")).unwrap();
        store::atomic_write(&root.path().join("sso.json"), config).unwrap();
        store::atomic_write(
            &root.path().join("metrics.token"),
            b"synthetic-dashboard-metrics-token-32",
        )
        .unwrap();
        Pod::spawn(database, key, root, "postgres").await
    }

    async fn begin_sign_in(&self) -> (String, String) {
        let start = self
            .http
            .get(format!("{}/accounts/sign-in", self.first.url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            start.status(),
            303,
            "dashboard sign-in must work in PostgreSQL mode"
        );
        let binding = start
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let authorize = self
            .http
            .get(start.headers()["location"].to_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(authorize.status(), 302);
        let callback =
            reqwest::Url::parse(authorize.headers()["location"].to_str().unwrap()).unwrap();
        let path = format!("{}?{}", callback.path(), callback.query().unwrap());
        (binding, path)
    }

    async fn callback(&self, pod: &Pod, binding: &str, path: &str) -> reqwest::Response {
        self.http
            .get(format!("{}{path}", pod.url))
            .header("cookie", binding)
            .send()
            .await
            .unwrap()
    }

    async fn sign_in(&self) -> (String, String) {
        let (binding, path) = self.begin_sign_in().await;
        let response = self.callback(&self.second, &binding, &path).await;
        assert_eq!(
            response.status(),
            303,
            "callback must work on the other pod"
        );
        let cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            cookie.contains("HttpOnly")
                && cookie.contains("SameSite=Lax")
                && cookie.contains("Path=/")
                && cookie.contains("Max-Age=3600")
        );
        let session = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|h| h.to_str().ok())
            .find(|h| h.starts_with("codexctl-session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        (session, path)
    }

    async fn finish(mut self) {
        self.first.stop().await;
        self.second.stop().await;
        self.issuer.kill().await.unwrap();
        self.control
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
    }

    fn database(&self) -> String {
        let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={}", self.schema));
        url.query_pairs_mut()
            .append_pair("application_name", &self.schema);
        url.to_string()
    }
}

#[tokio::test]
async fn postgres_dashboard_sign_in_callback_and_session_work_on_different_pods() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    for pod in [&f.first, &f.second] {
        let page = f
            .http
            .get(format!("{}/accounts", pod.url))
            .header("cookie", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert!(
            page.text()
                .await
                .unwrap()
                .contains("dashboard@example.invalid")
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_lists_only_the_company_users_machines_from_the_store() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let user: String = f
        .control
        .query_one(
            &format!(
                "SELECT id FROM {}.central_users WHERE email='dashboard@example.invalid'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    f.control.execute(&format!("INSERT INTO {}.central_users(id,email,enabled) VALUES('foreign','foreign@example.invalid',true)", f.schema), &[]).await.unwrap();
    for (id, tenant, owner) in [
        ("visible-machine", "sawmills", user.as_str()),
        ("foreign-machine", "sawmills", "foreign"),
        ("other-tenant-machine", "elsewhere", user.as_str()),
    ] {
        f.control.execute(&format!("INSERT INTO {}.central_devices(id,tenant,user_id,token_hash,revoked) VALUES($1,$2,$3,$4,false)", f.schema), &[&id,&tenant,&owner,&id]).await.unwrap();
    }
    for pod in [&f.first, &f.second] {
        let response = f
            .http
            .get(format!("{}/accounts/data", pod.url))
            .header("cookie", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let data: Value = response.json().await.unwrap();
        assert_eq!(data["machines"].as_array().unwrap().len(), 1);
        assert_eq!(data["machines"][0]["name"], "visible-machine");
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_callback_state_is_once_only_and_browser_bound() {
    let f = BrowserFixture::start().await;
    let (binding, path) = f.begin_sign_in().await;
    let wrong = f.callback(&f.second, "codexctl-login=wrong", &path).await;
    assert_eq!(wrong.status(), 401);
    let replay = f.callback(&f.first, &binding, &path).await;
    assert_eq!(replay.status(), 400);
    assert_eq!(
        replay.json::<Value>().await.unwrap()["error"],
        "invalid_sso_state"
    );
    let (_, completed) = f.sign_in().await;
    let replay = f.callback(&f.first, &binding, &completed).await;
    assert_eq!(replay.status(), 400);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_out_on_one_pod_ends_the_session_everywhere() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let cross_site = f
        .http
        .post(format!("{}/accounts/sign-out", f.second.url))
        .header("cookie", &session)
        .header("origin", "https://elsewhere.invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), 403);
    let still_live = f
        .http
        .get(format!("{}/accounts/data", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(still_live.status(), 200);
    let ended = f
        .http
        .post(format!("{}/accounts/sign-out", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .send()
        .await
        .unwrap();
    assert_eq!(ended.status(), 303);
    assert!(
        ended.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    for pod in [&f.first, &f.second] {
        let response = f
            .http
            .get(format!("{}/accounts", pod.url))
            .header("cookie", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/accounts/sign-in");
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_expired_sessions_redirect_and_are_retired() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.control
        .batch_execute(&format!(
            "UPDATE {}.browser_sessions SET expires_at=now()-interval '1 second'",
            f.schema
        ))
        .await
        .unwrap();
    let response = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers()["location"], "/accounts/sign-in");
    let count: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.browser_sessions", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_checks_disabled_and_deleted_company_users_on_each_request() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    for mutation in ["enabled=false", "enabled=true,deleted_at=now()"] {
        f.control
            .batch_execute(&format!("UPDATE {}.central_users SET {mutation}", f.schema))
            .await
            .unwrap();
        for pod in [&f.first, &f.second] {
            let response = f
                .http
                .get(format!("{}/accounts/data", pod.url))
                .header("cookie", &session)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "user_disabled"
            );
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sessions_store_cookie_digests_and_signed_auth_time() {
    let f = BrowserFixture::start().await;
    store::atomic_write(&f._root.path().join("identity.json"),&serde_json::to_vec(&json!({"sub":"dashboard-company-user","email":"dashboard@example.invalid","auth_time":1700000000})).unwrap()).unwrap();
    let (session, _) = f.sign_in().await;
    let hash = {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(session.split_once('=').unwrap().1.as_bytes())
        )
    };
    let row=f.control.query_one(&format!("SELECT token_hash,extract(epoch FROM signed_in_at)::bigint,extract(epoch FROM expires_at-now())::bigint FROM {}.browser_sessions",f.schema),&[]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), hash);
    assert_eq!(row.get::<_, i64>(1), 1700000000);
    assert!((3598..=3600).contains(&row.get::<_, i64>(2)));
    let payload: Vec<u8> = f
        .control
        .query_one(
            &format!(
                "SELECT encrypted_payload FROM {}.enrollment_challenges",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        serde_json::from_slice::<Value>(&payload).is_err(),
        "browser binding and nonce state must remain encrypted"
    );
    {
        use aes_gcm::{
            Aes256Gcm,
            aead::{Aead, KeyInit},
        };
        let cipher =
            Aes256Gcm::new_from_slice(&std::fs::read(f._root.path().join("key")).unwrap()).unwrap();
        let plain = cipher
            .decrypt((&payload[..12]).into(), &payload[12..])
            .unwrap();
        let stored: Value = serde_json::from_slice(&plain).unwrap();
        let keys: Vec<_> = stored
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["binding", "nonce"],
            "never persist a PKCE verifier, even in the encrypted flow"
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_session_cap_is_global_and_reclaims_expired_rows() {
    let f = BrowserFixture::start().await;
    f.sign_in().await;
    f.control.batch_execute(&format!("INSERT INTO {s}.browser_sessions(token_hash,user_id,signed_in_at,expires_at) SELECT lpad(n::text,64,'0'),(SELECT id FROM {s}.central_users LIMIT 1),now(),now()+interval '1 hour' FROM generate_series(1,1022) n",s=f.schema)).await.unwrap();
    let (binding_a, path_a) = f.begin_sign_in().await;
    let (binding_b, path_b) = f.begin_sign_in().await;
    let (a, b) = tokio::join!(
        f.callback(&f.first, &binding_a, &path_a),
        f.callback(&f.second, &binding_b, &path_b)
    );
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(
        statuses,
        [303, 429],
        "only one remaining session slot exists across both pods"
    );
    f.control.batch_execute(&format!("UPDATE {}.browser_sessions SET expires_at=now()-interval '1 second' WHERE token_hash=lpad('1',64,'0')",f.schema)).await.unwrap();
    f.sign_in().await;
    let count: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.browser_sessions", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1024);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_migration_is_additive_repeatable_and_keeps_version_eight() {
    let f = BrowserFixture::start().await;
    let before = chrono::Utc::now().timestamp();
    let (session, _) = f.sign_in().await;
    for _ in 0..2 {
        let result = command(
            &f.database(),
            &f._root.path().join("state"),
            &f._root.path().join("key"),
            "migrate",
        )
        .output()
        .await
        .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let version: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT max(version)::bigint FROM {}.central_schema_migrations",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        version, 8,
        "rollback-compatible additive table must not advance READY_VERSION"
    );
    let signed: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT extract(epoch FROM signed_in_at)::bigint FROM {}.browser_sessions",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        signed >= before && signed <= chrono::Utc::now().timestamp(),
        "absent auth_time uses callback time"
    );
    let live = f
        .http
        .get(format!("{}/accounts/data", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), 200, "migrate preserves existing sessions");
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_support_does_not_enable_machine_enrollment() {
    let f = BrowserFixture::start().await;
    for (path, body) in [
        ("/v1/enrollment/start", json!({"name":"machine"})),
        ("/v1/enrollment/poll", json!({"device_code":"synthetic"})),
    ] {
        let response = f
            .http
            .post(format!("{}{path}", f.first.url))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }
    let response = f
        .http
        .post(format!("{}/auth/approve", f.first.url))
        .form(&[("approval", "synthetic")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let response = f
        .http
        .get(format!("{}/enroll?code=synthetic", f.first.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(f.first.launches(), 0);
    assert_eq!(f.second.launches(), 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_missing_browser_table_fails_only_the_dashboard_at_startup() {
    let mut f = BrowserFixture::start().await;
    f.control
        .batch_execute(&format!("DROP TABLE {}.browser_sessions", f.schema))
        .await
        .unwrap();
    f.first.stop().await;
    f.first = Pod::spawn(
        &{
            let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
            url.query_pairs_mut()
                .append_pair("options", &format!("-csearch_path={}", f.schema));
            url.to_string()
        },
        &f._root.path().join("key"),
        f.first.root,
        "postgres",
    )
    .await;
    let ready = f
        .http
        .get(format!("{}/ready", f.first.url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ready.status(),
        200,
        "a dashboard table must not block token service readiness"
    );
    let response = f
        .http
        .get(format!("{}/accounts/sign-in", f.first.url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "dashboard must fail before starting a sign-in flow"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "browser_sessions_unavailable"
    );
    let data = f
        .http
        .get(format!("{}/accounts/data", f.first.url))
        .send()
        .await
        .unwrap();
    assert_eq!(data.status(), 503);
    assert_eq!(
        data.json::<Value>().await.unwrap()["error"],
        "browser_sessions_unavailable"
    );
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth("synthetic-dashboard-metrics-token-32")
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    assert!(
        metrics.text().await.unwrap().contains(
            "codexctl_central_failed_requests_total{reason=\"browser_sessions_unavailable\"} 2"
        ),
        "each failed dashboard request counts once"
    );
    let log = std::fs::read_to_string(f.first.root.path().join("dashboard.log")).unwrap();
    let startup = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["stage"] == "startup" && v["reason"] == "browser_sessions_unavailable")
        .count();
    assert_eq!(startup, 1);
    assert_eq!(f.first.launches(), 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_expired_oidc_flow_does_not_create_a_session() {
    let f = BrowserFixture::start().await;
    let (binding, path) = f.begin_sign_in().await;
    f.control
        .batch_execute(&format!(
            "UPDATE {}.enrollment_challenges SET expires_at=now()-interval '1 second'",
            f.schema
        ))
        .await
        .unwrap();
    let response = f.callback(&f.second, &binding, &path).await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "invalid_sso_state"
    );
    let count: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.browser_sessions", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_serializes_competing_company_identities_with_the_same_email() {
    let mut f = BrowserFixture::start().await;
    let database = f.database();
    let key = f._root.path().join("key");
    for pod in [&mut f.first, &mut f.second] {
        pod.stop().await;
        let config_file = pod.root.path().join("sso.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
        config["allowed_hosted_domains"] = json!(["example.invalid"]);
        store::atomic_write(&config_file, &serde_json::to_vec(&config).unwrap()).unwrap();
        let root = std::mem::replace(&mut pod.root, tempfile::tempdir().unwrap());
        *pod = Pod::spawn(&database, &key, root, "postgres").await;
    }
    let mut callbacks = Vec::new();
    for sub in ["first-company-identity", "second-company-identity"] {
        store::atomic_write(&f._root.path().join("identity.json"),&serde_json::to_vec(&json!({"sub":sub,"email":"same@example.invalid","hd":"example.invalid","freeze_on_authorize":true})).unwrap()).unwrap();
        callbacks.push(f.begin_sign_in().await);
    }
    f.control
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {}.central_users IN SHARE MODE",
            f.schema
        ))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for (pod, (binding, path)) in [&f.first, &f.second].into_iter().zip(callbacks) {
        let http = f.http.clone();
        let url = pod.url.clone();
        tasks.push(tokio::spawn(async move {
            http.get(format!("{url}{path}"))
                .header("cookie", binding)
                .send()
                .await
                .unwrap()
        }));
    }
    timeout(Duration::from_secs(1),async {
        loop {
            f.control.query_one("SELECT pg_stat_clear_snapshot()", &[]).await.unwrap();
            let waiting:i64=f.control.query_one("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock'",&[&f.schema]).await.unwrap().get(0);
            if waiting>=2 { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("both callbacks must reach the contested registry before release");
    f.control.batch_execute("COMMIT").await.unwrap();
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap().status().as_u16());
    }
    statuses.sort();
    assert_eq!(
        statuses,
        [303, 403],
        "one verified email must not acquire two company-user identities"
    );
    let users: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT count(*) FROM {}.central_users WHERE email='same@example.invalid'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(users, 1);
    f.finish().await;
}
