#![cfg(feature = "central-prototype")]

use std::os::unix::process::CommandExt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use codexctl::{central, store};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct BrokerTest {
    root: tempfile::TempDir,
    child: Child,
    url: String,
    http: reqwest::blocking::Client,
    token: String,
    key: PathBuf,
}

impl BrokerTest {
    fn start() -> Self {
        Self::start_with(&[])
    }
    fn start_with(options: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"user-central","generation":0,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-central","chatgpt_plan_type":"pro"}})).unwrap());
        let auth = json!({"auth_mode":"chatgpt","tokens":{"access_token":format!("eyJhbGciOiJub25lIn0.{payload}."),"refresh_token":"synthetic-initial-refresh","account_id":"acct-central"}});
        store::atomic_write(
            &root.path().join("auth.json"),
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        central::init(
            &root.path().join("state"),
            &key,
            &root.path().join("auth.json"),
            "personal",
            "amir",
            "amir",
        )
        .unwrap();
        central::register(
            &root.path().join("state"),
            "laptop",
            "amir",
            "amir",
            &root.path().join("laptop.token"),
        )
        .unwrap();
        store::atomic_write(&root.path().join("refresh-count"), b"0").unwrap();
        store::atomic_write(&root.path().join("mode"), b"").unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .args(["serve", "--state"])
            .arg(root.path().join("state"))
            .arg("--key-file")
            .arg(&key)
            .args(["--listen", "127.0.0.1:0", "--codex-bin"])
            .arg(if options.contains(&"--read-only") {
                PathBuf::from("/nonexistent/codex")
            } else {
                fixture()
            })
            .args(options)
            .process_group(0)
            .env("CENTRAL_TEST_MODE_FILE", root.path().join("mode"))
            .env("CENTRAL_TEST_KEY_FILE", &key)
            .env(
                "CENTRAL_TEST_REFRESH_COUNTER",
                root.path().join("refresh-count"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address: Value = serde_json::from_str(&line).unwrap();
        let url = format!("http://{}", address["listening"].as_str().unwrap());
        let token = std::fs::read_to_string(root.path().join("laptop.token")).unwrap();
        Self {
            root,
            child,
            url,
            token,
            key,
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .no_proxy()
                .build()
                .unwrap(),
        }
    }
    fn restart_read_only(&mut self) {
        self.child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .args(["serve", "--state"])
            .arg(self.state())
            .arg("--key-file")
            .arg(&self.key)
            .args(["--listen", "127.0.0.1:0", "--read-only"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(self.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        self.url = format!("http://{}", ready["listening"].as_str().unwrap());
    }
    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }
    fn token_request(&self, bearer: &str, body: Value) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}/v1/token", self.url))
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .unwrap()
    }
    fn grant(&self) -> Value {
        self.token_request(&self.token, json!({})).json().unwrap()
    }
    fn register_other(&self, tenant: &str, user: &str) -> String {
        let path = self.root.path().join("other.token");
        central::register(&self.state(), "other", tenant, user, &path).unwrap();
        std::fs::read_to_string(path).unwrap()
    }
    fn run_client(&self, token: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .args(["run", "--server", &self.url, "--token-file"])
            .arg(token)
            .arg("--codex-bin")
            .arg(fixture())
            .arg("Reply CENTRAL_OK")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}
impl Drop for BrokerTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py")
}

#[test]
fn when_device_is_revoked_then_future_token_requests_are_denied() {
    let broker = BrokerTest::start();
    let before = broker.token_request(&broker.token, json!({})).status();

    central::revoke(&broker.state(), "laptop").unwrap();
    let after = broker.token_request(&broker.token, json!({})).status();

    assert_eq!((before.as_u16(), after.as_u16()), (200, 401));
}

#[test]
fn when_device_belongs_to_another_tenant_then_account_access_is_denied() {
    let broker = BrokerTest::start();
    let token = broker.register_other("another-company", "amir");

    let response = broker.token_request(&token, json!({}));

    assert_eq!(response.status().as_u16(), 403);
}

#[test]
fn when_device_belongs_to_another_user_then_account_access_is_denied() {
    let broker = BrokerTest::start();
    let token = broker.register_other("amir", "another-user");

    let response = broker.token_request(&token, json!({}));

    assert_eq!(response.status().as_u16(), 403);
}

#[test]
fn when_tokens_are_issued_then_refresh_credentials_stay_on_the_server() {
    let broker = BrokerTest::start();
    let expected = vec![
        "accessToken",
        "chatgptAccountId",
        "chatgptPlanType",
        "revision",
    ];

    let grant = broker.grant();

    assert_eq!(
        grant
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        !String::from_utf8_lossy(&std::fs::read(broker.state().join("vault.enc")).unwrap())
            .contains("synthetic-initial-refresh")
    );
}

#[test]
fn when_two_clients_refresh_the_same_revision_then_the_owner_rotates_once() {
    let broker = BrokerTest::start();
    let initial = broker.grant();
    let body =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});

    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            broker
                .token_request(&broker.token, body.clone())
                .json::<Value>()
                .unwrap()
        });
        let second = scope.spawn(|| {
            broker
                .token_request(&broker.token, body.clone())
                .json::<Value>()
                .unwrap()
        });
        (first.join().unwrap(), second.join().unwrap())
    });

    assert_eq!(first, second);
    assert_ne!(first["revision"], initial["revision"]);
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "1"
    );
}

