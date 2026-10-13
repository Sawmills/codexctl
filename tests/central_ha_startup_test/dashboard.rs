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
        self.begin_sign_in_route("/accounts/sign-in", false).await
    }

    async fn begin_sign_in_route(&self, route: &str, fresh: bool) -> (String, String) {
        let start = self
            .http
            .get(format!("{}{route}", self.first.url))
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
        let location = reqwest::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
        if fresh {
            assert!(
                location
                    .query_pairs()
                    .any(|(k, v)| k == "max_age" && v == "600"),
                "fresh Google sign-in asks for max_age=600"
            );
        }
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
        let root = f
            .http
            .get(format!("{}/", pod.url))
            .header("cookie", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(root.status(), 303);
        assert_eq!(root.headers()["location"], "/accounts");
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
    // A failure while reading another user's device must not break this user's list.
    f.control.batch_execute(&format!(
        "ALTER TABLE {0}.central_devices RENAME TO central_devices_source; CREATE FUNCTION {0}.device_token_hash(text,text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN IF $2='foreign' THEN RAISE EXCEPTION 'synthetic foreign device read failure'; END IF; RETURN $1; END $$; CREATE VIEW {0}.central_devices AS SELECT id,tenant,user_id,{0}.device_token_hash(token_hash,user_id) AS token_hash,revoked,revision,deleted_at FROM {0}.central_devices_source", f.schema,
    )).await.unwrap();
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
            ["binding", "fresh", "nonce"],
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
    // Recreate PR1's table shape with a live session before upgrading.
    f.control.batch_execute(&format!("ALTER TABLE {0}.browser_sessions DROP COLUMN auth_time_verified; DROP TABLE {0}.dashboard_audit",f.schema)).await.unwrap();
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
    let verified: bool = f
        .control
        .query_one(
            &format!(
                "SELECT auth_time_verified FROM {}.browser_sessions",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        !verified,
        "legacy callback timestamps never gain fresh sign-in authority during migration"
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
    for cookie in [None, Some("codexctl-session=synthetic-expired-session")] {
        let mut landing = f.http.get(format!("{}/", f.first.url));
        if let Some(cookie) = cookie {
            landing = landing.header("cookie", cookie);
        }
        assert_eq!(landing.send().await.unwrap().status(), 200);
    }
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth("synthetic-dashboard-metrics-token-32")
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    let metrics = metrics.text().await.unwrap();
    assert_eq!(
        metrics.lines().find(|line| line.starts_with(
            "codexctl_central_failed_requests_total{reason=\"browser_sessions_unavailable\"}"
        )),
        Some("codexctl_central_failed_requests_total{reason=\"browser_sessions_unavailable\"} 2"),
        "only failed dashboard requests count; successful public visits do not"
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
async fn postgres_dashboard_startup_probe_failure_preserves_the_database_cause() {
    let f = BrowserFixture::start().await;
    // Scope the fault to this schema and this probe; other startup queries still work.
    f.control.batch_execute(&format!(
        "CREATE FUNCTION {0}.to_regclass(text) RETURNS regclass LANGUAGE plpgsql AS $$ BEGIN IF $1='browser_sessions' THEN RAISE EXCEPTION 'synthetic browser probe failure'; END IF; RETURN pg_catalog.to_regclass($1); END $$", f.schema,
    )).await.unwrap();
    let mut database = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    database
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={},pg_catalog", f.schema));
    let root = tempfile::tempdir().unwrap();
    central::managed::setup(&root.path().join("state"), &root.path().join("key")).unwrap();
    let mut child = command(
        database.as_str(),
        &root.path().join("state"),
        &f._root.path().join("key"),
        "serve",
    )
    .args([
        "--listen",
        "127.0.0.1:0",
        "--public-url",
        "http://127.0.0.1:8787",
        "--sso-config",
    ])
    .arg(f.first.root.path().join("sso.json"))
    .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut line = String::new();
    timeout(
        Duration::from_secs(5),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        line.is_empty(),
        "a failed database probe must not publish server readiness: {line}"
    );
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let event = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["reason"] == "browser_sessions_probe_failed")
        .expect("distinct probe failure event");
    assert_eq!(event["stage"], "startup");
    assert!(
        event["error"]
            .as_str()
            .unwrap()
            .contains("synthetic browser probe failure")
    );
    assert!(
        !stderr.contains("browser_sessions_unavailable"),
        "do not misdiagnose a database fault as missing DDL"
    );
    let mut token_only = command(
        database.as_str(),
        &root.path().join("state"),
        &f._root.path().join("key"),
        "serve",
    )
    .args([
        "--listen",
        "127.0.0.1:0",
        "--public-url",
        "http://127.0.0.1:8787",
    ])
    .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut ready_line = String::new();
    timeout(
        Duration::from_secs(5),
        BufReader::new(token_only.stdout.take().unwrap()).read_line(&mut ready_line),
    )
    .await
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&ready_line)
        .expect("a dashboard-only probe must not block token-only startup");
    let health = f
        .http
        .get(format!(
            "http://{}/ready",
            ready["listening"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    token_only.kill().await.unwrap();
    token_only.wait().await.unwrap();
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

#[tokio::test]
async fn postgres_dashboard_pending_flows_are_bounded_and_retired_on_new_sign_in() {
    let f = BrowserFixture::start().await;
    f.control.batch_execute(&format!("INSERT INTO {}.enrollment_challenges(challenge_hash,encrypted_payload,expires_at) SELECT lpad(n::text,64,'0'),decode('','hex'),now()+interval '1 hour' FROM generate_series(1,1024) n",f.schema)).await.unwrap();
    for pod in [&f.first, &f.second] {
        let response = f
            .http
            .get(format!("{}/accounts/sign-in", pod.url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            429,
            "pending sign-ins share one capacity bound across pods"
        );
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "enrollment_capacity"
        );
    }
    f.control
        .batch_execute(&format!(
            "UPDATE {}.enrollment_challenges SET expires_at=now()-interval '1 second'",
            f.schema
        ))
        .await
        .unwrap();
    f.sign_in().await;
    f.begin_sign_in().await;
    let count: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.enrollment_challenges", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "a new sign-in retires expired and consumed state");
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_last_pending_flow_slot_is_global_across_pods() {
    let f = BrowserFixture::start().await;
    for _ in 0..3 {
        f.control.batch_execute(&format!(
            "DELETE FROM {0}.enrollment_challenges; INSERT INTO {0}.enrollment_challenges(challenge_hash,encrypted_payload,expires_at) SELECT lpad(n::text,64,'0'),decode('','hex'),now()+interval '1 hour' FROM generate_series(1,1023) n",
            f.schema,
        )).await.unwrap();
        let (first, second) = tokio::join!(
            f.http
                .get(format!("{}/accounts/sign-in", f.first.url))
                .send(),
            f.http
                .get(format!("{}/accounts/sign-in", f.second.url))
                .send(),
        );
        let mut statuses = [
            first.unwrap().status().as_u16(),
            second.unwrap().status().as_u16(),
        ];
        statuses.sort();
        assert_eq!(
            statuses,
            [303, 429],
            "only one pod can admit the final pending flow"
        );
        let count: i64 = f.control.query_one(
            &format!("SELECT count(*) FROM {}.enrollment_challenges WHERE consumed_at IS NULL AND expires_at > now()", f.schema),
            &[],
        ).await.unwrap().get(0);
        assert_eq!(count, 1024);
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_logs_only_committed_identity_links_once() {
    let mut f = BrowserFixture::start().await;
    let source = "c".repeat(64);
    f.control.execute(
        &format!("INSERT INTO {}.central_users(id,email,enabled) VALUES($1,'dashboard@example.invalid',true)", f.schema),
        &[&source],
    ).await.unwrap();
    store::atomic_write(&f._root.path().join("identity.json"), &serde_json::to_vec(&json!({
        "sub":"dashboard-company-user", "email":"dashboard@example.invalid", "hd":"example.invalid"
    })).unwrap()).unwrap();
    let database = f.database();
    let key = f._root.path().join("key");
    for pod in [&mut f.first, &mut f.second] {
        pod.stop().await;
        let config_file = pod.root.path().join("sso.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_file).unwrap()).unwrap();
        config["allowed_hosted_domains"] = json!(["example.invalid"]);
        config["clerk_migration"] =
            json!({"users":[{"user_id":source,"email":"dashboard@example.invalid"}]});
        store::atomic_write(&config_file, &serde_json::to_vec(&config).unwrap()).unwrap();
        let root = std::mem::replace(&mut pod.root, tempfile::tempdir().unwrap());
        *pod = Pod::spawn(&database, &key, root, "postgres").await;
    }
    // A deferred failure proves that logging an UPDATE before commit is insufficient.
    f.control.batch_execute(&format!(
        "CREATE FUNCTION {0}.reject_identity_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic identity commit failure'; END $$; CREATE CONSTRAINT TRIGGER reject_identity_commit AFTER UPDATE ON {0}.central_users DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {0}.reject_identity_commit()", f.schema,
    )).await.unwrap();
    let (binding, path) = f.begin_sign_in().await;
    assert_eq!(f.callback(&f.second, &binding, &path).await.status(), 503);
    let log_path = f.second.root.path().join("dashboard.log");
    assert!(
        !std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("SSO_IDENTITY_LINKED")
    );
    f.control
        .batch_execute(&format!(
            "DROP TRIGGER reject_identity_commit ON {}.central_users",
            f.schema
        ))
        .await
        .unwrap();
    f.sign_in().await;
    f.sign_in().await;
    store::atomic_write(&f._root.path().join("identity.json"), &serde_json::to_vec(&json!({
        "sub":"hostile-alternate-subject", "email":"dashboard@example.invalid", "hd":"example.invalid"
    })).unwrap()).unwrap();
    let (binding, path) = f.begin_sign_in().await;
    assert_eq!(f.callback(&f.second, &binding, &path).await.status(), 403);
    let audit = std::fs::read_to_string(log_path).unwrap();
    let events: Vec<_> = audit
        .lines()
        .filter(|line| line.starts_with("SSO_IDENTITY_LINKED "))
        .collect();
    assert_eq!(events.len(), 1, "only the committed first link is audited");
    let suffix = events[0]
        .strip_prefix(&format!(
            "SSO_IDENTITY_LINKED company_user={source} oidc_identity="
        ))
        .unwrap();
    assert_eq!(suffix.len(), 64);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    for secret in [
        "dashboard@example.invalid",
        "dashboard-company-user",
        "hostile-alternate-subject",
        "synthetic-company-client-secret",
        binding.split_once('=').unwrap().1,
    ] {
        assert!(!events[0].contains(secret));
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_in_capacity_lock_is_isolated_from_other_schemas() {
    let f = BrowserFixture::start().await;
    // Hold the legacy database-wide key and the conventional key of another schema.
    // A sign-in in this schema must not wait for either unrelated transaction.
    f.control.query_one("SELECT pg_advisory_lock(73912758), pg_advisory_lock(hashtextextended('unrelated-browser-schema',73912758))", &[]).await.unwrap();
    let response = f
        .http
        .get(format!("{}/accounts/sign-in", f.first.url))
        .send()
        .await
        .unwrap();
    f.control
        .query_one("SELECT pg_advisory_unlock_all()", &[])
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        303,
        "another schema cannot consume this dashboard's capacity wait budget"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_session_capacity_lock_is_isolated_from_other_schemas() {
    let f = BrowserFixture::start().await;
    let (binding, path) = f.begin_sign_in().await;
    f.control.query_one("SELECT pg_advisory_lock(73912757), pg_advisory_lock(hashtextextended('unrelated-browser-schema',73912757))", &[]).await.unwrap();
    let response = f.callback(&f.second, &binding, &path).await;
    f.control
        .query_one("SELECT pg_advisory_unlock_all()", &[])
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        303,
        "another schema cannot block a verified session from being created"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_in_does_not_wait_for_account_admission() {
    let f = BrowserFixture::start().await;
    let (binding, path) = f.begin_sign_in().await;
    f.control
        .query_one(
            "SELECT pg_advisory_lock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    let callback_http = f.http.clone();
    let callback_url = format!("{}{path}", f.first.url);
    let callback = tokio::spawn(async move {
        callback_http
            .get(callback_url)
            .header("cookie", binding)
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(1), async {
        loop {
            let waiting: i64 = f.control.query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock'",
                &[&f.schema],
            ).await.unwrap().get(0);
            if waiting > 0 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("callback must be waiting for the account admission lock");
    let pending = f
        .http
        .get(format!("{}/accounts/sign-in", f.first.url))
        .send()
        .await;
    f.control
        .query_one(
            "SELECT pg_advisory_unlock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    assert_eq!(pending.unwrap().status(), 303);
    assert_eq!(
        callback.await.unwrap().status(),
        303,
        "a pending sign-in must not exhaust the verified callback's admission deadline"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_rejects_missing_or_wrong_origin_and_form_token() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    for (origin, token) in [
        (None, None),
        (Some("https://hostile.example.invalid"), Some("wrong")),
        (Some("http://127.0.0.1:8787"), None),
        (Some("http://127.0.0.1:8787"), Some("wrong")),
    ] {
        let mut request = f
            .http
            .post(format!("{}/accounts/machines/revoke", f.first.url))
            .header("cookie", &session);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let mut form = vec![("id", "missing-machine")];
        if let Some(token) = token {
            form.push(("form_token", token));
        }
        let response = request.form(&form).send().await.unwrap();
        assert_eq!(
            response.status(),
            403,
            "request check runs before machine lookup"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    assert_eq!(f.first.launches(), 0);
    f.finish().await;
}

impl BrowserFixture {
    fn identity(&self, auth_time: Option<i64>) {
        let mut identity =
            json!({"sub":"dashboard-company-user", "email":"dashboard@example.invalid"});
        if let Some(time) = auth_time {
            identity["auth_time"] = json!(time);
        }
        store::atomic_write(
            &self._root.path().join("identity.json"),
            &serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
    }

    async fn machine(&self, id: &str, token: &str) {
        self.control.execute(&format!("INSERT INTO {}.central_devices(id,tenant,user_id,token_hash,revoked) SELECT $1,'sawmills',id,$2,false FROM {}.central_users WHERE email='dashboard@example.invalid'",self.schema,self.schema), &[&id,&{ use sha2::Digest; format!("{:x}", sha2::Sha256::digest(token.as_bytes())) }]).await.unwrap();
    }

    async fn form_token(&self, session: &str) -> String {
        let page = self
            .http
            .get(format!("{}/accounts", self.first.url))
            .header("cookie", session)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert_eq!(page.headers()["cache-control"], "no-store");
        assert!(
            page.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("form-action 'self'")
        );
        let html = page.text().await.unwrap();
        html.split("<input")
            .filter_map(|element| element.split('>').next())
            .find(|element| element.contains("name=\"form_token\""))
            .expect("Revoke form on PostgreSQL dashboard")
            .split("value=\"")
            .nth(1)
            .expect("form token value")
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn revoke(&self, session: &str, token: &str, id: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/accounts/machines/revoke", self.second.url))
            .header("cookie", session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[("id", id), ("form_token", token)])
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn postgres_dashboard_revoke_is_audited_and_duplicate_api_revoke_stays_idempotent() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("target-machine", "synthetic-target-machine-token")
        .await;
    f.machine("caller-machine", "synthetic-caller-machine-token")
        .await;
    let token = f.form_token(&session).await;
    // Keep a real synthetic refresh owner alive while all revoke paths run.
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let claims = json!({"sub":"dashboard-login","iat":2000000000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":"dashboard-seat","chatgpt_plan_type":"pro"}});
    let auth = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),"refresh_token":"synthetic-dashboard-refresh","account_id":"dashboard-seat"}});
    let imported = f
        .http
        .post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-caller-machine-token")
        .json(&json!({"alias":"fixture-account","auth":auth}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        imported.status(),
        200,
        "seed a live synthetic refresh owner"
    );
    assert!(f.first.launches() > 0);
    let refreshes_before = std::fs::read_to_string(f.first.root.path().join("count")).unwrap();
    let revision_before: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT revision FROM {}.central_accounts WHERE alias='fixture-account'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let response = f.revoke(&session, &token, "target-machine").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.text().await.unwrap().contains("Machine revoked"));
    let duplicate = f.revoke(&session, &token, "target-machine").await;
    assert_eq!(duplicate.status(), 409);
    assert!(duplicate.text().await.unwrap().contains("Already revoked"));
    for _ in 0..2 {
        let api = f
            .http
            .post(format!("{}/v1/devices/revoke", f.first.url))
            .bearer_auth("synthetic-caller-machine-token")
            .header("x-codexctl-device", "caller-machine")
            .json(&json!({"id":"target-machine"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            api.status(),
            204,
            "API is idempotent even after dashboard revocation"
        );
    }
    let rows = f
        .control
        .query(
            &format!(
                "SELECT actor,operation,target,result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (row, result) in rows.iter().zip(["ok", "refused:already_revoked"]) {
        assert_eq!(
            row.get::<_, String>(0),
            "dashboard:dashboard@example.invalid"
        );
        assert_eq!(row.get::<_, String>(1), "machine_revoke");
        assert_eq!(row.get::<_, String>(2), "target-machine");
        assert_eq!(row.get::<_, String>(3), result);
    }
    let page = f
        .http
        .get(format!("{}/accounts/data", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(
        page["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "target-machine" && m["status"] == "revoked")
    );
    assert!(
        !page.to_string().contains(&token),
        "JSON does not carry form credentials"
    );
    let log = std::fs::read_to_string(f.second.root.path().join("dashboard.log")).unwrap();
    assert!(
        !log.contains("dashboard@example.invalid"),
        "application audit logs contain an actor digest; email stays in the database audit only"
    );
    let audits: Vec<_> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["operation"] == "machine_revoke")
        .collect();
    assert_eq!(audits.len(), 2);
    for secret in [
        &token,
        session.split_once('=').unwrap().1,
        "synthetic-target-machine-token",
    ] {
        assert!(!log.contains(secret));
    }
    assert_eq!(
        std::fs::read_to_string(f.first.root.path().join("count")).unwrap(),
        refreshes_before,
        "all revoke paths add zero provider refreshes"
    );
    let revision_after: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT revision FROM {}.central_accounts WHERE alias='fixture-account'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        revision_after, revision_before,
        "revoke never rotates the server account"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_requires_verified_recent_auth_time_even_for_legacy_sessions() {
    let f = BrowserFixture::start().await;
    for auth_time in [None, Some(chrono::Utc::now().timestamp() - 700)] {
        f.identity(auth_time);
        let (session, _) = f.sign_in().await;
        let id = if auth_time.is_none() {
            "unknown-auth-time"
        } else {
            "old-auth-time"
        };
        f.machine(id, id).await;
        let token = f.form_token(&session).await;
        // PR1's legacy fallback could store a recent callback time. It must not prove fresh authentication.
        if auth_time.is_none() {
            f.control
                .batch_execute(&format!(
                    "UPDATE {}.browser_sessions SET signed_in_at=now()",
                    f.schema
                ))
                .await
                .unwrap();
        }
        let response = f.revoke(&session, &token, id).await;
        assert_eq!(
            response.status(),
            303,
            "old or unproven sign-in cannot revoke"
        );
        assert_eq!(
            response.headers()["location"],
            "/accounts/sign-in?fresh=true"
        );
        let revoked: bool = f
            .control
            .query_one(
                &format!(
                    "SELECT revoked FROM {}.central_devices WHERE id=$1",
                    f.schema
                ),
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!revoked);
    }
    let results: Vec<String> = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        results,
        [
            "refused:fresh_sign_in_required",
            "refused:fresh_sign_in_required"
        ]
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_fresh_sign_in_validates_signed_auth_time_with_clock_skew() {
    let f = BrowserFixture::start().await;
    for auth_time in [
        None,
        Some(chrono::Utc::now().timestamp() - 700),
        Some(chrono::Utc::now().timestamp() + 120),
    ] {
        f.identity(auth_time);
        let (binding, path) = f
            .begin_sign_in_route("/accounts/sign-in?fresh=true", true)
            .await;
        let callback = f.callback(&f.second, &binding, &path).await;
        assert_eq!(
            callback.status(),
            401,
            "missing, stale or future auth_time is refused"
        );
        assert!(
            callback
                .headers()
                .get_all("set-cookie")
                .iter()
                .all(|v| !v.to_str().unwrap().starts_with("codexctl-session="))
        );
    }
    let sessions: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.browser_sessions", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(sessions, 0);
    for offset in [-620, -599, 25] {
        f.identity(Some(chrono::Utc::now().timestamp() + offset));
        let (binding, path) = f
            .begin_sign_in_route("/accounts/sign-in?fresh=true", true)
            .await;
        let callback = f.callback(&f.second, &binding, &path).await;
        assert_eq!(
            callback.status(),
            303,
            "valid auth_time within the 30s allowance"
        );
        let session = callback
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
        let id = format!("fresh-{offset}");
        f.machine(&id, &id).await;
        let token = f.form_token(&session).await;
        assert_eq!(f.revoke(&session, &token, &id).await.status(), 200);
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_counts_each_failure_once() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("once-only", "synthetic-once-only-token").await;
    let token = f.form_token(&session).await;
    assert_eq!(f.revoke(&session, &token, "once-only").await.status(), 200);
    assert_eq!(f.revoke(&session, &token, "once-only").await.status(), 409);
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth("synthetic-dashboard-metrics-token-32")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains(
        "codexctl_central_failed_requests_total{reason=\"dashboard_already_revoked\"} 1\n"
    ));
    assert!(
        !metrics.contains("reason=\"http_rejected\""),
        "a recorded refusal is not counted again by HTTP middleware"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_is_user_scoped_and_form_tokens_are_session_bound() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("own-machine", "synthetic-own-machine-token")
        .await;
    let token = f.form_token(&session).await;
    let (other_session, _) = f.sign_in().await;
    assert_eq!(
        f.revoke(&other_session, &token, "own-machine")
            .await
            .status(),
        403
    );
    for origin in [
        None,
        Some("https://hostile.example.invalid"),
        Some("null"),
        Some("http://127.0.0.1:8787/"),
    ] {
        let mut request = f
            .http
            .post(format!("{}/accounts/machines/revoke", f.second.url))
            .header("cookie", &session)
            .form(&[("id", "own-machine"), ("form_token", &token)]);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        assert_eq!(request.send().await.unwrap().status(), 403);
    }
    let count: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "rejected request checks produce no audit row");
    f.control.execute(&format!("INSERT INTO {}.central_users(id,email,enabled) VALUES('foreign-action','foreign-action@example.invalid',true)",f.schema),&[]).await.unwrap();
    for (id, tenant, user) in [
        ("foreign-action-machine", "sawmills", "foreign-action"),
        ("other-tenant-action-machine", "elsewhere", "foreign-action"),
    ] {
        f.control.execute(&format!("INSERT INTO {}.central_devices(id,tenant,user_id,token_hash,revoked) VALUES($1,$2,$3,$1,false)",f.schema),&[&id,&tenant,&user]).await.unwrap();
        assert_eq!(f.revoke(&session, &token, id).await.status(), 404);
        let revoked: bool = f
            .control
            .query_one(
                &format!(
                    "SELECT revoked FROM {}.central_devices WHERE id=$1",
                    f.schema
                ),
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!revoked);
    }
    let rows = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r.get::<_, String>(0) == "refused:device_not_found")
    );
    // Revocation of the session owner immediately removes action authority too.
    f.control
        .batch_execute(&format!(
            "UPDATE {}.central_users SET enabled=false WHERE email='dashboard@example.invalid'",
            f.schema
        ))
        .await
        .unwrap();
    assert_eq!(
        f.revoke(&session, &token, "own-machine").await.status(),
        403
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_storage_failure_is_audited_and_alerted_without_a_refresh() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("faulted-machine", "synthetic-faulted-token")
        .await;
    let token = f.form_token(&session).await;
    f.control.batch_execute(&format!("CREATE FUNCTION {0}.fail_revoke() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic revoke failure'; END $$; CREATE TRIGGER fail_revoke BEFORE UPDATE ON {0}.central_devices FOR EACH ROW EXECUTE FUNCTION {0}.fail_revoke()",f.schema)).await.unwrap();
    let response = f.revoke(&session, &token, "faulted-machine").await;
    assert_eq!(response.status(), 503);
    let row = f
        .control
        .query_one(
            &format!("SELECT result FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "failed:persistence_failed");
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth("synthetic-dashboard-metrics-token-32")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains(
        "codexctl_central_failed_requests_total{reason=\"dashboard_revoke_failed\"} 1\n"
    ));
    assert!(metrics.contains(
        "codexctl_central_last_failure_timestamp_seconds{reason=\"dashboard_revoke_failed\"}"
    ));
    assert!(!metrics.contains("reason=\"http_rejected\""));
    let log = std::fs::read_to_string(f.second.root.path().join("dashboard.log")).unwrap();
    let failure = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["operation"] == "dashboard_failure" && v["stage"] == "revoke_transaction")
        .expect("database failure class is actionable without raw error text");
    assert_eq!(failure["failure_class"], "database");
    assert_eq!(failure["sqlstate"], "P0001");
    assert!(!log.contains("synthetic revoke failure"));
    assert_eq!(f.first.launches() + f.second.launches(), 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_does_not_commit_without_its_audit_row() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("audit-fault-machine", "synthetic-audit-fault-token")
        .await;
    let token = f.form_token(&session).await;
    f.control.batch_execute(&format!("CREATE FUNCTION {0}.fail_dashboard_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic audit failure'; END $$; CREATE TRIGGER fail_dashboard_audit BEFORE INSERT ON {0}.dashboard_audit FOR EACH ROW EXECUTE FUNCTION {0}.fail_dashboard_audit()",f.schema)).await.unwrap();
    assert_eq!(
        f.revoke(&session, &token, "audit-fault-machine")
            .await
            .status(),
        503
    );
    let revoked: bool = f
        .control
        .query_one(
            &format!(
                "SELECT revoked FROM {}.central_devices WHERE id='audit-fault-machine'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        !revoked,
        "a failed audit transaction must roll back the revoke"
    );
    f.control
        .batch_execute(&format!(
            "DROP TRIGGER fail_dashboard_audit ON {}.dashboard_audit",
            f.schema
        ))
        .await
        .unwrap();
    assert_eq!(
        f.revoke(&session, &token, "audit-fault-machine")
            .await
            .status(),
        200
    );
    let rows: i64 = f
        .control
        .query_one(
            &format!("SELECT count(*) FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 1);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_never_audits_unresolved_request_text() {
    let f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("log-safe-machine", "synthetic-log-safe-token")
        .await;
    let token = f.form_token(&session).await;
    let hostile = "sensitive credential injected as a machine id";
    assert_eq!(f.revoke(&session, &token, hostile).await.status(), 404);
    let target: String = f
        .control
        .query_one(
            &format!("SELECT target FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(target.starts_with("unresolved:"));
    let log = std::fs::read_to_string(f.second.root.path().join("dashboard.log")).unwrap();
    assert!(!log.contains(hostile));
    let missing = f
        .http
        .post(format!("{}/accounts/machines/revoke", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&Vec::<(&str, &str)>::new())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 403);
    assert_eq!(missing.headers()["cache-control"], "no-store");
    let oversize = f
        .http
        .post(format!("{}/accounts/machines/revoke", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("id", "x".repeat(5000)), ("form_token", token)])
        .send()
        .await
        .unwrap();
    assert_eq!(oversize.status(), 413);
    assert_eq!(oversize.headers()["cache-control"], "no-store");
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_revoke_keeps_one_audit_when_commit_acknowledgement_is_lost() {
    let mut f = BrowserFixture::start().await;
    f.identity(Some(chrono::Utc::now().timestamp()));
    let database = reqwest::Url::parse(&f.database()).unwrap();
    let proxy_root = tempfile::tempdir().unwrap();
    let mut proxy = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/postgres_commit_proxy.py"))
        .arg(database.host_str().unwrap())
        .arg(database.port_or_known_default().unwrap_or(5432).to_string())
        .arg(proxy_root.path())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut line = String::new();
    timeout(
        Duration::from_secs(5),
        BufReader::new(proxy.stdout.take().unwrap()).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    let mut proxied = database;
    proxied.set_host(Some("127.0.0.1")).unwrap();
    proxied
        .set_port(Some(ready["port"].as_u64().unwrap() as u16))
        .unwrap();
    for pod in [&mut f.first, &mut f.second] {
        pod.stop().await;
        let root = std::mem::replace(&mut pod.root, tempfile::tempdir().unwrap());
        *pod = Pod::spawn(
            proxied.as_str(),
            &f._root.path().join("key"),
            root,
            "postgres",
        )
        .await;
    }
    let (session, _) = f.sign_in().await;
    f.machine(
        "ambiguous-commit-machine",
        "synthetic-ambiguous-commit-token",
    )
    .await;
    let token = f.form_token(&session).await;
    store::atomic_write(&proxy_root.path().join("lose-commit-ack"), b"enabled").unwrap();
    let response = f.revoke(&session, &token, "ambiguous-commit-machine").await;
    assert_eq!(
        response.status(),
        503,
        "the failed acknowledgement cannot be reported as success"
    );
    assert!(
        proxy_root.path().join("ack-dropped").exists(),
        "the fault must happen after the actual COMMIT"
    );
    let revoked: bool = f
        .control
        .query_one(
            &format!(
                "SELECT revoked FROM {}.central_devices WHERE id='ambiguous-commit-machine'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        revoked,
        "PostgreSQL committed the revoke despite its lost response"
    );
    let rows = f
        .control
        .query(
            &format!("SELECT result FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one HTTP attempt has one durable audit even after an ambiguous COMMIT"
    );
    assert_eq!(
        rows[0].get::<_, String>(0),
        "ok",
        "the committed result is preserved"
    );
    f.finish().await;
    proxy.kill().await.unwrap();
    proxy.wait().await.unwrap();
}

#[tokio::test]
async fn postgres_dashboard_revoke_accepts_canonical_origin() {
    let mut f = BrowserFixture::start().await;
    let database = f.database();
    let key = f._root.path().join("key");
    for pod in [&mut f.first, &mut f.second] {
        pod.stop().await;
        let root = std::mem::replace(&mut pod.root, tempfile::tempdir().unwrap());
        store::atomic_write(&root.path().join("public-url"), b"http://127.0.0.1:80").unwrap();
        *pod = Pod::spawn(&database, &key, root, "postgres").await;
    }
    f.identity(Some(chrono::Utc::now().timestamp()));
    let (session, _) = f.sign_in().await;
    f.machine("canonical-origin-machine", "synthetic-origin-machine-token")
        .await;
    let token = f.form_token(&session).await;
    for origin in ["null", "http://elsewhere.invalid", "http://127.0.0.1:81"] {
        let response = f
            .http
            .post(format!("{}/accounts/machines/revoke", f.second.url))
            .header("cookie", &session)
            .header("origin", origin)
            .form(&[("id", "canonical-origin-machine"), ("form_token", &token)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    let response = f
        .http
        .post(format!("{}/accounts/machines/revoke", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1")
        .form(&[("id", "canonical-origin-machine"), ("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_render_failure_is_uncacheable_html() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("render-machine", "synthetic-render-machine-token")
        .await;
    f.form_token(&session).await;
    std::fs::remove_file(f._root.path().join("key")).unwrap();
    for route in ["/accounts", "/accounts/data"] {
        let response = f
            .http
            .get(format!("{}{route}", f.first.url))
            .header("cookie", &session)
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(response.text().await.unwrap().contains("data-load-error"));
    }
    f.finish().await;
}
