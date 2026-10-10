#![cfg(feature = "central-prototype")]

#[path = "fixtures/daemon.rs"]
mod daemon_fixture;

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

#[test]
fn loopback_does_not_receive_a_device_credential_without_explicit_test_opt_in() {
    use std::io::{Read, Write};
    let root = tempfile::tempdir().unwrap();
    let token_file = root.path().join("device.token");
    store::atomic_write(&token_file, b"synthetic-loopback-device-credential").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let observer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut bytes = [0u8; 4096];
                    let length = stream.read(&mut bytes).unwrap();
                    let received = String::from_utf8_lossy(&bytes[..length])
                        .to_ascii_lowercase()
                        .contains("authorization: bearer");
                    stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    return received;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("listener failed: {error}"),
            }
        }
        false
    });
    let result = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["run", "--server", &url, "--token-file"])
        .arg(token_file)
        .arg("synthetic prompt")
        .env_remove("CODEXCTL_ALLOW_INSECURE_LOOPBACK")
        .output()
        .unwrap();
    let received = observer.join().unwrap();
    assert!(!result.status.success());
    assert!(
        !received,
        "a loopback listener must not receive the device bearer credential"
    );
}

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
        Self::start_with_plan(options, Some("pro"))
    }
    fn start_with_plan(options: &[&str], plan: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"user-central","generation":0,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-central","chatgpt_plan_type":plan}})).unwrap());
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
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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
            .env("CENTRAL_TEST_RETRY_CLOCK", root.path().join("retry-clock"))
            .env("CENTRAL_TEST_KEY_FILE", &key)
            .env(
                "CENTRAL_TEST_REFRESH_COUNTER",
                root.path().join("refresh-count"),
            )
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(root.path().join("server.stderr")).unwrap())
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
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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
fn legacy_token_endpoint_reports_both_forced_refresh_decisions() {
    let broker = BrokerTest::start();
    let initial = broker.grant();
    let rotated: Value = broker
        .token_request(
            &broker.token,
            json!({"previousRevision":initial["revision"]}),
        )
        .json()
        .unwrap();
    let recent: Value = broker
        .token_request(
            &broker.token,
            json!({"previousRevision":rotated["revision"]}),
        )
        .json()
        .unwrap();
    assert_eq!(recent["revision"], rotated["revision"]);
    let metrics = broker
        .http
        .get(format!("{}/metrics", broker.url))
        .bearer_auth(&broker.token)
        .send()
        .unwrap()
        .text()
        .unwrap();
    for outcome in ["refreshed", "served_recent"] {
        assert!(
            metrics.contains(&format!(
                "codexctl_central_forced_refresh_total{{outcome=\"{outcome}\"}} 1\n"
            )),
            "{metrics}"
        );
    }
    let stderr = std::fs::read_to_string(broker.root.path().join("server.stderr")).unwrap();
    let entries: Vec<Value> = stderr
        .lines()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|entry| entry["operation"] == "forced_refresh")
        .collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["outcome"], "refreshed");
    assert_eq!(entries[1]["outcome"], "served_recent");
    for entry in entries {
        assert_eq!(entry["device"], "laptop");
        assert_eq!(entry["account"], "personal");
    }
    for secret in [
        broker.token.as_str(),
        initial["accessToken"].as_str().unwrap(),
        rotated["accessToken"].as_str().unwrap(),
        "synthetic-initial-refresh",
        "synthetic-rotated-refresh",
    ] {
        assert!(!stderr.contains(secret));
        assert!(!metrics.contains(secret));
    }
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
        "nativeRoutingSupported",
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
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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

