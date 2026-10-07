#![cfg(all(unix, feature = "central-real-db-tests"))]

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use codexctl::{central, store};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

struct Pod {
    child: Child,
    root: tempfile::TempDir,
    url: String,
}

impl Pod {
    async fn start(database: &str, key: &Path) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        central::managed::setup(&state, &root.path().join("key")).unwrap();
        Self::spawn(database, key, root, "postgres").await
    }

    async fn spawn(database: &str, key: &Path, root: tempfile::TempDir, mode: &str) -> Self {
        let state = root.path().join("state");
        store::atomic_write(&root.path().join("mode"), b"startup").unwrap();
        for name in ["count", "launch-count"] {
            store::atomic_write(&root.path().join(name), b"0").unwrap();
        }
        let mut child = command(database, &state, key, "serve")
            .env("CODEXCTL_CENTRAL_STORE", mode)
            .args([
                "--listen",
                "127.0.0.1:0",
                "--public-url",
                "http://127.0.0.1:8787",
                "--codex-bin",
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env("CENTRAL_TEST_MODE_FILE", root.path().join("mode"))
            .env("CENTRAL_TEST_REFRESH_COUNTER", root.path().join("count"))
            .env(
                "CENTRAL_TEST_LAUNCH_COUNTER",
                root.path().join("launch-count"),
            )
            .env("CENTRAL_TEST_EXIT_FILE", root.path().join("exited"))
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut line = String::new();
        timeout(
            Duration::from_secs(20),
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
        )
        .await
        .expect("server startup deadline")
        .unwrap();
        let ready: Value = serde_json::from_str(&line).expect("server must start");
        Self {
            child,
            root,
            url: format!("http://{}", ready["listening"].as_str().unwrap()),
        }
    }

    fn launches(&self) -> u32 {
        std::fs::read_to_string(self.root.path().join("launch-count"))
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn stop(&mut self) {
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        timeout(Duration::from_secs(10), self.child.wait())
            .await
            .unwrap()
            .unwrap();
    }
}

fn command(database: &str, state: &Path, key: &Path, operation: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl-central"));
    command
        .arg(operation)
        .arg("--state")
        .arg(state)
        .arg("--key-file")
        .arg(key)
        .env("DATABASE_URL", database)
        .env("CODEXCTL_CENTRAL_STORE", "postgres")
        .env("CODEXCTL_CENTRAL_DB_TLS", "0")
        .env_remove("CODEXCTL_CENTRAL_DUAL_WRITE")
        .env("CODEXCTL_CENTRAL_BACKGROUND_RECOVERY", "0")
        // Synthetic credentials must never reach an external usage service.
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("https_proxy", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    command
}

async fn request(http: &reqwest::Client, pod: &Pod, token: &str) -> reqwest::Response {
    http.post(format!("{}/v1/token", pod.url))
        .bearer_auth(token)
        .json(&json!({"alias":"seat","billing":true}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn three_postgres_servers_start_without_refresh_children_and_launch_only_under_lease() {
    let Ok(database) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("CI").ok().as_deref(),
            Some("true"),
            "DATABASE_URL required in CI"
        );
        eprintln!("skipping real PostgreSQL startup test: DATABASE_URL unset");
        return;
    };
    let (control, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(async move { connection.await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let schema = format!(
        "startup_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    control
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let mut database = reqwest::Url::parse(&database).unwrap();
    database
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));

    let state = root.path().join("state");
    let key = root.path().join("key");
    central::managed::setup(&state, &key).unwrap();
    store::atomic_write(
        &state.join("users.json"),
        &serde_json::to_vec(&json!([
            {"id":"test","email":"test@example.invalid","enabled":true}
        ]))
        .unwrap(),
    )
    .unwrap();
    let token_file = root.path().join("machine-token");
    central::register(&state, "test-machine", "sawmills", "test", &token_file).unwrap();
    let token = std::fs::read_to_string(&token_file).unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(75))
        .build()
        .unwrap();
    let mut seed = Pod::spawn(database.as_str(), &key, root, "file").await;
    store::atomic_write(&seed.root.path().join("mode"), b"").unwrap();
    for (alias, account) in [("seat", "synthetic-seat"), ("other", "other-seat")] {
        let claims = json!({"sub":account,"iat":2000000000_u64,"exp":4102444800_u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}});
        let auth = json!({"tokens":{
            "access_token":format!("header.{}.", URL_SAFE_NO_PAD.encode(claims.to_string())),
            "refresh_token":"synthetic-refresh","account_id":account}});
        let response = http
            .post(format!("{}/v1/accounts", seed.url))
            .bearer_auth(&token)
            .json(&json!({"alias":alias,"auth":auth}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "file-mode seed import");
    }
    let seed_token: Value = request(&http, &seed, &token).await.json().await.unwrap();
    assert_eq!(generation(&seed_token), 1);
    seed.stop().await;
    for operation in ["migrate", "backfill"] {
        let output = command(database.as_str(), &state, &key, operation)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let (first, second, third) = tokio::join!(
        Pod::start(database.as_str(), &key),
        Pod::start(database.as_str(), &key),
        Pod::start(database.as_str(), &key)
    );
    let mut pods = [first, second, third];
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(75))
        .build()
        .unwrap();
    for pod in &pods {
        assert_eq!(
            http.get(format!("{}/ready", pod.url))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        pods.each_ref().map(Pod::launches),
        [0, 0, 0],
        "startup must not create refresh children on any replica"
    );
    // Login records are replica-local, so shared mode refuses new-account login.
    let add = http
        .post(format!("{}/v1/accounts/login/start", pods[0].url))
        .bearer_auth(&token)
        .json(&json!({"alias":"new-account","id":"a".repeat(64)}))
        .send()
        .await
        .unwrap();
    assert_eq!(add.status(), 503);
    assert_eq!(
        add.json::<Value>().await.unwrap()["error"],
        "account_login_unavailable"
    );

    let token = std::fs::read_to_string(token_file).unwrap();
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup-hold").unwrap();
    let first_request = request(&http, &pods[0], &token);
    let contenders = async {
        timeout(Duration::from_secs(5), async {
            while !pods[0].root.path().join("initialize-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leased initialize did not start");
        let same_account: Vec<_> = (0..2)
            .map(|_| {
                let waiting_http = http.clone();
                let waiting_url = pods[0].url.clone();
                let waiting_token = token.clone();
                tokio::spawn(async move {
                    waiting_http
                        .post(format!("{waiting_url}/v1/token"))
                        .bearer_auth(waiting_token)
                        .json(&json!({"alias":"seat","billing":true}))
                        .send()
                        .await
                        .unwrap()
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            same_account.iter().all(|task| !task.is_finished()),
            "same-account contention should queue"
        );
        let other = timeout(
            Duration::from_secs(2),
            http.post(format!("{}/v1/token", pods[0].url))
                .bearer_auth(&token)
                .json(&json!({"alias":"other","billing":true}))
                .send(),
        )
        .await
        .expect("held initialize blocked another account on the same replica")
        .unwrap();
        assert_eq!(other.status(), 200);
        std::fs::remove_file(pods[0].root.path().join("exited")).unwrap();
        let leases: i64 = control
            .query_one(
                &format!(
                    "SELECT count(*) FROM {schema}.account_refresh_leases WHERE expires_at > now()"
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            leases, 1,
            "a live database lease must cover child initialization"
        );
        for pod in &pods[1..] {
            let response = request(&http, pod, &token).await;
            assert_eq!(response.status(), 503);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "refresh_in_progress"
            );
        }
        assert_eq!(pods.each_ref().map(Pod::launches), [2, 0, 0]);
        // Fail one scheduled renewal while initialize is still unfinished.
        // The renewer must survive the query error and retry on its next tick.
        control
            .batch_execute(&format!(
                "CREATE SEQUENCE {schema}.live_renew_attempts;
             CREATE FUNCTION {schema}.fail_live_renewal() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF nextval('{schema}.live_renew_attempts') = 1 THEN
                 RAISE EXCEPTION 'synthetic live renewal failure';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER fail_live_renewal BEFORE UPDATE ON {schema}.account_refresh_leases
             FOR EACH ROW WHEN (NEW.expires_at > OLD.expires_at)
             EXECUTE FUNCTION {schema}.fail_live_renewal();"
            ))
            .await
            .unwrap();
        timeout(Duration::from_secs(65), async {
            loop {
                let retried: bool = control
                    .query_one(
                        &format!(
                            "SELECT is_called AND last_value >= 2 FROM {schema}.live_renew_attempts"
                        ),
                        &[],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if retried {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("transient renewal failure stopped the live child lease renewer");
        control
            .batch_execute(&format!(
                "DROP TRIGGER fail_live_renewal ON {schema}.account_refresh_leases"
            ))
            .await
            .unwrap();
        store::atomic_write(&pods[0].root.path().join("release-initialize"), b"go").unwrap();
        let mut generations = Vec::new();
        for task in same_account {
            let queued = task.await.unwrap();
            assert_eq!(
                queued.status(),
                200,
                "queued same-account request must succeed"
            );
            generations.push(generation(&queued.json::<Value>().await.unwrap()));
        }
        generations.sort_unstable();
        assert_eq!(generations, [3, 4]);
    };
    let (response, ()) = tokio::join!(first_request, contenders);
    assert_eq!(response.status(), 200);
    let first: Value = response.json().await.unwrap();
    assert_eq!(generation(&first), 2);
    assert!(
        pods[0].root.path().join("exited").exists(),
        "lease must not be released with a refresh child alive"
    );
    for (index, pod) in pods[1..].iter().enumerate() {
        let response = request(&http, pod, &token).await;
        assert_eq!(response.status(), 200);
        let next: Value = response.json().await.unwrap();
        assert_ne!(
            next["revision"], first["revision"],
            "initialize rotation must be committed for the next replica"
        );
        assert_eq!(generation(&next), index + 5);
        assert!(pod.root.path().join("exited").exists());
    }
    assert_eq!(pods.each_ref().map(Pod::launches), [4, 1, 1]);
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup-error").unwrap();
    std::fs::remove_file(pods[0].root.path().join("exited")).unwrap();
    let failed = request(&http, &pods[0], &token).await;
    assert_eq!(failed.status(), 503);
    assert_eq!(
        failed.json::<Value>().await.unwrap()["error"],
        "owner_unavailable"
    );
    timeout(Duration::from_secs(5), async {
        while !pods[0].root.path().join("exited").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed initialize child did not exit");
    let recovered = timeout(Duration::from_secs(5), async {
        loop {
            let response = request(&http, &pods[1], &token).await;
            if response.status() == 200 {
                break response.json::<Value>().await.unwrap();
            }
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "refresh_in_progress"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed initialize retained its lease");
    assert_eq!(
        generation(&recovered),
        8,
        "failed initialize must publish its rotation before the next replica launches"
    );
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup").unwrap();
    let retry = timeout(Duration::from_secs(5), async {
        loop {
            let response = request(&http, &pods[0], &token).await;
            if response.status() == 200 {
                break response.json::<Value>().await.unwrap();
            }
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "owner_unavailable"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("replica stayed fenced after initialize failure settled");
    assert_eq!(generation(&retry), 9);
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup-exit").unwrap();
    let failed = request(&http, &pods[0], &token).await;
    assert_eq!(failed.status(), 503);
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup").unwrap();
    let retry = timeout(Duration::from_secs(5), async {
        loop {
            let response = request(&http, &pods[0], &token).await;
            if response.status() == 200 {
                break response.json::<Value>().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dead initialize child retained its lease or replica fence");
    assert_eq!(generation(&retry), 11);
    store::atomic_write(&pods[0].root.path().join("mode"), b"startup-hold-error").unwrap();
    for name in ["initialize-started", "release-initialize"] {
        std::fs::remove_file(pods[0].root.path().join(name)).unwrap();
    }
    let stopped_request = request(&http, &pods[0], &token);
    let shutdown = async {
        timeout(Duration::from_secs(5), async {
            while !pods[0].root.path().join("initialize-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        unsafe {
            libc::kill(pods[0].child.id().unwrap() as i32, libc::SIGTERM);
        }
        timeout(Duration::from_secs(5), async {
            while http
                .get(format!("{}/ready", pods[0].url))
                .send()
                .await
                .is_ok_and(|r| r.status() == 200)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server did not start shutdown");
        // A sequence survives the failed transaction, so only the first
        // settlement renewal fails. Later renewals can persist the rotation.
        control
            .batch_execute(&format!(
                "CREATE SEQUENCE {schema}.renew_attempts;
             CREATE FUNCTION {schema}.fail_first_renewal() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF nextval('{schema}.renew_attempts') = 1 THEN
                 RAISE EXCEPTION 'synthetic transient renewal failure';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER fail_first_renewal BEFORE UPDATE ON {schema}.account_refresh_leases
             FOR EACH ROW WHEN (NEW.expires_at > OLD.expires_at)
             EXECUTE FUNCTION {schema}.fail_first_renewal();"
            ))
            .await
            .unwrap();
        store::atomic_write(&pods[0].root.path().join("release-initialize"), b"go").unwrap();
    };
    let (response, ()) = tokio::join!(stopped_request, shutdown);
    assert_eq!(response.status(), 503);
    timeout(Duration::from_secs(10), pods[0].child.wait())
        .await
        .unwrap()
        .unwrap();
    let renew_attempts: i64 = control
        .query_one(
            &format!("SELECT last_value FROM {schema}.renew_attempts"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        renew_attempts >= 2,
        "shutdown abandoned a transient renewal error"
    );
    let successor = request(&http, &pods[1], &token).await;
    assert_eq!(
        successor.status(),
        200,
        "graceful shutdown must release its settled lease"
    );
    assert_eq!(
        generation(&successor.json::<Value>().await.unwrap()),
        13,
        "graceful shutdown must publish initialization rotation for its successor"
    );
    for pod in &mut pods {
        pod.stop().await;
    }
    // A migrated, verified vault can retain a promoted relogin journal. A
    // shared store cannot finish that replica-local journal, so startup must
    // refuse before it launches or serves anything for that account.
    let [first, _, _] = pods;
    let account = std::fs::read_dir(first.root.path().join("state/accounts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            // The stable account key is a hash of the user and alias.
            path.file_name().unwrap().to_str().unwrap() == account_key("test", "seat")
        })
        .unwrap();
    let operation = "f".repeat(64);
    let journal = account.join("relogin").join(&operation);
    store::ensure_private_dir(&journal.join("home")).unwrap();
    store::atomic_write(&journal.join("home/spawn-failed"), b"not-started").unwrap();
    store::atomic_write(
        &journal.join("record.json"),
        &serde_json::to_vec(&json!({
            "sequence":1,"id":operation,"user":"test","device":"test-machine",
            "alias":"seat","broker":{"pid":1,"incarnation":"synthetic-dead"},
            "child":{"status":"not_started"},"original_revision":"synthetic",
            "candidate_revision":null,"candidate":null,"phase":"promoted",
            "code":null,"error":null,"retired":false
        }))
        .unwrap(),
    )
    .unwrap();
    let state = first.root.path().join("state");
    let refused = command(database.as_str(), &state, &key, "serve")
        .env("CODEXCTL_CENTRAL_STORE", "postgres")
        .args([
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8787",
        ])
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        !refused.status.success(),
        "unfinished shared relogin must refuse startup"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("finish pending logins in file mode before switching storage"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(control);
    connection.await.unwrap();
}

fn generation(response: &Value) -> usize {
    let payload = response["accessToken"]
        .as_str()
        .unwrap()
        .split('.')
        .nth(1)
        .unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    claims["generation"].as_u64().unwrap() as usize
}

fn account_key(user: &str, alias: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(format!("{user}\0{alias}").as_bytes())
    )
}

#[tokio::test]
async fn postgres_import_settles_refresh_children_before_releasing_its_lease() {
    let Ok(database) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("CI").ok().as_deref(),
            Some("true"),
            "DATABASE_URL required in CI"
        );
        eprintln!("skipping real PostgreSQL startup test: DATABASE_URL unset");
        return;
    };
    let (control, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(async move { connection.await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let schema = format!(
        "startup_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    control
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let mut database = reqwest::Url::parse(&database).unwrap();
    database
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));

    let state = root.path().join("state");
    let key = root.path().join("key");
    central::managed::setup(&state, &key).unwrap();
    store::atomic_write(
        &state.join("users.json"),
        &serde_json::to_vec(&json!([
            {"id":"test","email":"test@example.invalid","enabled":true}
        ]))
        .unwrap(),
    )
    .unwrap();
    let token_file = root.path().join("machine-token");
    central::register(&state, "test-machine", "sawmills", "test", &token_file).unwrap();
    let token = std::fs::read_to_string(&token_file).unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();

    for operation in ["migrate", "backfill"] {
        let output = command(database.as_str(), &state, &key, operation)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut importer = Pod::start(database.as_str(), &key).await;
    let mut peer = Pod::start(database.as_str(), &key).await;
    let auth = |account: &str| {
        let claims = json!({"sub":account,"iat":2000000000_u64,"exp":4102444800_u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}});
        json!({"tokens":{"access_token":format!("header.{}.", URL_SAFE_NO_PAD.encode(claims.to_string())),
            "refresh_token":"synthetic-refresh","account_id":account}})
    };
    store::atomic_write(&importer.root.path().join("mode"), b"startup-hold").unwrap();
    let importing = http
        .post(format!("{}/v1/accounts", importer.url))
        .bearer_auth(&token)
        .json(&json!({"alias":"seat","auth":auth("synthetic-seat")}))
        .send();
    let during_import = async {
        timeout(Duration::from_secs(5), async {
            while !importer.root.path().join("initialize-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let leases: i64 = control
            .query_one(
                &format!(
                    "SELECT count(*) FROM {schema}.account_refresh_leases WHERE expires_at > now()"
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(leases, 1, "import initialization must hold a lease");
        assert_eq!(peer.launches(), 0);
        store::atomic_write(&importer.root.path().join("mode"), b"exit-rotation").unwrap();
        store::atomic_write(&importer.root.path().join("release-initialize"), b"go").unwrap();
    };
    let (imported, ()) = tokio::join!(importing, during_import);
    assert_eq!(imported.unwrap().status(), 200);
    assert!(
        importer.root.path().join("exited").exists(),
        "import released its lease with a live refresh child"
    );
    let successor = request(&http, &peer, &token).await;
    assert_eq!(successor.status(), 200);
    assert_eq!(
        generation(&successor.json::<Value>().await.unwrap()),
        4,
        "import must publish the child's exit-time rotation"
    );
    store::atomic_write(&importer.root.path().join("mode"), b"startup").unwrap();
    let repeated = http
        .post(format!("{}/v1/accounts", importer.url))
        .bearer_auth(&token)
        .json(&json!({"alias":"seat","auth":auth("synthetic-seat")}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        repeated.status(),
        200,
        "verified import retry must take a fresh lease"
    );
    let successor = request(&http, &peer, &token).await;
    assert_eq!(successor.status(), 200);
    assert_eq!(
        generation(&successor.json::<Value>().await.unwrap()),
        7,
        "import retry must start from the latest shared credential"
    );

    store::atomic_write(&importer.root.path().join("mode"), b"startup-error").unwrap();
    let failed_retry = http
        .post(format!("{}/v1/accounts", importer.url))
        .bearer_auth(&token)
        .json(&json!({"alias":"seat","auth":auth("synthetic-seat")}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed_retry.status(), 503);
    store::atomic_write(&importer.root.path().join("mode"), b"startup").unwrap();
    let restored = timeout(Duration::from_secs(5), async {
        loop {
            let response = request(&http, &importer, &token).await;
            if response.status() == 200 {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed verified import must not permanently fence the account");
    assert_eq!(generation(&restored.json::<Value>().await.unwrap()), 9);

    control
        .batch_execute(&format!(
            "CREATE FUNCTION {schema}.fail_import_write() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic import write failure'; END $$;
         CREATE TRIGGER fail_import_write BEFORE UPDATE ON {schema}.central_accounts
         FOR EACH ROW WHEN (NEW.alias = 'failed') EXECUTE FUNCTION {schema}.fail_import_write();"
        ))
        .await
        .unwrap();
    std::fs::remove_file(importer.root.path().join("exited")).unwrap();
    store::atomic_write(&importer.root.path().join("mode"), b"startup-error").unwrap();
    let failed = http
        .post(format!("{}/v1/accounts", importer.url))
        .bearer_auth(&token)
        .json(&json!({"alias":"failed","auth":auth("failed-seat")}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 503);
    timeout(Duration::from_secs(5), async {
        while !importer.root.path().join("exited").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed import must stop its refresh child");
    store::atomic_write(&importer.root.path().join("mode"), b"startup").unwrap();
    let other = timeout(Duration::from_secs(2), request(&http, &importer, &token))
        .await
        .expect("pending import settlement blocked another account");
    assert_eq!(other.status(), 200);
    let busy = timeout(
        Duration::from_secs(2),
        http.post(format!("{}/v1/accounts", importer.url))
            .bearer_auth(&token)
            .json(&json!({"alias":"failed","auth":auth("failed-seat")}))
            .send(),
    )
    .await
    .expect("pending import retry blocked instead of refusing")
    .unwrap();
    assert_eq!(busy.status(), 503);
    control
        .batch_execute(&format!(
            "DROP TRIGGER fail_import_write ON {schema}.central_accounts"
        ))
        .await
        .unwrap();
    let retried = timeout(Duration::from_secs(5), async {
        loop {
            let response = http
                .post(format!("{}/v1/accounts", peer.url))
                .bearer_auth(&token)
                .json(&json!({"alias":"failed","auth":auth("failed-seat")}))
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                break response;
            }
            assert_eq!(response.status(), 503);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed import kept its lease after settlement");
    assert_eq!(retried.status(), 200);
    let recovered = http
        .post(format!("{}/v1/token", peer.url))
        .bearer_auth(&token)
        .json(&json!({"alias":"failed","billing":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), 200);
    assert_eq!(
        generation(&recovered.json::<Value>().await.unwrap()),
        4,
        "failed import must persist initialization rotation before retry"
    );
    store::atomic_write(&importer.root.path().join("mode"), b"cached-rejection").unwrap();
    let mut rejected_auth = auth("rejected-seat");
    rejected_auth["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    let rejected = timeout(
        Duration::from_secs(5),
        http.post(format!("{}/v1/accounts", importer.url))
            .bearer_auth(&token)
            .json(&json!({"alias":"rejected","auth":rejected_auth}))
            .send(),
    )
    .await
    .expect("unchanged rejected grant must not hang settlement")
    .unwrap();
    assert_eq!(rejected.status(), 503);
    store::atomic_write(&importer.root.path().join("mode"), b"startup").unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let response = http
                .post(format!("{}/v1/accounts", importer.url))
                .bearer_auth(&token)
                .json(&json!({"alias":"rejected","auth":auth("rejected-seat")}))
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                break;
            }
            assert_eq!(response.status(), 503);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("rejected grant must settle so a replacement can be verified");
    importer.stop().await;
    peer.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(control);
    connection.await.unwrap();
}

#[tokio::test]
async fn postgres_renewal_shares_pending_status_and_cancel_without_stopping_token_service() {
    let LoginFixture {
        mut first,
        mut second,
        http,
        token,
        control,
        connection,
        schema,
        _seed,
        ..
    } = login_fixture().await;
    let operation = json!({"alias":"seat","id":"b".repeat(64)});
    let response = http
        .post(format!("{}/v1/relogin/start", first.url))
        .bearer_auth(&token)
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "PostgreSQL must support renewal");
    let started: Value = response.json().await.unwrap();
    assert_eq!(started["status"], "pending");
    let pending: Value = http
        .post(format!("{}/v1/relogin/status", second.url))
        .bearer_auth(&token)
        .json(&operation)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["userCode"], "TEST-LOGIN");
    assert_eq!(
        request(&http, &second, &token).await.status(),
        200,
        "browser approval must not stop the existing account"
    );
    let canceled = http
        .post(format!("{}/v1/relogin/cancel", second.url))
        .bearer_auth(&token)
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(canceled.status(), 200);
    timeout(Duration::from_secs(8), async {
        loop {
            let record: Value = http
                .post(format!("{}/v1/relogin/status", second.url))
                .bearer_auth(&token)
                .json(&operation)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if record["status"] == "canceled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cross-replica cancellation deadline");
    let renewed = json!({"alias":"seat","id":"c".repeat(64)});
    let response = http
        .post(format!("{}/v1/relogin/start", first.url))
        .bearer_auth(&token)
        .json(&renewed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let fresh = renewal_grant();
    store::atomic_write(
        &first.root.path().join("login-release"),
        &serde_json::to_vec(&fresh).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            let result: Value = http
                .post(format!("{}/v1/relogin/status", second.url))
                .bearer_auth(&token)
                .json(&renewed)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_ne!(result["status"], "failed", "{result}");
            if result["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("renewal must verify and complete");
    let result = request(&http, &second, &token).await;
    assert_eq!(result.status(), 200);
    assert_eq!(
        generation(&result.json::<Value>().await.unwrap()),
        13,
        "replica B must use the renewed grant"
    );
    assert!(!second.root.path().join("login-pid").exists());
    // The client can lose completion and retry through another replica.
    let receipt: Value = http
        .post(format!("{}/v1/relogin/start", second.url))
        .bearer_auth(&token)
        .json(&renewed)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt["status"], "completed");
    first.stop().await;
    second.stop().await;
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    connection.abort();
}

#[tokio::test]
async fn postgres_backfill_refuses_legacy_device_login_journals() {
    let database = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let key = root.path().join("key");
    central::managed::setup(&state, &key).unwrap();
    let operation = "a".repeat(64);
    let journal = state
        .join("account-logins")
        .join(account_key("test", "new"))
        .join("relogin")
        .join(&operation);
    store::ensure_private_dir(&journal).unwrap();
    store::atomic_write(
        &journal.join("record.json"),
        &serde_json::to_vec(&json!({
            "sequence":1,"id":operation,"user":"test","device":"test-machine","alias":"new",
            "broker":{"pid":1,"incarnation":"synthetic"},"child":{"status":"not_started"},
            "original_revision":"","candidate_revision":null,"candidate":null,"phase":"starting",
            "code":null,"error":null,"retired":false
        }))
        .unwrap(),
    )
    .unwrap();
    let result = command(&database, &state, &key, "backfill")
        .output()
        .await
        .unwrap();
    assert!(
        !result.status.success(),
        "backfill must refuse pending legacy logins"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("finish pending logins in file mode"));
}

struct LoginFixture {
    first: Pod,
    second: Pod,
    http: reqwest::Client,
    token: String,
    control: tokio_postgres::Client,
    connection: tokio::task::JoinHandle<()>,
    schema: String,
    database: String,
    key: std::path::PathBuf,
    _seed: Pod,
}
async fn login_fixture() -> LoginFixture {
    let Ok(database) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("CI").ok().as_deref(),
            Some("true"),
            "DATABASE_URL required in CI"
        );
        eprintln!("skipping real PostgreSQL startup test: DATABASE_URL unset");
        panic!("DATABASE_URL required for HA renewal test");
    };
    let (control, connection) = tokio_postgres::connect(&database, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(async move { connection.await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let schema = format!(
        "startup_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    control
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let mut database = reqwest::Url::parse(&database).unwrap();
    database
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));

    let state = root.path().join("state");
    let key = root.path().join("key");
    central::managed::setup(&state, &key).unwrap();
    store::atomic_write(
        &state.join("users.json"),
        &serde_json::to_vec(&json!([
            {"id":"test","email":"test@example.invalid","enabled":true}
        ]))
        .unwrap(),
    )
    .unwrap();
    let token_file = root.path().join("machine-token");
    central::register(&state, "test-machine", "sawmills", "test", &token_file).unwrap();
    let token = std::fs::read_to_string(&token_file).unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(75))
        .build()
        .unwrap();
    let mut seed = Pod::spawn(database.as_str(), &key, root, "file").await;
    store::atomic_write(&seed.root.path().join("mode"), b"").unwrap();
    for (alias, account) in [("seat", "synthetic-seat"), ("other", "other-seat")] {
        let claims = json!({"sub":account,"iat":2000000000_u64,"exp":4102444800_u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}});
        let auth = json!({"tokens":{
            "access_token":format!("header.{}.", URL_SAFE_NO_PAD.encode(claims.to_string())),
            "refresh_token":"synthetic-refresh","account_id":account}});
        let response = http
            .post(format!("{}/v1/accounts", seed.url))
            .bearer_auth(&token)
            .json(&json!({"alias":alias,"auth":auth}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "file-mode seed import");
    }
    let seed_token: Value = request(&http, &seed, &token).await.json().await.unwrap();
    assert_eq!(generation(&seed_token), 1);
    seed.stop().await;
    for operation in ["migrate", "backfill"] {
        let output = command(database.as_str(), &state, &key, operation)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first = Pod::start(database.as_str(), &key).await;
    let second = Pod::start(database.as_str(), &key).await;
    LoginFixture {
        first,
        second,
        http,
        token,
        control,
        connection,
        schema,
        database: database.to_string(),
        key,
        _seed: seed,
    }
}

async fn login_request(f: &LoginFixture, pod: &Pod, action: &str, id: &str) -> Value {
    login_alias_request(f, pod, "seat", action, id).await
}
async fn login_alias_request(
    f: &LoginFixture,
    pod: &Pod,
    alias: &str,
    action: &str,
    id: &str,
) -> Value {
    let response = f
        .http
        .post(format!("{}/v1/relogin/{action}", pod.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":alias,"id":id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{action}");
    response.json().await.unwrap()
}
async fn stop_fixture(mut f: LoginFixture) {
    f.first.stop().await;
    f.second.stop().await;
    f.control
        .batch_execute(&format!("DROP SCHEMA {} CASCADE", f.schema))
        .await
        .unwrap();
    f.connection.abort();
}
async fn wait_dead(pid: u32) {
    timeout(Duration::from_secs(8), async {
        loop {
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("login child must stop after lease loss");
}
#[tokio::test]
async fn postgres_renewal_stops_polling_when_its_operation_epoch_is_replaced() {
    let f = login_fixture().await;
    let id = "d".repeat(64);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET epoch=epoch+1,holder_id='replacement' WHERE id=$1",f.schema),&[&id]).await.unwrap();
    wait_dead(pid).await;
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        200,
        "polling lease loss leaves existing account usable"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_cancel_stops_a_login_holder_cut_off_from_the_database() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut database = reqwest::Url::parse(&f.database).unwrap();
    let upstream = format!(
        "{}:{}",
        database.host_str().unwrap(),
        database.port().unwrap_or(5432)
    );
    let proxy = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut incoming, _) = listener.accept().await.unwrap();
            let upstream = upstream.clone();
            connections.spawn(async move {
                let mut outgoing = tokio::net::TcpStream::connect(upstream).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
            });
        }
    });
    database.set_host(Some("127.0.0.1")).unwrap();
    database.set_port(Some(address.port())).unwrap();
    f.first = Pod::start(database.as_str(), &f.key).await;
    let id = "e".repeat(64);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    let pid = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    proxy.abort();
    let _ = proxy.await;
    login_request(&f, &f.second, "cancel", &id).await;
    wait_dead(pid).await;
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

fn renewal_grant() -> Value {
    let claims = json!({"sub":"synthetic-seat","iat":2000001000_u64,"exp":4102444800_u64,"generation":10,
        "https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat","chatgpt_plan_type":"pro"}});
    json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),"refresh_token":"synthetic-renewal","account_id":"synthetic-seat"}})
}
#[tokio::test]
async fn postgres_verification_loss_fences_the_candidate_across_replica_restart() {
    let mut f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    let id = "f".repeat(64);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let pid: Value = serde_json::from_slice(
        &std::fs::read(
            f.first
                .root
                .path()
                .join("state/accounts")
                .join(account_key("test", "seat"))
                .join("runtime/pid"),
        )
        .unwrap(),
    )
    .unwrap();
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET epoch=epoch+1,holder_id='replacement',expires_at=clock_timestamp() WHERE account_id=$1",f.schema),&[&account_key("test","seat")]).await.unwrap();
    wait_dead(pid["pid"].as_u64().unwrap() as u32).await;
    assert_eq!(
        login_request(&f, &f.second, "status", &id).await["status"],
        "failed"
    );
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "unresolved verification must block normal refresh"
    );
    f.first.stop().await;
    f.first = Pod::start(&f.database, &f.key).await;
    assert_eq!(
        f.first.launches(),
        0,
        "startup must not verify an in-flight candidate again"
    );
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 503);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_wrong_account_candidate_keeps_both_accounts_reserved() {
    wrong_candidate_reservation(false, false).await;
}
#[tokio::test]
async fn postgres_candidate_publication_failure_keeps_its_identity_reserved() {
    wrong_candidate_reservation(true, false).await;
}
#[tokio::test]
async fn postgres_failed_native_login_preserves_a_saved_grant() {
    wrong_candidate_reservation(false, true).await;
}
async fn wrong_candidate_reservation(fail_publication: bool, fail_child: bool) {
    let f = login_fixture().await;
    if fail_child {
        store::atomic_write(&f.first.root.path().join("mode"), b"login-error-after-save").unwrap();
    }
    if fail_publication {
        f.control.batch_execute(&format!("CREATE SEQUENCE {0}.capture_attempt; CREATE FUNCTION {0}.reject_first_capture() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='candidate' AND nextval('{0}.capture_attempt')=1 THEN RAISE EXCEPTION 'synthetic candidate publication failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_capture BEFORE UPDATE ON {0}.central_login_operations FOR EACH ROW EXECUTE FUNCTION {0}.reject_first_capture()",f.schema)).await.unwrap();
    }
    let id = "1".repeat(64);
    login_request(&f, &f.first, "start", &id).await;
    let claims = json!({"sub":"other-seat","iat":2000001000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":"other-seat","chatgpt_plan_type":"pro"}});
    let wrong = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),"refresh_token":"synthetic-wrong","account_id":"other-seat"}});
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let other = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"other","billing":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status(),
        503,
        "wrong-account grant must reserve the affected identity across replicas"
    );
    if !fail_publication && !fail_child {
        // Only an explicit verified renewal may repair a stopped rejection.
        for (alias, grant, request_id) in [
            ("other", wrong, "3".repeat(64)),
            ("seat", renewal_grant(), "4".repeat(64)),
        ] {
            let release = f.second.root.path().join("login-release");
            if release.exists() {
                std::fs::remove_file(&release).unwrap();
            }
            let started = login_alias_request(&f, &f.second, alias, "start", &request_id).await;
            assert_eq!(
                started["status"], "pending",
                "explicit repair must get a new operation"
            );
            store::atomic_write(&release, &serde_json::to_vec(&grant).unwrap()).unwrap();
            timeout(Duration::from_secs(10), async {
                loop {
                    let result =
                        login_alias_request(&f, &f.first, alias, "status", &request_id).await;
                    assert_ne!(result["status"], "failed", "{result}");
                    if result["status"] == "completed" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("explicit renewal must repair a stopped wrong-account reservation");
            let result = f
                .http
                .post(format!("{}/v1/token", f.first.url))
                .bearer_auth(&f.token)
                .json(&json!({"alias":alias,"billing":true}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                result.status(),
                200,
                "verified repair must retire only its matching reservation"
            );
            if alias == "other" {
                assert_eq!(
                    request(&f.http, &f.first, &f.token).await.status(),
                    503,
                    "repair of the candidate identity must not retire the selected account reservation"
                );
            }
        }
        assert_eq!(
            login_request(&f, &f.first, "start", &id).await["status"],
            "failed",
            "old receipt stays immutable"
        );
    }
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_waiting_for_refresh_does_not_block_other_accounts() {
    let f = login_fixture().await;
    let id = "2".repeat(64);
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at) VALUES($1,'settling-owner',1,clock_timestamp()+interval '120 seconds') ON CONFLICT(account_id) DO UPDATE SET holder_id='settling-owner',expires_at=clock_timestamp()+interval '120 seconds'", f.schema), &[&account_key("test", "seat")]).await.unwrap();
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "verifying" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    // Let the worker enter its lease wait. This holder represents an outstanding settlement.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let other = timeout(
        Duration::from_secs(5),
        f.http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"other","billing":true}))
            .send(),
    )
    .await
    .expect("waiting renewal blocked an unrelated account")
    .unwrap();
    assert_eq!(other.status(), 200);
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET expires_at=clock_timestamp() WHERE account_id=$1",f.schema), &[&account_key("test","seat")]).await.unwrap();
    timeout(Duration::from_secs(10), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must proceed after the previous refresh settles");
    stop_fixture(f).await;
}
