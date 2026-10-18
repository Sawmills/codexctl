#![cfg(feature = "central-prototype")]
//! A real CLI with a synthetic account server and child; no provider requests.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[path = "fixtures/relay.rs"]
mod relay_fixture;

struct Fixture {
    home: tempfile::TempDir,
    server: String,
    receipts: Arc<Mutex<Vec<Value>>>,
    probes: Arc<AtomicUsize>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(class: &str, reject_reports: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let probes = Arc::new(AtomicUsize::new(0));
        let plan = if class == "usage_based" {
            "team_usage_based"
        } else {
            "pro"
        };
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
            "sub":"synthetic-login", "iat":2000000000_u64, "exp":4102444800_u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat","chatgpt_plan_type":plan}
        })).unwrap());
        let account = json!({"userId":"synthetic-user","alias":"paid","label":null,
            "accountId":"synthetic-seat","plan":plan,"billingClass":class,
            "primaryUsed":20.0,"secondaryUsed":10.0,"resetsAt":4102444800_u64,
            "available":true,"usageScore":20.0,"usageStale":false,"liveSessions":0});
        let token = json!({"userId":"synthetic-user","accessToken":format!("header.{claims}."),
            "chatgptAccountId":"synthetic-seat","chatgptPlanType":plan,"revision":"synthetic-revision",
            "billingClass":class,"nativeRoutingSupported":true});
        let reads = probes.clone();
        let receipt_home = home.path().to_path_buf();
        let app = Router::new()
            .route(
                "/v1/me",
                get(|| async { Json(json!({"id":"synthetic-user"})) }),
            )
            .route(
                "/v1/accounts",
                get(move || async move { Json(json!([account])) }),
            )
            .route(
                "/v1/token",
                post(move || async move {
                    reads.fetch_add(1, Ordering::Relaxed);
                    Json(token)
                }),
            )
            .route(
                "/v1/relay/capacity-events",
                post(
                    move |State(receipts): State<Arc<Mutex<Vec<Value>>>>,
                          headers: HeaderMap,
                          Json(report): Json<Value>| async move {
                        assert_eq!(headers["authorization"], "Bearer synthetic-machine");
                        let paths = codexctl::config::Paths::from_home(receipt_home);
                        assert!(
                            codexctl::store::try_lock(&paths).unwrap().is_some(),
                            "receipt delivery must follow mutation-lock release"
                        );
                        receipts.lock().unwrap().push(report);
                        if reject_reports {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::NO_CONTENT
                        }
                    },
                ),
            )
            .with_state(receipts.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let directory = home.path().join(".codexctl/central");
        let credential = directory.join("device.token");
        codexctl::store::atomic_write(&credential, b"synthetic-machine").unwrap();
        codexctl::store::atomic_write(
            &directory.join(".server.json"),
            &serde_json::to_vec(&json!({
                "server":url,"token_file":credential,"user_id":"synthetic-user"
            }))
            .unwrap(),
        )
        .unwrap();
        let child = home.path().join("bin/codex");
        codexctl::store::atomic_write(&child, b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex 0.161.0'; else echo synthetic-child; fi\n").unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            })
        });
        Self {
            home,
            server: url,
            receipts,
            probes,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(self.home.path().join("bin"))
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .env_remove("CODEXCTL_PINNED_CONNECTION")
            .env_remove("CODEXCTL_SERVICE_TOKEN")
            .env_remove("CODEXCTL_SERVER_URL");
        command
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        self.command(args).output().unwrap()
    }

    fn service_cli(&self, args: &[&str]) -> std::process::Output {
        self.command(args)
            .env("CODEXCTL_SERVER_URL", &self.server)
            .env("CODEXCTL_SERVICE_TOKEN", "synthetic-machine")
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn billed_selection_receipt_excludes_declined_probes_and_automatic_placement() {
    let fixture = Fixture::new("usage_based", false);
    let declined = fixture.cli(&["codex", "--account", "paid", "exec", "synthetic"]);
    assert!(!declined.status.success());
    assert!(
        fixture.probes.load(Ordering::Relaxed) > 0,
        "must exercise a billing=true token probe"
    );
    assert!(fixture.receipts.lock().unwrap().is_empty());
    let automatic = fixture.service_cli(&["codex", "exec", "synthetic"]);
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr).contains("included"),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    assert!(fixture.receipts.lock().unwrap().is_empty());
    let approved = fixture.cli(&[
        "codex",
        "--account",
        "paid",
        "--allow-billing",
        "exec",
        "synthetic",
    ]);
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert!(String::from_utf8_lossy(&approved.stdout).contains("synthetic-child"));
    assert_eq!(
        *fixture.receipts.lock().unwrap(),
        vec![json!({"billed_selection":"explicit"})]
    );
}

#[test]
fn included_automatic_launch_sends_no_billed_selection_receipt() {
    let fixture = Fixture::new("rate_limited", false);
    let output = fixture.service_cli(&["codex", "exec", "synthetic"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.probes.load(Ordering::Relaxed) > 0);
    assert!(fixture.receipts.lock().unwrap().is_empty());
}

#[test]
fn selection_telemetry_failure_keeps_the_launch_successful_and_has_its_own_counter() {
    let fixture = Fixture::new("usage_based", true);
    let output = fixture.cli(&[
        "codex",
        "--account",
        "paid",
        "--allow-billing",
        "exec",
        "synthetic",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.receipts.lock().unwrap().len(),
        1,
        "no report retries"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("billed_selection_report_failed"));
    let second = fixture.cli(&[
        "codex",
        "--account",
        "paid",
        "--allow-billing",
        "exec",
        "synthetic",
    ]);
    assert!(second.status.success());
    assert_eq!(fixture.receipts.lock().unwrap().len(), 2);
    let relay = relay_fixture::TestRelay::start(fixture.home.path());
    let metrics = reqwest::blocking::get(format!("http://{}/metrics", relay.address))
        .unwrap()
        .text()
        .unwrap();
    assert!(
        metrics.contains("codexctl_client_outcome_reports_failed_total 2\n"),
        "{metrics}"
    );
}

#[test]
fn activation_receipt_is_once_per_selection_and_refreshes_do_not_add_receipts() {
    let fixture = Fixture::new("usage_based", false);
    let selected = fixture.cli(&["use", "paid", "--allow-billing"]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert_eq!(fixture.receipts.lock().unwrap().len(), 1);
    let refreshed = fixture.cli(&["central-token", "--active"]);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    assert_eq!(fixture.receipts.lock().unwrap().len(), 1);
}

#[test]
fn app_auth_enable_is_a_selection_but_refresh_is_not() {
    let fixture = Fixture::new("usage_based", false);
    let enabled = fixture.cli(&[
        "app-auth",
        "enable",
        "--account",
        "paid",
        "--allow-billing",
        "--no-agent",
    ]);
    assert!(
        enabled.status.success(),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    assert_eq!(
        *fixture.receipts.lock().unwrap(),
        vec![json!({"billed_selection":"explicit"})]
    );
    let refreshed = fixture.cli(&["app-auth", "refresh"]);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    assert_eq!(fixture.receipts.lock().unwrap().len(), 1);
}