fn failed_request_metrics(counters: &str) -> Vec<&str> {
    counters
        .lines()
        .filter(|line| line.starts_with("codexctl_central_failed_requests_total{"))
        .collect()
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
    assert_eq!(failed_request_metrics(&counters), [expected.trim_end()]);
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
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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
    std::fs::write(broker.root.path().join("mode"), "disconnect").unwrap();
    let request =
        json!({"previousRevision":initial["revision"],"accountId":initial["chatgptAccountId"]});
    use std::io::Write;
    let mut stream =
        std::net::TcpStream::connect(broker.url.trim_start_matches("http://")).unwrap();
    let body = serde_json::to_string(&request).unwrap();
    write!(stream, "POST /v1/token HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", broker.token, body.len(), body).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !broker.root.path().join("refresh-started").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "refresh never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    stream.shutdown(std::net::Shutdown::Both).unwrap();
    let latest = broker.grant();

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
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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

    assert_eq!(restored["accessToken"], rotated["accessToken"]);
    assert_eq!(restored["revision"], rotated["revision"]);
    assert_eq!(rotated["nativeRoutingSupported"], true);
    assert_eq!(restored["nativeRoutingSupported"], false);
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
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
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

    assert_eq!(failed_request_metrics(&counters), [expected.trim_end()]);
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

struct NativeClient {
    broker: BrokerTest,
    home: PathBuf,
}
impl NativeClient {
    fn local_auth_bytes() -> Vec<u8> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"unrelated-local-login","exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"unrelated-local-seat"}})).unwrap());
        serde_json::to_vec(&json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"synthetic-unrelated-refresh","account_id":"unrelated-local-seat"}})).unwrap()
    }
    fn start() -> Self {
        Self::with_plan(Some("pro"))
    }
    fn with_plan(plan: Option<&str>) -> Self {
        let broker = BrokerTest::start_with_plan(&[], plan);
        let home = broker.root.path().join("client");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        // The host's container overlay mount emits an lsof diagnostic that
        // production must treat as an uncertain inventory. Keep these tests
        // focused on session behavior by wrapping the real lsof with -w.
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let lsof = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("lsof"))
            .find(|path| path.is_file())
            .expect("lsof must be installed for central session tests");
        std::fs::write(
            bin.join("lsof"),
            format!("#!/bin/sh\nexec {} -w \"$@\"\n", lsof.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("lsof"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "# Personal preference\nmodel = 'gpt-6.1-sol'\nmodel_provider = 'openai'\n",
        )
        .unwrap();
        store::atomic_write(&home.join(".codex/auth.json"), &Self::local_auth_bytes()).unwrap();
        Self { broker, home }
    }
    fn run(&self, bin: &str, args: &[&str]) -> std::process::Output {
        Command::new(bin)
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(args)
            .env("HOME", &self.home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.home.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .output()
            .unwrap()
    }
    fn connect(&self) {
        self.connect_alias("remote");
    }
    fn connect_alias(&self, alias: &str) {
        let output = self.run(
            env!("CARGO_BIN_EXE_codexctl-central"),
            &[
                "connect",
                "--alias",
                alias,
                "--server",
                &self.broker.url,
                "--token-file",
                self.broker
                    .root
                    .path()
                    .join("laptop.token")
                    .to_str()
                    .unwrap(),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn select(&self) -> std::process::Output {
        self.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "remote"])
    }
    fn helper(&self) -> std::process::Output {
        self.run(
            env!("CARGO_BIN_EXE_codexctl"),
            &[
                "central-token",
                "--connection",
                self.home
                    .join(".codexctl/central/remote.json")
                    .to_str()
                    .unwrap(),
            ],
        )
    }
    fn helper_active(&self) -> std::process::Output {
        self.run(
            env!("CARGO_BIN_EXE_codexctl"),
            &["central-token", "--active"],
        )
    }

    fn launch_command(&self, args: &[&str]) -> Command {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let codex = bin.join("codex");
        std::fs::write(
            &codex,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\"\nexit 23\n",
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl"));
        command
            .arg("codex")
            .args(args)
            .current_dir(&self.home)
            .env("PATH", &bin)
            .env("HOME", &self.home)
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS");
        command
    }
}

#[test]
fn when_resuming_with_a_server_account_then_the_launch_overrides_the_saved_provider() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let args = [
        "resume",
        "old-openai-session",
        "--",
        "keep this prompt intact",
    ];
    let cwd = std::fs::canonicalize(&client.home).unwrap();
    let expected = format!(
        "{}\n-c\nmodel_provider=\"codexctl-central\"\n--cd\n{}\n{}\n",
        cwd.display(),
        cwd.display(),
        args.join("\n")
    );

    let output = client.launch_command(&args).output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert_eq!(
        std::fs::read(client.home.join(".codex/auth.json")).unwrap(),
        NativeClient::local_auth_bytes()
    );
}

#[test]
fn when_a_server_launch_has_an_explicit_directory_then_it_keeps_that_override() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let args = ["resume", "old-session", "-C", "/chosen/project"];
    let expected = format!(
        "{}\n-c\nmodel_provider=\"codexctl-central\"\n{}\n",
        std::fs::canonicalize(&client.home).unwrap().display(),
        args.join("\n")
    );

    let output = client.launch_command(&args).output().unwrap();

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

#[test]
fn when_a_server_launch_inherits_a_codex_home_then_it_refuses_before_starting_codex() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let output = client
        .launch_command(&["resume", "old-session"])
        .env("CODEX_HOME", client.home.join(".codex"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("inherited or pinned Codex home"));
    assert!(output.stdout.is_empty());
}

#[test]
fn when_a_server_launch_child_receives_a_signal_then_the_launcher_preserves_its_status() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let mut command = client.launch_command(&[]);
    std::fs::write(client.home.join("bin/codex"), "#!/bin/sh\nkill -TERM $$\n").unwrap();

    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM));
}

#[test]
fn when_a_server_launch_inherits_a_pinned_alias_then_it_refuses_before_starting_codex() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let output = client
        .launch_command(&[])
        .env("CODEXCTL_PINNED_ALIAS", "local")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("inherited or pinned Codex home"));
    assert!(output.stdout.is_empty());
}

#[test]
fn when_the_provider_is_local_despite_a_marker_then_the_local_launcher_keeps_its_arguments() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    std::fs::write(
        client.home.join(".codex/config.toml"),
        "model_provider='openai'\n",
    )
    .unwrap();
    let args = ["resume", "local-session"];
    let cwd = std::fs::canonicalize(&client.home).unwrap();
    let expected = format!(
        "{}\n--cd\n{}\n{}\n",
        cwd.display(),
        cwd.display(),
        args.join("\n")
    );

    let output = client.launch_command(&args).output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace('\r', ""),
        expected
    );
}

#[test]
fn when_a_server_launch_is_running_then_selection_and_the_token_helper_work() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let mut command = client.launch_command(&[]);
    std::fs::write(
        client.home.join("bin/codex"),
        "#!/bin/sh\nprintf 'ready\\n'\nread -r finish\n",
    )
    .unwrap();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();

    let (selected, automatic) = std::thread::scope(|scope| {
        let explicit = scope.spawn(|| client.select());
        let automatic = scope.spawn(|| client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]));
        (explicit.join().unwrap(), automatic.join().unwrap())
    });
    let token = client.helper();
    drop(child.stdin.take());
    child.wait().unwrap();

    assert_eq!(ready, "ready\n");
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        automatic.status.success(),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
}

