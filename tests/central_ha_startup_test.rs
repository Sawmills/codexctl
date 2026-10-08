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
        Self::spawn_with_recovery(database, key, root, mode, false).await
    }

    async fn spawn_with_recovery(
        database: &str,
        key: &Path,
        root: tempfile::TempDir,
        mode: &str,
        recovery: bool,
    ) -> Self {
        let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        Self::spawn_with_binary(database, key, root, mode, recovery, &binary).await
    }

    async fn spawn_with_binary(
        database: &str,
        key: &Path,
        root: tempfile::TempDir,
        mode: &str,
        recovery: bool,
        binary: &Path,
    ) -> Self {
        let state = root.path().join("state");
        store::atomic_write(&root.path().join("mode"), b"startup").unwrap();
        for name in ["count", "launch-count"] {
            store::atomic_write(&root.path().join(name), b"0").unwrap();
        }
        let mut child = command(database, &state, key, "serve")
            .env("CODEXCTL_CENTRAL_STORE", mode)
            .env(
                "CODEXCTL_CENTRAL_BACKGROUND_RECOVERY",
                if recovery { "1" } else { "0" },
            )
            .env("CENTRAL_TEST_RETRY_CLOCK", root.path().join("retry-clock"))
            .env("CENTRAL_TEST_RECOVERY_INTERVAL_MS", "20")
            .args([
                "--listen",
                "127.0.0.1:0",
                "--public-url",
                "http://127.0.0.1:8787",
                "--codex-bin",
            ])
            .arg(binary)
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
            .json(&json!({"alias":alias,"auth":auth,"label":"Original seat"}))
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
    // Add polling now uses a shared receipt before the account exists.
    let add = http
        .post(format!("{}/v1/accounts/login/start", pods[0].url))
        .bearer_auth(&token)
        .json(&json!({"alias":"new-account","id":"a".repeat(64)}))
        .send()
        .await
        .unwrap();
    assert_eq!(add.status(), 200);
    assert_eq!(add.json::<Value>().await.unwrap()["status"], "pending");
    let cancel = http
        .post(format!("{}/v1/accounts/login/cancel", pods[1].url))
        .bearer_auth(&token)
        .json(&json!({"alias":"new-account","id":"a".repeat(64)}))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    let rename = http
        .post(format!("{}/v1/accounts/rename", pods[0].url))
        .bearer_auth(&token)
        .json(&json!({"alias":"seat","newAlias":"renamed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rename.status(), 503);
    assert_eq!(
        rename.json::<Value>().await.unwrap()["error"],
        "account_rename_unavailable"
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
    login_fixture_with_legacy_alias(false).await
}
async fn login_fixture_with_legacy_alias(legacy: bool) -> LoginFixture {
    login_fixture_with_accounts(legacy, false).await
}
async fn login_fixture_with_accounts(legacy: bool, foreign: bool) -> LoginFixture {
    login_fixture_with_rename(legacy, foreign, false).await
}
async fn login_fixture_with_rename(legacy: bool, foreign: bool, renamed: bool) -> LoginFixture {
    let Ok(database) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("CI").ok().as_deref(),
            Some("true"),
            "DATABASE_URL required in CI"
        );
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
            {"id":"test","email":"test@example.invalid","enabled":true},
            {"id":"foreign","email":"foreign@example.invalid","enabled":true}
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
            .json(&json!({"alias":alias,"auth":auth,"label":"Original seat"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "file-mode seed import");
    }
    if foreign {
        let foreign_file = seed.root.path().join("foreign-machine-token");
        central::register(
            &state,
            "foreign-machine",
            "sawmills",
            "foreign",
            &foreign_file,
        )
        .unwrap();
        let foreign_token = std::fs::read_to_string(&foreign_file).unwrap();
        // File-mode registry snapshots need the newly registered machine.
        seed.stop().await;
        seed = Pod::spawn(database.as_str(), &key, seed.root, "file").await;
        store::atomic_write(&seed.root.path().join("mode"), b"").unwrap();
        let response = http
            .post(format!("{}/v1/accounts", seed.url))
            .bearer_auth(&foreign_token)
            .json(&json!({"alias":"foreign-seat","auth":foreign_grant()}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "foreign company user import");
    }
    let seed_token: Value = request(&http, &seed, &token).await.json().await.unwrap();
    assert_eq!(generation(&seed_token), if foreign { 2 } else { 1 });
    if renamed {
        let renamed = http
            .post(format!("{}/v1/accounts/rename", seed.url))
            .bearer_auth(&token)
            .json(&json!({"alias":"seat","newAlias":"renamed-seat"}))
            .send()
            .await
            .unwrap();
        assert_eq!(renamed.status(), 200);
    }
    seed.stop().await;
    if legacy {
        use aes_gcm::{
            Aes256Gcm,
            aead::{Aead, KeyInit},
        };
        let original = state.join("accounts").join(account_key("test", "seat"));
        let bytes = std::fs::read(original.join("vault.enc")).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&std::fs::read(&key).unwrap()).unwrap();
        let plain = cipher.decrypt((&bytes[..12]).into(), &bytes[12..]).unwrap();
        let mut saved: Value = serde_json::from_slice(&plain).unwrap();
        saved["alias"] = json!(" seat ");
        // Used once with this fixture's fresh random key, for a legacy on-disk account.
        let nonce = [123_u8; 12];
        let encrypted = cipher
            .encrypt(
                (&nonce).into(),
                serde_json::to_vec(&saved).unwrap().as_slice(),
            )
            .unwrap();
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        store::atomic_write(&original.join("vault.enc"), &bytes).unwrap();
        std::fs::rename(
            &original,
            state.join("accounts").join(account_key("test", " seat ")),
        )
        .unwrap();
    }

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
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    let expired = login_request(&f, &f.second, "status", &id).await;
    assert_eq!(
        expired["error"], "login_expired_requires_recovery",
        "a superseded holder cannot clear the replacement's reservation"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 503);
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
    interrupted_verification(false).await;
}
#[tokio::test]
async fn postgres_cancel_stops_verification_and_preserves_its_reservation() {
    interrupted_verification(true).await;
}
async fn interrupted_verification(cancel: bool) {
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
    let resumed = timeout(
        Duration::from_secs(3),
        login_request(&f, &f.first, "start", &"ab".repeat(32)),
    )
    .await
    .expect("resume must consult the shared operation without waiting for its verifier");
    assert_eq!(resumed["id"], id);
    let unrelated = timeout(
        Duration::from_secs(5),
        f.http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"other","billing":true}))
            .send(),
    )
    .await
    .expect("slow renewal initialization blocked an unrelated account")
    .unwrap();
    assert_eq!(unrelated.status(), 200);
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
    if cancel {
        login_request(&f, &f.second, "cancel", &id).await;
    } else {
        f.control.execute(&format!("UPDATE {}.account_refresh_leases SET epoch=epoch+1,holder_id='replacement',expires_at=clock_timestamp() WHERE account_id=$1",f.schema),&[&account_key("test","seat")]).await.unwrap();
    }
    wait_dead(pid["pid"].as_u64().unwrap() as u32).await;
    // Native exit precedes the shared status write. Observe both boundaries.
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("interrupted verification must publish its unresolved status");
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
    wrong_candidate_reservation(false, None).await;
}
#[tokio::test]
async fn postgres_candidate_publication_failure_keeps_its_identity_reserved() {
    wrong_candidate_reservation(true, None).await;
}
#[tokio::test]
async fn postgres_failed_native_login_preserves_a_saved_grant() {
    wrong_candidate_reservation(false, Some("login-error-after-save")).await;
}
#[tokio::test]
async fn postgres_canceled_native_login_preserves_a_saved_grant_and_allows_repair() {
    wrong_candidate_reservation(false, Some("login-hold-after-save")).await;
}
async fn wrong_candidate_reservation(fail_publication: bool, mode: Option<&str>) {
    let f = login_fixture().await;
    if let Some(mode) = mode {
        store::atomic_write(&f.first.root.path().join("mode"), mode.as_bytes()).unwrap();
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
    if mode == Some("login-hold-after-save") {
        timeout(Duration::from_secs(8), async {
            while !f.first.root.path().join("login-saved").exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        login_request(&f, &f.second, "cancel", &id).await;
    }
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
    if !fail_publication {
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
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET expires_at=clock_timestamp(),released=true WHERE account_id=$1",f.schema), &[&account_key("test","seat")]).await.unwrap();
    timeout(Duration::from_secs(10), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must proceed after the previous refresh settles");
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_unpublished_login_grant_survives_worker_exit() {
    let mut f = login_fixture().await;
    f.control.batch_execute(&format!("CREATE FUNCTION {0}.reject_capture() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase IN ('candidate','unresolved','rejected') THEN RAISE EXCEPTION 'synthetic persistent publication failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_capture BEFORE UPDATE ON {0}.central_login_operations FOR EACH ROW EXECUTE FUNCTION {0}.reject_capture()",f.schema)).await.unwrap();
    let id = "5".repeat(64);
    login_request(&f, &f.first, "start", &id).await;
    let pid = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let claims = json!({"sub":"other-seat","iat":2000001000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":"other-seat","chatgpt_plan_type":"pro"}});
    let grant = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),"refresh_token":"synthetic-wrong","account_id":"other-seat"}});
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    wait_dead(pid).await;
    // Graceful shutdown drains the worker, including its failed fallback write.
    f.first.stop().await;
    let saved = std::fs::read_dir(f.first.root.path().join("state/shared-logins").join(&id))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|home| home.join("auth.json").is_file())
        .expect("an unpublished issued grant must remain on disk after worker exit");
    let retained: Value =
        serde_json::from_slice(&std::fs::read(saved.join("auth.json")).unwrap()).unwrap();
    assert_eq!(retained, grant);
    let operation: Value =
        serde_json::from_slice(&std::fs::read(saved.join("operation.json")).unwrap()).unwrap();
    assert_eq!(operation["id"], id);
    assert_eq!(operation["accountId"], account_key("test", "seat"));
    assert_eq!(operation["epoch"], 1);
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    assert_eq!(
        login_request(&f, &f.second, "status", &id).await["status"],
        "expired"
    );
    let retry = login_request(&f, &f.second, "start", &"c9".repeat(32)).await;
    assert_eq!(
        retry["status"], "pending",
        "expiry must permit a new device login"
    );
    for alias in ["seat", "other"] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias,"billing":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            503,
            "an unpublished grant must remain fenced through expiry and retry: {alias}"
        );
    }
    login_request(&f, &f.second, "cancel", &"c9".repeat(32)).await;
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_swapped_login_candidates_are_rejected_without_waiting_for_refresh() {
    let f = login_fixture().await;
    for alias in ["seat", "other"] {
        f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at) VALUES($1,'settling-owner',1,clock_timestamp()+interval '120 seconds') ON CONFLICT(account_id) DO UPDATE SET holder_id='settling-owner',expires_at=clock_timestamp()+interval '120 seconds'",f.schema), &[&account_key("test", alias)]).await.unwrap();
    }
    let a = "6".repeat(64);
    let b = "7".repeat(64);
    login_request(&f, &f.first, "start", &a).await;
    login_alias_request(&f, &f.second, "other", "start", &b).await;
    let claims = json!({"sub":"other-seat","iat":2000001000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":"other-seat","chatgpt_plan_type":"pro"}});
    let wrong = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),"refresh_token":"synthetic-wrong","account_id":"other-seat"}});
    for (pod, grant) in [(&f.first, wrong), (&f.second, renewal_grant())] {
        store::atomic_write(
            &pod.root.path().join("login-release"),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
    }
    timeout(Duration::from_secs(5), async {
        loop {
            let a = login_request(&f, &f.second, "status", &a).await;
            let b = login_alias_request(&f, &f.first, "other", "status", &b).await;
            if a["error"] == "wrong_account" && b["error"] == "wrong_account" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("wrong identities must be rejected before either refresh lease becomes available");
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_does_not_retire_a_quarantine_issued_after_its_verification() {
    let f = login_fixture().await;
    let a = "8".repeat(64);
    let b = "9".repeat(64);
    store::atomic_write(&f.first.root.path().join("mode"), b"billing-hold").unwrap();
    login_request(&f, &f.first, "start", &a).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("billing-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("verification must reach billing after its forced refresh");
    login_alias_request(&f, &f.second, "other", "start", &b).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_alias_request(&f, &f.second, "other", "status", &b).await["error"]
            != "wrong_account"
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    store::atomic_write(&f.first.root.path().join("release-billing"), b"release").unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &a).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "completion must not erase a newer grant's quarantine"
    );
    store::atomic_write(&f.first.root.path().join("mode"), b"startup").unwrap();
    std::fs::remove_file(f.first.root.path().join("login-release")).unwrap();
    let repair = "a0".repeat(32);
    login_request(&f, &f.first, "start", &repair).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &repair).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a fresh verified renewal must repair the newer quarantine");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_cancel_monitors_previous_owner_settlement_after_lease_expiry() {
    let f = login_fixture().await;
    let id = "ac".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("mode"),
        b"billing-error-held-exit",
    )
    .unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let token = f.token.clone();
    let token_request = tokio::spawn(async move {
        http.post(format!("{url}/v1/token"))
            .bearer_auth(token)
            .json(&json!({"alias":"seat","billing":true}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("exited").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("token failure must reach child settlement");
    let process: Value = serde_json::from_slice(
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
    // Model an outage lasting past lease expiry while the native child cannot settle.
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET expires_at=clock_timestamp() WHERE account_id=$1",f.schema), &[&account_key("test","seat")]).await.unwrap();
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(45), async {
        loop {
            let row = f
                .control
                .query_one(
                    &format!(
                        "SELECT holder_id FROM {}.account_refresh_leases WHERE account_id=$1",
                        f.schema
                    ),
                    &[&account_key("test", "seat")],
                )
                .await
                .unwrap();
            if row.get::<_, String>(0).ends_with(&id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must take the expired lease during a deferred settlement retry");
    let other = timeout(
        Duration::from_secs(5),
        f.http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"other","billing":true}))
            .send(),
    )
    .await
    .expect("old-owner settlement must not block unrelated accounts")
    .unwrap();
    assert_eq!(other.status(), 200);
    login_request(&f, &f.second, "cancel", &id).await;
    wait_dead(process["pid"].as_u64().unwrap() as u32).await;
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("interrupted settlement must publish its unresolved receipt");
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "interrupted native settlement must retain its reservation"
    );
    assert_eq!(token_request.await.unwrap().status(), 503);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_shared_renewal_restores_a_previously_fenced_replica() {
    let f = login_fixture().await;
    let before: Value = request(&f.http, &f.first, &f.token)
        .await
        .json()
        .await
        .unwrap();
    store::atomic_write(&f.first.root.path().join("mode"), b"error").unwrap();
    let failed = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","billing":true,"previousRevision":before["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 503);
    store::atomic_write(&f.first.root.path().join("mode"), b"startup").unwrap();
    assert_eq!(
        request(&f.http, &f.first, &f.token).await.status(),
        503,
        "a permanently fenced owner must not retry unchanged credentials"
    );
    let id = "ad".repeat(32);
    login_request(&f, &f.second, "start", &id).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    // The committed renewal can advance through normal refresh before A observes it.
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    assert_eq!(
        request(&f.http, &f.first, &f.token).await.status(),
        200,
        "completed shared renewal must restore a previously fenced replica"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_preserves_the_legacy_alias_account_key() {
    let f = login_fixture_with_legacy_alias(true).await;
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    let id = "ae".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    assert_eq!(login_request(&f, &f.second, "status", "").await["id"], id);
    login_request(&f, &f.second, "cancel", "").await;
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "canceled" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let renewed = "af".repeat(32);
    login_request(&f, &f.first, "start", &renewed).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &renewed).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must complete against the stored legacy key");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_unreadable_saved_grant_preserves_shared_reservations() {
    let f = login_fixture().await;
    let id = "b0".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(&f.first.root.path().join("login-release"), b"{truncated").unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    for alias in ["seat", "other"] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias,"billing":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            503,
            "unknown grant identity must fence {alias}"
        );
    }
    assert_eq!(
        login_request(&f, &f.second, "start", &"b1".repeat(32)).await["id"],
        id
    );
    let retained = f.first.root.path().join("state/shared-logins").join(&id);
    assert!(
        std::fs::read_dir(retained).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .join("auth.json")
            .exists())
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_committed_renewal_recovers_after_its_response_times_out() {
    let f = login_fixture().await;
    // Delay COMMIT after its command is sent, so a lost response can still leave durable completion.
    f.control
        .batch_execute(&format!(
            r#"
        CREATE FUNCTION {0}.delay_completion() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase IN ('verifying','completed') THEN
                PERFORM set_config('statement_timeout', '0', false);
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER prepare_completion_delay AFTER UPDATE ON {0}.central_login_operations
        FOR EACH ROW EXECUTE FUNCTION {0}.delay_completion();
        CREATE FUNCTION {0}.delay_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='completed' AND OLD.phase<>'completed' THEN PERFORM pg_sleep(3); END IF;
            RETURN NEW;
        END $$;
        CREATE CONSTRAINT TRIGGER delay_commit AFTER UPDATE ON {0}.central_login_operations
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {0}.delay_commit();
    "#,
            f.schema
        ))
        .await
        .unwrap();
    let id = "b2".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(10), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        login_request(&f, &f.second, "start", &id).await["status"],
        "completed"
    );
    let response = request(&f.http, &f.first, &f.token).await;
    assert_eq!(
        response.status(),
        200,
        "durable completion must restore the initiating replica"
    );
    let before: Value = response.json().await.unwrap();
    store::atomic_write(&f.first.root.path().join("mode"), b"error").unwrap();
    let failed = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","billing":true,"previousRevision":before["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 503);
    store::atomic_write(&f.first.root.path().join("mode"), b"startup").unwrap();
    assert_eq!(
        request(&f.http, &f.first, &f.token).await.status(),
        503,
        "a later rejection must not reuse the old completion proof"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_uses_committed_revision_after_unpublished_refresh() {
    renewal_after_unpublished_refresh(false, false, false).await;
}

#[tokio::test]
async fn postgres_obsolete_settlement_cannot_fence_a_completed_renewal() {
    renewal_after_unpublished_refresh(true, false, false).await;
}

#[tokio::test]
async fn postgres_remote_renewal_recovers_an_unpublished_local_revision() {
    renewal_after_unpublished_refresh(false, true, false).await;
}

#[tokio::test]
async fn postgres_remote_renewal_reconciles_unpublished_credentials_after_restart() {
    renewal_after_unpublished_refresh(false, true, true).await;
}

async fn renewal_after_unpublished_refresh(overlap: bool, remote: bool, restart: bool) {
    let mut f = login_fixture().await;
    f.control
        .batch_execute(&format!(
            r#"
        CREATE SEQUENCE {0}.publication_attempt;
        CREATE FUNCTION {0}.reject_refresh_publication() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NOT EXISTS (SELECT 1 FROM {0}.central_login_operations WHERE phase='verifying') THEN
                PERFORM nextval('{0}.publication_attempt');
                RAISE EXCEPTION 'synthetic refresh publication failure';
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER reject_refresh_publication BEFORE UPDATE ON {0}.central_accounts
        FOR EACH ROW EXECUTE FUNCTION {0}.reject_refresh_publication();
    "#,
            f.schema
        ))
        .await
        .unwrap();
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 503);
    if overlap {
        timeout(Duration::from_secs(8), async {
            loop {
                let row = f
                    .control
                    .query_one(
                        &format!("SELECT last_value FROM {}.publication_attempt", f.schema),
                        &[],
                    )
                    .await
                    .unwrap();
                if row.get::<_, i64>(0) >= 4 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background publication must enter its retry delay");
    }
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET holder_id='lost',epoch=epoch+1,expires_at=clock_timestamp(),released=true WHERE account_id=$1", f.schema), &[&account_key("test","seat")]).await.unwrap();
    if overlap {
        store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    } else {
        // Allow the old settlement retry to observe its lost epoch before this renewal.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let id = "b3".repeat(32);
    let renewer = if remote { &f.second } else { &f.first };
    login_request(&f, renewer, "start", &id).await;
    store::atomic_write(
        &renewer.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    if overlap {
        timeout(Duration::from_secs(8), async {
            while !f.first.root.path().join("initialize-started").exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        // Keep the new verifier's mutex held across the old settlement retry.
        tokio::time::sleep(Duration::from_secs(2)).await;
        store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    }
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must commit even when a previous refresh advanced only local state");
    if overlap {
        let completed_failures: i64 = f
            .control
            .query_one(
                &format!(
                    "SELECT count(*) FROM {}.central_login_operations WHERE phase='completed' AND failure_reported",
                    f.schema
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            completed_failures, 0,
            "a committed receipt must not count as a failure"
        );
    }
    if overlap {
        // The obsolete callback can now acquire the renewed owner.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    f.control
        .batch_execute(&format!(
            "DROP TRIGGER reject_refresh_publication ON {}.central_accounts",
            f.schema
        ))
        .await
        .unwrap();
    if restart {
        f.first.stop().await;
        f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    }
    let response = request(&f.http, &f.first, &f.token).await;
    assert_eq!(response.status(), 200);
    let served: Value = response.json().await.unwrap();
    assert!(
        generation(&served) >= 10,
        "replica must serve the renewed grant, not unpublished old credentials"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_waits_for_foreign_refresh_settlement_after_lease_expiry() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let token = f.token.clone();
    let pending = tokio::spawn(async move {
        http.post(format!("{url}/v1/token"))
            .bearer_auth(token)
            .json(&json!({"alias":"seat","billing":true}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET expires_at=clock_timestamp() WHERE account_id=$1", f.schema), &[&account_key("test","seat")]).await.unwrap();
    let id = "b4".repeat(32);
    login_request(&f, &f.second, "start", &id).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        f.second.launches(),
        0,
        "expired foreign lease does not prove its native child stopped"
    );
    login_request(&f, &f.second, "cancel", &id).await;
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(pending.await.unwrap().status(), 503);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_confirmed_native_rejection_allows_fresh_renewal() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"error").unwrap();
    let id = "b5".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_file(f.first.root.path().join("login-release")).unwrap();
    store::atomic_write(&f.first.root.path().join("mode"), b"startup").unwrap();
    let repair = "b6".repeat(32);
    let resumed = login_request(&f, &f.first, "start", &repair).await;
    assert_eq!(
        resumed["id"], repair,
        "confirmed rejection must allow another device login"
    );
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &repair).await["status"] != "completed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_rejection_with_unknown_exit_evidence_stays_unresolved() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    let id = "b7".repeat(32);
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
    // Losing process evidence cannot become positive proof of exit merely
    // because a termination attempt returned without an error to its caller.
    let pid = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "seat"))
        .join("runtime/pid");
    store::atomic_write(&pid, b"unknown-process-evidence").unwrap();
    store::atomic_write(&f.first.root.path().join("mode"), b"error").unwrap();
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let resumed = login_request(&f, &f.second, "start", &"b8".repeat(32)).await;
    assert_eq!(
        resumed["id"], id,
        "unknown process exit must not permit replacement verification"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 503);
    stop_fixture(f).await;
}

fn foreign_grant() -> Value {
    let claims = json!({"sub":"foreign-login","iat":2000000000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":"foreign-workspace","chatgpt_plan_type":"pro"}});
    json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),
        "refresh_token":"synthetic-refresh","account_id":"foreign-workspace"}})
}

#[tokio::test]
async fn postgres_unidentified_login_does_not_fence_another_company_user() {
    let f = login_fixture_with_accounts(false, true).await;
    let id = "c0".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(&f.first.root.path().join("login-release"), b"{truncated").unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let foreign_token =
        std::fs::read_to_string(f._seed.root.path().join("foreign-machine-token")).unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(foreign_token)
        .json(&json!({"alias":"foreign-seat","billing":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "an unidentified grant must fence only its company user"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 503);
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(
            "codexctl_central_failed_requests_total{reason=\"relogin_identity_unresolved\"} 1"
        ),
        "{metrics}"
    );
    stop_fixture(f).await;
}

#[cfg(target_os = "linux")]
async fn wait_parent_bound_exit(pid: u32) {
    timeout(Duration::from_secs(8), async {
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(stat)
                    if stat.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z") =>
                {
                    break;
                }
                Err(error) => panic!("child liveness: {error}"),
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("parent death must terminate device polling");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_killed_device_login_holder_allows_retry_and_completion_on_another_replica() {
    let mut f = login_fixture().await;
    let abandoned = "f1".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &abandoned).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.first.child.kill().await.unwrap();
    wait_parent_bound_exit(pid).await;
    let expired = timeout(Duration::from_secs(45), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &abandoned).await;
            if receipt["status"] == "expired" {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the dead holder's lease must expire");
    assert!(
        expired["error"].is_null(),
        "bound device polling can recover after holder loss: {expired}"
    );
    let fresh = "f2".repeat(32);
    assert_eq!(
        login_request(&f, &f.second, "start", &fresh).await["status"],
        "pending"
    );
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &fresh).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("B must complete the replacement login");
    assert_eq!(f.second.launches(), 1, "only the replacement verifies");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    assert_eq!(
        login_request(&f, &f.second, "start", &abandoned).await["status"],
        "expired"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_fresh_request_recovers_killed_holder_without_abandoned_receipt_polling() {
    let mut f = login_fixture().await;
    let abandoned = "f1".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &abandoned).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.first.child.kill().await.unwrap();
    wait_parent_bound_exit(pid).await;
    // No old-ID read performs recovery before this fresh request.
    tokio::time::sleep(Duration::from_secs(31)).await;
    let fresh = "f2".repeat(32);
    assert_eq!(
        login_request(&f, &f.second, "start", &fresh).await["status"],
        "pending"
    );
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &fresh).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("B must complete the replacement login");
    assert_eq!(f.second.launches(), 1, "only the replacement verifies");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    assert_eq!(
        login_request(&f, &f.second, "start", &abandoned).await["status"],
        "expired"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_killed_candidate_holder_resumes_without_another_device_login() {
    let mut f = login_fixture().await;
    let account = account_key("test", "seat");
    // A real foreign refresh owner still needs settlement. Keep verification
    // before its in-flight marker until that independent owner releases.
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account]).await.unwrap();
    let id = "f3".repeat(32);
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
    assert_eq!(
        f.first.launches(),
        0,
        "the candidate is durable before verifier launch"
    );
    f.first.child.kill().await.unwrap();
    // Expire only the killed replica's leases, using the real DB clock.
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    let receipt = login_request(&f, &f.second, "start", &id).await;
    assert_eq!(receipt["id"], id);
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "a recovered candidate must still wait for the foreign owner's settlement"
    );
    assert_eq!(f.second.launches(), 0);
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=true WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("B must resume the durable candidate after refresh settlement");
    assert!(
        !f.second.root.path().join("login-pid").exists(),
        "candidate takeover must not repeat device login"
    );
    assert_eq!(f.second.launches(), 1);
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_candidate_takeover_recovers_after_its_commit_response_times_out() {
    let mut f = login_fixture().await;
    let account = account_key("test", "seat");
    // A real foreign refresh owner still needs settlement. Keep verification
    // before its in-flight marker until that independent owner releases.
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account]).await.unwrap();
    f.control.batch_execute(&format!(r#"
        CREATE FUNCTION {0}.prepare_takeover_delay() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='candidate' AND NEW.holder_id<>OLD.holder_id THEN
                PERFORM set_config('statement_timeout','0',false);
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER prepare_takeover_delay AFTER UPDATE ON {0}.central_login_operations
        FOR EACH ROW EXECUTE FUNCTION {0}.prepare_takeover_delay();
        CREATE FUNCTION {0}.delay_takeover_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='candidate' AND NEW.holder_id<>OLD.holder_id THEN PERFORM pg_sleep(3); END IF;
            RETURN NEW;
        END $$;
        CREATE CONSTRAINT TRIGGER delay_takeover_commit AFTER UPDATE ON {0}.central_login_operations
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {0}.delay_takeover_commit();
    "#,f.schema)).await.unwrap();
    let id = "26".repeat(32);
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
    assert_eq!(
        f.first.launches(),
        0,
        "the candidate is durable before verifier launch"
    );
    f.first.child.kill().await.unwrap();
    // Expire only the killed replica's leases, using the real DB clock.
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    let response = f
        .http
        .post(format!("{}/v1/relogin/start", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","id":id}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "the takeover commit response must exceed the DB deadline"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let receipt = login_request(&f, &f.second, "start", &id).await;
    assert_eq!(receipt["id"], id);
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "a recovered candidate must still wait for the foreign owner's settlement"
    );
    assert_eq!(f.second.launches(), 0);
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=true WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    let mut retries = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let http = f.http.clone();
        let url = f.second.url.clone();
        let token = f.token.clone();
        let id = id.clone();
        retries.spawn(async move {
            http.post(format!("{url}/v1/relogin/status"))
                .bearer_auth(token)
                .json(&json!({"alias":"seat","id":id}))
                .send()
                .await
                .unwrap()
        });
    }
    while let Some(response) = retries.join_next().await {
        assert_eq!(response.unwrap().status(), 200);
    }
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("B must resume the durable candidate after refresh settlement");
    assert!(
        !f.second.root.path().join("login-pid").exists(),
        "candidate takeover must not repeat device login"
    );
    assert_eq!(f.second.launches(), 1);
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_killed_verifier_retains_unresolved_receipt_without_reverification() {
    let mut f = login_fixture().await;
    let id = "f4".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
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
    assert_eq!(f.first.launches(), 1);
    f.first.child.kill().await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    for action in ["status", "status", "start"] {
        let receipt = login_request(&f, &f.second, action, &id).await;
        assert_eq!(receipt["status"], "failed", "{receipt}");
        assert_eq!(receipt["error"], "relogin_verification_unresolved");
    }
    assert_eq!(
        login_request(&f, &f.second, "start", &"f5".repeat(32)).await["id"],
        id
    );
    assert_eq!(
        f.second.launches(),
        0,
        "an in-flight marker must never launch a second verifier"
    );
    assert!(!f.second.root.path().join("login-pid").exists());
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 503);
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"),
        "repeated receipt reads must count one failed operation: {metrics}"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_add_retries_killed_polling_holder_with_a_fresh_id() {
    let mut f = login_fixture().await;
    let old = json!({"alias":"new-account","id":"f6".repeat(32),"label":"New seat"});
    assert_eq!(
        add_request(&f, &f.first, "start", &old).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.first.child.kill().await.unwrap();
    wait_parent_bound_exit(pid).await;
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema,f.schema), &[&old["id"].as_str().unwrap()]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&old["id"].as_str().unwrap()]).await.unwrap();
    let fresh = json!({"alias":"new-account","id":"f7".repeat(32),"label":"New seat"});
    assert_eq!(
        add_request(&f, &f.second, "start", &fresh).await["id"],
        fresh["id"]
    );
    assert_eq!(
        complete_add(
            &f,
            &f.second,
            &fresh,
            &add_grant("new-workspace", Some("new-login"), None)
        )
        .await["status"],
        "completed"
    );
    for _ in 0..2 {
        let expired = add_request(&f, &f.second, "status", &old).await;
        assert_eq!(expired["status"], "expired");
        assert!(expired["error"].is_null());
    }
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"),
        "{metrics}"
    );
    assert_eq!(f.second.launches(), 1);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_add_candidate_takeover_preserves_same_user_landing() {
    let mut f = login_fixture().await;
    let account = account_key("test", "seat");
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account]).await.unwrap();
    let op = json!({"alias":"candidate-add","id":"f8".repeat(32),"label":"Unused label"});
    add_request(&f, &f.first, "start", &op).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = add_request(&f, &f.second, "status", &op).await;
            if receipt["landedAlias"] == "seat" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.first.launches(), 0);
    f.first.child.kill().await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema,f.schema), &[&op["id"].as_str().unwrap()]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&op["id"].as_str().unwrap()]).await.unwrap();
    assert_eq!(
        add_request(&f, &f.second, "start", &op).await["landedAlias"],
        "seat"
    );
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=true WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    assert_eq!(wait_add_terminal(&f, &op).await["status"], "completed");
    assert!(!f.second.root.path().join("login-pid").exists());
    assert_eq!(f.second.launches(), 1);
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[cfg(target_os = "linux")]
async fn login_epoch(f: &LoginFixture, id: &str) -> i64 {
    f.control
        .query_one(
            &format!(
                "SELECT epoch FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

#[cfg(target_os = "linux")]
async fn relogin_failure_metrics(f: &LoginFixture, pod: &Pod) -> Vec<String> {
    let metrics = f
        .http
        .get(format!("{}/metrics", pod.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    metrics
        .lines()
        .filter(|line| line.starts_with("codexctl_central_failed_requests_total{reason=\"relogin_"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_live_holder_resumes_its_own_candidate_after_worker_lease_loss() {
    let f = login_fixture().await;
    let account = account_key("test", "seat");
    // Hold the candidate before verification so its worker waits in the lease loop.
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account]).await.unwrap();
    let id = "e1".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.first, "status", &id).await["status"] != "verifying" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let epoch = login_epoch(&f, &id).await;
    // Expire only the operation lease. The worker stops and its terminal save is
    // fenced, while the replica's holder stays live.
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            let receipt = login_request(&f, &f.first, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if login_epoch(&f, &id).await > epoch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the live holder must take over its own expired candidate");
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=true WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.first, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the resumed candidate must complete after refresh settlement");
    assert_eq!(
        relogin_failure_metrics(&f, &f.first).await,
        Vec::<String>::new(),
        "a fenced worker attempt that later completes is not a failure"
    );
    assert_eq!(f.first.launches(), 1);
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_live_holder_settles_its_own_verification_after_worker_lease_loss() {
    let f = login_fixture().await;
    let id = "e2".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
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
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            let receipt = login_request(&f, &f.first, "status", &id).await;
            if receipt["status"] == "failed" {
                assert_eq!(receipt["error"], "relogin_verification_unresolved");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the live holder must settle its own expired verification");
    assert_eq!(
        f.first.launches(),
        1,
        "an in-flight marker must never launch a second verifier"
    );
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 503);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_fenced_worker_without_a_grant_still_counts_its_failure() {
    let f = login_fixture().await;
    let id = "e4".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    // Expire only the operation lease: the worker stops, and its terminal save
    // is fenced. No grant exists, so the attempt cannot resume as a success.
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", f.schema), &[&id]).await.unwrap();
    // No receipt read here: the worker itself must count the failure.
    timeout(Duration::from_secs(10), async {
        while relogin_failure_metrics(&f, &f.first).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a fenced worker without a grant must count its failure");
    assert_eq!(
        relogin_failure_metrics(&f, &f.first).await,
        ["codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"]
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_unregistered_holder_failure_is_left_to_its_own_replica() {
    let mut f = login_fixture().await;
    let id = "e3".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    // Stop the supervisor before it can record grant absence, as in an old
    // replica that never registered a holder row.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let supervisor: i32 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(f.first.child.id().unwrap() as i32, libc::SIGSTOP) },
        0
    );
    assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
    f.first.child.kill().await.unwrap();
    wait_parent_bound_exit(pid).await;
    f.control.execute(&format!("DELETE FROM {}.central_login_holders WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)", f.schema, f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    for _ in 0..2 {
        let old = login_request(&f, &f.second, "status", &id).await;
        assert_eq!(old["error"], "login_expired_requires_recovery");
    }
    assert_eq!(
        relogin_failure_metrics(&f, &f.second).await,
        Vec::<String>::new(),
        "an unregistered holder may still report its own failure"
    );
    let reported: bool = f
        .control
        .query_one(
            &format!(
                "SELECT failure_reported FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!reported);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_missing_or_unbound_polling_holder_keeps_refresh_fenced() {
    for missing in [false, true] {
        let mut f = login_fixture().await;
        let id = "f9".repeat(32);
        login_request(&f, &f.first, "start", &id).await;
        let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
            .unwrap()
            .parse()
            .unwrap();
        // Model an old or unsupported holder with no durable supervisor receipt.
        // Kill the supervisor too, while its parent cannot record local evidence.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let supervisor: i32 = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(f.first.child.id().unwrap() as i32, libc::SIGSTOP) },
            0
        );
        assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
        f.first.child.kill().await.unwrap();
        wait_parent_bound_exit(pid).await;
        let query = if missing {
            format!(
                "UPDATE {}.central_login_holders SET deleted_at=clock_timestamp() WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",
                f.schema, f.schema
            )
        } else {
            format!(
                "UPDATE {}.central_login_holders SET polling_bound=false,expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",
                f.schema, f.schema
            )
        };
        f.control.execute(&query, &[&id]).await.unwrap();
        f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
        let old = login_request(&f, &f.second, "status", &id).await;
        assert_eq!(old["error"], "login_expired_requires_recovery");
        let fresh = "fa".repeat(32);
        assert_eq!(
            login_request(&f, &f.second, "start", &fresh).await["status"],
            "pending"
        );
        assert_eq!(
            request(&f.http, &f.second, &f.token).await.status(),
            503,
            "missing or non-parent-bound holder evidence must not authorize refresh"
        );
        login_request(&f, &f.second, "cancel", &fresh).await;
        stop_fixture(f).await;
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_holder_renewal_recovers_readiness_after_database_outage() {
    let f = login_fixture().await;
    assert_eq!(
        f.http
            .get(format!("{}/ready", f.first.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let account = account_key("test", "seat");
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    let lease_holder = format!(
        "SELECT holder_id FROM {}.account_refresh_leases WHERE account_id=$1",
        f.schema
    );
    let refresh_holder: String = f
        .control
        .query_one(&lease_holder, &[&account])
        .await
        .unwrap()
        .get(0);
    f.control
        .batch_execute(&format!(
            r#"
            CREATE FUNCTION {0}.fail_holder_renewal() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                RAISE EXCEPTION 'synthetic database outage';
            END $$;
            CREATE TRIGGER fail_holder_renewal
                BEFORE UPDATE ON {0}.central_login_holders
                FOR EACH ROW EXECUTE FUNCTION {0}.fail_holder_renewal();
            "#,
            f.schema
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(30), async {
        loop {
            let response = f
                .http
                .get(format!("{}/ready", f.first.url))
                .send()
                .await
                .unwrap();
            assert_eq!(
                f.http
                    .get(format!("{}/health", f.first.url))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
            if response.status() == 503 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("holder loss must fence readiness after the retry window");
    f.control
        .batch_execute(&format!(
            "DROP TRIGGER fail_holder_renewal ON {}.central_login_holders; DROP FUNCTION {}.fail_holder_renewal()",
            f.schema, f.schema
        ))
        .await
        .unwrap();
    // Keep the abandoned incarnation live. Recovery must give it up at once,
    // so other replicas can recover its operations.
    let abandoned = format!(
        "UPDATE {}.central_login_holders SET expires_at=clock_timestamp()+interval '60 seconds' WHERE holder_id=$1",
        f.schema
    );
    assert_eq!(
        f.control
            .execute(&abandoned, &[&refresh_holder])
            .await
            .unwrap(),
        1
    );
    timeout(Duration::from_secs(20), async {
        loop {
            if f.http
                .get(format!("{}/ready", f.first.url))
                .send()
                .await
                .unwrap()
                .status()
                == 200
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("readiness must recover after the database returns");
    let abandoned_live: bool = f
        .control
        .query_one(
            &format!(
                "SELECT expires_at>clock_timestamp() FROM {}.central_login_holders WHERE holder_id=$1",
                f.schema
            ),
            &[&refresh_holder],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!abandoned_live, "recovery must expire the abandoned holder");
    // A request cut off by the outage could not release its refresh lease.
    // The recovered replica still owns it and must not refuse its own lease.
    f.control
        .execute(
            &format!(
                "UPDATE {}.account_refresh_leases SET released=false,expires_at=clock_timestamp()+interval '120 seconds' WHERE account_id=$1",
                f.schema
            ),
            &[&account],
        )
        .await
        .unwrap();
    assert_eq!(request(&f.http, &f.first, &f.token).await.status(), 200);
    let after: String = f
        .control
        .query_one(&lease_holder, &[&account])
        .await
        .unwrap()
        .get(0);
    assert_eq!(after, refresh_holder);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_stopped_parent_has_an_independent_polling_watchdog() {
    let f = login_fixture().await;
    let id = "fb".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        !central_child_dead(pid),
        "the polling child must be live before the parent stops"
    );
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    let stopped = timeout(Duration::from_secs(15), async {
        loop {
            if central_child_dead(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGCONT) }, 0);
    assert!(
        stopped.is_ok(),
        "a paused parent must not leave polling alive beyond the independent watchdog"
    );
    stop_fixture(f).await;
}
#[cfg(target_os = "linux")]
fn central_child_dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Ok(stat) => stat.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z"),
        Err(error) => panic!("child liveness: {error}"),
    }
}
#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_killed_parent_after_local_grant_does_not_clear_unknown_evidence() {
    let mut f = login_fixture().await;
    let id = "fc".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"login-hold-after-save").unwrap();
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("other-seat", Some("other-seat"), None)).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-saved").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.first.child.kill().await.unwrap();
    wait_parent_bound_exit(pid).await;
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    login_request(&f, &f.second, "status", &id).await;
    for alias in ["seat", "other"] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias,"billing":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            503,
            "a locally saved grant must remain fenced after abrupt parent death: {alias}"
        );
    }
    assert_eq!(f.second.launches(), 0);
    for _ in 0..2 {
        login_request(&f, &f.second, "status", &id).await;
    }
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"),
        "supervisor failure must be reported once: {metrics}"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_cancel_expired_candidate_records_intent_without_waiting_for_admission() {
    let mut f = login_fixture().await;
    let account = account_key("test", "seat");
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account]).await.unwrap();
    let id = "fd".repeat(32);
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
    f.first.child.kill().await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    // Block admission only. Cancel intent has its own authorized write.
    f.control
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock(hashtextextended('{}',12484))",
            f.schema
        ))
        .await
        .unwrap();
    let canceled = timeout(
        Duration::from_secs(1),
        login_request(&f, &f.second, "cancel", &id),
    )
    .await;
    f.control.batch_execute("COMMIT").await.unwrap();
    assert!(
        canceled.is_ok(),
        "cancellation must not wait for candidate takeover admission"
    );
    assert_eq!(f.second.launches(), 0);
    timeout(Duration::from_secs(8), async {
        while login_request(&f, &f.second, "status", &id).await["status"] != "failed" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.second.launches(),
        0,
        "cancellation must precede any verifier launch"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_another_machine_retries_an_expired_polling_alias_without_old_receipt_access() {
    let mut f = login_fixture().await;
    let id = "fe".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    let token_file = f._seed.root.path().join("takeover-machine-token");
    let output = command(
        &f.database,
        &f._seed.root.path().join("state"),
        &f.key,
        "register",
    )
    .args([
        "--device",
        "takeover-machine",
        "--tenant",
        "sawmills",
        "--user",
        "test",
        "--token-file",
    ])
    .arg(&token_file)
    .output()
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "synthetic machine registration failed"
    );
    f.first.child.kill().await.unwrap();
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    wait_parent_bound_exit(pid).await;
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    f.token = std::fs::read_to_string(token_file).unwrap();
    let forbidden = f
        .http
        .post(format!("{}/v1/relogin/status", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","id":id}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        forbidden.status(),
        409,
        "the initiating machine still owns its receipt"
    );
    let fresh = "ff".repeat(32);
    let started = login_request(&f, &f.second, "start", &fresh).await;
    assert_eq!(started["id"], fresh);
    assert_eq!(started["status"], "pending");
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &fresh).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.second.launches(), 1);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_native_spawn_failure_does_not_leave_an_unidentified_grant_fence() {
    use std::os::unix::fs::PermissionsExt;

    let mut f = login_fixture().await;
    f.first.stop().await;
    let binary = f.first.root.path().join("unlaunchable-codex");
    store::atomic_write(&binary, b"#!/missing-synthetic-interpreter\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    f.first = Pod::spawn_with_binary(
        &f.database,
        &f.key,
        f.first.root,
        "postgres",
        false,
        &binary,
    )
    .await;
    let operation = json!({"alias":"new-one","id":"20".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    let receipt = wait_add_terminal(&f, &operation).await;
    assert_eq!(receipt["status"], "failed");
    // The worker counts the failure just after its durable save.
    timeout(Duration::from_secs(5), async {
        while relogin_failure_metrics(&f, &f.first).await
            != ["codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"]
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a durably failed login must count once");
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        200,
        "a failed spawn issued no grant and must not fence the company user's accounts"
    );
    let retry = json!({"alias":"new-one","id":"21".repeat(32)});
    assert_eq!(
        add_request(&f, &f.second, "start", &retry).await["id"],
        retry["id"]
    );
    complete_add(
        &f,
        &f.second,
        &retry,
        &add_grant("new-workspace", Some("new-login"), None),
    )
    .await;
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_immediate_native_exit_can_retry_without_process_identity() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    f.first = Pod::spawn_with_binary(
        &f.database,
        &f.key,
        f.first.root,
        "postgres",
        false,
        Path::new("/bin/false"),
    )
    .await;
    // Repeat the real fast-exit race: the child may exit before /proc supplies
    // its incarnation. Every awaited failure still proves no grant was saved.
    for attempt in 0..24 {
        let operation = json!({"alias":"new-one","id":format!("{:064x}", 256 + attempt)});
        add_request(&f, &f.first, "start", &operation).await;
        assert_eq!(wait_add_terminal(&f, &operation).await["status"], "failed");
        assert_eq!(
            request(&f.http, &f.second, &f.token).await.status(),
            200,
            "an awaited fast exit without a grant must not fence accounts (attempt {attempt})"
        );
    }
    let retry = json!({"alias":"new-one","id":"22".repeat(32)});
    assert_eq!(
        add_request(&f, &f.second, "start", &retry).await["id"],
        retry["id"]
    );
    complete_add(
        &f,
        &f.second,
        &retry,
        &add_grant("new-workspace", Some("new-login"), None),
    )
    .await;
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_candidate_publication_recovers_after_its_commit_response_times_out() {
    let f = login_fixture().await;
    f.control.batch_execute(&format!(r#"
        CREATE FUNCTION {0}.prepare_candidate_delay() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='candidate' AND OLD.phase IN ('starting','pending') THEN
                PERFORM set_config('statement_timeout','0',false);
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER prepare_candidate_delay AFTER UPDATE ON {0}.central_login_operations
        FOR EACH ROW EXECUTE FUNCTION {0}.prepare_candidate_delay();
        CREATE FUNCTION {0}.delay_candidate_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='candidate' AND OLD.phase IN ('starting','pending') THEN PERFORM pg_sleep(3); END IF;
            RETURN NEW;
        END $$;
        CREATE CONSTRAINT TRIGGER delay_candidate_commit AFTER UPDATE ON {0}.central_login_operations
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {0}.delay_candidate_commit();
    "#,f.schema)).await.unwrap();
    let id = "27".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let response = f
                .http
                .post(format!("{}/v1/relogin/status", f.second.url))
                .bearer_auth(&f.token)
                .json(&json!({"alias":"seat","id":id}))
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                let receipt: Value = response.json().await.unwrap();
                assert_ne!(
                    receipt["status"], "failed",
                    "a committed candidate survives its lost response: {receipt}"
                );
                if receipt["status"] == "completed" {
                    break;
                }
            } else {
                assert_eq!(
                    response.status(),
                    503,
                    "transient DB lock wait remains explicit"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.first.launches(), 1);
    assert!(!f.second.root.path().join("login-pid").exists());
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_fast_login_commits_before_parent_reads_its_challenge() {
    let f = login_fixture().await;
    let id = "10".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"login-prompt-gate").unwrap();
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let token = f.token.clone();
    let request_id = id.clone();
    let start = tokio::spawn(async move {
        http.post(format!("{url}/v1/relogin/start"))
            .bearer_auth(token)
            .json(&json!({"alias":"seat","id":request_id}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    f.control
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock(hashtextextended('{}',12484))",
            f.schema
        ))
        .await
        .unwrap();
    store::atomic_write(&f.first.root.path().join("login-prompt-release"), b"go").unwrap();
    // The supervisor can reach publication while the parent cannot read output.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGCONT) }, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.control.batch_execute("COMMIT").await.unwrap();
    assert_eq!(start.await.unwrap().status(), 200);
    timeout(Duration::from_secs(12),async {
        loop {
            let receipt=login_request(&f,&f.second,"status",&id).await;
            assert_ne!(receipt["status"],"failed","a successful native grant must not become rejected by challenge publication: {receipt}");
            if receipt["status"]=="completed" {break;}
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.unwrap();
    assert_eq!(f.first.launches(), 1);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_supervised_login_resolves_a_relative_key_from_server_cwd() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    let root = f.first.root;
    store::atomic_write(
        &root.path().join("relative-key"),
        &std::fs::read(&f.key).unwrap(),
    )
    .unwrap();
    store::atomic_write(&root.path().join("mode"), b"").unwrap();
    let mut child = command(
        &f.database,
        &root.path().join("state"),
        Path::new("relative-key"),
        "serve",
    )
    .current_dir(root.path())
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
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    f.first = Pod {
        child,
        root,
        url: format!("http://{}", ready["listening"].as_str().unwrap()),
    };
    let id = "11".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    stop_fixture(f).await;
}

#[tokio::test]
#[ignore = "requires TLS-enabled PostgreSQL and CODEXCTL_TEST_DB_CA_FILE"]
#[cfg(target_os = "linux")]
async fn postgres_supervised_login_resolves_a_relative_database_ca_from_server_cwd() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    let root = f.first.root;
    let ca = std::env::var("CODEXCTL_TEST_DB_CA_FILE").expect("test CA bundle required");
    store::atomic_write(
        &root.path().join("relative-ca.crt"),
        &std::fs::read(ca).unwrap(),
    )
    .unwrap();
    store::atomic_write(&root.path().join("mode"), b"").unwrap();
    let mut child = command(
        &f.database.replace("sslmode=disable", "sslmode=require"),
        &root.path().join("state"),
        &f.key,
        "serve",
    )
    .current_dir(root.path())
    .env("CODEXCTL_CENTRAL_DB_TLS", "1")
    .env("CODEXCTL_CENTRAL_DB_CA_FILE", "relative-ca.crt")
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
    .unwrap()
    .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    f.first = Pod {
        child,
        root,
        url: format!("http://{}", ready["listening"].as_str().unwrap()),
    };
    let id = "25".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_cancel_before_supervisor_launch_preserves_grant_absence() {
    let f = login_fixture().await;
    f.control.batch_execute(&format!("CREATE SEQUENCE {0}.starting_tick; CREATE FUNCTION {0}.pause_first_tick() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='starting' AND OLD.phase='starting' AND NOT NEW.cancel_requested AND nextval('{0}.starting_tick')=1 THEN PERFORM pg_sleep(0.5); END IF; RETURN NEW; END $$; CREATE TRIGGER pause_tick BEFORE UPDATE ON {0}.central_login_operations FOR EACH ROW EXECUTE FUNCTION {0}.pause_first_tick()",f.schema)).await.unwrap();
    let id = "12".repeat(32);
    let http = f.http.clone();
    let url = f.first.url.clone();
    let token = f.token.clone();
    let request_id = id.clone();
    let start = tokio::spawn(async move {
        http.post(format!("{url}/v1/relogin/start"))
            .bearer_auth(token)
            .json(&json!({"alias":"seat","id":request_id}))
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8), async {
        loop {
            let exists: bool = f
                .control
                .query_one(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {}.central_login_operations WHERE id=$1)",
                        f.schema
                    ),
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            if exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    login_request(&f, &f.second, "cancel", &id).await;
    assert_eq!(start.await.unwrap().status(), 200);
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(
                receipt["status"], "failed",
                "a pre-launch cancellation has no unknown grant: {receipt}"
            );
            if receipt["status"] == "canceled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(!f.first.root.path().join("login-pid").exists());
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_cancel_on_b_stops_polling_while_a_is_stopped() {
    let f = login_fixture().await;
    let id = "13".repeat(32);
    assert_eq!(
        login_request(&f, &f.first, "start", &id).await["status"],
        "pending"
    );
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    login_request(&f, &f.second, "cancel", &id).await;
    let stopped = timeout(Duration::from_secs(4), async {
        while !central_child_dead(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGCONT) }, 0);
    assert!(
        stopped.is_ok(),
        "durable cancellation must not wait for the parent's watchdog timeout"
    );
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "canceled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_unreadable_settlement_receipt_keeps_an_unpublished_grant_fenced() {
    let f = login_fixture().await;
    let id = "24".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"login-hold-after-save").unwrap();
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("other-seat", Some("other-login"), None)).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-saved").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    // Allow the parent's normal tick to observe the published challenge before
    // failing its later settlement read at the same operation sequence.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    // Make the shared receipt unreadable at the settlement boundary. Updates
    // remain available, so the worker must not mistake failed reads for absence.
    f.control.execute(&format!("UPDATE {}.central_login_operations SET encrypted_payload='\\x00'::bytea WHERE id=$1", f.schema), &[&id]).await.unwrap();
    store::atomic_write(&f.first.root.path().join("login-exit"), b"go").unwrap();
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    wait_parent_bound_exit(pid).await;
    // Let the independent supervisor attempt to read the damaged receipt.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGCONT) }, 0);
    timeout(Duration::from_secs(8), async {
        loop {
            let response = f
                .http
                .post(format!("{}/v1/relogin/status", f.second.url))
                .bearer_auth(&f.token)
                .json(&json!({"alias":"seat","id":id}))
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                let receipt: Value = response.json().await.unwrap();
                if receipt["status"] == "failed" {
                    break;
                }
            } else {
                assert_eq!(
                    response.status(),
                    503,
                    "receipt read failure stays explicit"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "an unreadable receipt cannot clear the unpublished grant's company-user fence"
    );
    assert_eq!(
        f.second.launches(),
        0,
        "unknown grant evidence forbids refresh"
    );
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_published_candidate_survives_local_login_home_loss() {
    let f = login_fixture().await;
    let id = "23".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"login-hold-after-save").unwrap();
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-saved").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    store::atomic_write(&f.first.root.path().join("login-exit"), b"go").unwrap();
    let published = timeout(Duration::from_secs(5), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            if receipt["status"] == "verifying" {
                break;
            }
            assert_ne!(receipt["status"], "failed", "{receipt}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    let home = std::fs::read_dir(f.first.root.path().join("state/shared-logins").join(&id))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    if published.is_ok() {
        // The grant is already durable in PostgreSQL. Lose only this fixture's
        // synthetic execution home before the parent reads that grant back.
        std::fs::remove_dir_all(home).unwrap();
    }
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGCONT) }, 0);
    published.expect("the independent supervisor must publish before parent recovery");
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(
                receipt["status"], "failed",
                "local home loss must retain the durable candidate: {receipt}"
            );
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.first.launches(),
        1,
        "verify the saved candidate exactly once"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_lost_polling_home_cannot_prove_grant_absence() {
    let mut f = login_fixture().await;
    let id = "14".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"login-hold-after-save").unwrap();
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("other-seat", Some("other-seat"), None)).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-saved").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let parent = f.first.child.id().unwrap();
    assert_eq!(unsafe { libc::kill(parent as i32, libc::SIGSTOP) }, 0);
    let home = std::fs::read_dir(f.first.root.path().join("state/shared-logins").join(&id))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // Delete only this fixture's synthetic home to model a lost execution volume.
    std::fs::remove_dir_all(home).unwrap();
    f.first.child.kill().await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {}.central_login_operations WHERE id=$1)",f.schema,f.schema), &[&id]).await.unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    login_request(&f, &f.second, "status", &id).await;
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "a deleted home is unknown evidence, not proof that no grant was saved"
    );
    assert_eq!(f.second.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_expired_device_login_receipt_allows_a_fresh_request() {
    let f = login_fixture().await;
    let id = "c1".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE user_id='test' AND id=$1",f.schema), &[&id]).await.unwrap();
    // This slice checks expiry admission after the lease watchdog stops polling.
    // Killing the replica and takeover remain in PR3.
    wait_dead(pid).await;
    let fresh = login_request(&f, &f.second, "start", &"c2".repeat(32)).await;
    assert_eq!(fresh["status"], "pending");
    assert_eq!(fresh["id"], "c2".repeat(32));
    let expired = login_request(&f, &f.second, "status", &id).await;
    assert_eq!(expired["status"], "expired");
    assert_eq!(
        login_request(&f, &f.second, "start", &id).await["status"],
        "expired"
    );
    login_request(&f, &f.second, "cancel", &"c2".repeat(32)).await;
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_login_authorization_outage_is_failed_instead_of_canceled() {
    let f = login_fixture().await;
    let id = "c3".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control
        .batch_execute(&format!(
            "ALTER TABLE {}.central_devices RENAME TO unavailable_devices",
            f.schema
        ))
        .await
        .unwrap();
    wait_dead(pid).await;
    f.control
        .batch_execute(&format!(
            "ALTER TABLE {}.unavailable_devices RENAME TO central_devices",
            f.schema
        ))
        .await
        .unwrap();
    let terminal = timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            if receipt["status"] != "pending" {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        terminal["status"], "failed",
        "a registry read failure is not a cancel request"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_expired_verification_keeps_its_reservation_and_request_id() {
    let f = login_fixture().await;
    let id = "c4".repeat(32);
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
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
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE user_id='test' AND id=$1",f.schema), &[&id]).await.unwrap();
    assert_eq!(
        login_request(&f, &f.second, "status", &id).await["status"],
        "verifying"
    );
    assert_eq!(
        login_request(&f, &f.second, "start", &"c5".repeat(32)).await["id"],
        id
    );
    assert!(!f.second.root.path().join("login-pid").exists());
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 503);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_legacy_lease_upgrade_requires_settled_owner_handoff_before_renewal() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    f.second.stop().await;
    // Seed the schema and clean-release row written by the pre-PR1 server.
    f.control
        .batch_execute(&format!(
            "ALTER TABLE {}.account_refresh_leases DROP COLUMN released",
            f.schema
        ))
        .await
        .unwrap();
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at) VALUES($1,'legacy-holder',41,clock_timestamp()-interval '1 second') ON CONFLICT(account_id) DO UPDATE SET holder_id='legacy-holder',epoch=41,expires_at=clock_timestamp()-interval '1 second'",f.schema), &[&account_key("test","seat")]).await.unwrap();
    let output = command(
        &f.database,
        f._seed.root.path().join("state").as_path(),
        &f.key,
        "migrate",
    )
    .output()
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "plain migration: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT released FROM {}.account_refresh_leases WHERE account_id=$1",
                f.schema
            ),
            &[&account_key("test", "seat")],
        )
        .await
        .unwrap();
    assert!(
        !row.get::<_, bool>(0),
        "expiry alone must not prove old owner settlement"
    );
    let output = command(
        &f.database,
        f._seed.root.path().join("state").as_path(),
        &f.key,
        "migrate",
    )
    .arg("--confirm-legacy-owners-settled")
    .output()
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "confirmed legacy handoff: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["legacyLeasesReleased"], 1);
    f.first = Pod::start(&f.database, &f.key).await;
    f.second = Pod::start(&f.database, &f.key).await;
    let id = "c6".repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("first renewal after the legacy handoff must complete");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_login_client_retires_expired_receipts_for_retry_and_cancel() {
    for cancel in [false, true] {
        let f = login_fixture().await;
        let id = "c7".repeat(32);
        login_request(&f, &f.first, "start", &id).await;
        let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
            .unwrap()
            .parse()
            .unwrap();
        f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
        wait_dead(pid).await;
        // The client keeps this ID after a lost response and retries on replica B.
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".codexctl/central");
        store::ensure_private_dir(&directory).unwrap();
        let token_file = directory.join(".device.token");
        store::atomic_write(&token_file, f.token.as_bytes()).unwrap();
        store::atomic_write(
            &directory.join(".server.json"),
            &serde_json::to_vec(&json!({
                "server":f.second.url,"token_file":token_file,"user_id":"test"
            }))
            .unwrap(),
        )
        .unwrap();
        let receipt = directory.join(format!(".login-{}.json", account_key_for_alias("seat")));
        store::atomic_write(&receipt,&serde_json::to_vec(&json!({
            "server":f.second.url,"userId":"test","alias":"seat","id":id,"kind":"renewal","label":null
        })).unwrap()).unwrap();
        let mut client = Command::new(env!("CARGO_BIN_EXE_codexctl"));
        client
            .args(["login", "seat", "--no-browser"])
            .env("HOME", home.path())
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS");
        if cancel {
            client.arg("--cancel");
        }
        let output = client.output().await.unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("server login expired"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Observe a fresh operation through the same CLI, not through its receipt file.
        let mut fresh = Command::new(env!("CARGO_BIN_EXE_codexctl"));
        fresh
            .args(["login", "seat", "--no-browser"])
            .env("HOME", home.path())
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut fresh = fresh.spawn().unwrap();
        let mut output = BufReader::new(fresh.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(8), async {
            while !line.contains("TEST-LOGIN") {
                assert!(output.read_line(&mut line).await.unwrap() > 0, "{line}");
            }
        })
        .await
        .expect("expired receipt must permit a new CLI login");
        let current = login_request(&f, &f.second, "status", "").await;
        assert_ne!(current["id"], id);
        store::atomic_write(
            &f.second.root.path().join("login-release"),
            &serde_json::to_vec(&renewal_grant()).unwrap(),
        )
        .unwrap();
        let completed = timeout(Duration::from_secs(8), fresh.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            completed.status.success(),
            "{}",
            String::from_utf8_lossy(&completed.stderr)
        );
        stop_fixture(f).await;
    }
}

fn account_key_for_alias(alias: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(alias.as_bytes()))
}

#[tokio::test]
async fn postgres_polling_deadline_does_not_abort_lease_wait_or_verification() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false) ON CONFLICT(account_id) DO UPDATE SET holder_id='foreign-settling',epoch=41,expires_at=clock_timestamp()-interval '1 second',released=false",f.schema), &[&account_key("test","seat")]).await.unwrap();
    let id = "ca".repeat(32);
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
    f.control.execute(&format!("UPDATE {}.central_login_operations SET deadline=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema), &[&id]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        login_request(&f, &f.second, "status", &id).await["status"],
        "verifying",
        "a published candidate must retain authority past the polling deadline"
    );
    f.control.execute(&format!("UPDATE {}.account_refresh_leases SET released=true,expires_at=clock_timestamp()-interval '1 second' WHERE account_id=$1",f.schema), &[&account_key("test","seat")]).await.unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        login_request(&f, &f.second, "status", &id).await["status"],
        "verifying",
        "the polling deadline must not interrupt a running verifier"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let receipt = login_request(&f, &f.second, "status", &id).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("late polling approval must finish verification");
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_shares_pending_receipt_and_cancel_across_replicas() {
    let f = login_fixture().await;
    let operation = json!({"alias":"new-account","id":"a".repeat(64),"label":"New seat"});
    let response = f
        .http
        .post(format!("{}/v1/accounts/login/start", f.first.url))
        .bearer_auth(&f.token)
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "PostgreSQL must support add login");
    let started: Value = response.json().await.unwrap();
    assert_eq!(started["status"], "pending");
    let adopted: Value = f
        .http
        .post(format!("{}/v1/accounts/login/start", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"new-account","id":"b".repeat(64)}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(adopted["id"], started["id"]);
    for action in ["status", "cancel"] {
        let response = f
            .http
            .post(format!("{}/v1/accounts/login/{action}", f.second.url))
            .bearer_auth(&f.token)
            .json(&operation)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["userCode"],
            "TEST-LOGIN"
        );
    }
    timeout(Duration::from_secs(8), async {
        loop {
            let status: Value = f
                .http
                .post(format!("{}/v1/accounts/login/status", f.second.url))
                .bearer_auth(&f.token)
                .json(&operation)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["status"] == "canceled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cross-replica cancellation deadline");
    assert!(!f.second.root.path().join("login-pid").exists());
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

fn add_grant(workspace: &str, subject: Option<&str>, uid: Option<&str>) -> Value {
    let mut claims = json!({"iat":2000000000_u64,"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":workspace,"chatgpt_plan_type":"pro"}});
    if let Some(subject) = subject {
        claims["sub"] = json!(subject);
    }
    if let Some(uid) = uid {
        claims["https://api.openai.com/auth"]["chatgpt_user_id"] = json!(uid);
    }
    json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(claims.to_string())),
        "refresh_token":"synthetic-add-refresh","account_id":workspace}})
}
async fn add_request(f: &LoginFixture, pod: &Pod, action: &str, operation: &Value) -> Value {
    let response = f
        .http
        .post(format!("{}/v1/accounts/login/{action}", pod.url))
        .bearer_auth(&f.token)
        .json(operation)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let receipt: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{action}: {receipt}");
    receipt
}
async fn complete_add(f: &LoginFixture, pod: &Pod, operation: &Value, grant: &Value) -> Value {
    store::atomic_write(
        &pod.root.path().join("login-release"),
        &serde_json::to_vec(grant).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = add_request(f, &f.second, "status", operation).await;
            assert_ne!(receipt["status"], "failed", "{receipt}");
            if receipt["status"] == "completed" {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("add must finish")
}
#[tokio::test]
async fn postgres_add_creates_verified_account_and_replays_completed_receipt_on_another_replica() {
    let f = login_fixture().await;
    let operation = json!({"alias":"new-account","id":"d0".repeat(32),"label":"New seat"});
    assert_eq!(
        add_request(&f, &f.first, "start", &operation).await["status"],
        "pending"
    );
    complete_add(
        &f,
        &f.first,
        &operation,
        &add_grant("new-workspace", Some("new-login"), None),
    )
    .await;
    let launches = f.first.launches() + f.second.launches();
    let receipt = add_request(&f, &f.second, "start", &operation).await;
    assert_eq!(receipt["status"], "completed");
    assert!(
        !f.second.root.path().join("login-pid").exists(),
        "receipt retry must not run another login"
    );
    assert_eq!(f.first.launches() + f.second.launches(), launches);
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"new-account"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "new account must be verified and usable"
    );
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = catalog.as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|a| a["alias"] == "new-account").count(),
        1
    );
    assert_eq!(
        rows.iter().find(|a| a["alias"] == "new-account").unwrap()["label"],
        "New seat"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_uses_typed_proof_without_decrypting_unrelated_workspace_accounts() {
    let f = login_fixture().await;
    f.control.batch_execute(&format!(r#"
        INSERT INTO {0}.central_accounts(account_id,user_id,alias,workspace,login,encrypted_vault,revision)
        SELECT 'unrelated-'||i,'test','unrelated-'||i,'proof-workspace','unrelated-sub-'||i,'\x00',1
        FROM generate_series(1,128) i;
        INSERT INTO {0}.central_account_identity_claims(workspace,namespace,claim,account_id)
        SELECT 'proof-workspace','sub','unrelated-sub-'||i,'unrelated-'||i FROM generate_series(1,128) i;
        INSERT INTO {0}.central_account_identity_claims(workspace,namespace,claim,account_id)
        SELECT 'proof-workspace','uid','unrelated-uid-'||i,'unrelated-'||i FROM generate_series(1,128) i;
    "#, f.schema)).await.unwrap();
    let operation = json!({"alias":"proved-new-seat","id":"f0".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    let receipt = complete_add(
        &f,
        &f.first,
        &operation,
        &add_grant(
            "proof-workspace",
            Some("arriving-sub"),
            Some("arriving-uid"),
        ),
    )
    .await;
    assert_eq!(receipt["status"], "completed");
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"proved-new-seat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "unrelated ciphertext does not block a proved different account"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_lands_on_existing_alias_and_preserves_its_label() {
    let f = login_fixture().await;
    let operation = json!({"alias":"typo","id":"d1".repeat(32),"label":"Replacement label"});
    add_request(&f, &f.first, "start", &operation).await;
    let receipt = complete_add(&f, &f.first, &operation, &renewal_grant()).await;
    assert_eq!(receipt["landedAlias"], "seat");
    assert_eq!(
        add_request(&f, &f.second, "start", &operation).await["landedAlias"],
        "seat"
    );
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = catalog.as_array().unwrap();
    assert_eq!(rows.len(), 2, "landing must not add a duplicate account");
    assert_eq!(
        rows.iter().find(|a| a["alias"] == "seat").unwrap()["label"],
        "Original seat"
    );
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    stop_fixture(f).await;
}

async fn wait_add_terminal(f: &LoginFixture, operation: &Value) -> Value {
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = add_request(f, &f.second, "status", operation).await;
            if ["completed", "failed", "canceled", "expired"]
                .iter()
                .any(|s| receipt["status"] == *s)
            {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("add must settle or report failure")
}
#[tokio::test]
async fn postgres_add_refuses_an_account_owned_by_another_company_user_before_verification() {
    let f = login_fixture_with_accounts(false, true).await;
    let operation = json!({"alias":"foreign-copy","id":"d2".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&foreign_grant()).unwrap(),
    )
    .unwrap();
    let receipt = wait_add_terminal(&f, &operation).await;
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["error"], "account_already_owned");
    assert_eq!(
        f.first.launches(),
        0,
        "ownership refusal precedes any refresh child"
    );
    let foreign_token =
        std::fs::read_to_string(f._seed.root.path().join("foreign-machine-token")).unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&foreign_token)
        .json(&json!({"alias":"foreign-seat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "rightful owner's credentials remain usable"
    );
    // Polling an already failed receipt must not count another failed attempt.
    add_request(&f, &f.second, "status", &operation).await;
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"relogin_failed\"} 1"),
        "count the refused admission once: {metrics}"
    );
    stop_fixture(f).await;
}

async fn assert_in_flight_add_refusal(second_grant: Value) {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let first = json!({"alias":"new-one","id":"d3".repeat(32)});
    let second = json!({"alias":"new-two","id":"d4".repeat(32)});
    let grant = add_grant("new-workspace", Some("new-login"), Some("new-uid"));
    add_request(&f, &f.first, "start", &first).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    add_request(&f, &f.second, "start", &second).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&second_grant).unwrap(),
    )
    .unwrap();
    let refused = wait_add_terminal(&f, &second).await;
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["error"], "relogin_reserved");
    assert_eq!(
        f.second.launches(),
        0,
        "matching in-flight add must not start a second verifier"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(wait_add_terminal(&f, &first).await["status"], "completed");
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 3, "one new seat only");
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_second_login_refuses_an_in_flight_add_identity_reservation() {
    assert_in_flight_add_refusal(add_grant(
        "new-workspace",
        Some("new-login"),
        Some("new-uid"),
    ))
    .await;
}
#[tokio::test]
async fn postgres_identity_reservation_matches_uid_when_the_second_grant_has_no_subject() {
    assert_in_flight_add_refusal(add_grant("new-workspace", None, Some("new-uid"))).await;
}

#[tokio::test]
async fn postgres_completed_add_releases_identity_reservation_for_a_later_landing() {
    let f = login_fixture().await;
    let first = json!({"alias":"new-one","id":"d5".repeat(32),"label":"First label"});
    let second = json!({"alias":"new-two","id":"d6".repeat(32),"label":"Ignored label"});
    let grant = add_grant("new-workspace", Some("new-login"), Some("new-uid"));
    add_request(&f, &f.first, "start", &first).await;
    complete_add(&f, &f.first, &first, &grant).await;
    add_request(&f, &f.second, "start", &second).await;
    let receipt = complete_add(&f, &f.second, &second, &grant).await;
    assert_eq!(receipt["landedAlias"], "new-one");
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = catalog.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().find(|a| a["alias"] == "new-one").unwrap()["label"],
        "First label"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_landing_preserves_all_namespaced_claims_across_token_transitions() {
    let f = login_fixture().await;
    let grants = [
        add_grant("new-workspace", Some("stable-subject"), Some("stable-uid")),
        add_grant("new-workspace", None, Some("stable-uid")),
        add_grant("new-workspace", Some("stable-subject"), None),
    ];
    for (i, grant) in grants.iter().enumerate() {
        let pod = if i == 1 { &f.second } else { &f.first };
        let gate = pod.root.path().join("login-release");
        if gate.exists() {
            std::fs::remove_file(&gate).unwrap();
        }
        let operation = json!({"alias":format!("claim-{i}"),"id":format!("e{i}").repeat(32)});
        add_request(&f, pod, "start", &operation).await;
        let receipt = complete_add(&f, pod, &operation, grant).await;
        if i > 0 {
            assert_eq!(receipt["landedAlias"], "claim-0");
        }
    }
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        catalog.as_array().unwrap().len(),
        3,
        "claim transitions must keep one account"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_import_refuses_a_matching_in_flight_add_before_creating_a_second_account() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let operation = json!({"alias":"new-one","id":"e3".repeat(32)});
    let grant = add_grant("new-workspace", Some("new-login"), Some("new-uid"));
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let response = timeout(
        Duration::from_secs(6),
        f.http
            .post(format!("{}/v1/accounts", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"import-copy","auth":grant}))
            .send(),
    )
    .await
    .expect("reserved import must refuse before waiting on a lease")
    .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "relogin_reserved");
    assert_eq!(f.second.launches(), 0);
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        catalog.as_array().unwrap().len(),
        3,
        "refused import must not leave another account"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_distinct_uid_only_logins_in_one_workspace_do_not_share_a_reservation() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let first = json!({"alias":"uid-one","id":"e4".repeat(32)});
    let second = json!({"alias":"uid-two","id":"e5".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("shared-workspace", None, Some("uid-one"))).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("shared-workspace", None, Some("uid-two")),
    )
    .await;
    assert_eq!(
        add_request(&f, &f.second, "status", &first).await["status"],
        "verifying"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(wait_add_terminal(&f, &first).await["status"], "completed");
    for alias in ["uid-one", "uid-two"] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{alias} must stay usable");
    }
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_login_adoption_keeps_the_add_and_renewal_receipts_separate() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let operation = json!({"alias":"new-one","id":"e6".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("new-workspace", Some("new-login"), None)).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let renewal = f
        .http
        .post(format!("{}/v1/relogin/start", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"new-one","id":"e7".repeat(32)}))
        .send()
        .await
        .unwrap();
    let status = renewal.status();
    let body: Value = renewal.json().await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "relogin_reserved");
    let adopted = add_request(
        &f,
        &f.second,
        "start",
        &json!({"alias":"new-one","id":"e8".repeat(32)}),
    )
    .await;
    assert_eq!(
        adopted["id"], operation["id"],
        "same-device add retries retain the original operation after account creation"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_equal_text_in_different_login_namespaces_is_unresolved_and_keeps_the_grant_fenced()
 {
    let f = login_fixture().await;
    let first = json!({"alias":"uid-owner","id":"e9".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", None, Some("same-text")),
    )
    .await;
    let second = json!({"alias":"subject-copy","id":"ea".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("new-workspace", Some("same-text"), None)).unwrap(),
    )
    .unwrap();
    let receipt = wait_add_terminal(&f, &second).await;
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["error"], "account_identity_unresolved");
    for _ in 0..3 {
        assert_eq!(
            add_request(&f, &f.second, "status", &second).await,
            receipt,
            "receipt reads preserve the admission failure"
        );
    }
    let token = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"uid-owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        token.status(),
        503,
        "an incomparable issued grant remains quarantined"
    );
    let metrics = f
        .http
        .get(format!("{}/metrics", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(
            "codexctl_central_failed_requests_total{reason=\"relogin_identity_unresolved\"} 1"
        ),
        "count the failed admission once: {metrics}"
    );
    assert!(
        metrics
            .lines()
            .filter(|line| line
                .starts_with("codexctl_central_failed_requests_total{reason=\"relogin_failed\"}"))
            .all(|line| line.ends_with(" 0")),
        "receipt recovery must not count the admission failure again: {metrics}"
    );
    assert_eq!(
        f.second.launches(),
        0,
        "incomparable claims cannot start verification"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_completion_keeps_claims_learned_during_verification() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"gain-uid").unwrap();
    for (i, grant) in [
        add_grant("new-workspace", Some("stable-subject"), None),
        add_grant("new-workspace", Some("stable-subject"), None),
        add_grant("new-workspace", None, Some("learned-uid")),
    ]
    .iter()
    .enumerate()
    {
        let pod = if i == 0 { &f.first } else { &f.second };
        let gate = pod.root.path().join("login-release");
        if gate.exists() {
            std::fs::remove_file(&gate).unwrap();
        }
        let operation = json!({"alias":format!("learned-{i}"),"id":format!("f{i}").repeat(32)});
        add_request(&f, pod, "start", &operation).await;
        let receipt = complete_add(&f, pod, &operation, grant).await;
        if i > 0 {
            assert_eq!(receipt["landedAlias"], "learned-0");
        }
    }
    stop_fixture(f).await;
}

async fn wait_renewal_terminal(f: &LoginFixture, alias: &str, id: &str) -> Value {
    timeout(Duration::from_secs(12), async {
        loop {
            let receipt = login_alias_request(f, &f.second, alias, "status", id).await;
            if ["completed", "failed", "canceled", "expired"]
                .iter()
                .any(|s| receipt["status"] == *s)
            {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renewal must settle or report failure")
}
#[tokio::test]
async fn postgres_repair_releases_a_wrong_grant_by_uid_without_releasing_its_selected_account() {
    let f = login_fixture().await;
    let grant = add_grant("new-workspace", None, Some("uid-owner"));
    let add = json!({"alias":"uid-owner","id":"f3".repeat(32)});
    add_request(&f, &f.first, "start", &add).await;
    complete_add(&f, &f.first, &add, &grant).await;
    std::fs::remove_file(f.first.root.path().join("login-release")).unwrap();
    let wrong = "f4".repeat(32);
    login_request(&f, &f.first, "start", &wrong).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    assert_eq!(
        wait_renewal_terminal(&f, "seat", &wrong).await["error"],
        "wrong_account"
    );
    let token = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"uid-owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        token.status(),
        503,
        "wrong grant must quarantine the matching UID owner"
    );
    let repair = "f5".repeat(32);
    login_alias_request(&f, &f.second, "uid-owner", "start", &repair).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    assert_eq!(
        wait_renewal_terminal(&f, "uid-owner", &repair).await["status"],
        "completed"
    );
    let token = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"uid-owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        token.status(),
        200,
        "verified UID agreement must release the matching quarantine"
    );
    assert_eq!(
        request(&f.http, &f.second, &f.token).await.status(),
        503,
        "the original selected seat stays reserved"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_refuses_identity_claimed_by_a_renewal_before_the_uid_is_promoted() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    let renewal = "f6".repeat(32);
    login_request(&f, &f.first, "start", &renewal).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "synthetic-seat",
            Some("synthetic-seat"),
            Some("renewal-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let operation = json!({"alias":"renewal-copy","id":"f7".repeat(32)});
    add_request(&f, &f.second, "start", &operation).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("synthetic-seat", None, Some("renewal-uid"))).unwrap(),
    )
    .unwrap();
    let receipt = wait_add_terminal(&f, &operation).await;
    assert_eq!(receipt["error"], "relogin_reserved");
    assert_eq!(f.second.launches(), 0);
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_renewal_terminal(&f, "seat", &renewal).await["status"],
        "completed"
    );
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        catalog.as_array().unwrap().len(),
        2,
        "a renewal's new UID cannot create another profile"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_rechecks_machine_authorization_after_admission_waits() {
    let f = login_fixture().await;
    let token_file = f._seed.root.path().join("observer-token");
    let registered = command(
        &f.database,
        &f._seed.root.path().join("state"),
        &f.key,
        "register",
    )
    .args([
        "--device",
        "observer",
        "--tenant",
        "sawmills",
        "--user",
        "test",
        "--token-file",
    ])
    .arg(&token_file)
    .output()
    .await
    .unwrap();
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let observer = std::fs::read_to_string(token_file).unwrap();
    let operation = json!({"alias":"revoked-add","id":"f8".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control
        .query_one(
            "SELECT pg_advisory_lock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("new-workspace", Some("new-login"), None)).unwrap(),
    )
    .unwrap();
    wait_dead(pid).await;
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_devices SET revoked=true WHERE id='test-machine'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    f.control
        .query_one(
            "SELECT pg_advisory_unlock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let response = f
                .http
                .post(format!("{}/v1/accounts/login/status", f.second.url))
                .bearer_auth(&observer)
                .json(&json!({"alias":"revoked-add","id":""}))
                .send()
                .await
                .unwrap();
            if response.status() == 404 {
                break;
            }
            assert_eq!(
                response.status(),
                409,
                "the original device still owns the active operation"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&observer)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        catalog.as_array().unwrap().len(),
        2,
        "revoked authorization cannot create an account after a lock wait"
    );
    assert_eq!(f.first.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_concurrent_add_claims_create_one_account_and_one_verifier() {
    let f = login_fixture().await;
    for pod in [&f.first, &f.second] {
        store::atomic_write(&pod.root.path().join("mode"), b"add-startup-hold").unwrap();
    }
    let first = json!({"alias":"race-one","id":"fa".repeat(32)});
    let second = json!({"alias":"race-two","id":"fb".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    add_request(&f, &f.second, "start", &second).await;
    let grant = serde_json::to_vec(&add_grant(
        "race-workspace",
        Some("race-login"),
        Some("race-uid"),
    ))
    .unwrap();
    for pod in [&f.first, &f.second] {
        store::atomic_write(&pod.root.path().join("login-release"), &grant).unwrap();
    }
    let winner = timeout(Duration::from_secs(8), async {
        loop {
            let a = add_request(&f, &f.second, "status", &first).await;
            let b = add_request(&f, &f.second, "status", &second).await;
            if f.first.launches() + f.second.launches() == 1
                && (a["status"] == "failed" || b["status"] == "failed")
            {
                let (held, refused) = if a["status"] == "failed" {
                    (&second, a)
                } else {
                    (&first, b)
                };
                assert_eq!(refused["error"], "relogin_reserved");
                return held.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("one claim holder and one refused duplicate");
    for pod in [&f.first, &f.second] {
        store::atomic_write(&pod.root.path().join("release-initialize"), b"ready").unwrap();
    }
    assert_eq!(wait_add_terminal(&f, &winner).await["status"], "completed");
    assert_eq!(f.first.launches() + f.second.launches(), 1);
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 3);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_cli_resumes_on_another_replica_without_a_second_device_login() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let f = login_fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let route_second = Arc::new(AtomicBool::new(false));
    let route = route_second.clone();
    let first_address = f.first.url.trim_start_matches("http://").to_owned();
    let second_address = f.second.url.trim_start_matches("http://").to_owned();
    let proxy = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut incoming, _) = listener.accept().await.unwrap();
            let target = if route.load(Ordering::Acquire) {
                second_address.clone()
            } else {
                first_address.clone()
            };
            connections.spawn(async move {
                let mut outgoing = tokio::net::TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
            });
        }
    });
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".codexctl/central");
    store::ensure_private_dir(&directory).unwrap();
    let token_file = directory.join(".device.token");
    store::atomic_write(&token_file, f.token.as_bytes()).unwrap();
    store::atomic_write(
        &directory.join(".server.json"),
        &serde_json::to_vec(
            &json!({"server":format!("http://{address}"),"token_file":token_file,"user_id":"test"}),
        )
        .unwrap(),
    )
    .unwrap();
    let mut cli = Command::new(env!("CARGO_BIN_EXE_codexctl"));
    cli.args(["login", "cli-new", "--no-browser", "--label", "CLI label"])
        .env("HOME", home.path())
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut interrupted = cli.spawn().unwrap();
    let mut output = BufReader::new(interrupted.stdout.take().unwrap());
    let mut line = String::new();
    timeout(Duration::from_secs(10), async {
        while !line.contains("TEST-LOGIN") {
            assert!(output.read_line(&mut line).await.unwrap() > 0, "{line}");
        }
    })
    .await
    .unwrap();
    interrupted.kill().await.unwrap();
    interrupted.wait().await.unwrap();
    route_second.store(true, Ordering::Release);
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("cli-workspace", Some("cli-login"), None)).unwrap(),
    )
    .unwrap();
    let resumed = timeout(Duration::from_secs(12), cli.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(
        !f.second.root.path().join("login-pid").exists(),
        "CLI receipt retry must not start a second device login"
    );
    let catalog: Value = f
        .http
        .get(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = catalog.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().find(|a| a["alias"] == "cli-new").unwrap()["label"],
        "CLI label"
    );
    proxy.abort();
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_accepts_retained_claims_after_a_uid_only_add_landing() {
    let f = login_fixture().await;
    let both = add_grant("new-workspace", Some("retained-sub"), Some("retained-uid"));
    let first = json!({"alias":"retained","id":"fc".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(&f, &f.first, &first, &both).await;
    let second = json!({"alias":"copy","id":"fd".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    std::fs::remove_file(f.first.root.path().join("login-release")).unwrap();
    let id = "fe".repeat(32);
    login_alias_request(&f, &f.first, "retained", "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&both).unwrap(),
    )
    .unwrap();
    assert_eq!(
        wait_renewal_terminal(&f, "retained", &id).await["status"],
        "completed"
    );
    stop_fixture(f).await;
}
#[tokio::test]
async fn postgres_add_uses_retained_subjects_to_distinguish_logins_during_launch() {
    let f = login_fixture().await;
    let first = json!({"alias":"retained","id":"a0".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    let second = json!({"alias":"copy","id":"a1".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    std::fs::remove_file(f.second.root.path().join("login-release")).unwrap();
    let other = json!({"alias":"different","id":"a2".repeat(32)});
    add_request(&f, &f.second, "start", &other).await;
    complete_add(
        &f,
        &f.second,
        &other,
        &add_grant("new-workspace", Some("different-sub"), None),
    )
    .await;
    for alias in ["retained", "different"] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{alias}");
    }
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_preserves_file_rename_tombstones_across_backfill_and_replicas() {
    let f = login_fixture_with_rename(false, false, true).await;
    let response = f
        .http
        .post(format!("{}/v1/accounts/login/start", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","id":"a3".repeat(32)}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        409,
        "a renamed alias must never identify another account"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "alias_renamed"
    );
    let imported = f
        .http
        .post(format!("{}/v1/accounts", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","auth":add_grant("fresh-workspace",Some("fresh-login"),None)}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        imported.status(),
        409,
        "import must preserve the same tombstone"
    );
    assert_eq!(
        imported.json::<Value>().await.unwrap()["error"],
        "alias_renamed"
    );
    assert!(!f.second.root.path().join("login-pid").exists());
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_renewal_reports_reservation_for_an_add_landed_from_another_alias() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"startup-hold").unwrap();
    let operation = json!({"alias":"typo","id":"a4".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
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
    let response = f
        .http
        .post(format!("{}/v1/relogin/start", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","id":"a5".repeat(32)}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "relogin_reserved"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    stop_fixture(f).await;
}
#[tokio::test]
async fn postgres_import_refuses_claims_incomparable_with_an_existing_account() {
    let f = login_fixture().await;
    let operation = json!({"alias":"uid-owner","id":"a6".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    complete_add(
        &f,
        &f.first,
        &operation,
        &add_grant("new-workspace", None, Some("same-text")),
    )
    .await;
    let response=f.http.post(format!("{}/v1/accounts",f.second.url)).bearer_auth(&f.token)
        .json(&json!({"alias":"different-namespace","auth":add_grant("new-workspace",Some("same-text"),None)})).send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "account_identity_unresolved");
    assert_eq!(f.second.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_identity_read_failure_before_refresh_is_retryable() {
    identity_database_failure("%LEFT JOIN central_account_identity_claims%", "a7").await;
}

#[tokio::test]
async fn postgres_candidate_validation_database_failure_is_not_wrong_account() {
    identity_database_failure(
        "%SELECT namespace,claim FROM central_account_identity_claims%",
        "a8",
    )
    .await;
}

async fn identity_database_failure(query_pattern: &str, request_prefix: &str) {
    let f = login_fixture().await;
    // Fail one database read boundary while receipts remain available. This view
    // is fault injection only; assertions use the public HTTP operation status.
    f.control
        .batch_execute(&format!(
            r#"
        ALTER TABLE {0}.central_account_identity_claims RENAME TO retained_claims;
        CREATE FUNCTION {0}.unavailable_identity(value text) RETURNS text
        LANGUAGE plpgsql VOLATILE AS $$
        BEGIN
            IF current_query() LIKE '{1}' THEN
                RAISE EXCEPTION 'synthetic identity read unavailable';
            END IF;
            RETURN value;
        END $$;
        CREATE VIEW {0}.central_account_identity_claims AS SELECT account_id, workspace, deleted_at,
            namespace, {0}.unavailable_identity(claim) AS claim FROM {0}.retained_claims;
    "#,
            f.schema, query_pattern
        ))
        .await
        .unwrap();
    let id = request_prefix.repeat(32);
    login_request(&f, &f.first, "start", &id).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&renewal_grant()).unwrap(),
    )
    .unwrap();
    let terminal = timeout(Duration::from_secs(8), async {
        loop {
            let status = login_request(&f, &f.second, "status", &id).await;
            if status["status"] == "failed" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        terminal["error"], "relogin_interrupted_retry",
        "a database failure before provider contact is neither a wrong account nor unresolved verification"
    );
    assert_eq!(f.first.launches(), 0);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_foreign_account_keeps_serving_while_add_admission_waits() {
    let f = login_fixture_with_accounts(false, true).await;
    // Queue a control session behind candidate publication's admission lock,
    // then hold that lock so admission cannot reject the foreign grant yet.
    f.control
        .batch_execute(&format!(
            r#"
        CREATE SEQUENCE {0}.candidate_barrier;
        CREATE FUNCTION {0}.pause_candidate() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.phase='candidate' AND OLD.phase<>'candidate' THEN
                PERFORM nextval('{0}.candidate_barrier');
                PERFORM pg_sleep(0.5);
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER pause_candidate AFTER UPDATE ON {0}.central_login_operations
        FOR EACH ROW EXECUTE FUNCTION {0}.pause_candidate();
    "#,
            f.schema
        ))
        .await
        .unwrap();
    let operation = json!({"alias":"foreign-copy","id":"a9".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&foreign_grant()).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let called: bool = f
                .control
                .query_one(
                    &format!("SELECT is_called FROM {}.candidate_barrier", f.schema),
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if called {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.control
        .query_one(
            "SELECT pg_advisory_lock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    assert_eq!(
        add_request(&f, &f.second, "status", &operation).await["status"],
        "verifying"
    );
    let foreign_token =
        std::fs::read_to_string(f._seed.root.path().join("foreign-machine-token")).unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&foreign_token)
        .json(&json!({"alias":"foreign-seat"}))
        .send();
    tokio::pin!(response);
    // Once the foreign refresh lease is admitted, its fenced credential write
    // also needs the lock. Release it then, before either bounded DB call expires.
    let early = tokio::select! {
        result = &mut response => Some(result.unwrap()),
        _ = timeout(Duration::from_secs(1), async {
            loop {
                let waiting: i64 = f.control.query_one(
                    "SELECT count(*) FROM pg_locks w JOIN pg_locks h ON w.locktype=h.locktype AND w.database=h.database AND w.classid=h.classid AND w.objid=h.objid AND w.objsubid=h.objsubid WHERE h.pid=pg_backend_pid() AND h.granted AND h.locktype='advisory' AND NOT w.granted", &[]).await.unwrap().get(0);
                if waiting >= 2 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }) => None,
    };
    f.control
        .query_one(
            "SELECT pg_advisory_unlock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    let status = match early {
        Some(response) => response.status(),
        None => response.await.unwrap().status(),
    };
    assert_eq!(
        status, 200,
        "an unadmitted add must not fence another company user's account"
    );
    assert_eq!(
        wait_add_terminal(&f, &operation).await["error"],
        "account_already_owned"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_restart_reconciles_a_completed_uid_only_landing() {
    let mut f = login_fixture().await;
    let first = json!({"alias":"retained","id":"aa".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    let second = json!({"alias":"copy","id":"ab".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    f.first.stop().await;
    f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"retained"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "a completed same-account landing must retire the stopped replica's old journal"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_import_uses_retained_subjects_beside_a_uid_only_account() {
    let f = login_fixture().await;
    let first = json!({"alias":"retained","id":"ac".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    let second = json!({"alias":"copy","id":"ad".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    let response = f.http.post(format!("{}/v1/accounts", f.second.url)).bearer_auth(&f.token)
        .json(&json!({"alias":"different","auth":add_grant("new-workspace",Some("different-sub"),None)}))
        .send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        status, 200,
        "retained subjects prove the imported login is distinct: {body}"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_background_recovery_uses_retained_subjects_after_a_uid_only_landing() {
    let mut f = login_fixture().await;
    let first = json!({"alias":"retained","id":"ae".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    let second = json!({"alias":"copy","id":"af".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    std::fs::remove_file(f.second.root.path().join("login-release")).unwrap();
    let other = json!({"alias":"different","id":"ba".repeat(32)});
    add_request(&f, &f.second, "start", &other).await;
    complete_add(
        &f,
        &f.second,
        &other,
        &add_grant("new-workspace", Some("different-sub"), None),
    )
    .await;
    f.second.stop().await;
    store::atomic_write(&f.second.root.path().join("retry-clock"), b"100000").unwrap();
    f.second = Pod::spawn_with_recovery(&f.database, &f.key, f.second.root, "postgres", true).await;
    store::atomic_write(&f.second.root.path().join("mode"), b"billing-error-marked").unwrap();
    let failed = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"retained","billing":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 503);
    store::atomic_write(&f.second.root.path().join("mode"), b"").unwrap();
    store::atomic_write(&f.second.root.path().join("retry-clock"), b"1000000").unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let accounts: Value = f
                .http
                .get(format!("{}/v1/accounts", f.second.url))
                .bearer_auth(&f.token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if accounts
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["alias"] == "retained")
                .unwrap()["available"]
                == true
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("background recovery must distinguish the retained subject from the other account");
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_restart_keeps_a_conflicting_journal_and_serves_unrelated_accounts() {
    let mut f = login_fixture().await;
    let first = json!({"alias":"retained","id":"bb".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    f.first.stop().await;
    let journal = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "retained"))
        .join("runtime/auth.json");
    let conflict = add_grant("unrelated-journal", Some("conflicting-login"), None);
    store::atomic_write(&journal, &serde_json::to_vec(&conflict).unwrap()).unwrap();
    let second = json!({"alias":"copy","id":"bc".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    complete_add(
        &f,
        &f.second,
        &second,
        &add_grant("new-workspace", None, Some("retained-uid")),
    )
    .await;
    f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"other"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "a conflicting account journal must not stop unrelated token delivery"
    );
    let accounts: Value = f
        .http
        .get(format!("{}/v1/accounts", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        accounts
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["alias"] == "retained")
            .unwrap()["available"],
        false,
        "the account with a conflicting journal must stay fenced"
    );
    let denied = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"retained"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        503,
        "a token request must not discard conflicting journal evidence"
    );
    let retained: Value = serde_json::from_slice(&std::fs::read(journal).unwrap()).unwrap();
    assert_eq!(
        retained, conflict,
        "unproven journal ownership must preserve credential evidence"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_restart_isolates_unreadable_shared_revision_evidence() {
    let mut f = login_fixture().await;
    let first = json!({"alias":"retained","id":"bd".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    f.first.stop().await;
    let marker = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "retained"))
        .join("shared-revision.json");
    store::atomic_write(&marker, b"unreadable-revision-evidence").unwrap();
    f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    for (alias, expected) in [("other", 200), ("retained", 503)] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{alias}");
    }
    assert_eq!(
        std::fs::read(marker).unwrap(),
        b"unreadable-revision-evidence",
        "a token request must preserve the rejected revision evidence"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_restart_preserves_a_conflicting_newer_local_vault_after_landing() {
    conflicting_local_vault(false, false, true).await;
}

#[tokio::test]
async fn postgres_restart_quarantines_agreeing_local_credentials_that_conflict_with_shared_identity()
 {
    conflicting_local_vault(true, false, true).await;
}

#[tokio::test]
async fn postgres_renewal_preserves_local_credentials_with_a_proven_identity_conflict() {
    conflicting_local_vault(true, true, true).await;
}

#[tokio::test]
async fn postgres_refresh_hydration_preserves_conflicting_local_credentials() {
    conflicting_local_vault(true, false, false).await;
}

async fn conflicting_local_vault(
    matching_journal: bool,
    attempt_renewal: bool,
    shared_landing: bool,
) {
    use aes_gcm::{
        Aes256Gcm,
        aead::{Aead, KeyInit},
    };
    let mut f = login_fixture().await;
    let first = json!({"alias":"retained","id":"be".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(
        &f,
        &f.first,
        &first,
        &add_grant("new-workspace", Some("retained-sub"), Some("retained-uid")),
    )
    .await;
    if !shared_landing {
        // A public token read records knowledge of the completed receipt before
        // another replica advances credentials through an ordinary refresh.
        let known = f
            .http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"retained"}))
            .send()
            .await
            .unwrap();
        assert_eq!(known.status(), 200);
    }
    f.first.stop().await;
    let second = json!({"alias":"copy","id":"bf".repeat(32)});
    let arriving = add_grant("new-workspace", None, Some("retained-uid"));
    if shared_landing {
        add_request(&f, &f.second, "start", &second).await;
        complete_add(&f, &f.second, &second, &arriving).await;
    } else {
        let refreshed = f
            .http
            .post(format!("{}/v1/token", f.second.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":"retained","billing":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(refreshed.status(), 200);
    }
    let account = f
        .first
        .root
        .path()
        .join("state/accounts")
        .join(account_key("test", "retained"));
    let path = account.join("vault.enc");
    let original = std::fs::read(&path).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&std::fs::read(&f.key).unwrap()).unwrap();
    let plain = cipher
        .decrypt((&original[..12]).into(), &original[12..])
        .unwrap();
    let mut local: Value = serde_json::from_slice(&plain).unwrap();
    local["auth"] = add_grant("unrelated-local-vault", Some("conflicting-login"), None);
    local["revision"] = json!(if shared_landing { 100 } else { 1 });
    // One synthetic corruption with a fresh fixture key. Preserve its bytes as evidence.
    let nonce = [124_u8; 12];
    let encrypted = cipher
        .encrypt((&nonce).into(), &serde_json::to_vec(&local).unwrap()[..])
        .unwrap();
    let mut evidence = nonce.to_vec();
    evidence.extend(encrypted);
    store::atomic_write(&path, &evidence).unwrap();
    let journal = account.join("runtime/auth.json");
    let journal_evidence = serde_json::to_vec(if matching_journal {
        &local["auth"]
    } else {
        &arriving
    })
    .unwrap();
    store::atomic_write(&journal, &journal_evidence).unwrap();
    f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    for (alias, expected) in [("other", 200), ("retained", 503)] {
        let response = f
            .http
            .post(format!("{}/v1/token", f.first.url))
            .bearer_auth(&f.token)
            .json(&json!({"alias":alias}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{alias}");
    }
    if attempt_renewal {
        std::fs::remove_file(f.first.root.path().join("login-release")).unwrap();
        let launches = f.first.launches();
        let id = "ca".repeat(32);
        login_alias_request(&f, &f.first, "retained", "start", &id).await;
        store::atomic_write(
            &f.first.root.path().join("login-release"),
            &serde_json::to_vec(&arriving).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_renewal_terminal(&f, "retained", &id).await["status"],
            "failed",
            "a new grant cannot override a proven local identity conflict"
        );
        assert_eq!(
            f.first.launches(),
            launches,
            "no verifier may start before old evidence is identified"
        );
    }
    assert!(
        std::fs::read(path).unwrap() == evidence,
        "shared advancement cannot overwrite unproven local vault ownership"
    );
    assert_eq!(std::fs::read(journal).unwrap(), journal_evidence);
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_replica_cached_during_add_verification_serves_after_completion() {
    let mut f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let operation = json!({"alias":"pending-cache","id":"cc".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "pending-workspace",
            Some("pending-sub"),
            Some("pending-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    f.second.stop().await;
    f.second = Pod::spawn(&f.database, &f.key, f.second.root, "postgres").await;
    store::atomic_write(&f.second.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        f.second.launches(),
        0,
        "the replica may cache pending data but must not verify it"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    let response = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"pending-cache"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "a completed receipt must promote the replica's safe pending cache"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_rejected_add_can_be_repaired_by_a_fresh_add_landing() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"error").unwrap();
    let first = json!({"alias":"repair-original","id":"cd".repeat(32)});
    let grant = add_grant("repair-workspace", Some("repair-sub"), Some("repair-uid"));
    add_request(&f, &f.first, "start", &first).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&grant).unwrap(),
    )
    .unwrap();
    assert_eq!(wait_add_terminal(&f, &first).await["status"], "failed");
    let second = json!({"alias":"repair-copy","id":"ce".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    let repaired = complete_add(&f, &f.second, &second, &grant).await;
    assert_eq!(repaired["landedAlias"], "repair-original");
    let token = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"repair-original"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        token.status(),
        200,
        "completed repair must release the old identity fence"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_continues_after_admission_commit_response_times_out() {
    let f = login_fixture().await;
    f.control.batch_execute(&format!(r#"
        CREATE FUNCTION {0}.prepare_admission_delay() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.kind='add' AND OLD.account_id IS NULL AND NEW.account_id IS NOT NULL THEN
                PERFORM set_config('statement_timeout','0',false);
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER prepare_admission_delay AFTER UPDATE ON {0}.central_login_operations
        FOR EACH ROW EXECUTE FUNCTION {0}.prepare_admission_delay();
        CREATE FUNCTION {0}.delay_admission_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.kind='add' AND OLD.account_id IS NULL AND NEW.account_id IS NOT NULL THEN
                PERFORM pg_sleep(3);
            END IF;
            RETURN NEW;
        END $$;
        CREATE CONSTRAINT TRIGGER delay_admission_commit AFTER UPDATE ON {0}.central_login_operations
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {0}.delay_admission_commit();
    "#,f.schema)).await.unwrap();
    let operation = json!({"alias":"commit-lost","id":"cf".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    complete_add(
        &f,
        &f.first,
        &operation,
        &add_grant("commit-workspace", Some("commit-sub"), Some("commit-uid")),
    )
    .await;
    assert_eq!(
        f.first.launches() + f.second.launches(),
        1,
        "one verification after admission"
    );
    let token = f
        .http
        .post(format!("{}/v1/token", f.second.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"commit-lost"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        token.status(),
        200,
        "committed admission must proceed to verification"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_reports_lock_timing_and_releases_it_before_native_verification() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let operation = json!({"alias":"timing","id":"ca".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant("timing-workspace", Some("timing-sub"), None)).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let count = metrics
        .lines()
        .find_map(|line| line.strip_prefix("codexctl_central_admission_lock_seconds_count "))
        .expect("admission exposes lock hold count")
        .parse::<u64>()
        .unwrap();
    assert!(count > 0);
    let seconds = metrics
        .lines()
        .find_map(|line| line.strip_prefix("codexctl_central_admission_lock_seconds_sum "))
        .expect("admission exposes cumulative hold time")
        .parse::<f64>()
        .unwrap();
    assert!(seconds.is_finite() && seconds > 0.0);
    let free: bool = f
        .control
        .query_one(
            "SELECT pg_try_advisory_lock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap()
        .get(0);
    assert!(free, "native verification must not hold the admission lock");
    f.control
        .query_one(
            "SELECT pg_advisory_unlock(hashtextextended($1,12484))",
            &[&f.schema],
        )
        .await
        .unwrap();
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_completed_add_preserves_reservation_history_when_claim_is_reused() {
    let f = login_fixture().await;
    let grant = add_grant(
        "history-workspace",
        Some("history-sub"),
        Some("history-uid"),
    );
    let first = json!({"alias":"history-original","id":"c8".repeat(32)});
    add_request(&f, &f.first, "start", &first).await;
    complete_add(&f, &f.first, &first, &grant).await;
    let retained:i64=f.control.query_one(&format!("SELECT count(*) FROM {}.central_login_identity_reservations WHERE workspace='history-workspace'",f.schema),&[]).await.unwrap().get(0);
    assert_eq!(retained, 2, "completed reservation rows must be retained");
    let second = json!({"alias":"history-copy","id":"c9".repeat(32)});
    add_request(&f, &f.second, "start", &second).await;
    assert_eq!(
        complete_add(&f, &f.second, &second, &grant).await["landedAlias"],
        "history-original"
    );
    let history=f.control.query_one(&format!("SELECT count(*),count(*) FILTER(WHERE deleted_at IS NULL) FROM {}.central_login_identity_reservation_history WHERE workspace='history-workspace'",f.schema),&[]).await.unwrap();
    assert_eq!(
        history.get::<_, i64>(0),
        4,
        "each operation retains both claim reservations"
    );
    assert_eq!(
        history.get::<_, i64>(1),
        0,
        "both completed operations have released claims"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_add_persists_typed_candidate_claims_for_identity_queries() {
    let f = login_fixture().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"add-startup-hold").unwrap();
    let operation = json!({"alias":"typed","id":"c7".repeat(32)});
    add_request(&f, &f.first, "start", &operation).await;
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "typed-workspace",
            Some("typed-sub"),
            Some("typed-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT candidate_uid,candidate_sub FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&operation["id"].as_str().unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "typed-uid");
    assert_eq!(row.get::<_, String>(1), "typed-sub");
    // Compatibility JSON is no longer the identity-query authority.
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_login_operations SET candidate_claims='{{}}'::jsonb WHERE id=$1",
                f.schema
            ),
            &[&operation["id"].as_str().unwrap()],
        )
        .await
        .unwrap();
    f.control
        .batch_execute(&format!("SET search_path TO {}", f.schema))
        .await
        .unwrap();
    let agrees:bool=f.control.query_one(&format!("SELECT {}.central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,account_id) FROM {}.central_login_operations WHERE id=$1",f.schema,f.schema),&[&operation["id"].as_str().unwrap()]).await.unwrap().get(0);
    assert!(
        agrees,
        "typed identity comparison must retain both namespaced facts"
    );
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"ready").unwrap();
    assert_eq!(
        wait_add_terminal(&f, &operation).await["status"],
        "completed"
    );
    stop_fixture(f).await;
}

#[tokio::test]
async fn postgres_older_reader_refuses_a_newer_schema_before_native_startup() {
    let mut f = login_fixture().await;
    f.first.stop().await;
    f.second.stop().await;
    f.control
        .execute(
            &format!(
                "INSERT INTO {}.central_schema_migrations(version) VALUES(8)",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let output = timeout(
        Duration::from_secs(5),
        command(
            &f.database,
            &f.first.root.path().join("state"),
            &f.key,
            "serve",
        )
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args([
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8787",
            "--codex-bin",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"))
        .kill_on_drop(true)
        .output(),
    )
    .await
    .expect("an unsupported schema must refuse before starting the server")
    .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("schema v8 is newer than this binary; upgrade codexctl-central")
    );
    assert_eq!(f.first.launches(), 0);
    stop_fixture(f).await;
}