#[test]
fn registered_devices_can_complete_concurrent_local_turns_with_central_refresh() {
    let broker = BrokerTest::start();
    broker.register_other("amir", "amir");
    let first = broker.run_client(&broker.root.path().join("laptop.token"));
    let second = broker.run_client(&broker.root.path().join("other.token"));

    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();

    assert_eq!(
        (
            first.status.success(),
            String::from_utf8(first.stdout).unwrap()
        ),
        (true, "CENTRAL_OK\n".into())
    );
    assert_eq!(
        (
            second.status.success(),
            String::from_utf8(second.stdout).unwrap()
        ),
        (true, "CENTRAL_OK\n".into())
    );
}

#[test]
fn when_an_owner_is_running_then_a_second_owner_is_refused() {
    let broker = BrokerTest::start();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .args(["serve", "--state"])
        .arg(broker.state())
        .arg("--key-file")
        .arg(&broker.key)
        .args(["--listen", "127.0.0.1:0", "--codex-bin"])
        .arg(fixture())
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("another process owns this state")
    );
}

#[test]
fn when_a_request_has_no_authority_then_one_failure_is_counted() {
    let broker = BrokerTest::start();
    let expected = "codexctl_central_failed_requests_total{reason=\"unauthorized\"} 1\n";

    let response = broker.token_request("unknown-device", json!({}));
    let counters = broker
        .http
        .get(format!("{}/metrics", broker.url))
        .bearer_auth(&broker.token)
        .send()
        .unwrap()
        .text()
        .unwrap();

    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(counters, expected);
}

#[test]
fn when_read_only_mode_is_used_then_no_owner_process_receives_refresh_credentials() {
    let broker = BrokerTest::start_with(&["--read-only"]);
    let initial = broker.grant();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});

    let response = broker.token_request(&broker.token, request);

    assert_eq!(response.status().as_u16(), 409);
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "0"
    );
}

#[test]
fn when_the_owner_rejects_refresh_then_subsequent_requests_fail_closed() {
    let broker = BrokerTest::start();
    let initial = broker.grant();
    std::fs::write(broker.root.path().join("mode"), "error").unwrap();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});

    let first = broker
        .token_request(&broker.token, request)
        .status()
        .as_u16();
    let second = broker
        .token_request(&broker.token, json!({}))
        .status()
        .as_u16();

    assert_eq!((first, second), (503, 503));
}

#[test]
fn when_refresh_changes_identity_then_the_owner_refuses_credential_distribution() {
    let broker = BrokerTest::start();
    let initial = broker.grant();
    std::fs::write(broker.root.path().join("mode"), "identity").unwrap();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});

    let response = broker.token_request(&broker.token, request);

    assert_eq!(response.status().as_u16(), 503);
}

#[test]
fn when_final_persistence_fails_then_shutdown_retains_rotated_credentials() {
    let mut broker = BrokerTest::start();
    let initial = broker.grant();
    std::fs::write(broker.root.path().join("mode"), "break-key").unwrap();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});
    broker.token_request(&broker.token, request);

    unsafe {
        libc::kill(broker.child.id() as i32, libc::SIGTERM);
    }
    broker.child.wait().unwrap();
    let runtime = std::fs::read_dir(broker.state())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("owner-runtime-")
        })
        .unwrap();

    assert!(
        std::fs::read_to_string(runtime.join("auth.json"))
            .unwrap()
            .contains("synthetic-rotated-refresh")
    );
}

#[test]
fn when_a_refresh_callback_precedes_the_turn_response_then_the_client_still_completes() {
    let broker = BrokerTest::start();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .args(["run", "--server", &broker.url, "--token-file"])
        .arg(broker.root.path().join("laptop.token"))
        .arg("--codex-bin")
        .arg(fixture())
        .arg("Reply CENTRAL_OK")
        .env("CENTRAL_TEST_EARLY_CALLBACK", "1")
        .output()
        .unwrap();

    assert_eq!(
        (
            result.status.success(),
            String::from_utf8(result.stdout).unwrap()
        ),
        (true, "CENTRAL_OK\n".into())
    );
}

#[test]
fn when_a_client_disconnects_during_refresh_then_the_owner_persists_the_result() {
    let broker = BrokerTest::start();
    let initial = broker.grant();
    std::fs::write(broker.root.path().join("mode"), "slow").unwrap();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});
    let impatient = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(30))
        .no_proxy()
        .build()
        .unwrap();

    let timed_out = impatient
        .post(format!("{}/v1/token", broker.url))
        .bearer_auth(&broker.token)
        .json(&request)
        .send();
    let latest = broker.grant();

    assert!(timed_out.is_err());
    assert_ne!(latest["revision"], initial["revision"]);
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "1"
    );
}