#[test]
fn when_a_remote_account_is_selected_then_native_configuration_preserves_the_local_login() {
    let client = NativeClient::start();
    client.connect();

    let selected = client.select();
    let configured: toml_edit::DocumentMut =
        std::fs::read_to_string(client.home.join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert!(
        configured["model_providers"]
            .get("codexctl-central")
            .is_some(),
        "remote provider definition must reach Codex"
    );

    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert_eq!(
        std::fs::read(client.home.join(".codex/auth.json")).unwrap(),
        NativeClient::local_auth_bytes()
    );
    assert!(
        std::fs::read_to_string(client.home.join(".codex/config.toml"))
            .unwrap()
            .contains("# Personal preference")
    );
}

#[test]
fn when_native_codex_requests_a_token_then_only_access_credentials_are_returned() {
    let client = NativeClient::start();
    client.connect();

    let token = client.helper();

    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
    assert!(codexctl::api::token_identity(String::from_utf8_lossy(&token.stdout).trim()).is_some());
    assert!(!String::from_utf8_lossy(&token.stdout).contains("synthetic-initial-refresh"));
    assert_eq!(
        std::fs::read_to_string(client.broker.root.path().join("refresh-count")).unwrap(),
        "0"
    );
}

#[test]
fn a_read_only_broker_cannot_register_a_native_provider_without_routing_proof() {
    let broker = BrokerTest::start_with(&["--read-only"]);
    let home = broker.root.path().join("client");
    let client = NativeClient { broker, home };
    let output = client.run(
        env!("CARGO_BIN_EXE_codexctl-central"),
        &[
            "connect",
            "--alias",
            "remote",
            "--server",
            &client.broker.url,
            "--token-file",
            client
                .broker
                .root
                .path()
                .join("laptop.token")
                .to_str()
                .unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("workspace routing"));
    assert!(!client.home.join(".codexctl/central/remote.json").exists());
}

#[test]
fn when_a_device_is_revoked_then_the_native_helper_returns_no_credentials() {
    let client = NativeClient::start();
    client.connect();
    central::revoke(&client.broker.state(), "laptop").unwrap();

    let token = client.helper();

    assert!(!token.status.success());
    assert!(token.stdout.is_empty());
}

#[test]
fn selecting_a_local_profile_restores_the_provider_after_remote_use() {
    let client = NativeClient::start();
    let paths = codexctl::config::Paths::from_home(client.home.clone());
    codexctl::profile::save_profile_to(&paths, "local", None, &paths.codex_auth_json()).unwrap();
    client.connect();
    assert!(client.select().status.success());

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "local"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = std::fs::read_to_string(client.home.join(".codex/config.toml")).unwrap();
    assert!(config.contains("model_provider = \"openai\""));
    assert!(!config.contains("model_providers"));
    assert_eq!(
        std::fs::read(paths.codex_auth_json()).unwrap(),
        NativeClient::local_auth_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(paths.active_file()).unwrap().trim(),
        "local"
    );
}

#[test]
fn when_remote_use_is_disconnected_then_the_previous_provider_is_restored() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let output = client.run(env!("CARGO_BIN_EXE_codexctl-central"), &["disconnect"]);

    assert!(output.status.success());
    let config = std::fs::read_to_string(client.home.join(".codex/config.toml")).unwrap();
    assert!(
        config.contains("# Personal preference")
            && config.contains("model_provider = \"openai\"")
            && !config.contains("Central Codex")
            && !config.contains("model_providers")
    );
}

#[test]
fn when_two_native_helpers_share_a_device_then_both_get_credentials() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "slow").unwrap();

    let outputs = std::thread::scope(|scope| {
        let first = scope.spawn(|| client.helper());
        let second = scope.spawn(|| client.helper());
        (first.join().unwrap(), second.join().unwrap())
    });

    assert!(
        outputs.0.status.success() && outputs.1.status.success(),
        "both concurrent helper requests must succeed"
    );
}

#[test]
fn when_the_selected_provider_is_removed_then_disconnect_preserves_the_user_edit() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    std::fs::write(
        client.home.join(".codex/config.toml"),
        "# Edited preference\nmodel = 'gpt-6.1-sol'\n",
    )
    .unwrap();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl-central"), &["disconnect"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(client.home.join(".codex/config.toml")).unwrap(),
        "# Edited preference\nmodel = 'gpt-6.1-sol'\n"
    );
}

#[test]
fn when_remote_mode_is_active_then_a_pinned_launch_cannot_use_the_wrong_account() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let output = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["exec", "--account", "local", "--", "echo", "unreachable"],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("remote provider is active"));
}

#[test]
fn when_config_is_linked_then_remote_selection_preserves_the_link() {
    let client = NativeClient::start();
    client.connect();
    let path = client.home.join(".codex/config.toml");
    use std::os::unix::fs::PermissionsExt;
    let original_parent_mode = std::fs::metadata(&client.home)
        .unwrap()
        .permissions()
        .mode();
    let target = client.home.join("preferences.toml");
    std::fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    let output = client.select();

    assert!(output.status.success());
    assert!(
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::metadata(&client.home)
            .unwrap()
            .permissions()
            .mode(),
        original_parent_mode
    );
}

#[test]
fn when_one_remote_account_is_registered_then_use_without_an_alias_selects_it() {
    let client = NativeClient::start();
    client.connect();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("switched to remote account remote"));
}

#[test]
fn when_remote_billing_is_usage_based_then_automatic_selection_is_refused() {
    let client = NativeClient::with_plan(Some("self_serve_business_usage_based"));
    client.connect();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "--allow-billing"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("automatic remote selection refuses"));
}

#[test]
fn when_remote_billing_is_unknown_then_noninteractive_explicit_selection_is_refused() {
    let client = NativeClient::with_plan(None);
    client.connect();

    let output = client.select();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-billing"));
}

#[test]
fn when_remote_billing_is_approved_then_the_helper_can_supply_access_credentials() {
    let client = NativeClient::with_plan(Some("self_serve_business_usage_based"));
    client.connect();

    let output = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "remote", "--allow-billing"],
    );
    let token = client.helper();

    assert!(output.status.success());
    assert!(token.status.success());
}

#[test]
fn when_no_remote_aliases_exist_then_local_use_keeps_the_local_error_and_configuration() {
    let client = NativeClient::start();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "missing"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
    assert_eq!(
        std::fs::read(client.home.join(".codex/auth.json")).unwrap(),
        NativeClient::local_auth_bytes()
    );
}

#[test]
fn when_a_remote_switch_inherits_codex_home_then_it_refuses_before_writing_configuration() {
    let client = NativeClient::start();
    client.connect();

    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["use", "remote"])
        .env("HOME", &client.home)
        .env("CODEX_HOME", client.home.join("pinned"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!client.home.join("pinned/config.toml").exists());
}

#[test]
fn when_multiple_remote_connections_exist_then_automatic_selection_requires_an_alias() {
    let client = NativeClient::start();
    client.connect();
    let connections = client.home.join(".codexctl/central");
    store::atomic_write(
        &connections.join("other.json"),
        &std::fs::read(connections.join("remote.json")).unwrap(),
    )
    .unwrap();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("explicit alias"));
}

#[test]
fn when_activation_cannot_resolve_config_then_no_stale_marker_blocks_recovery() {
    let client = NativeClient::start();
    client.connect();
    let config = client.home.join(".codex/config.toml");
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(client.home.join("missing.toml"), &config).unwrap();

    let output = client.select();

    assert!(!output.status.success());
    assert!(
        !client
            .home
            .join(".codexctl/central/.native-active.json")
            .exists()
    );
}

#[test]
fn when_remote_config_is_invalid_then_local_use_does_not_swap_credentials() {
    let client = NativeClient::start();
    client.connect();
    codexctl::profile::save_profile_to(
        &codexctl::config::Paths::from_home(client.home.clone()),
        "local",
        Some("local@example.invalid"),
        &client.home.join(".codex/auth.json"),
    )
    .unwrap();
    assert!(client.select().status.success());
    std::fs::write(client.home.join(".codex/config.toml"), "[invalid").unwrap();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "local"]);

    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(client.home.join(".codex/auth.json")).unwrap(),
        NativeClient::local_auth_bytes()
    );
}

#[test]
fn when_remote_mode_is_active_then_the_local_switch_picker_refuses_before_prompting() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["switch"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("remote provider is active"));
}

#[test]
fn when_an_alias_has_outer_spaces_then_connect_and_use_share_the_normalized_name() {
    let client = NativeClient::start();
    let connected = client.run(
        env!("CARGO_BIN_EXE_codexctl-central"),
        &[
            "connect",
            "--alias",
            " remote ",
            "--server",
            &client.broker.url,
            "--token-file",
            client
                .broker
                .root
                .path()
                .join("laptop.token")
                .to_str()
                .unwrap(),
        ],
    );

    let selected = client.select();

    assert!(connected.status.success());
    assert!(selected.status.success());
}

#[test]
fn when_allow_billing_is_unused_then_it_does_not_approve_future_billing_changes() {
    let client = NativeClient::start();
    client.connect();

    let output = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "remote", "--allow-billing"],
    );
    let connection: Value = serde_json::from_slice(
        &std::fs::read(client.home.join(".codexctl/central/remote.json")).unwrap(),
    )
    .unwrap();

    assert!(output.status.success());
    assert_eq!(connection["allow_billing"], false);
}

#[test]
fn when_no_remote_provider_is_active_then_invalid_codex_configuration_does_not_block_local_save() {
    let client = NativeClient::start();
    std::fs::write(client.home.join(".codex/config.toml"), "invalid = [").unwrap();
    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["save", "local"]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("TOML"));
}

#[test]
fn when_a_local_alias_collides_with_a_remote_alias_then_activation_refuses_without_changes() {
    let client = NativeClient::start();
    client.connect();
    std::fs::create_dir_all(client.home.join(".codexctl/profiles/remote")).unwrap();
    let before = std::fs::read(client.home.join(".codex/config.toml")).unwrap();
    let output = client.select();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("alias"));
    assert_eq!(
        std::fs::read(client.home.join(".codex/config.toml")).unwrap(),
        before
    );
}

#[test]
fn when_a_daemon_is_running_then_remote_activation_refuses_without_changes() {
    let client = NativeClient::start();
    client.connect();
    let directory = client.home.join(".codex/app-server-daemon");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("daemon.pid"),
        serde_json::to_vec(&json!({"pid": std::process::id()})).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(client.home.join(".codex/config.toml")).unwrap();
    let output = client.select();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon"));
    assert_eq!(
        std::fs::read(client.home.join(".codex/config.toml")).unwrap(),
        before
    );
}

fn server_restart_command(client: &NativeClient) -> Command {
    use std::os::unix::fs::PermissionsExt;
    let home = client.home.join(".codex");
    std::fs::create_dir_all(home.join("app-server-daemon")).unwrap();
    std::fs::write(
        home.join("app-server-daemon/daemon.pid"),
        serde_json::to_vec(&json!({"pid": std::process::id()})).unwrap(),
    )
    .unwrap();
    let bin = client.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("codex"),
        "#!/bin/sh\n[ \"$*\" = 'app-server daemon restart' ] || exit 2\n/bin/cat \"$CODEX_HOME/config.toml\" > \"$CODEX_HOME/restart-config.toml\"\necho '{\"status\":\"restarted\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl"));
    command
        .args(["use", "remote", "--restart-daemon"])
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .env("HOME", &client.home)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS");
    command
}