#[test]
fn when_a_request_names_another_account_then_no_refresh_occurs() {
    let broker = BrokerTest::start();

    let response = broker.token_request(&broker.token, json!({"accountId":"another-account"}));

    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "0"
    );
}

#[test]
fn when_an_owner_exits_abruptly_then_restart_refuses_an_unfinished_runtime() {
    let mut broker = BrokerTest::start();
    broker.child.kill().unwrap();
    broker.child.wait().unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .args(["serve", "--state"])
        .arg(broker.state())
        .arg("--key-file")
        .arg(&broker.key)
        .args(["--listen", "127.0.0.1:0", "--codex-bin"])
        .arg(fixture())
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("unfinished owner runtime")
    );
}

#[test]
fn when_the_owner_restarts_then_it_uses_the_encrypted_rotated_credentials() {
    let mut broker = BrokerTest::start();
    let initial = broker.grant();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});
    let rotated: Value = broker.token_request(&broker.token, request).json().unwrap();
    unsafe {
        libc::kill(broker.child.id() as i32, libc::SIGTERM);
    }
    broker.child.wait().unwrap();

    broker.restart_read_only();
    let restored = broker.grant();

    assert_eq!(restored, rotated);
}

#[test]
fn when_the_owner_emits_many_notifications_then_token_requests_still_succeed() {
    let broker = BrokerTest::start();
    std::fs::write(broker.root.path().join("mode"), "notifications").unwrap();

    let first = broker
        .token_request(&broker.token, json!({}))
        .status()
        .as_u16();
    let second = broker
        .token_request(&broker.token, json!({}))
        .status()
        .as_u16();

    assert_eq!((first, second), (200, 200));
}

#[test]
fn when_the_port_is_occupied_then_no_credential_owner_starts() {
    let mut broker = BrokerTest::start();
    unsafe {
        libc::kill(broker.child.id() as i32, libc::SIGTERM);
    }
    broker.child.wait().unwrap();
    std::fs::write(broker.root.path().join("mode"), "startup").unwrap();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .args(["serve", "--state"])
        .arg(broker.state())
        .arg("--key-file")
        .arg(&broker.key)
        .arg("--listen")
        .arg(occupied.local_addr().unwrap().to_string())
        .arg("--codex-bin")
        .arg(fixture())
        .env("CENTRAL_TEST_MODE_FILE", broker.root.path().join("mode"))
        .env(
            "CENTRAL_TEST_REFRESH_COUNTER",
            broker.root.path().join("refresh-count"),
        )
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "0"
    );
}

#[test]
fn when_the_codex_executable_is_missing_then_no_runtime_blocks_a_later_start() {
    let mut broker = BrokerTest::start();
    unsafe {
        libc::kill(broker.child.id() as i32, libc::SIGTERM);
    }
    broker.child.wait().unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .args(["serve", "--state"])
        .arg(broker.state())
        .arg("--key-file")
        .arg(&broker.key)
        .args([
            "--listen",
            "127.0.0.1:0",
            "--codex-bin",
            "/nonexistent/codex",
        ])
        .output()
        .unwrap();
    let has_runtime = std::fs::read_dir(broker.state()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("owner-runtime-")
    });

    assert!(!result.status.success());
    assert!(!has_runtime);
}

#[test]
fn when_the_owner_writes_credentials_at_exit_then_shutdown_persists_them() {
    let mut broker = BrokerTest::start();
    let initial = broker.grant();
    std::fs::write(broker.root.path().join("mode"), "exit-rotation").unwrap();

    unsafe {
        libc::kill(broker.child.id() as i32, libc::SIGTERM);
    }
    broker.child.wait().unwrap();
    broker.restart_read_only();
    let restored = broker.grant();

    assert_ne!(restored["revision"], initial["revision"]);
    assert_eq!(
        std::fs::read_to_string(broker.root.path().join("refresh-count")).unwrap(),
        "1"
    );
}

#[test]
fn when_an_account_mismatch_is_rejected_then_it_is_not_counted_as_an_owner_outage() {
    let broker = BrokerTest::start();
    let expected = "codexctl_central_failed_requests_total{reason=\"account_mismatch\"} 1\n";

    broker.token_request(&broker.token, json!({"accountId":"another-account"}));
    let counters = broker
        .http
        .get(format!("{}/metrics", broker.url))
        .bearer_auth(&broker.token)
        .send()
        .unwrap()
        .text()
        .unwrap();

    assert_eq!(counters, expected);
}

#[test]
fn when_the_terminal_signals_the_broker_group_then_shutdown_finishes_cleanly() {
    let mut broker = BrokerTest::start();

    unsafe {
        libc::kill(-(broker.child.id() as i32), libc::SIGINT);
    }
    let status = broker.child.wait().unwrap();
    let has_runtime = std::fs::read_dir(broker.state()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("owner-runtime-")
    });

    assert!(status.success());
    assert!(!has_runtime);
}