#[test]
fn server_switch_with_restart_daemon_restarts_after_provider_rewrite() {
    let client = NativeClient::start();
    client.connect();
    let home = client.home.join(".codex");
    let output = server_restart_command(&client).output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: toml_edit::DocumentMut = std::fs::read_to_string(home.join("restart-config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(config["model_provider"].as_str(), Some("codexctl-central"));
    assert_eq!(
        std::fs::read(home.join("auth.json")).unwrap(),
        NativeClient::local_auth_bytes()
    );
}

#[test]
fn server_restart_flag_does_not_start_an_absent_daemon() {
    let client = NativeClient::start();
    client.connect();
    let mut command = server_restart_command(&client);
    std::fs::remove_file(client.home.join(".codex/app-server-daemon/daemon.pid")).unwrap();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!client.home.join(".codex/restart-config.toml").exists());
}

#[test]
fn server_restart_flag_does_not_approve_billing_or_restart_a_rejected_switch() {
    let client = NativeClient::with_plan(Some("usage_based"));
    client.connect();
    let before = std::fs::read(client.home.join(".codex/config.toml")).unwrap();
    let output = server_restart_command(&client).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-billing"));
    assert_eq!(
        std::fs::read(client.home.join(".codex/config.toml")).unwrap(),
        before
    );
    assert!(!client.home.join(".codex/restart-config.toml").exists());
}

#[test]
fn server_restart_resumes_running_and_limited_sessions_with_local_flags() {
    server_restart_sessions(false, false);
}

#[test]
fn server_restart_reports_failed_sessions_and_resumes_the_others() {
    server_restart_sessions(true, false);
}

#[test]
fn server_restart_failure_reports_the_provider_is_already_active() {
    server_restart_sessions(false, true);
}

fn server_restart_sessions(fail_resume: bool, fail_restart: bool) {
    // Keep the Unix socket path below macOS's length limit.
    let short = tempfile::Builder::new()
        .prefix("b12")
        .tempdir_in("/tmp")
        .unwrap();
    let mut client = NativeClient::start();
    let relocated = short.path().join("client");
    std::fs::rename(&client.home, &relocated).unwrap();
    client.home = relocated;
    client.connect();
    let home = client.home.join(".codex");
    // The old daemon matches the preserved local auth file. That must not
    // suppress a restart onto the selected server provider.
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "sub":"unrelated-local-login", "exp":4102444800_u64,
            "https://api.openai.com/auth":{"chatgpt_account_id":"unrelated-local-seat"},
            "https://api.openai.com/profile":{"email":"local@test"}
        }))
        .unwrap(),
    );
    let local_auth = serde_json::to_vec(&json!({"tokens":{"access_token":format!("header.{payload}."),"account_id":"unrelated-local-seat"}})).unwrap();
    store::atomic_write(&home.join("auth.json"), &local_auth).unwrap();
    let mut command = server_restart_command(&client);
    let sessions = home.join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let metadata = b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n";
    for thread in ["running", "limited", "done"] {
        std::fs::write(sessions.join(format!("rollout-{thread}.jsonl")), metadata).unwrap();
    }
    // The running rollout is old but open; the usage-limited rollout is recent.
    // Both must retain their saved provider when activation repairs other files.
    let held = std::fs::File::open(sessions.join("rollout-running.jsonl")).unwrap();
    held.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
    )
    .unwrap();
    let daemon = daemon_fixture::Daemon::start(&home, fail_resume);
    if fail_restart {
        std::fs::write(
            client.home.join("bin/codex"),
            "#!/bin/sh\necho 'synthetic restart failure' >&2\nexit 1\n",
        )
        .unwrap();
    }

    let output = command.output().unwrap();
    let seen = daemon.finish();
    assert_eq!(
        std::fs::read(sessions.join("rollout-running.jsonl")).unwrap(),
        metadata
    );
    assert_eq!(
        std::fs::read(sessions.join("rollout-limited.jsonl")).unwrap(),
        metadata
    );
    drop(held);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.success(),
        !fail_resume && !fail_restart,
        "{stderr}"
    );
    assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), local_auth);
    let config: toml_edit::DocumentMut = std::fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(config["model_provider"].as_str(), Some("codexctl-central"));
    let turns: Vec<_> = seen
        .iter()
        .filter(|request| request["method"] == "turn/start")
        .collect();
    if fail_restart {
        assert!(
            stderr.contains("server account is active; daemon restart failed"),
            "{stderr}"
        );
        assert!(stderr.contains("synthetic restart failure"), "{stderr}");
        assert!(turns.is_empty());
        return;
    }
    assert!(home.join("restart-config.toml").exists());
    for request in seen.iter().filter(|r| r["method"] == "thread/resume") {
        assert_eq!(request["params"]["modelProvider"], "codexctl-central");
    }
    let resumed: Vec<_> = turns
        .iter()
        .map(|request| request["params"]["threadId"].as_str().unwrap())
        .collect();
    assert_eq!(
        resumed,
        if fail_resume {
            vec!["limited"]
        } else {
            vec!["running", "limited"]
        }
    );
    for request in turns {
        assert_eq!(request["params"]["approvalPolicy"], "never");
        assert_eq!(
            request["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        assert_eq!(
            request["params"]["input"][0]["text"],
            "Continue the previous request."
        );
    }
    for request in seen
        .iter()
        .filter(|r| r["method"] == "thread/resume" && r["params"]["approvalPolicy"].is_string())
    {
        assert_eq!(request["params"]["approvalPolicy"], "never");
        assert_eq!(request["params"]["sandbox"], "danger-full-access");
    }
    if fail_resume {
        assert!(stderr.contains("1 session did not resume"), "{stderr}");
        assert!(
            stderr.contains("codex resume <id>` for: running"),
            "{stderr}"
        );
    }
}

#[test]
fn when_a_new_configuration_write_fails_then_its_marker_and_billing_approval_are_not_installed() {
    use std::os::unix::fs::PermissionsExt;
    let client = NativeClient::with_plan(Some("usage_based"));
    client.connect();
    let home = client.home.join(".codex");
    std::fs::remove_file(home.join("config.toml")).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
    let output = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "remote", "--allow-billing"],
    );
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!output.status.success());
    assert!(
        !client
            .home
            .join(".codexctl/central/.native-active.json")
            .exists()
    );
    let connection: Value = serde_json::from_slice(
        &std::fs::read(client.home.join(".codexctl/central/remote.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(connection["allow_billing"], false);
}

#[test]
fn server_switch_config_write_failure_keeps_the_previous_pointer() {
    // Root can write through mode 0500, so this permission-based failure is not
    // meaningful in the root-owned development container.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let client = NativeClient::with_plan(Some("self_serve_business_usage_based"));
    client.connect();
    client.connect_alias("other");
    let first = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "remote", "--allow-billing"],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let connection_path = client.home.join(".codexctl/central/other.json");
    let mut connection: Value =
        serde_json::from_slice(&std::fs::read(&connection_path).unwrap()).unwrap();
    connection["allow_billing"] = Value::Bool(false);
    store::atomic_write(&connection_path, &serde_json::to_vec(&connection).unwrap()).unwrap();

    let config = client.home.join(".codex/config.toml");
    let readonly = client.home.join("readonly-config");
    std::fs::create_dir(&readonly).unwrap();
    let target = readonly.join("config.toml");
    std::fs::rename(&config, &target).unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

    let failed = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "other", "--allow-billing"],
    );

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("Permission denied"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(client.home.join(".codexctl/central/.active-account"))
            .unwrap()
            .trim(),
        "remote"
    );
    let connection: Value =
        serde_json::from_slice(&std::fs::read(connection_path).unwrap()).unwrap();
    assert_eq!(connection["allow_billing"], false);
}

#[test]
fn when_the_default_codex_profile_overrides_the_provider_then_remote_activation_refuses() {
    let client = NativeClient::start();
    client.connect();
    let config = client.home.join(".codex/config.toml");
    let text = "profile = 'work'\n[profiles.work]\nmodel_provider = 'openai'\n";
    std::fs::write(&config, text).unwrap();
    let output = client.select();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("profile"));
    assert_eq!(std::fs::read_to_string(config).unwrap(), text);
}

#[test]
fn when_the_remote_daemon_is_running_then_disconnect_keeps_the_marker_and_configuration() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let directory = client.home.join(".codex/app-server-daemon");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("daemon.pid"),
        serde_json::to_vec(&json!({"pid":std::process::id()})).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(client.home.join(".codex/config.toml")).unwrap();
    let output = client.run(env!("CARGO_BIN_EXE_codexctl-central"), &["disconnect"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon"));
    assert!(
        client
            .home
            .join(".codexctl/central/.native-active.json")
            .exists()
    );
    assert_eq!(
        std::fs::read(client.home.join(".codex/config.toml")).unwrap(),
        before
    );
}

#[test]
fn when_an_organizational_plan_has_paid_credits_then_automatic_remote_selection_is_refused() {
    let client = NativeClient::with_plan(Some("team"));
    std::fs::write(client.broker.root.path().join("mode"), "credits").unwrap();
    client.connect();
    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("billing"));
}

#[test]
fn when_subscription_credits_change_with_headroom_then_the_helper_needs_no_approval() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    std::fs::write(client.broker.root.path().join("mode"), "credits").unwrap();
    let output = client.helper();
    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
}

#[test]
fn when_a_pinned_shell_sees_the_remote_provider_then_the_helper_refuses_credentials() {
    let client = NativeClient::start();
    client.connect();
    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["central-token", "--connection"])
        .arg(client.home.join(".codexctl/central/remote.json"))
        .env("HOME", &client.home)
        .env("CODEXCTL_PINNED_ALIAS", "local")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("pinned"));
}

#[test]
fn active_token_helper_reads_the_pointer_and_fails_closed_when_it_is_invalid() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let config = std::fs::read_to_string(client.home.join(".codex/config.toml")).unwrap();
    assert!(config.contains("central-token"));
    assert!(config.contains("--active"), "{config}");

    let pointer = client.home.join(".codexctl/central/.active-account");
    std::fs::write(&pointer, "remote \n").unwrap();
    let output = client.helper_active();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active account pointer"));
}

#[test]
fn a_slow_token_fetch_does_not_hold_native_lock() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    std::fs::write(client.broker.root.path().join("mode"), "billing-slow").unwrap();
    let first = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["central-token", "--active"])
        .env("HOME", &client.home)
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let marker = client.broker.root.path().join("billing-started");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let lock_probe_started = std::time::Instant::now();
    let lock_probe = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["session-provider", "dry-run"],
    );
    let lock_probe_elapsed = lock_probe_started.elapsed();
    assert!(
        lock_probe.status.success(),
        "{}",
        String::from_utf8_lossy(&lock_probe.stderr)
    );
    assert!(
        lock_probe_elapsed < Duration::from_secs(2),
        "native lock was held during the slow fetch: {lock_probe_elapsed:?}"
    );
    let second = client.helper_active();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(first.wait_with_output().unwrap().status.success());
}

#[test]
fn when_an_inherited_home_is_local_then_global_save_still_refuses_remote_mode() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    let isolated = client.home.join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["save", "local"])
        .env("HOME", &client.home)
        .env("CODEX_HOME", isolated)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("remote provider is active"));
}

#[test]
fn when_the_billing_read_rotates_credentials_then_restart_keeps_the_returned_token() {
    let mut client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "billing-rotation").unwrap();
    let output = client.helper();
    assert!(output.status.success());
    let token = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    unsafe {
        libc::kill(client.broker.child.id() as i32, libc::SIGINT);
    }
    assert!(client.broker.child.wait().unwrap().success());
    client.broker.restart_read_only();
    assert_eq!(client.broker.grant()["accessToken"], token);
}

#[test]
fn when_the_billing_read_fails_then_the_helper_returns_no_token_and_one_failure_is_counted() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "billing-error").unwrap();
    let output = client.helper();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let counters = client
        .broker
        .http
        .get(format!("{}/metrics", client.broker.url))
        .bearer_auth(&client.broker.token)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert_eq!(
        failed_request_metrics(&counters),
        ["codexctl_central_failed_requests_total{reason=\"owner_unavailable\"} 1"]
    );
}

#[test]
fn when_billing_approval_is_withdrawn_during_a_refresh_then_the_helper_returns_no_token() {
    let client = NativeClient::with_plan(Some("usage_based"));
    client.connect();
    assert!(
        client
            .run(
                env!("CARGO_BIN_EXE_codexctl"),
                &["use", "remote", "--allow-billing"]
            )
            .status
            .success()
    );
    // Seed the same parent's healthy call; the immediate retry below forces refresh.
    assert!(client.helper().status.success());
    std::fs::write(client.broker.root.path().join("mode"), "disconnect").unwrap();
    let path = client.home.join(".codexctl/central/remote.json");
    let helper = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["central-token", "--connection"])
        .arg(&path)
        .env("HOME", &client.home)
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !client.broker.root.path().join("refresh-started").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut connection: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    connection["allow_billing"] = json!(false);
    store::atomic_write(&path, &serde_json::to_vec(&connection).unwrap()).unwrap();
    let output = helper.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn selecting_a_server_account_repairs_saved_session_providers() {
    let client = NativeClient::start();
    client.connect();
    let rollout = client.home.join(".codex/sessions/rollout-old.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let old = "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n";
    std::fs::write(&rollout, old).unwrap();
    std::fs::File::open(&rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
    let result = client.select();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(rollout).unwrap(),
        old.replace("openai", "codexctl-central")
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("rewritten=1"));
}

#[test]
fn repair_failure_reports_that_the_server_account_is_already_active() {
    let client = NativeClient::start();
    client.connect();
    let rollout = client.home.join(".codex/sessions/rollout-malformed.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    std::fs::write(&rollout, b"not JSON\n").unwrap();
    std::fs::File::open(&rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
    let result = client.select();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("account remote is active; session repair failed"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(rollout).unwrap(), b"not JSON\n");
}

#[test]
fn server_restart_activation_failure_warns_daemon_still_runs_old_account() {
    let client = NativeClient::start();
    client.connect();
    let rollout = client.home.join(".codex/sessions/rollout-malformed.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    std::fs::write(&rollout, b"not JSON\n").unwrap();
    std::fs::File::open(&rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();

    let output = server_restart_command(&client).output().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("account remote is active; session repair failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("daemon still runs the old account"),
        "{stderr}"
    );
    assert!(stderr.contains("--restart-daemon"), "{stderr}");
    assert!(!client.home.join(".codex/restart-config.toml").exists());
    let config: toml_edit::DocumentMut =
        std::fs::read_to_string(client.home.join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert_eq!(config["model_provider"].as_str(), Some("codexctl-central"));
}

#[test]
fn when_subscription_cap_is_closed_then_automatic_use_and_helper_succeed() {
    let client = NativeClient::with_plan(Some("team"));
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "closed-spend-cap").unwrap();

    let selected = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]);
    let token = client.helper();

    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
}

#[test]
fn when_subscription_cap_is_closed_then_explicit_use_needs_no_consent() {
    let client = NativeClient::with_plan(Some("team"));
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "closed-spend-cap").unwrap();

    let selected = client.select();

    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
}

#[test]
fn when_subscription_cap_reopens_then_the_helper_refuses_token_delivery() {
    let client = NativeClient::with_plan(Some("team"));
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "closed-spend-cap").unwrap();
    let selected = client.select();
    std::fs::write(client.broker.root.path().join("mode"), "open-spend-cap").unwrap();

    let token = client.helper();

    assert!(selected.status.success());
    assert!(!token.status.success());
    assert!(token.stdout.is_empty());
}

#[test]
fn when_subscription_has_included_headroom_then_automatic_use_and_helper_succeed() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "included-weekly").unwrap();

    let selected = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use"]);
    let helper = client.helper();

    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        helper.status.success(),
        "{}",
        String::from_utf8_lossy(&helper.stderr)
    );
}

#[test]
fn when_subscription_has_included_headroom_then_explicit_use_needs_no_billing_consent() {
    let client = NativeClient::with_plan(Some("plus"));
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "included-weekly").unwrap();

    let selected = client.select();

    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
}

#[test]
fn when_included_headroom_exhausts_then_helper_returns_no_token() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "included-weekly").unwrap();
    let selected = client.select();
    let initial = client.helper();
    std::fs::write(client.broker.root.path().join("mode"), "exhausted-weekly").unwrap();
    // Require a new native billing observation beyond the recent-token guard.
    std::fs::write(
        client.broker.root.path().join("retry-clock"),
        (chrono::Utc::now().timestamp_millis() + 61_000).to_string(),
    )
    .unwrap();

    let helper = client.helper();

    assert!(selected.status.success() && initial.status.success());
    assert!(!helper.status.success());
    assert!(helper.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&helper.stderr);
    assert!(
        stderr.contains("remote account remote is exhausted"),
        "{stderr}"
    );
    assert!(stderr.contains("run codexctl use"), "{stderr}");
}

#[test]
fn when_included_headroom_is_exhausted_then_explicit_use_requires_consent() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "exhausted-weekly").unwrap();

    let selected = client.select();

    assert!(!selected.status.success());
    assert!(String::from_utf8_lossy(&selected.stderr).contains("--allow-billing"));
}

#[test]
fn when_remote_use_succeeds_then_provider_refreshes_every_minute() {
    let client = NativeClient::start();
    client.connect();
    let selected = client.select();
    let config: toml_edit::DocumentMut =
        std::fs::read_to_string(client.home.join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();

    assert!(selected.status.success());
    assert_eq!(
        config["model_providers"]["codexctl-central"]["auth"]["refresh_interval_ms"].as_integer(),
        Some(60_000)
    );
    assert_eq!(
        config["model_providers"]["codexctl-central"]["request_max_retries"].as_integer(),
        Some(12)
    );
    assert_eq!(
        config["model_providers"]["codexctl-central"]["stream_max_retries"].as_integer(),
        Some(12)
    ); // The relay reads HTTPS event streams; a WebSocket session would bypass it.
    assert!(
        config["model_providers"]["codexctl-central"]
            .get("supports_websockets")
            .and_then(toml_edit::Item::as_bool)
            .is_none_or(|enabled| !enabled)
    );
}

#[test]
fn when_remote_use_rewrites_provider_then_retry_limits_are_preserved() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());

    let path = client.home.join(".codex/config.toml");
    let mut config: toml_edit::DocumentMut =
        std::fs::read_to_string(&path).unwrap().parse().unwrap();
    config["model_providers"]["codexctl-central"]["request_max_retries"] = toml_edit::value(4);
    config["model_providers"]["codexctl-central"]["stream_max_retries"] = toml_edit::value(7);
    std::fs::write(&path, config.to_string()).unwrap();

    let selected = client.select();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let rewritten: toml_edit::DocumentMut = std::fs::read_to_string(path).unwrap().parse().unwrap();
    assert_eq!(
        rewritten["model_providers"]["codexctl-central"]["request_max_retries"].as_integer(),
        Some(4)
    );
    assert_eq!(
        rewritten["model_providers"]["codexctl-central"]["stream_max_retries"].as_integer(),
        Some(7)
    );
}

#[test]
fn when_included_headroom_is_exhausted_then_consent_allows_use_and_refresh() {
    let client = NativeClient::start();
    client.connect();
    std::fs::write(client.broker.root.path().join("mode"), "exhausted-weekly").unwrap();
    let selected = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["use", "remote", "--allow-billing"],
    );
    let helper = client.helper();

    assert!(selected.status.success());
    assert!(helper.status.success());
}

#[test]
fn when_repaired_sessions_remain_then_disconnect_requires_restore() {
    let client = NativeClient::start();
    client.connect();
    let rollout = client.home.join(".codex/sessions/rollout-old.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    std::fs::write(
        &rollout,
        b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n",
    )
    .unwrap();
    std::fs::File::open(&rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
    assert!(client.select().status.success());

    let output = client.run(env!("CARGO_BIN_EXE_codexctl-central"), &["disconnect"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session-provider restore"));
    assert!(
        client
            .home
            .join(".codexctl/central/.native-active.json")
            .exists()
    );

    let restored = client.run(
        env!("CARGO_BIN_EXE_codexctl"),
        &["session-provider", "restore"],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(
        client
            .run(env!("CARGO_BIN_EXE_codexctl-central"), &["disconnect"])
            .status
            .success()
    );
}

#[test]
fn when_a_recent_repaired_session_is_archived_then_local_selection_requires_restore() {
    let client = NativeClient::start();
    let paths = codexctl::config::Paths::from_home(client.home.clone());
    codexctl::profile::save_profile_to(&paths, "local", None, &paths.codex_auth_json()).unwrap();
    client.connect();
    let rollout = client.home.join(".codex/sessions/rollout-old.jsonl");
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    std::fs::write(
        &rollout,
        b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n",
    )
    .unwrap();
    std::fs::File::open(&rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
    assert!(client.select().status.success());
    let archived = client
        .home
        .join(".codex/archived_sessions/rollout-old.jsonl");
    std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
    std::fs::rename(&rollout, &archived).unwrap();
    std::fs::File::open(&archived)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .unwrap();

    let output = client.run(env!("CARGO_BIN_EXE_codexctl"), &["use", "local"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session-provider restore"));
    assert!(
        std::fs::read_to_string(client.home.join(".codex/config.toml"))
            .unwrap()
            .contains("model_provider = \"codexctl-central\"")
    );
}

#[test]
fn statusline_token_helper_populates_cache_without_exposing_credentials() {
    let client = NativeClient::start();
    client.connect();
    assert!(client.select().status.success());
    store::atomic_write(&client.broker.root.path().join("mode"), b"status-reset").unwrap();

    let helper = client.helper();
    let line = client.run(env!("CARGO_BIN_EXE_codexctl"), &["statusline"]);

    assert!(helper.status.success());
    assert!(String::from_utf8_lossy(&line.stdout).starts_with("remote 63% wk · "));
    assert!(String::from_utf8_lossy(&line.stdout).ends_with(" · 100% 5h\n"));
}
