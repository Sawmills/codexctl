#![cfg(feature = "central-prototype")]
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use codexctl::{central, store};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Server {
    root: tempfile::TempDir,
    child: Child,
    url: String,
    http: reqwest::blocking::Client,
    amir: String,
    alex: String,
}

#[test]
fn readiness_reports_file_mode_and_registry_health() {
    let server = Server::start();
    let ready = || {
        server
            .http
            .get(format!("{}/ready", server.url))
            .send()
            .unwrap()
    };
    let response = ready();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().unwrap(),
        json!({
            "ready": true, "storeMode": "file", "databaseReachable": null
        })
    );
    std::fs::write(
        server.root.path().join("state/users.json"),
        b"invalid registry",
    )
    .unwrap();
    let response = ready();
    assert_eq!(response.status(), 503);
    assert_eq!(response.json::<Value>().unwrap()["ready"], false);
}
fn auth(subject: &str, account: &str) -> Value {
    let payload=URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":subject,"iat":2000000000_u64,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}})).unwrap());
    json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"synthetic-refresh","account_id":account}})
}
impl Server {
    fn login_request(
        &self,
        token: &str,
        operation: &str,
        alias: &str,
        id: &str,
    ) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}/v1/relogin/{operation}", self.url))
            .bearer_auth(token)
            .json(&json!({"alias":alias,"id":id}))
            .send()
            .unwrap()
    }
    fn await_login(&self, token: &str, alias: &str, id: &str, terminal: &str) -> Value {
        for _ in 0..200 {
            let status: Value = self
                .login_request(token, "status", alias, id)
                .json()
                .unwrap();
            if status["status"] == terminal {
                return status;
            }
            if matches!(
                status["status"].as_str(),
                Some("completed" | "failed" | "canceled")
            ) {
                panic!("unexpected login result: {}", status["status"]);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("login did not finish");
    }
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        central::managed::setup(&state, &key).unwrap();
        store::atomic_write(
            &state.join("users.json"),
            &serde_json::to_vec(&vec![
                central::managed::User {
                    id: "amir".into(),
                    email: "amir@sawmills.ai".into(),
                    enabled: true,
                    oidc_identity: None,
                },
                central::managed::User {
                    id: "alex".into(),
                    email: "alex@sawmills.ai".into(),
                    enabled: true,
                    oidc_identity: None,
                },
            ])
            .unwrap(),
        )
        .unwrap();
        central::register(
            &state,
            "amir-laptop",
            "sawmills",
            "amir",
            &root.path().join("amir.token"),
        )
        .unwrap();
        central::register(
            &state,
            "alex-laptop",
            "sawmills",
            "alex",
            &root.path().join("alex.token"),
        )
        .unwrap();
        store::atomic_write(
            &root.path().join("metrics.token"),
            b"synthetic-monitoring-credential-only",
        )
        .unwrap();
        store::atomic_write(&root.path().join("mode"), b"").unwrap();
        store::atomic_write(&root.path().join("count"), b"0").unwrap();
        let (child, url) = Self::spawn(&root);
        let amir = std::fs::read_to_string(root.path().join("amir.token")).unwrap();
        let alex = std::fs::read_to_string(root.path().join("alex.token")).unwrap();
        Self {
            root,
            child,
            url,
            http: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            amir,
            alex,
        }
    }
    fn spawn(root: &tempfile::TempDir) -> (Child, String) {
        Self::spawn_binary(
            root,
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"),
        )
    }
    fn spawn_binary(root: &tempfile::TempDir, binary: &std::path::Path) -> (Child, String) {
        Self::spawn_binary_with_flags(root, binary, &[])
    }
    fn spawn_binary_with_flags(
        root: &tempfile::TempDir,
        binary: &std::path::Path,
        flags: &[&str],
    ) -> (Child, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(["serve", "--state"])
            .arg(root.path().join("state"))
            .arg("--key-file")
            .arg(root.path().join("key"))
            .arg("--metrics-token-file")
            .arg(root.path().join("metrics.token"))
            .args([
                "--listen",
                "127.0.0.1:0",
                "--public-url",
                "http://127.0.0.1:8787",
                "--codex-bin",
            ])
            .arg(binary)
            .args(flags)
            // Synthetic access credentials must never reach an external usage API.
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("https_proxy", "http://127.0.0.1:1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("CENTRAL_TEST_MODE_FILE", root.path().join("mode"))
            .env("CENTRAL_TEST_REFRESH_COUNTER", root.path().join("count"))
            .env("CENTRAL_TEST_KEY_FILE", root.path().join("key"))
            .env("CENTRAL_TEST_OWNER_CWD_FILE", root.path().join("owner-cwd"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: Value = serde_json::from_str(&line).expect("managed server must start");
        (
            child,
            format!("http://{}", ready["listening"].as_str().unwrap()),
        )
    }
    fn import(
        &self,
        token: &str,
        alias: &str,
        subject: &str,
        account: &str,
    ) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}/v1/accounts", self.url))
            .bearer_auth(token)
            .json(&json!({"alias":alias,"label":"Personal","auth":auth(subject,account)}))
            .send()
            .unwrap()
    }
    fn accounts(&self, token: &str) -> Value {
        self.http
            .get(format!("{}/v1/accounts", self.url))
            .bearer_auth(token)
            .send()
            .unwrap()
            .json()
            .unwrap()
    }
    fn token(
        &self,
        token: &str,
        alias: &str,
        previous: Option<&str>,
    ) -> reqwest::blocking::Response {
        self.http
            .post(format!("{}/v1/token", self.url))
            .bearer_auth(token)
            .json(&json!({"alias":alias,"previousRevision":previous,"billing":true}))
            .send()
            .unwrap()
    }
    fn stop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        assert!(self.child.wait().unwrap().success());
    }
    fn restart(&mut self) {
        let (child, url) = Self::spawn(&self.root);
        self.child = child;
        self.url = url;
    }
    fn cli(&self, home: &std::path::Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(args)
            .env("HOME", home)
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .output()
            .unwrap()
    }
    fn connected_home(&self) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".codexctl/central");
        store::ensure_private_dir(&directory).unwrap();
        let token = directory.join(".device.token");
        store::atomic_write(&token, self.amir.as_bytes()).unwrap();
        store::atomic_write(
            &directory.join(".server.json"),
            &serde_json::to_vec(&json!({"server":self.url,"token_file":token,"user_id":"amir"}))
                .unwrap(),
        )
        .unwrap();
        home
    }
}

#[test]
fn server_relogin_rejects_and_retains_another_login_in_the_same_workspace() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "shared-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "work", "alex-login", "shared-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "other", "other-login", "other-seat")
            .status()
            .is_success()
    );
    let before: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let id = "b".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("alex-login", "shared-seat")).unwrap(),
    )
    .unwrap();
    let failed = server.await_login(&server.amir, "personal", &id, "failed");
    assert_eq!(failed["error"], "wrong_account");
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(server.token(&server.alex, "work", None).status(), 503);
    assert_eq!(server.token(&server.alex, "other", None).status(), 200);
    assert_eq!(
        server
            .import(&server.amir, "stolen", "alex-login", "shared-seat")
            .status(),
        409
    );
    let original = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.join(format!("relogin/{id}/record.json")).exists())
        .unwrap();
    let retained: Value = serde_json::from_slice(
        &std::fs::read(original.join(format!("relogin/{id}/record.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(retained["candidate"]["tokens"]["account_id"], "shared-seat");
    let original_auth: Value =
        serde_json::from_slice(&std::fs::read(original.join("runtime/auth.json")).unwrap())
            .unwrap();
    assert_eq!(
        original_auth["tokens"]["access_token"],
        before["accessToken"]
    );
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.alex, "work", None).status(), 503);
    assert_eq!(server.token(&server.alex, "other", None).status(), 200);
}

#[test]
fn hq_same_device_resumes_but_other_devices_cannot_read_or_cancel() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    central::register(
        &server.root.path().join("state"),
        "amir-desktop",
        "sawmills",
        "amir",
        &server.root.path().join("desktop.token"),
    )
    .unwrap();
    let desktop = std::fs::read_to_string(server.root.path().join("desktop.token")).unwrap();
    let id = "c".repeat(64);
    let first: Value = server
        .login_request(&server.amir, "start", "personal", &id)
        .json()
        .unwrap();
    let resumed: Value = server
        .login_request(&server.amir, "start", "personal", &"d".repeat(64))
        .json()
        .unwrap();
    assert_eq!(first["id"], resumed["id"]);
    for operation in ["start", "status", "cancel"] {
        assert_eq!(
            server
                .login_request(&desktop, operation, "personal", &id)
                .status(),
            409
        );
    }
    assert_eq!(
        server
            .login_request(&server.alex, "cancel", "personal", &id)
            .status(),
        404
    );
    assert_eq!(
        server
            .login_request(&server.amir, "cancel", "personal", &id)
            .status(),
        200
    );
    server.await_login(&server.amir, "personal", &id, "canceled");
    assert_eq!(server.token(&desktop, "personal", None).status(), 503);
    assert_eq!(
        server
            .login_request(&desktop, "start", "personal", &"e".repeat(64))
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&desktop, "cancel", "personal", &"e".repeat(64))
            .status(),
        200
    );
    server.await_login(&desktop, "personal", &"e".repeat(64), "canceled");
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
}

#[test]
fn server_login_uses_the_existing_cli_without_changing_local_credentials() {
    server_login_cli_scenario(false, true);
}

#[test]
fn server_login_renews_while_a_server_provider_is_active() {
    server_login_cli_scenario(true, true);
}

#[test]
fn server_login_discovers_an_alias_without_a_cached_catalog() {
    server_login_cli_scenario(false, false);
}

fn server_login_cli_scenario(active: bool, cached: bool) {
    let server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".codexctl/central");
    store::ensure_private_dir(&directory).unwrap();
    let token = directory.join("device.token");
    store::atomic_write(&token, server.amir.as_bytes()).unwrap();
    store::atomic_write(
        &directory.join(".server.json"),
        &serde_json::to_vec(&json!({"server":server.url,"token_file":token,"user_id":"amir"}))
            .unwrap(),
    )
    .unwrap();
    if cached {
        assert!(server.cli(home.path(), &["list"]).status.success());
    } else {
        assert!(!directory.join(".catalog.json").exists());
    }
    if active {
        let selected = server.cli(home.path(), &["use", "personal", "--allow-billing"]);
        assert!(
            selected.status.success(),
            "{}",
            String::from_utf8_lossy(&selected.stderr)
        );
    }
    let marker = directory.join(".native-active.json");
    let active_before = std::fs::read(&marker).ok();
    let config_file = home.path().join(".codex/config.toml");
    let config_before = std::fs::read(&config_file).ok();
    let auth_file = home.path().join(".codex/auth.json");
    store::ensure_private_dir(auth_file.parent().unwrap()).unwrap();
    store::atomic_write(&auth_file, b"local-auth-sentinel").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    let output = server.cli(home.path(), &["login", "personal", "--no-browser"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Server login renewed"));
    assert_eq!(std::fs::read(&marker).ok(), active_before);
    assert_eq!(std::fs::read(&config_file).ok(), config_before);
    assert_eq!(std::fs::read(auth_file).unwrap(), b"local-auth-sentinel");
    assert!(!home.path().join(".codexctl/login-homes").exists());
    if active {
        let local = server.cli(home.path(), &["login", "new-local-profile"]);
        assert!(!local.status.success());
        assert!(String::from_utf8_lossy(&local.stderr).contains("remote provider is active"));
        assert!(!home.path().join(".codexctl/login-homes").exists());
    }
}

#[test]
fn server_exec_explains_unsupported_pinning_with_an_active_provider() {
    server_exec_unsupported_scenario(true, false);
}

#[test]
fn server_exec_explains_unsupported_pinning_without_prior_discovery() {
    server_exec_unsupported_scenario(false, false);
}

#[test]
fn server_exec_explains_unsupported_pinning_when_the_cached_server_is_offline() {
    server_exec_unsupported_scenario(false, true);
}

fn server_exec_unsupported_scenario(active: bool, offline: bool) {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    if active {
        let selected = server.cli(home.path(), &["use", "personal", "--allow-billing"]);
        assert!(
            selected.status.success(),
            "{}",
            String::from_utf8_lossy(&selected.stderr)
        );
    }
    if offline {
        assert!(server.cli(home.path(), &["list"]).status.success());
        server.stop();
    }
    let config = home.path().join(".codex/config.toml");
    let marker = home.path().join(".codexctl/central/.native-active.json");
    let config_before = std::fs::read(&config).ok();
    let marker_before = std::fs::read(&marker).ok();
    let auth_file = home.path().join(".codex/auth.json");
    store::ensure_private_dir(auth_file.parent().unwrap()).unwrap();
    store::atomic_write(&auth_file, b"local-auth-sentinel").unwrap();
    let result = server.cli(
        home.path(),
        &[
            "exec",
            "--account",
            "personal",
            "--",
            "sh",
            "-c",
            "echo CHILD_STARTED",
        ],
    );
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(
        error.contains("pinned execution of server accounts is not supported"),
        "{error}"
    );
    assert!(error.contains("codexctl use <alias>"), "{error}");
    assert!(error.contains("codexctl codex"), "{error}");
    assert!(error.contains("changes the active account"), "{error}");
    assert!(!String::from_utf8_lossy(&result.stdout).contains("CHILD_STARTED"));
    assert!(!home.path().join(".codexctl/exec-homes/personal").exists());
    assert_eq!(std::fs::read(&config).ok(), config_before);
    assert_eq!(std::fs::read(&marker).ok(), marker_before);
    assert_eq!(std::fs::read(&auth_file).unwrap(), b"local-auth-sentinel");
}

#[test]
fn unknown_exec_alias_keeps_the_missing_profile_error_on_a_connected_machine() {
    let server = Server::start();
    let home = server.connected_home();
    let result = server.cli(home.path(), &["exec", "--account", "missing", "--", "true"]);
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(error.contains("profile 'missing' not found"), "{error}");
    assert!(
        !error.contains("pinned execution of server accounts is not supported"),
        "{error}"
    );
    assert!(!home.path().join(".codexctl/exec-homes/missing").exists());
}

#[test]
fn local_exec_guard_does_not_claim_server_pinning_is_unsupported() {
    local_exec_error_scenario(true);
}

#[test]
fn corrupt_local_exec_metadata_keeps_its_specific_error() {
    local_exec_error_scenario(false);
}

fn local_exec_error_scenario(active: bool) {
    let server = Server::start();
    let home = server.connected_home();
    if active {
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        assert!(
            server
                .cli(home.path(), &["use", "personal", "--allow-billing"])
                .status
                .success()
        );
    }
    let profile = home.path().join(".codexctl/profiles/local");
    store::ensure_private_dir(&profile).unwrap();
    store::atomic_write(
        &profile.join("auth.json"),
        &serde_json::to_vec(&auth("local-login", "local-seat")).unwrap(),
    )
    .unwrap();
    let meta: &[u8] = if active {
        br#"{"alias":"local","saved_at":"2026-10-03T00:00:00Z"}"#
    } else {
        b"{"
    };
    store::atomic_write(&profile.join("meta.json"), meta).unwrap();
    let result = server.cli(
        home.path(),
        &["exec", "--account", "local", "--", "echo", "CHILD_STARTED"],
    );
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    if active {
        assert!(error.contains("remote provider is active"), "{error}");
    } else {
        assert!(
            error.contains("failed to parse") && error.contains("meta.json"),
            "{error}"
        );
    }
    assert!(
        !error.contains("pinned execution of server accounts is not supported"),
        "{error}"
    );
    assert!(!error.contains("codexctl use <alias>"), "{error}");
    assert!(!String::from_utf8_lossy(&result.stdout).contains("CHILD_STARTED"));
    assert!(!home.path().join(".codexctl/exec-homes/local").exists());
}

#[test]
fn server_login_revocation_stops_the_native_child_and_rejects_future_status_delivery() {
    let server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status()
            .is_success()
    );
    let id = "1".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    let response = server
        .http
        .post(format!("{}/v1/devices/revoke", server.url))
        .bearer_auth(&server.amir)
        .json(&json!({"id":"amir-laptop"}))
        .send()
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        server
            .login_request(&server.amir, "status", "personal", &id)
            .status(),
        401
    );
    central::register(
        &server.root.path().join("state"),
        "amir-replacement",
        "sawmills",
        "amir",
        &server.root.path().join("replacement.token"),
    )
    .unwrap();
    let replacement =
        std::fs::read_to_string(server.root.path().join("replacement.token")).unwrap();
    assert_eq!(
        server
            .login_request(&replacement, "status", "personal", &id)
            .status(),
        409
    );
    let record_path = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .map(|e| e.unwrap().path().join(format!("relogin/{id}/record.json")))
        .find(|p| p.exists())
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        if record["phase"] == "canceled" {
            assert_eq!(record["child"]["status"], "exited");
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
    let metrics = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth("synthetic-monitoring-credential-only")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"relogin_stopped\"} 1")
    );
}

#[test]
fn identical_operation_ids_do_not_cross_company_user_boundaries() {
    let server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status()
            .is_success()
    );
    let id = "2".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&server.alex, "start", "personal", &id)
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&server.alex, "cancel", "personal", &id)
            .status(),
        200
    );
    server.await_login(&server.alex, "personal", &id, "canceled");
    let amir: Value = server
        .login_request(&server.amir, "status", "personal", &id)
        .json()
        .unwrap();
    assert_eq!(amir["status"], "pending");
    server.login_request(&server.amir, "cancel", "personal", &id);
    server.await_login(&server.amir, "personal", &id, "canceled");
}

#[test]
fn unsupported_native_login_output_is_stopped_and_can_be_retried() {
    let server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    store::atomic_write(&server.root.path().join("mode"), b"login-invalid-prompt").unwrap();
    let id = "3".repeat(64);
    let result: Value = server
        .login_request(&server.amir, "start", "personal", &id)
        .json()
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["userCode"].is_null());
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let retry = "4".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &retry)
            .status(),
        200
    );
    server.login_request(&server.amir, "cancel", "personal", &retry);
    server.await_login(&server.amir, "personal", &retry, "canceled");
}

#[test]
fn restarting_after_native_grant_save_recovers_it_without_another_browser_login() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    store::atomic_write(&server.root.path().join("mode"), b"login-hold-after-save").unwrap();
    let id = "5".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let native = account.join(format!("relogin/{id}/home/auth.json"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !server.root.path().join("login-saved").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(native.exists());
    let process: Value = serde_json::from_slice(
        &std::fs::read(account.join(format!("relogin/{id}/record.json"))).unwrap(),
    )
    .unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    // macOS has no parent-death signal. Stop this test's recorded native child explicitly.
    unsafe {
        libc::kill(
            process["child"]["process"]["pid"].as_i64().unwrap() as i32,
            libc::SIGKILL,
        );
    }
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    server.restart();
    server.await_login(&server.amir, "personal", &id, "completed");
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

#[test]
fn an_interrupted_pending_login_becomes_retryable_after_server_restart() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    let id = "6".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let process: Value = serde_json::from_slice(
        &std::fs::read(account.join(format!("relogin/{id}/record.json"))).unwrap(),
    )
    .unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    unsafe {
        libc::kill(
            process["child"]["process"]["pid"].as_i64().unwrap() as i32,
            libc::SIGKILL,
        );
    }
    std::thread::sleep(Duration::from_millis(100));
    server.restart();
    let previous: Value = server
        .login_request(&server.amir, "start", "personal", &id)
        .json()
        .unwrap();
    assert_eq!(previous["status"], "failed");
    let retry = "7".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &retry)
            .status(),
        200
    );
    server.login_request(&server.amir, "cancel", "personal", &retry);
    server.await_login(&server.amir, "personal", &retry, "canceled");
}

#[test]
fn read_only_restart_never_verifies_a_pending_replacement_and_writable_restart_recovers() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    let id = "8".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"routing-error").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result: Value = server
            .login_request(&server.amir, "status", "personal", &id)
            .json()
            .unwrap();
        if result["error"] == "relogin_failed" {
            assert_eq!(result["status"], "verifying");
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    server.stop();
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let count = std::fs::read_to_string(server.root.path().join("count")).unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
    let (child, url) = Server::spawn_binary_with_flags(&server.root, &fixture, &["--read-only"]);
    server.child = child;
    server.url = url;
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        count
    );
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    server.stop();
    server.restart();
    server.await_login(&server.amir, "personal", &id, "completed");
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

#[test]
fn verified_replacement_finishes_quarantine_retirement_before_recovery_reports_success() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "shared-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "work", "alex-login", "shared-seat")
            .status()
            .is_success()
    );
    let wrong = "9".repeat(64);
    server.login_request(&server.amir, "start", "personal", &wrong);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("alex-login", "shared-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &wrong, "failed");
    std::fs::remove_file(server.root.path().join("login-release")).unwrap();
    let repair = "a".repeat(64);
    server.login_request(&server.alex, "start", "work", &repair);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("alex-login", "shared-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.alex, "work", &repair, "completed");
    server.stop();
    for entry in std::fs::read_dir(server.root.path().join("state/accounts")).unwrap() {
        let account = entry.unwrap().path();
        for (id, stage) in [(&wrong, "failed"), (&repair, "retiring")] {
            let path = account.join(format!("relogin/{id}/record.json"));
            if !path.exists() {
                continue;
            }
            let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            record["phase"] = json!(stage);
            record["retired"] = json!(false);
            store::atomic_write(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        }
    }
    server.restart();
    server.await_login(&server.alex, "work", &repair, "completed");
    assert_eq!(server.token(&server.alex, "work", None).status(), 200);
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
}

#[test]
fn server_relogin_replaces_only_the_original_account_and_survives_restart() {
    let mut server = Server::start();
    assert!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status()
            .is_success()
    );
    assert!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status()
            .is_success()
    );
    let before: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let start = server
        .http
        .post(format!("{}/v1/relogin/start", server.url))
        .bearer_auth(&server.amir)
        .json(&json!({"alias":"personal","id":"a".repeat(64)}))
        .send()
        .unwrap();
    assert_eq!(
        start.status(),
        200,
        "server-managed re-login must be available"
    );
    let flow: Value = start.json().unwrap();
    assert_eq!(flow["status"], "pending");
    assert_eq!(
        flow["verificationUrl"],
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(flow["userCode"], "TEST-LOGIN");
    let retry = server
        .import(&server.amir, "personal", "amir-login", "amir-seat")
        .status();
    if retry != 409 {
        server.login_request(
            &server.amir,
            "cancel",
            "personal",
            flow["id"].as_str().unwrap(),
        );
        server.await_login(
            &server.amir,
            "personal",
            flow["id"].as_str().unwrap(),
            "canceled",
        );
    }
    assert_eq!(
        retry, 409,
        "the selected account stays reserved before a candidate grant exists"
    );
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
    let forbidden = server
        .http
        .post(format!("{}/v1/relogin/status", server.url))
        .bearer_auth(&server.alex)
        .json(&json!({"alias":"personal","id":flow["id"]}))
        .send()
        .unwrap();
    assert_eq!(forbidden.status(), 404);
    let mut replacement = auth("amir-login", "amir-seat");
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"amir-login","iat":2000000020_u64,"exp":4102444800_u64,"generation":20,"https://api.openai.com/auth":{"chatgpt_account_id":"amir-seat","chatgpt_plan_type":"pro"}})).unwrap());
    replacement["tokens"]["access_token"] = json!(format!("header.{body}."));
    replacement["tokens"]["refresh_token"] = json!("synthetic-fresh-relogin-grant");
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&replacement).unwrap(),
    )
    .unwrap();
    let mut done = false;
    for _ in 0..200 {
        let status: Value = server
            .http
            .post(format!("{}/v1/relogin/status", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","id":flow["id"]}))
            .send()
            .unwrap()
            .json()
            .unwrap();
        if status["status"] == "completed" {
            done = true;
            break;
        }
        assert!(
            status["error"].is_null(),
            "re-login failed: {}",
            status["error"]
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(done);
    let after: Value = server.token(&server.amir, "personal", None).json().unwrap();
    assert_ne!(before["revision"], after["revision"]);
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn local_profile(home: &std::path::Path, alias: &str) -> PathBuf {
    let directory = home.join(".codexctl/profiles").join(alias);
    store::atomic_write(
        &directory.join("meta.json"),
        &serde_json::to_vec(&json!({"alias":alias,"saved_at":"2026-10-02T00:00:00Z"})).unwrap(),
    )
    .unwrap();
    // An unreadable credential must still leave the profile visible.
    store::atomic_write(&directory.join("auth.json"), b"{}").unwrap();
    directory
}

#[test]
fn connected_status_before_migration_shows_local_profiles() {
    let server = Server::start();
    let home = server.connected_home();
    local_profile(home.path(), "laptop-profile");

    let output = server.cli(home.path(), &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(
        stdout.contains("laptop-profile") && stdout.contains("local"),
        "{stdout}"
    );
    assert!(stdout.contains("No server accounts yet") && !stdout.contains("Label"));
}

#[test]
fn connected_status_formats_server_reset() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"status-reset").unwrap();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    let date = chrono::DateTime::from_timestamp(4102444800, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%a %b %d %H:%M")
        .to_string();

    let output = server.cli(home.path(), &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(
        stdout.contains("Resets") && stdout.contains("in ") && stdout.contains(&date),
        "{stdout}"
    );
}

#[test]
fn connected_list_before_migration_shows_local_profiles() {
    let server = Server::start();
    let home = server.connected_home();
    local_profile(home.path(), "laptop-profile");

    let output = server.cli(home.path(), &["list"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(
        stdout.contains("laptop-profile") && stdout.contains("local"),
        "{stdout}"
    );
    assert!(stdout.contains("No server accounts yet") && !stdout.contains("Plan"));
}

#[test]
fn connected_filtered_status_keeps_local_profile_errors() {
    let server = Server::start();
    let home = server.connected_home();
    local_profile(home.path(), "laptop-profile");

    let output = server.cli(home.path(), &["status", "--rate-limited"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(
        stdout.contains("laptop-profile") && stdout.contains("credentials unavailable"),
        "{stdout}"
    );
}

#[test]
fn connected_status_shows_server_and_unmigrated_profiles_with_the_same_alias() {
    let server = Server::start();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    local_profile(home.path(), "personal");
    let transferred = local_profile(home.path(), "retired-profile");
    store::atomic_write(&transferred.join(".central-transfer.json"), b"{}").unwrap();

    let output = server.cli(home.path(), &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(stdout.matches("personal").count(), 2, "{stdout}");
    assert!(
        stdout.contains("local")
            && stdout.contains("server")
            && !stdout.contains("retired-profile")
            && !stdout.contains("No server accounts yet"),
        "{stdout}"
    );
}

#[test]
fn connected_list_fetches_local_usage_and_preserves_credentials_on_failure() {
    use std::io::Write;
    let server = Server::start();
    let home = server.connected_home();
    let directory = local_profile(home.path(), "laptop-profile");
    let credentials = serde_json::to_vec(&auth("local-login", "local-seat")).unwrap();
    store::atomic_write(&directory.join("auth.json"), &credentials).unwrap();
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    proxy.set_nonblocking(true).unwrap();
    let request = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match proxy.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("usage request did not reach proxy: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        stream
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        line
    });

    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .arg("list")
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .env("HTTPS_PROXY", format!("http://{proxy_address}"))
        .env("https_proxy", format!("http://{proxy_address}"))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .output()
        .unwrap();

    assert!(
        request
            .join()
            .unwrap()
            .starts_with("CONNECT chatgpt.com:443")
    );
    assert!(
        output.status.success()
            && String::from_utf8(output.stdout)
                .unwrap()
                .contains("usage unavailable")
    );
    assert_eq!(
        std::fs::read(directory.join("auth.json")).unwrap(),
        credentials
    );
}

#[test]
fn when_users_share_an_alias_then_each_catalog_contains_only_their_own_seat() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    server.import(&server.alex, "personal", "alex-login", "alex-seat");
    let amir = server.accounts(&server.amir);
    let alex = server.accounts(&server.alex);
    assert_eq!(amir[0]["accountId"], "amir-seat");
    assert_eq!(alex[0]["accountId"], "alex-seat");
    assert_eq!(
        (
            amir.as_array().unwrap().len(),
            alex.as_array().unwrap().len()
        ),
        (1, 1)
    );
}
#[test]
fn when_a_user_requests_another_users_alias_then_no_access_token_is_returned() {
    let server = Server::start();
    server.import(&server.amir, "private", "amir-login", "amir-seat");
    let response = server.token(&server.alex, "private", None);
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.json::<Value>().unwrap(),
        json!({"error":"account_not_found"})
    );
}
#[test]
fn when_an_import_is_retried_after_refresh_then_the_latest_credentials_are_preserved() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let initial: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let refreshed: Value = server
        .token(&server.amir, "personal", initial["revision"].as_str())
        .json()
        .unwrap();
    let retry = server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let after: Value = server.token(&server.amir, "personal", None).json().unwrap();
    assert_eq!(retry.status(), 200);
    assert_eq!(after["revision"], refreshed["revision"]);
}
#[test]
fn when_a_user_imports_a_seat_already_owned_by_someone_else_then_the_server_refuses() {
    let server = Server::start();
    server.import(&server.amir, "personal", "same-login", "same-seat");
    let response = server.import(&server.alex, "personal", "same-login", "same-seat");
    assert_eq!(response.status(), 409);
    assert_eq!(server.accounts(&server.alex), json!([]));
}
#[test]
fn when_a_device_is_revoked_then_it_cannot_list_or_refresh_accounts() {
    let server = Server::start();
    central::revoke(&server.root.path().join("state"), "amir-laptop").unwrap();
    let response = server.token(&server.amir, "personal", None);
    assert_eq!(response.status(), 401);
    assert_eq!(
        server.accounts(&server.amir),
        json!({"error":"unauthorized"})
    );
}
#[test]
fn when_a_user_is_disabled_then_existing_devices_lose_access() {
    let server = Server::start();
    central::managed::set_user(&server.root.path().join("state"), "amir", false).unwrap();
    let response = server.token(&server.amir, "personal", None);
    assert_eq!(response.status(), 403);
}
#[test]
fn when_the_server_restarts_then_it_preserves_the_refreshed_account() {
    let mut server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let initial: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let refreshed: Value = server
        .token(&server.amir, "personal", initial["revision"].as_str())
        .json()
        .unwrap();
    server.stop();
    server.restart();
    let after: Value = server.token(&server.amir, "personal", None).json().unwrap();
    assert_eq!(after["revision"], refreshed["revision"]);
}
#[test]
fn when_multiple_clients_refresh_the_same_revision_then_the_owner_rotates_once() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let initial: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            server
                .token(&server.amir, "personal", initial["revision"].as_str())
                .json::<Value>()
                .unwrap()
        });
        let second = scope.spawn(|| {
            server
                .token(&server.amir, "personal", initial["revision"].as_str())
                .json::<Value>()
                .unwrap()
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(results.0["revision"], results.1["revision"]);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
}
#[test]
fn when_a_machine_uses_the_server_then_the_regular_codex_provider_has_a_token_helper() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let home = server.connected_home();
    let output = server.cli(home.path(), &["use"]);
    let config = std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(config.contains("central-token"));
    assert!(!home.path().join(".codex/auth.json").exists());
}
#[test]
fn when_a_local_profile_is_migrated_then_local_refresh_is_fenced_and_remote_use_works() {
    let server = Server::start();
    let home = server.connected_home();
    let profile = home.path().join(".codexctl/profiles/personal");
    store::ensure_private_dir(&profile).unwrap();
    store::atomic_write(
        &profile.join("auth.json"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    store::atomic_write(
        &profile.join("meta.json"),
        br#"{"alias":"personal","label":"Personal","saved_at":"2026-09-29"}"#,
    )
    .unwrap();
    let paths = codexctl::config::Paths::from_home(home.path().to_owned());
    store::atomic_write(
        &paths.codex_auth_json(),
        &std::fs::read(profile.join("auth.json")).unwrap(),
    )
    .unwrap();
    codexctl::profile::set_active_from(&paths, "personal").unwrap();
    let migration = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    assert!(
        migration.status.success(),
        "{}",
        String::from_utf8_lossy(&migration.stderr)
    );
    let identity = server.cli(home.path(), &["whoami"]);
    assert!(String::from_utf8_lossy(&identity.stdout).contains("no active profile"));
    let usage = server.cli(home.path(), &["use", "personal"]);
    assert!(
        usage.status.success(),
        "{}",
        String::from_utf8_lossy(&usage.stderr)
    );
    assert!(api_read_fails(&profile.join("auth.json")));
    assert!(server.cli(home.path(), &["disconnect"]).status.success());
    let identity = server.cli(home.path(), &["whoami"]);
    assert!(String::from_utf8_lossy(&identity.stdout).contains("no active profile"));
}
fn api_read_fails(path: &std::path::Path) -> bool {
    codexctl::api::read_auth_json(path).is_err()
}

struct EnrollmentServer {
    server: Server,
    issuer: Child,
}
impl EnrollmentServer {
    fn start(identity: Value) -> Self {
        Self::with_sso(identity, json!({}))
    }
    fn with_sso(identity: Value, extra: Value) -> Self {
        Self::with_setup(identity, extra, |_| {})
    }
    fn with_setup(identity: Value, extra: Value, setup: impl FnOnce(&Server)) -> Self {
        let mut server = Server::start();
        server.stop();
        setup(&server);
        store::atomic_write(
            &server.root.path().join("identity.json"),
            &serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        let mut issuer = Command::new("python3")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/company_oidc.py"))
            .arg(server.root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(issuer.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: Value = serde_json::from_str(&line).unwrap();
        let secret = server.root.path().join("oidc.secret");
        store::atomic_write(&secret, b"synthetic-company-client-secret").unwrap();
        let mut configuration = json!({"issuer":ready["issuer"],"client_id":"codexctl-test","client_secret_file":secret,"allowed_domains":["sawmills.ai"]});
        configuration
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let sso = server.root.path().join("sso.json");
        store::atomic_write(&sso, &serde_json::to_vec(&configuration).unwrap()).unwrap();
        Self::spawn_broker(&mut server);
        Self { server, issuer }
    }
    fn spawn_broker(server: &mut Server) {
        let sso = server.root.path().join("sso.json");
        let mut line = String::new();
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let url = format!("http://{address}");
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(["serve", "--state"])
            .arg(server.root.path().join("state"))
            .arg("--key-file")
            .arg(server.root.path().join("key"))
            .arg("--listen")
            .arg(address.to_string())
            .arg("--public-url")
            .arg(&url)
            // Keep all synthetic OpenAI reads on a closed loopback proxy.
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("https_proxy", "http://127.0.0.1:1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .arg("--sso-config")
            .arg(&sso)
            .arg("--metrics-token-file")
            .arg(server.root.path().join("metrics.token"))
            .arg("--codex-bin")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"))
            .env("CENTRAL_TEST_MODE_FILE", server.root.path().join("mode"))
            .env(
                "CENTRAL_TEST_REFRESH_COUNTER",
                server.root.path().join("count"),
            )
            .stdout(Stdio::piped())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(server.root.path().join("sso-audit.log"))
                    .unwrap(),
            )
            .spawn()
            .unwrap();
        line.clear();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let _: Value = serde_json::from_str(&line).expect("SSO server must start");
        server.child = child;
        server.url = url;
    }
    fn challenge(&self) -> Value {
        self.server
            .http
            .post(format!("{}/v1/enrollment/start", self.server.url))
            .json(&json!({"name":"Amir MacBook"}))
            .send()
            .unwrap()
            .json()
            .unwrap()
    }
    fn browser(&self, url: &str) -> reqwest::blocking::Response {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
            .get(url)
            .send()
            .unwrap()
    }
    fn approve(&self, html: &str) -> reqwest::blocking::Response {
        let approval = html
            .split("name=\"approval\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        self.server
            .http
            .post(format!("{}/auth/approve", self.server.url))
            .form(&[("approval", approval)])
            .send()
            .unwrap()
    }
    fn poll(&self, challenge: &Value) -> reqwest::blocking::Response {
        self.server
            .http
            .post(format!("{}/v1/enrollment/poll", self.server.url))
            .json(&json!({"device_code":challenge["deviceCode"]}))
            .send()
            .unwrap()
    }
}
impl Drop for EnrollmentServer {
    fn drop(&mut self) {
        let _ = self.issuer.kill();
        let _ = self.issuer.wait();
    }
}
fn company_identity() -> Value {
    json!({"sub":"company-amir","email":"amir@sawmills.ai"})
}
#[test]
fn enrollment_pages_allow_only_their_bundled_styles_and_escape_device_names() {
    use base64::engine::general_purpose::STANDARD;
    use sha2::{Digest, Sha256};

    fn check_page(response: reqwest::blocking::Response) -> String {
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        let html = response.text().unwrap();
        let style = html
            .split("<style>")
            .nth(1)
            .unwrap()
            .split("</style>")
            .next()
            .unwrap();
        let hash = STANDARD.encode(Sha256::digest(style.as_bytes()));
        assert_eq!(
            policy,
            format!(
                "default-src 'none'; style-src 'sha256-{hash}'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
            )
        );
        html
    }

    let issuer = EnrollmentServer::start(company_identity());
    let challenge: Value = issuer
        .server
        .http
        .post(format!("{}/v1/enrollment/start", issuer.server.url))
        .json(&json!({"name":"Mac <img src=x onerror=alert(1)> & laptop"}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let html = check_page(issuer.browser(challenge["verificationUrl"].as_str().unwrap()));
    assert!(html.contains("Mac &lt;img src=x onerror=alert(1)&gt; &amp; laptop"));
    assert!(!html.contains("<img"));
    check_page(issuer.approve(&html));
    assert_eq!(issuer.poll(&challenge).status(), 200);
}
#[test]
fn when_company_sign_in_is_confirmed_then_only_the_requesting_device_receives_a_credential() {
    let issuer = EnrollmentServer::start(company_identity());
    let challenge = issuer.challenge();
    let page = issuer
        .browser(challenge["verificationUrl"].as_str().unwrap())
        .text()
        .unwrap();
    issuer.approve(&page);
    let grant = issuer.poll(&challenge).json::<Value>().unwrap();
    let me: Value = issuer
        .server
        .http
        .get(format!("{}/v1/me", issuer.server.url))
        .bearer_auth(grant["deviceToken"].as_str().unwrap())
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(me["id"].as_str().is_some());
    assert_eq!(issuer.poll(&challenge).status(), 410);
}
#[test]
fn when_the_company_callback_has_not_been_confirmed_then_no_device_credential_is_issued() {
    let issuer = EnrollmentServer::start(company_identity());
    let challenge = issuer.challenge();
    issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    let response = issuer.poll(&challenge);
    assert_eq!(response.status(), 202);
    assert_eq!(
        response.json::<Value>().unwrap(),
        json!({"status":"pending"})
    );
}
// The loopback issuer signs Google-style claims; no real Google credentials are used.
#[test]
fn google_workspace_refuses_a_consumer_with_a_verified_company_email() {
    let identity = json!({"sub": "consumer", "email": "consumer@sawmills.ai"});
    let issuer = EnrollmentServer::with_sso(
        identity,
        json!({
            "allowed_hosted_domains": ["sawmills.ai"]
        }),
    );
    let challenge = issuer.challenge();
    let response = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().unwrap()["error"],
        "company_identity_required"
    );
}
#[test]
fn google_workspace_checks_the_signed_domain_and_verified_email() {
    for (hd, verified, status) in [
        ("other.example", true, 403),
        ("sawmills.ai", false, 403),
        ("sawmills.ai", true, 200),
    ] {
        let mut identity = company_identity();
        identity["email"] = json!("workspace-member@sawmills.ai");
        identity["hd"] = json!(hd);
        identity["verified"] = json!(verified);
        let issuer = EnrollmentServer::with_sso(
            identity,
            json!({
                "allowed_hosted_domains": ["sawmills.ai"]
            }),
        );
        let challenge = issuer.challenge();
        let start = issuer
            .server
            .http
            .get(challenge["verificationUrl"].as_str().unwrap())
            .build()
            .unwrap();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let redirect = client.execute(start).unwrap();
        let location =
            reqwest::Url::parse(redirect.headers()["location"].to_str().unwrap()).unwrap();
        assert!(
            location
                .query_pairs()
                .any(|(key, value)| key == "hd" && value == "sawmills.ai")
        );
        let response = issuer.browser(location.as_str());
        assert_eq!(response.status(), status);
        if status == 403 {
            assert_eq!(
                response.json::<Value>().unwrap()["error"],
                "company_identity_required"
            );
        }
    }
}
fn clerk_user_id() -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(b"https://clerk.sawmills.ai\0company-amir")
    )
}
fn google_migration_server(identity: Value) -> EnrollmentServer {
    EnrollmentServer::with_setup(
        identity,
        json!({
            "allowed_hosted_domains": ["sawmills.ai"],
            "clerk_migration": {
                "users": [{"user_id": clerk_user_id(), "email": "amir@sawmills.ai"}]
            }
        }),
        |server| {
            // Synthetic legacy registry, including a machine credential issued before cutover.
            let state = server.root.path().join("state");
            let mut users: Value =
                serde_json::from_slice(&std::fs::read(state.join("users.json")).unwrap()).unwrap();
            users[0]["id"] = json!(clerk_user_id());
            store::atomic_write(
                &state.join("users.json"),
                &serde_json::to_vec(&users).unwrap(),
            )
            .unwrap();
            let mut devices: Value =
                serde_json::from_slice(&std::fs::read(state.join("devices.json")).unwrap())
                    .unwrap();
            for device in devices.as_array_mut().unwrap() {
                if device["user"] == "amir" {
                    device["user"] = json!(clerk_user_id());
                }
            }
            store::atomic_write(
                &state.join("devices.json"),
                &serde_json::to_vec(&devices).unwrap(),
            )
            .unwrap();
        },
    )
}
fn google_identity() -> Value {
    json!({"sub": "google-amir", "email": "amir@sawmills.ai", "hd": "sawmills.ai"})
}
fn enroll(issuer: &EnrollmentServer) -> String {
    let challenge = issuer.challenge();
    let page = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(page.status(), 200);
    assert_eq!(issuer.approve(&page.text().unwrap()).status(), 200);
    issuer.poll(&challenge).json::<Value>().unwrap()["deviceToken"]
        .as_str()
        .unwrap()
        .into()
}
#[test]
fn google_migration_links_once_and_preserves_accounts_and_machine_credentials() {
    let issuer = google_migration_server(google_identity());
    assert_eq!(
        issuer
            .server
            .import(&issuer.server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let cookie = issuer.dashboard_cookie();
    let dashboard = issuer
        .server
        .http
        .get(format!("{}/accounts/data", issuer.server.url))
        .header("cookie", cookie)
        .send()
        .unwrap();
    assert_eq!(dashboard.status(), 200);
    assert!(dashboard.text().unwrap().contains("personal"));
    let token = enroll(&issuer);
    assert_eq!(issuer.server.accounts(&token)[0]["alias"], "personal");
    assert_eq!(
        issuer.server.accounts(&issuer.server.amir)[0]["alias"],
        "personal"
    );
    assert_eq!(
        issuer
            .server
            .token(&issuer.server.amir, "personal", None)
            .status(),
        200
    );
    let me: Value = issuer
        .server
        .http
        .get(format!("{}/v1/me", issuer.server.url))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(me["id"], clerk_user_id());
    let second = enroll(&issuer);
    assert_eq!(issuer.server.accounts(&second)[0]["alias"], "personal");
    let mut other = google_identity();
    other["sub"] = json!("another-google-subject");
    store::atomic_write(
        &issuer.server.root.path().join("identity.json"),
        &serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    let challenge = issuer.challenge();
    assert_eq!(
        issuer
            .browser(challenge["verificationUrl"].as_str().unwrap())
            .status(),
        403
    );
    let audit = std::fs::read_to_string(issuer.server.root.path().join("sso-audit.log")).unwrap();
    assert_eq!(audit.matches("SSO_IDENTITY_LINKED").count(), 1);
    assert!(!audit.contains("amir@sawmills.ai"));
}
#[test]
fn concurrent_google_sign_ins_consume_only_one_migration_authorization() {
    let issuer = google_migration_server(google_identity());
    let first = issuer.challenge();
    let second = issuer.challenge();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let callbacks: Vec<_> = [&first, &second]
            .into_iter()
            .map(|challenge| {
                let issuer = &issuer;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    issuer
                        .browser(challenge["verificationUrl"].as_str().unwrap())
                        .status()
                })
            })
            .collect();
        for callback in callbacks {
            assert_eq!(callback.join().unwrap(), 200);
        }
    });
    let audit = std::fs::read_to_string(issuer.server.root.path().join("sso-audit.log")).unwrap();
    assert_eq!(audit.matches("SSO_IDENTITY_LINKED").count(), 1);
}
#[test]
fn google_migration_survives_restart_with_the_cutover_config_removed() {
    let mut issuer = google_migration_server(google_identity());
    issuer
        .server
        .import(&issuer.server.amir, "personal", "amir-login", "amir-seat");
    let token = enroll(&issuer);
    issuer.server.child.kill().unwrap();
    issuer.server.child.wait().unwrap();
    let path = issuer.server.root.path().join("sso.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("clerk_migration");
    store::atomic_write(&path, &serde_json::to_vec(&config).unwrap()).unwrap();
    EnrollmentServer::spawn_broker(&mut issuer.server);
    let new_token = enroll(&issuer);
    for machine in [&token, &new_token, &issuer.server.amir] {
        assert_eq!(issuer.server.accounts(machine)[0]["alias"], "personal");
    }
    let audit = std::fs::read_to_string(issuer.server.root.path().join("sso-audit.log")).unwrap();
    assert_eq!(audit.matches("SSO_IDENTITY_LINKED").count(), 1);
}
#[test]
fn google_migration_refuses_the_original_subject_after_issuer_rollback() {
    let mut old_identity = company_identity();
    old_identity["email"] = json!("rollback@sawmills.ai");
    let mut original = EnrollmentServer::start(old_identity);
    let old_token = enroll(&original);
    let me: Value = original
        .server
        .http
        .get(format!("{}/v1/me", original.server.url))
        .bearer_auth(&old_token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let path = original.server.root.path().join("sso.json");
    let old_config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut new_identity = google_identity();
    new_identity["email"] = json!("rollback@sawmills.ai");
    let google = EnrollmentServer::with_sso(
        new_identity,
        json!({
            "allowed_hosted_domains": ["sawmills.ai"]
        }),
    );
    let google_config: Value =
        serde_json::from_slice(&std::fs::read(google.server.root.path().join("sso.json")).unwrap())
            .unwrap();
    original.server.stop();
    let mut cutover = old_config.clone();
    cutover["issuer"] = google_config["issuer"].clone();
    cutover["allowed_hosted_domains"] = json!(["sawmills.ai"]);
    cutover["clerk_migration"] = json!({
        "users": [{"user_id": me["id"], "email": "rollback@sawmills.ai"}]
    });
    store::atomic_write(&path, &serde_json::to_vec(&cutover).unwrap()).unwrap();
    EnrollmentServer::spawn_broker(&mut original.server);
    let linked_token = enroll(&original);
    let linked: Value = original
        .server
        .http
        .get(format!("{}/v1/me", original.server.url))
        .bearer_auth(&linked_token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(linked["id"], me["id"]);

    original.server.stop();
    // Restore the actual original issuer and subject, without hosted-domain policy
    // or migration authorization: refusal must come from the consumed-source guard.
    store::atomic_write(&path, &serde_json::to_vec(&old_config).unwrap()).unwrap();
    EnrollmentServer::spawn_broker(&mut original.server);
    let challenge = original.challenge();
    let response = original.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().unwrap()["error"],
        "identity_link_refused"
    );
}
#[test]
fn google_migration_refuses_unverified_or_mismatched_company_email() {
    for (email, verified) in [("amir@sawmills.ai", false), ("alex@sawmills.ai", true)] {
        let mut identity = google_identity();
        identity["email"] = json!(email);
        identity["verified"] = json!(verified);
        let issuer = google_migration_server(identity);
        let challenge = issuer.challenge();
        assert_eq!(
            issuer
                .browser(challenge["verificationUrl"].as_str().unwrap())
                .status(),
            403
        );
    }
}
#[test]
fn google_migration_refuses_ambiguous_disabled_or_unapproved_legacy_users() {
    for mode in [
        "duplicate",
        "disabled",
        "unapproved",
        "wrong-user-id",
        "mismatched-legacy-email",
    ] {
        let mut issuer = google_migration_server(google_identity());
        issuer.server.stop();
        let state = issuer.server.root.path().join("state");
        let mut users: Value =
            serde_json::from_slice(&std::fs::read(state.join("users.json")).unwrap()).unwrap();
        match mode {
            "duplicate" => users[1]["email"] = json!("amir@sawmills.ai"),
            "disabled" => users[0]["enabled"] = json!(false),
            "mismatched-legacy-email" => users[0]["email"] = json!("someone-else@sawmills.ai"),
            _ => {
                let path = issuer.server.root.path().join("sso.json");
                let mut config: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if mode == "wrong-user-id" {
                    config["clerk_migration"]["users"][0]["user_id"] = json!("f".repeat(64));
                } else {
                    config.as_object_mut().unwrap().remove("clerk_migration");
                }
                store::atomic_write(&path, &serde_json::to_vec(&config).unwrap()).unwrap();
            }
        }
        store::atomic_write(
            &state.join("users.json"),
            &serde_json::to_vec(&users).unwrap(),
        )
        .unwrap();
        EnrollmentServer::spawn_broker(&mut issuer.server);
        let challenge = issuer.challenge();
        assert_eq!(
            issuer
                .browser(challenge["verificationUrl"].as_str().unwrap())
                .status(),
            403,
            "{mode}"
        );
    }
}
#[test]
fn when_the_oidc_nonce_is_wrong_then_sign_in_is_rejected() {
    let mut identity = company_identity();
    identity["nonce"] = json!("wrong-nonce");
    let issuer = EnrollmentServer::start(identity);
    let challenge = issuer.challenge();
    let response = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 401);
}
#[test]
fn when_the_oidc_audience_is_wrong_then_sign_in_is_rejected() {
    let mut identity = company_identity();
    identity["aud"] = json!("another-app");
    let issuer = EnrollmentServer::start(identity);
    let challenge = issuer.challenge();
    let response = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 401);
}
#[test]
fn when_the_email_is_unverified_then_company_access_is_rejected() {
    let mut identity = company_identity();
    identity["verified"] = json!(false);
    let issuer = EnrollmentServer::start(identity);
    let challenge = issuer.challenge();
    let response = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 403);
}
#[test]
fn when_the_email_is_outside_the_company_then_access_is_rejected() {
    let issuer = EnrollmentServer::start(json!({"sub":"external","email":"external@example.com"}));
    let challenge = issuer.challenge();
    let response = issuer.browser(challenge["verificationUrl"].as_str().unwrap());
    assert_eq!(response.status(), 403);
}
#[test]
fn when_a_user_replays_the_approval_then_the_server_refuses_a_second_device() {
    let issuer = EnrollmentServer::start(company_identity());
    let challenge = issuer.challenge();
    let page = issuer
        .browser(challenge["verificationUrl"].as_str().unwrap())
        .text()
        .unwrap();
    issuer.approve(&page);
    let replay = issuer.approve(&page);
    assert_eq!(replay.status(), 400);
}
#[test]
fn when_connect_is_run_then_the_machine_stores_only_its_own_device_credential() {
    let issuer = EnrollmentServer::start(company_identity());
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args([
            "connect",
            "--server",
            &issuer.server.url,
            "--no-browser",
            "--name",
            "Amir MacBook",
        ])
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut line).unwrap();
    let url = line
        .trim()
        .strip_prefix("Sign in with company SSO: ")
        .unwrap();
    let html = issuer.browser(url).text().unwrap();
    issuer.approve(&html);
    let output = child.wait_with_output().unwrap();
    let connection =
        std::fs::read_to_string(home.path().join(".codexctl/central/.server.json")).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!connection.contains("refresh_token"));
    assert!(connection.contains("user_id"));
}

#[test]
fn when_import_verification_fails_then_it_cannot_reserve_someone_elses_account() {
    for restart in [false, true] {
        let mut server = Server::start();
        store::atomic_write(&server.root.path().join("mode"), b"error").unwrap();
        let failed = server.import(&server.alex, "claimed", "same-login", "same-seat");
        store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
        if restart {
            server.stop();
            server.restart();
        }
        let legitimate = server.import(&server.amir, "personal", "same-login", "same-seat");
        assert_eq!(failed.status(), 503);
        assert_eq!(legitimate.status(), 200);
    }
}
#[test]
fn when_a_saved_import_failed_to_start_then_retry_reconciles_it() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"error").unwrap();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let response = retry_import(&server);
    assert_eq!(response.status(), 200);
    assert_eq!(server.accounts(&server.amir)[0]["accountId"], "amir-seat");
}
fn retry_import(server: &Server) -> reqwest::blocking::Response {
    retry_import_for(server, &server.amir, "personal", "amir-login", "amir-seat")
}
#[test]
fn when_a_crash_leaves_a_newer_journal_then_restart_recovers_it() {
    let mut server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let state = server.root.path().join("state/accounts");
    let account = std::fs::read_dir(state)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let old = std::fs::read(account.join("vault.enc")).unwrap();
    let initial: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let refreshed: Value = server
        .token(&server.amir, "personal", initial["revision"].as_str())
        .json()
        .unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    store::atomic_write(&account.join("vault.enc"), &old).unwrap();
    restart_after_crash(&mut server);
    let after: Value = server.token(&server.amir, "personal", None).json().unwrap();
    assert_eq!(after["revision"], refreshed["revision"]);
}
fn restart_after_crash(server: &mut Server) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(["serve", "--state"])
            .arg(server.root.path().join("state"))
            .arg("--key-file")
            .arg(server.root.path().join("key"))
            .args([
                "--listen",
                "127.0.0.1:0",
                "--public-url",
                "http://127.0.0.1:8787",
                "--codex-bin",
            ])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"))
            .env("CENTRAL_TEST_MODE_FILE", server.root.path().join("mode"))
            .env(
                "CENTRAL_TEST_REFRESH_COUNTER",
                server.root.path().join("count"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        if let Ok(ready) = serde_json::from_str::<Value>(&line) {
            server.url = format!("http://{}", ready["listening"].as_str().unwrap());
            server.child = child;
            return;
        }
        child.wait().unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "restart did not recover"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn when_another_users_refresh_is_blocked_then_your_account_catalog_still_responds() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    server.import(&server.alex, "personal", "alex-login", "alex-seat");
    let initial: Value = server.token(&server.alex, "personal", None).json().unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    let response = std::thread::scope(|scope| {
        let refresh =
            scope.spawn(|| server.token(&server.alex, "personal", initial["revision"].as_str()));
        wait_for_refresh(&server);
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .send();
        store::atomic_write(&server.root.path().join("release"), b"released").unwrap();
        refresh.join().unwrap();
        response
    });
    assert_eq!(
        response.unwrap().json::<Value>().unwrap()[0]["accountId"],
        "amir-seat"
    );
}
fn wait_for_refresh(server: &Server) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !server.root.path().join("refresh-started").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn when_metrics_credentials_are_used_then_they_cannot_access_accounts_and_rejections_count_once() {
    let server = Server::start();
    let metrics = "synthetic-monitoring-credential-only";
    assert_eq!(
        server
            .http
            .get(format!("{}/v1/accounts", server.url))
            .bearer_auth(metrics)
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .header("content-type", "application/json")
            .body("{")
            .send()
            .unwrap()
            .status(),
        400
    );
    let response = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth(metrics)
        .send()
        .unwrap();
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let output = response.text().unwrap();
    assert!(output.contains("reason=\"unauthorized\"} 1\n"), "{output}");
    assert!(
        output.contains("reason=\"invalid_request\"} 1\n"),
        "{output}"
    );
    assert_eq!(
        server
            .http
            .get(format!("{}/metrics", server.url))
            .bearer_auth(&server.amir)
            .send()
            .unwrap()
            .status(),
        401
    );
    let output = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth(metrics)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        output.contains("reason=\"metrics_unauthorized\"} 1\n"),
        "{output}"
    );
}

fn account_directory(server: &Server, user: &str, alias: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    server.root.path().join("state/accounts").join(format!(
        "{:x}",
        Sha256::digest(format!("{user}\0{}", alias.to_lowercase()).as_bytes())
    ))
}
#[test]
fn when_an_import_has_an_uncertain_reply_then_its_owner_finishes_before_shutdown() {
    let mut server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"late-error").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        503
    );
    let runtime = account_directory(&server, "amir", "personal").join("runtime");
    let process: Value =
        serde_json::from_slice(&std::fs::read(runtime.join("pid")).unwrap()).unwrap();
    assert_eq!(
        unsafe { libc::kill(process["pid"].as_u64().unwrap() as i32, 0) },
        0
    );
    server.stop();
    let auth: Value =
        serde_json::from_slice(&std::fs::read(runtime.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["tokens"]["refresh_token"], "synthetic-rotated-refresh");
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    server.restart();
    assert_eq!(retry_import(&server).status(), 200);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
}
#[test]
fn when_a_client_leaves_during_import_then_shutdown_waits_for_credential_persistence() {
    use std::io::Write;
    let mut server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    let body =
        serde_json::to_string(&json!({"alias":"personal","auth":auth("alex-login","alex-seat")}))
            .unwrap();
    let mut stream =
        std::net::TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
    write!(stream, "POST /v1/accounts HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", server.alex, body.len(), body).unwrap();
    stream.flush().unwrap();
    wait_for_refresh(&server);
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    drop(stream);
    let release = server.root.path().join("release");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            server.stop();
            tx.send(()).unwrap();
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        store::atomic_write(&release, b"released").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
    server.restart();
    assert_eq!(server.accounts(&server.alex)[0]["available"], true);
}
#[test]
fn when_an_account_journal_is_corrupt_then_other_users_can_still_get_tokens_after_restart() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    server.stop();
    let runtime = account_directory(&server, "amir", "personal").join("runtime/auth.json");
    store::atomic_write(
        &runtime,
        &serde_json::to_vec(&auth("wrong-login", "wrong-seat")).unwrap(),
    )
    .unwrap();
    store::ensure_private_dir(&server.root.path().join("state/accounts/unfinished-import"))
        .unwrap();
    server.restart();
    assert_eq!(server.accounts(&server.amir)[0]["available"], false);
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
    let output = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth("synthetic-monitoring-credential-only")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        output.contains("reason=\"recovery_failed\"} 2\n"),
        "{output}"
    );
    assert_eq!(
        std::fs::read(runtime).unwrap(),
        serde_json::to_vec(&auth("wrong-login", "wrong-seat")).unwrap()
    );
}

#[test]
fn when_an_uncertain_import_matches_another_request_then_the_first_owner_settles_before_conflict() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"late-error").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        409
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
    assert_eq!(server.accounts(&server.alex)[0]["available"], false);
}

#[test]
fn when_pending_recovery_fails_then_overlapping_import_preserves_evidence_and_refuses() {
    let mut server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"error").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    server.stop();
    let runtime = account_directory(&server, "alex", "pending").join("runtime/auth.json");
    store::atomic_write(&runtime, b"{invalid").unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    server.restart();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(std::fs::read(&runtime).unwrap(), b"{invalid");
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "0"
    );
    assert_eq!(server.accounts(&server.amir)["error"], "recovery_failed");
}

#[cfg(unix)]
#[test]
fn when_the_owner_executable_is_fixed_then_the_import_retries_after_restart() {
    let mut server = Server::start();
    server.stop();
    let executable = server.root.path().join("missing-codex");
    let (child, url) = Server::spawn_binary(&server.root, &executable);
    server.child = child;
    server.url = url;
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        503
    );
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "0"
    );
    server.stop();
    std::os::unix::fs::symlink(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"),
        &executable,
    )
    .unwrap();
    let (child, url) = Server::spawn_binary(&server.root, &executable);
    server.child = child;
    server.url = url;
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
    assert_eq!(server.accounts(&server.amir)[0]["available"], true);
}

#[test]
fn when_an_import_crashes_after_rotation_then_other_aliases_cannot_replay_the_input() {
    let mut server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"late-error").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(server.root.path().join("count")).unwrap() != "1" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let account = account_directory(&server, "alex", "pending");
    let pid: Value =
        serde_json::from_slice(&std::fs::read(account.join("runtime/pid")).unwrap()).unwrap();
    // Stop this fixture before its broker, so Linux parent-death cleanup cannot free
    // and reuse its PID before the explicit test cleanup signal.
    unsafe { libc::kill(pid["pid"].as_i64().unwrap() as i32, libc::SIGTERM) };
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    restart_after_crash(&mut server);
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
    assert!(server.accounts(&server.amir).as_array().unwrap().is_empty());
    assert_eq!(
        retry_import_for(&server, &server.alex, "pending", "same-login", "same-seat").status(),
        200
    );
    let latest: Value = server.token(&server.alex, "pending", None).json().unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(
                latest["accessToken"]
                    .as_str()
                    .unwrap()
                    .split('.')
                    .nth(1)
                    .unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["generation"], 2);
}
fn retry_import_for(
    server: &Server,
    device: &str,
    alias: &str,
    login: &str,
    seat: &str,
) -> reqwest::blocking::Response {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = server.import(device, alias, login, seat);
        if result.status().is_success() || std::time::Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn when_a_replacement_fails_validation_then_its_account_stays_reserved() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "Personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"billing-error").unwrap();
    assert_eq!(server.token(&server.amir, "Personal", None).status(), 503);
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        409
    );
    assert_eq!(server.token(&server.amir, "Personal", None).status(), 503);
    assert_eq!(
        server
            .import(&server.amir, "another", "amir-login", "amir-seat")
            .status(),
        409
    );
}
#[test]
fn when_a_vault_is_unreadable_then_new_imports_are_fenced_but_healthy_accounts_work() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    server.stop();
    let account = account_directory(&server, "amir", "personal");
    let journal = std::fs::read(account.join("runtime/auth.json")).unwrap();
    store::atomic_write(&account.join("vault.enc"), b"corrupt vault").unwrap();
    server.restart();
    assert_eq!(
        server
            .import(&server.amir, "another", "amir-login", "amir-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read(account.join("runtime/auth.json")).unwrap(),
        journal
    );
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    let missing_catalog = server
        .http
        .get(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.amir)
        .send()
        .unwrap();
    assert_eq!(missing_catalog.status(), 503);
    let metrics = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth("synthetic-monitoring-credential-only")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        metrics.contains("codexctl_central_ownership_unresolved{reason=\"recovery_failed\"} 1\n")
    );
    assert!(
        metrics.contains("codexctl_central_failed_requests_total{reason=\"recovery_failed\"} 4\n")
    );
}

#[test]
fn when_unreadable_ownership_has_a_live_process_then_restart_waits_before_any_replacement() {
    let mut target = Server::start();
    assert_eq!(
        target
            .import(&target.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    target.stop();
    let mut source = Server::start();
    assert_eq!(
        source
            .import(&source.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    let original = account_directory(&source, "alex", "personal");
    let unresolved = target.root.path().join("state/accounts/unresolved");
    store::atomic_write(&unresolved.join("vault.enc"), b"corrupt vault").unwrap();
    for name in ["auth.json", "pid"] {
        store::atomic_write(
            &unresolved.join("runtime").join(name),
            &std::fs::read(original.join("runtime").join(name)).unwrap(),
        )
        .unwrap();
    }
    target.restart();
    assert_eq!(target.token(&target.alex, "personal", None).status(), 503);
    assert_eq!(
        std::fs::read_to_string(target.root.path().join("count")).unwrap(),
        "1"
    );
    source.stop();
    target.stop();
    target.restart();
    assert_eq!(target.token(&target.alex, "personal", None).status(), 503);
    // A dead process settles liveness, not the unreadable vault's overlapping
    // journal. Reconcile that retained entry before releasing replacement.
    target.stop();
    std::fs::remove_dir_all(&unresolved).unwrap();
    target.restart();
    assert_eq!(target.token(&target.alex, "personal", None).status(), 200);
}

#[test]
fn when_a_migration_is_partial_then_healthy_aliases_work_and_failed_aliases_stay_fenced() {
    let server = Server::start();
    let home = server.connected_home();
    for (alias, login, seat) in [
        ("alpha", "good-login", "good-seat"),
        ("beta", "bad-login", "bad-seat"),
    ] {
        let profile = home.path().join(".codexctl/profiles").join(alias);
        store::atomic_write(
            &profile.join("auth.json"),
            &serde_json::to_vec(&auth(login, seat)).unwrap(),
        )
        .unwrap();
        store::atomic_write(
            &profile.join("meta.json"),
            &serde_json::to_vec(&json!({"alias":alias,"saved_at":"2026-09-30"})).unwrap(),
        )
        .unwrap();
    }
    store::atomic_write(&server.root.path().join("mode"), b"partial-migration").unwrap();
    assert!(
        !server
            .cli(home.path(), &["migrate", "--all", "--exclusive-owner"])
            .status
            .success()
    );
    for args in [
        &["list"][..],
        &["status"][..],
        &["use", "alpha"][..],
        &["use"][..],
    ] {
        let result = server.cli(home.path(), args);
        assert!(
            result.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(!server.cli(home.path(), &["use", "beta"]).status.success());
    let failed = home.path().join(".codexctl/profiles/beta");
    assert!(api_read_fails(&failed.join("auth.json")));
    let transfer: Value =
        serde_json::from_slice(&std::fs::read(failed.join(".central-transfer.json")).unwrap())
            .unwrap();
    assert_eq!(transfer["confirmed"], false);
    assert!(!home.path().join(".codexctl/central/beta.json").exists());
}

#[test]
fn when_local_credentials_use_another_alias_then_remote_activation_requires_handoff() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "remote", "amir-login", "amir-seat")
            .status(),
        200
    );
    for relative in [
        ".codex/auth.json",
        ".codexctl/profiles/old-alias/auth.json",
        ".codexctl/exec-homes/old-alias/auth.json",
        ".codexctl/login-homes/old-alias/session-interrupted/auth.json",
    ] {
        let home = server.connected_home();
        let holder = home.path().join(relative);
        store::atomic_write(
            &holder,
            &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
        )
        .unwrap();
        let result = server.cli(home.path(), &["use", "remote"]);
        assert!(
            !result.status.success(),
            "activation reused local holder {relative}"
        );
        assert!(holder.exists());
        assert!(
            !home
                .path()
                .join(".codexctl/central/.native-active.json")
                .exists()
        );
    }
}

#[test]
fn when_a_journal_changes_identity_then_imports_cannot_start_a_competing_owner() {
    for restart in [false, true] {
        let mut server = Server::start();
        store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
        assert_eq!(
            server
                .import(&server.alex, "pending", "same-login", "same-seat")
                .status(),
            503
        );
        if restart {
            unsafe { libc::kill(server.child.id() as i32, libc::SIGTERM) };
            assert!(!server.child.wait().unwrap().success());
            server.restart();
        }
        store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
        assert_eq!(
            server
                .import(&server.amir, "personal", "different-login", "same-seat")
                .status(),
            503
        );
        assert_eq!(
            std::fs::read_to_string(server.root.path().join("count")).unwrap(),
            "1"
        );
    }
}

#[test]
fn when_an_import_is_definitively_rejected_then_fresh_credentials_can_repair_its_alias() {
    for restart in [false, true] {
        let mut server = Server::start();
        let mut rejected = auth("amir-login", "amir-seat");
        rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
        let response = server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","auth":rejected}))
            .send()
            .unwrap();
        assert_eq!(response.status(), 503);
        if restart {
            server.stop();
            server.restart();
        }
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        assert_eq!(
            std::fs::read_to_string(server.root.path().join("count")).unwrap(),
            "1"
        );
    }
}

#[test]
fn when_a_conflicting_journal_names_an_existing_seat_then_restart_does_not_launch_it() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "different-login", "same-seat")
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    // The bad journal must survive shutdown as evidence. Its broker exits with an error.
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    unsafe { libc::kill(server.child.id() as i32, libc::SIGTERM) };
    assert!(!server.child.wait().unwrap().success());
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
}

#[test]
fn when_rejected_import_repair_stops_between_journal_and_vault_then_retry_keeps_the_new_grant() {
    let mut server = Server::start();
    let mut rejected = auth("amir-login", "amir-seat");
    rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    assert_eq!(
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","auth":rejected}))
            .send()
            .unwrap()
            .status(),
        503
    );
    server.stop();
    // Simulate the durable first write of an explicitly permitted replacement.
    store::atomic_write(
        &account_directory(&server, "amir", "personal").join("runtime/auth.json"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.restart();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "1"
    );
}

#[test]
fn when_forgetting_and_selection_race_then_no_successful_forget_leaves_a_broken_provider() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    for _ in 0..12 {
        let home = server.connected_home();
        assert!(
            server
                .cli(home.path(), &["use", "personal"])
                .status
                .success()
        );
        std::thread::scope(|scope| {
            let select = scope.spawn(|| server.cli(home.path(), &["use", "personal"]));
            let forget = scope.spawn(|| server.cli(home.path(), &["disconnect", "--forget"]));
            let _ = select.join().unwrap();
            let forgotten = forget.join().unwrap();
            assert!(
                forgotten.status.success(),
                "{}",
                String::from_utf8_lossy(&forgotten.stderr)
            );
        });
        let config: toml_edit::DocumentMut =
            std::fs::read_to_string(home.path().join(".codex/config.toml"))
                .unwrap()
                .parse()
                .unwrap();
        assert_ne!(
            config
                .get("model_provider")
                .and_then(toml_edit::Item::as_str),
            Some("codexctl-central")
        );
        assert!(!home.path().join(".codexctl/central/.server.json").exists());
        assert!(
            !home
                .path()
                .join(".codexctl/central/.native-active.json")
                .exists()
        );
    }
}

#[test]
fn when_a_holder_has_no_subject_then_migration_refuses_before_upload() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    let claims = json!({"iat":2000000000,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"amir-seat","chatgpt_user_id":"same-uid"}});
    let mut unknown = auth("amir-login", "amir-seat");
    unknown["tokens"]["access_token"] = json!(format!(
        "header.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    ));
    let holder = paths.exec_homes_dir().join("old-name/auth.json");
    store::atomic_write(&holder, &serde_json::to_vec(&unknown).unwrap()).unwrap();
    let result = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    assert!(!result.status.success());
    assert!(holder.exists());
    assert!(paths.codex_auth_json().exists());
    assert!(
        !paths
            .profiles_dir()
            .join("personal/.central-transfer.json")
            .exists()
    );
    assert!(server.accounts(&server.amir).as_array().unwrap().is_empty());
}

#[test]
fn when_account_read_fails_for_routing_then_the_candidate_stays_reserved() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"routing-error").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "other", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "0"
    );
    assert_eq!(
        retry_import_for(&server, &server.alex, "pending", "same-login", "same-seat").status(),
        200
    );
}

#[test]
fn persisted_plaintext_registration_refuses_migration_before_retiring_credentials() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let source = serde_json::to_vec(&auth("local-login", "local-seat")).unwrap();
    store::atomic_write(&paths.codex_auth_json(), &source).unwrap();
    codexctl::profile::save_profile_to(&paths, "local", None, &paths.codex_auth_json()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .args(["migrate", "--all", "--exclusive-owner"])
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEXCTL_ALLOW_INSECURE_LOOPBACK")
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS origin"));
    assert_eq!(std::fs::read(paths.codex_auth_json()).unwrap(), source);
    assert!(
        !paths
            .profiles_dir()
            .join("local/.central-transfer.json")
            .exists()
    );
    assert!(server.accounts(&server.amir).as_array().unwrap().is_empty());
}

#[test]
fn persisted_plaintext_registration_cannot_send_device_revocation() {
    let server = Server::start();
    let home = server.connected_home();
    let result = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .args(["devices", "--revoke", "amir-laptop"])
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEXCTL_ALLOW_INSECURE_LOOPBACK")
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS origin"));
    assert!(
        server
            .http
            .get(format!("{}/v1/devices", server.url))
            .bearer_auth(&server.amir)
            .send()
            .unwrap()
            .status()
            .is_success()
    );
}

#[test]
fn managed_enrollment_preserves_offline_explicit_local_selection() {
    let mut server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let source = serde_json::to_vec(&auth("local-login", "local-seat")).unwrap();
    store::atomic_write(&paths.codex_auth_json(), &source).unwrap();
    codexctl::profile::save_profile_to(&paths, "local", None, &paths.codex_auth_json()).unwrap();
    assert_eq!(
        server
            .import(&server.amir, "remote", "remote-login", "remote-seat")
            .status(),
        200
    );
    assert!(server.cli(home.path(), &["use", "remote"]).status.success());
    unsafe {
        libc::kill(server.child.id() as i32, libc::SIGTERM);
    }
    server.child.wait().unwrap();

    let result = server.cli(home.path(), &["use", "local"]);

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(paths.codex_auth_json()).unwrap(), source);
    assert_eq!(
        std::fs::read_to_string(paths.active_file()).unwrap().trim(),
        "local"
    );
    assert!(paths.codexctl_dir().join("central/.server.json").exists());
    assert!(
        !paths
            .codexctl_dir()
            .join("central/.native-active.json")
            .exists()
    );
    assert!(
        !std::fs::read_to_string(paths.codex_home().join("config.toml"))
            .unwrap()
            .contains("codexctl-central")
    );
    let identity = server.cli(home.path(), &["whoami"]);
    assert!(identity.status.success());
    assert!(String::from_utf8_lossy(&identity.stdout).starts_with("local ("));
}

#[test]
fn when_registration_is_forgotten_during_migration_then_completion_does_not_recreate_it() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&auth("alex-login", "same-seat")).unwrap(),
    )
    .unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["migrate", "--all", "--exclusive-owner"])
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_refresh(&server);
    assert!(
        server
            .cli(home.path(), &["disconnect", "--forget"])
            .status
            .success()
    );
    store::atomic_write(&server.root.path().join("release"), b"released").unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(!home.path().join(".codexctl/central/personal.json").exists());
    let transfer: Value = serde_json::from_slice(
        &std::fs::read(paths.profiles_dir().join("personal/.central-transfer.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(transfer["confirmed"], true);
}

#[test]
fn native_credential_owners_run_inside_their_private_home() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("owner-cwd")).unwrap(),
        account_directory(&server, "amir", "personal")
            .join("runtime")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[test]
fn permanent_rejection_without_an_rpc_error_still_uses_native_auth_status_evidence() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"rejected-success").unwrap();
    let mut rejected = auth("same-login", "same-seat");
    rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    assert_eq!(
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.alex)
            .json(&json!({"alias":"pending","auth":rejected}))
            .send()
            .unwrap()
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        200
    );
}

#[test]
fn non_exportable_auth_metadata_cannot_prove_that_the_candidate_was_rejected() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"non-exportable").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "other", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "0"
    );
}

#[test]
fn refresh_only_rotation_changes_the_revision_and_is_not_replayed_for_another_client() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let before: Value = server.token(&server.amir, "personal", None).json().unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"refresh-only").unwrap();
    let after: Value = server
        .token(&server.amir, "personal", before["revision"].as_str())
        .json()
        .unwrap();
    assert_eq!(after["accessToken"], before["accessToken"]);
    assert_ne!(after["revision"], before["revision"]);
    let second: Value = server
        .token(&server.amir, "personal", before["revision"].as_str())
        .json()
        .unwrap();
    assert_eq!(second["revision"], after["revision"]);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
}

#[test]
fn relative_server_state_paths_still_load_the_private_owner_home() {
    let mut server = Server::start();
    server.stop();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .current_dir(server.root.path())
        .args([
            "serve",
            "--state",
            "state",
            "--key-file",
            "key",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8787",
            "--codex-bin",
        ])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"))
        .env("CENTRAL_TEST_MODE_FILE", server.root.path().join("mode"))
        .env(
            "CENTRAL_TEST_REFRESH_COUNTER",
            server.root.path().join("count"),
        )
        .env("CENTRAL_TEST_KEY_FILE", server.root.path().join("key"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    server.child = child;
    server.url = format!("http://{}", ready["listening"].as_str().unwrap());
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

fn auth_with_uid(subject: &str, account: &str, uid: &str) -> Value {
    let mut value = auth(subject, account);
    let token = value["tokens"]["access_token"].as_str().unwrap();
    let mut claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    claims["https://api.openai.com/auth"]["chatgpt_user_id"] = json!(uid);
    value["tokens"]["access_token"] = json!(format!(
        "header.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    ));
    value
}
#[test]
fn a_verified_alias_refuses_a_conflicting_uid_even_when_the_subject_matches() {
    let server = Server::start();
    let send = |value: Value| {
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","auth":value}))
            .send()
            .unwrap()
    };
    assert_eq!(
        send(auth_with_uid("same-login", "same-seat", "original-uid")).status(),
        200
    );
    assert_eq!(
        send(auth_with_uid("same-login", "same-seat", "conflicting-uid")).status(),
        409
    );
}
#[test]
fn a_legacy_migration_retry_keeps_the_uid_learned_by_the_server() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"gain-uid").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        200
    );
    let token: Value = server.token(&server.amir, "personal", None).json().unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"routing-error").unwrap();
    assert_eq!(
        server
            .token(&server.amir, "personal", token["revision"].as_str())
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        200
    );
    let recovered: Value = server.token(&server.amir, "personal", None).json().unwrap();
    let token = recovered["accessToken"].as_str().unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        claims["https://api.openai.com/auth"]["chatgpt_user_id"],
        "learned-uid"
    );
}

#[test]
fn unreadable_vault_journals_block_a_matching_loaded_owner_on_restart() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    server.stop();
    let unknown = account_directory(&server, "amir", "personal");
    let healthy = account_directory(&server, "alex", "personal");
    let journal = std::fs::read(healthy.join("runtime/auth.json")).unwrap();
    store::atomic_write(&unknown.join("runtime/auth.json"), &journal).unwrap();
    store::atomic_write(&unknown.join("vault.enc"), b"corrupt vault").unwrap();
    server.restart();
    assert_eq!(server.token(&server.alex, "personal", None).status(), 503);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
    assert_eq!(
        std::fs::read(unknown.join("runtime/auth.json")).unwrap(),
        journal
    );
}
#[test]
fn a_legacy_conflicting_journal_blocks_an_owner_with_a_known_uid_on_restart() {
    let mut server = Server::start();
    let send = |token: &str, alias: &str, value: Value| {
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(token)
            .json(&json!({"alias":alias,"auth":value}))
            .send()
            .unwrap()
    };
    assert_eq!(
        send(
            &server.amir,
            "personal",
            auth_with_uid("amir-login", "same-seat", "known-uid")
        )
        .status(),
        200
    );
    assert_eq!(
        send(&server.alex, "other", auth("other-login", "other-seat")).status(),
        200
    );
    server.stop();
    let journal = account_directory(&server, "alex", "other").join("runtime/auth.json");
    store::atomic_write(
        &journal,
        &serde_json::to_vec(&auth("amir-login", "same-seat")).unwrap(),
    )
    .unwrap();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
}
#[test]
fn a_non_exportable_baseline_never_attempts_import_refresh() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"baseline-null").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "0"
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "other", "same-login", "same-seat")
            .status(),
        503
    );
}

#[test]
fn a_conflicting_candidate_quarantines_both_identities_without_fencing_other_users() {
    for restart in [false, true] {
        let mut server = Server::start();
        store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
        assert_eq!(
            server
                .import(&server.amir, "candidate", "same-login", "same-seat")
                .status(),
            503
        );
        let directory = account_directory(&server, "amir", "candidate");
        let retained = std::fs::read(directory.join("runtime/auth.json")).unwrap();
        store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
        if restart {
            server.child.kill().unwrap();
            server.child.wait().unwrap();
            restart_after_crash(&mut server);
        }

        assert_eq!(
            server
                .import(&server.alex, "personal", "alex-login", "alex-seat")
                .status(),
            200
        );
        assert_eq!(
            server
                .import(&server.alex, "old-seat", "same-login", "same-seat")
                .status(),
            503
        );
        assert_eq!(
            server
                .import(&server.alex, "rotated-seat", "different-login", "same-seat")
                .status(),
            503
        );
        assert_eq!(
            std::fs::read(directory.join("runtime/auth.json")).unwrap(),
            retained
        );
        assert_eq!(
            std::fs::read_to_string(server.root.path().join("count")).unwrap(),
            "2"
        );
    }
}

#[test]
fn a_settled_conflicting_candidate_with_nonzero_exit_does_not_fence_other_users() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"identity-nonzero").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "candidate", "same-login", "same-seat")
            .status(),
        503
    );
    let directory = account_directory(&server, "amir", "candidate");
    let retained = std::fs::read(directory.join("runtime/auth.json")).unwrap();
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    for login in ["same-login", "different-login"] {
        assert_eq!(
            server
                .import(&server.alex, "reserved", login, "same-seat")
                .status(),
            503
        );
    }
    assert_eq!(
        std::fs::read(directory.join("runtime/auth.json")).unwrap(),
        retained
    );
}

#[test]
fn proactive_cached_login_rotation_is_reconciled_before_import_verification() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"cached-rotation").unwrap();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(server.accounts(&server.amir)[0]["available"], true);
    let directory = account_directory(&server, "amir", "personal");
    assert!(directory.join("vault.enc").exists());
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        "2"
    );
}

#[test]
fn proactive_cached_login_rejection_allows_a_fresh_same_account_grant() {
    for restart in [false, true] {
        let mut server = Server::start();
        store::atomic_write(&server.root.path().join("mode"), b"cached-rejection").unwrap();
        let mut rejected = auth("amir-login", "amir-seat");
        rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
        let response = server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","auth":rejected}))
            .send()
            .unwrap();
        assert_eq!(response.status(), 503);
        if restart {
            server.stop();
            server.restart();
        }
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
    }
}

#[test]
fn constrained_or_unknown_workspace_routes_are_refused_before_native_activation() {
    for mode in [
        "routing-us",
        "routing-us_cr",
        "routing-regional",
        "routing-missing",
    ] {
        let server = Server::start();
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        let home = server.connected_home();
        store::atomic_write(&server.root.path().join("mode"), mode.as_bytes()).unwrap();

        let output = server.cli(home.path(), &["use", "personal"]);

        assert!(
            !output.status.success(),
            "unsupported routing must refuse activation: {mode}"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("workspace routing"));
        assert!(!home.path().join(".codex/config.toml").exists());
        assert!(
            !home
                .path()
                .join(".codexctl/central/.native-active.json")
                .exists()
        );
    }
}

#[test]
fn completed_routing_policy_errors_preserve_token_and_catalog_recovery() {
    for mode in [
        "routing-policy-missing",
        "routing-policy-override",
        "billing-routing-policy-missing",
        "billing-routing-policy-override",
    ] {
        for catalog in [false, true] {
            let server = Server::start();
            assert_eq!(
                server
                    .import(&server.amir, "personal", "amir-login", "amir-seat")
                    .status(),
                200
            );
            store::atomic_write(&server.root.path().join("mode"), mode.as_bytes()).unwrap();
            if catalog {
                assert_eq!(server.accounts(&server.amir)[0]["available"], true);
            }
            {
                let refusal = server.token(&server.amir, "personal", None);
                assert_eq!(refusal.status(), 409);
                assert_eq!(
                    refusal.json::<Value>().unwrap()["error"],
                    "unsupported_workspace_routing"
                );
            }
            assert_eq!(server.accounts(&server.amir)[0]["available"], false);
            let directory = account_directory(&server, "amir", "personal");
            let journal: Value = serde_json::from_slice(
                &std::fs::read(directory.join("runtime/auth.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(saved_vault(&server, &directory)["auth"], journal);
            let secret = std::fs::read_to_string(server.root.path().join("metrics.token")).unwrap();
            let metrics = server
                .http
                .get(format!("{}/metrics", server.url))
                .bearer_auth(secret)
                .send()
                .unwrap()
                .text()
                .unwrap();
            assert!(metrics.contains(
                "codexctl_central_failed_requests_total{reason=\"unsupported_workspace_routing\"} 1"
            ));
            assert!(metrics.contains(
                "codexctl_central_failed_requests_total{reason=\"catalog_owner_unavailable\"} 0"
            ));
            store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
            assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
            assert_eq!(server.accounts(&server.amir)[0]["available"], true);
        }
    }
}

#[test]
fn similar_errors_without_the_pinned_routing_contract_stay_unavailable() {
    for mode in ["routing-policy-wrong-code", "billing-policy-wrong-method"] {
        let server = Server::start();
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        store::atomic_write(&server.root.path().join("mode"), mode.as_bytes()).unwrap();
        assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
        store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
        assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    }
}

#[test]
fn a_changed_workspace_route_refuses_native_token_delivery_and_counts_one_failure() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );
    store::atomic_write(&server.root.path().join("mode"), b"routing-us").unwrap();
    let connection = home.path().join(".codexctl/central/personal.json");

    let output = server.cli(
        home.path(),
        &[
            "central-token",
            "--connection",
            connection.to_str().unwrap(),
        ],
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("workspace routing"));
    let secret = std::fs::read_to_string(server.root.path().join("metrics.token")).unwrap();
    let metrics = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth(secret)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(metrics.contains(
        "codexctl_central_failed_requests_total{reason=\"unsupported_workspace_routing\"} 1"
    ));
}

fn saved_vault(server: &Server, directory: &std::path::Path) -> Value {
    use aes_gcm::{
        Aes256Gcm,
        aead::{Aead, KeyInit},
    };
    let key = std::fs::read(server.root.path().join("key")).unwrap();
    let bytes = std::fs::read(directory.join("vault.enc")).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let plain = cipher.decrypt(bytes[..12].into(), &bytes[12..]).unwrap();
    serde_json::from_slice(&plain).unwrap()
}

#[test]
fn unsupported_route_imports_stay_unverified_and_recoverable_across_restart() {
    for mode in [
        "routing-us",
        "routing-us_cr",
        "routing-regional",
        "routing-missing",
        "routing-policy-missing",
        "routing-policy-override",
    ] {
        let mut server = Server::start();
        store::atomic_write(&server.root.path().join("mode"), mode.as_bytes()).unwrap();
        let directory = account_directory(&server, "amir", "personal");
        for restart in [false, true] {
            if restart {
                server.stop();
                server.restart();
            }
            let response = server.import(&server.amir, "personal", "amir-login", "amir-seat");
            assert_eq!(response.status(), 409, "{mode}");
            assert_eq!(
                response.json::<Value>().unwrap()["error"],
                "unsupported_workspace_routing"
            );
            assert_eq!(saved_vault(&server, &directory)["verified"], false);
            assert_eq!(saved_vault(&server, &directory)["import_rejected"], false);
        }
        store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        assert_eq!(saved_vault(&server, &directory)["verified"], true);
    }
}

#[test]
fn a_previously_verified_alias_cannot_bypass_current_routing_on_import_retry() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    server.stop();
    store::atomic_write(&server.root.path().join("mode"), b"routing-us").unwrap();
    server.restart();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        409
    );
}

#[test]
fn routing_refresh_cannot_reuse_billing_evidence_for_an_older_revision() {
    for mode in ["routing-billing-change", "billing-late-change"] {
        let server = Server::start();
        assert_eq!(
            server
                .import(&server.amir, "personal", "amir-login", "amir-seat")
                .status(),
            200
        );
        let home = server.connected_home();
        assert!(
            server
                .cli(home.path(), &["use", "personal"])
                .status
                .success()
        );
        let connection = home.path().join(".codexctl/central/personal.json");
        assert!(
            server
                .cli(
                    home.path(),
                    &[
                        "central-token",
                        "--connection",
                        connection.to_str().unwrap()
                    ]
                )
                .status
                .success()
        );
        store::atomic_write(&server.root.path().join("mode"), mode.as_bytes()).unwrap();
        let output = server.cli(
            home.path(),
            &[
                "central-token",
                "--connection",
                connection.to_str().unwrap(),
            ],
        );
        assert!(
            !output.status.success(),
            "new plan must not inherit the old plan's approval"
        );
        assert!(output.stdout.is_empty());
        let saved = saved_vault(&server, &account_directory(&server, "amir", "personal"));
        let token = saved["auth"]["tokens"]["access_token"].as_str().unwrap();
        assert_eq!(
            codexctl::api::token_identity(token)
                .unwrap()
                .plan
                .as_deref(),
            Some("business")
        );
    }
}
#[test]
fn device_cleanup_remains_retryable_when_removing_the_credential_fails() {
    let server = Server::start();
    let home = server.connected_home();
    let directory = home.path().join(".codexctl/central");
    let token = directory.join(".device.token");
    std::fs::remove_file(&token).unwrap();
    std::fs::create_dir(&token).unwrap();
    assert!(
        !server
            .cli(home.path(), &["disconnect", "--forget"])
            .status
            .success()
    );
    assert!(directory.join(".server.json").exists());
    std::fs::remove_dir(&token).unwrap();
    assert!(
        server
            .cli(home.path(), &["disconnect", "--forget"])
            .status
            .success()
    );
    assert!(!directory.join(".server.json").exists());
    assert!(!token.exists());
}

#[test]
fn failed_migration_retries_preserve_confirmed_transfer_receipts() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    let first = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    central::managed::set_user(&server.root.path().join("state"), "amir", false).unwrap();
    assert!(
        !server
            .cli(home.path(), &["migrate", "--all", "--exclusive-owner"])
            .status
            .success()
    );
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(paths.profiles_dir().join("personal/.central-transfer.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["confirmed"], true);
    central::managed::set_user(&server.root.path().join("state"), "amir", true).unwrap();
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );
}
#[test]
fn api_key_only_auth_with_null_tokens_does_not_block_migration_or_activation() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    let api_key = json!({"OPENAI_API_KEY":"synthetic-api-key-only","tokens":null});
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&api_key).unwrap(),
    )
    .unwrap();
    let migrated = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );
    let retained: Value =
        serde_json::from_slice(&std::fs::read(paths.codex_auth_json()).unwrap()).unwrap();
    assert_eq!(retained, api_key);
}

#[cfg(unix)]
#[test]
fn inaccessible_sessions_refuse_migration_before_any_upload() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let credentials = serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap();
    store::atomic_write(&paths.codex_auth_json(), &credentials).unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    let session = paths.login_homes_dir().join("old-name/session-old");
    store::atomic_write(&session.join("auth.json"), &credentials).unwrap();
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o0)).unwrap();
    let result = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!result.status.success());
    assert!(server.accounts(&server.amir).as_array().unwrap().is_empty());
    assert!(session.join("auth.json").exists());
    assert!(
        !paths
            .profiles_dir()
            .join("personal/.central-transfer.json")
            .exists()
    );
}
#[cfg(unix)]
#[test]
fn inaccessible_sessions_refuse_remote_activation() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let session = paths.exec_homes_dir().join("old-name/session-old");
    store::atomic_write(
        &session.join("auth.json"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o0)).unwrap();
    let result = server.cli(home.path(), &["use", "personal"]);
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!result.status.success());
    assert!(session.join("auth.json").exists());
    assert!(
        !home
            .path()
            .join(".codexctl/central/.native-active.json")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn inaccessible_account_directories_fence_all_startup_replacements() {
    use std::os::unix::fs::PermissionsExt;
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    server.stop();
    let hidden = account_directory(&server, "amir", "personal");
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o0)).unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"startup").unwrap();
    server.restart();
    let status = server.token(&server.alex, "personal", None).status();
    let count = std::fs::read_to_string(server.root.path().join("count")).unwrap();
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(status, 503);
    assert_eq!(count, "2");
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
}

#[test]
fn an_interrupted_enrollment_install_keeps_a_cleanup_reference() {
    let server = Server::start();
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".codexctl/central");
    store::ensure_private_dir(&directory).unwrap();
    let destination = directory.join(".server.json");
    let mut protocol = Command::new("python3")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enrollment_install.py"))
        .arg(&destination)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(protocol.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    let result = server.cli(
        home.path(),
        &[
            "connect",
            "--server",
            ready["url"].as_str().unwrap(),
            "--no-browser",
        ],
    );
    let _ = protocol.kill();
    let _ = protocol.wait();
    assert!(!result.status.success());
    assert!(
        destination.is_dir(),
        "protocol must reach the installation write"
    );
    std::fs::remove_dir(&destination).unwrap();
    assert!(
        server
            .cli(home.path(), &["disconnect", "--forget"])
            .status
            .success()
    );
    assert!(!std::fs::read_dir(&directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|value| value == "token")
    }));
}
#[test]
fn a_top_level_chatgpt_account_id_can_be_migrated_and_selected() {
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let mut credentials = auth("amir-login", "amir-seat");
    let token = credentials["tokens"]["access_token"].as_str().unwrap();
    let mut claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    claims["https://api.openai.com/auth"]
        .as_object_mut()
        .unwrap()
        .remove("chatgpt_account_id");
    credentials["tokens"]["access_token"] = json!(format!(
        "header.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    ));
    credentials["tokens"]["account_id"] = Value::Null;
    credentials["chatgpt_account_id"] = json!("amir-seat");
    store::atomic_write(
        &paths.codex_auth_json(),
        &serde_json::to_vec(&credentials).unwrap(),
    )
    .unwrap();
    codexctl::profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
    let result = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );
}
#[cfg(unix)]
#[test]
fn enrollment_storage_failures_return_a_monitored_service_error() {
    use std::os::unix::fs::PermissionsExt;
    let issuer = EnrollmentServer::start(company_identity());
    let challenge = issuer.challenge();
    let page = issuer
        .browser(challenge["verificationUrl"].as_str().unwrap())
        .text()
        .unwrap();
    let lock = issuer.server.root.path().join("state/users.lock");
    store::atomic_write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o400)).unwrap();
    let response = issuer.approve(&page);
    let status = response.status();
    let body: Value = response.json().unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(status, 503);
    assert_eq!(body["error"], "registry_unavailable");
    assert_eq!(
        issuer
            .server
            .http
            .get(format!("{}/ready", issuer.server.url))
            .send()
            .unwrap()
            .status(),
        200
    );
    let metrics = issuer
        .server
        .http
        .get(format!("{}/metrics", issuer.server.url))
        .bearer_auth("synthetic-monitoring-credential-only")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        metrics.contains(
            "codexctl_central_failed_requests_total{reason=\"registry_unavailable\"} 1\n"
        ),
        "{metrics}"
    );
    assert!(metrics.contains(
        "codexctl_central_last_failure_timestamp_seconds{reason=\"registry_unavailable\"}"
    ));
    let timestamp = metrics
        .lines()
        .find(|line| {
            line.starts_with(
                "codexctl_central_last_failure_timestamp_seconds{reason=\"registry_unavailable\"}",
            )
        })
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!(timestamp > 0.0);
}

#[test]
fn enrollment_for_a_disabled_user_remains_a_policy_denial() {
    let issuer = EnrollmentServer::start(company_identity());
    let challenge = issuer.challenge();
    let page = issuer
        .browser(challenge["verificationUrl"].as_str().unwrap())
        .text()
        .unwrap();
    assert_eq!(issuer.approve(&page).status(), 200);
    let grant: Value = issuer.poll(&challenge).json().unwrap();
    let user: Value = issuer
        .server
        .http
        .get(format!("{}/v1/me", issuer.server.url))
        .bearer_auth(grant["deviceToken"].as_str().unwrap())
        .send()
        .unwrap()
        .json()
        .unwrap();
    let challenge = issuer.challenge();
    let page = issuer
        .browser(challenge["verificationUrl"].as_str().unwrap())
        .text()
        .unwrap();
    central::managed::set_user(
        &issuer.server.root.path().join("state"),
        user["id"].as_str().unwrap(),
        false,
    )
    .unwrap();
    let response = issuer.approve(&page);
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().unwrap()["error"],
        "user_unavailable"
    );
}

#[cfg(unix)]
#[test]
fn an_inaccessible_activation_marker_refuses_local_profile_save() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::start();
    let home = server.connected_home();
    let paths = codexctl::config::Paths::from_home(home.path().into());
    let credentials = serde_json::to_vec(&auth("previous-login", "previous-seat")).unwrap();
    store::atomic_write(&paths.codex_auth_json(), &credentials).unwrap();
    store::atomic_write(
        &paths.codex_home().join("config.toml"),
        b"model_provider = 'codexctl-central'\n",
    )
    .unwrap();
    let central = paths.codexctl_dir().join("central");
    store::atomic_write(
        &central.join(".native-active.json"),
        &serde_json::to_vec(&json!({"home": paths.codex_home(), "original_provider": null}))
            .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&central, std::fs::Permissions::from_mode(0o0)).unwrap();
    let result = server.cli(home.path(), &["save", "previous-local"]);
    std::fs::set_permissions(&central, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!result.status.success());
    assert!(!paths.profiles_dir().join("previous-local").exists());
    assert_eq!(std::fs::read(paths.codex_auth_json()).unwrap(), credentials);
}

#[test]
fn a_monitoring_token_with_a_trailing_newline_authenticates_scrapes() {
    let mut server = Server::start();
    server.stop();
    store::atomic_write(
        &server.root.path().join("metrics.token"),
        b"synthetic-monitoring-credential-only\n",
    )
    .unwrap();
    server.restart();
    assert_eq!(
        server
            .http
            .get(format!("{}/metrics", server.url))
            .bearer_auth("synthetic-monitoring-credential-only")
            .send()
            .unwrap()
            .status(),
        200
    );
}

#[cfg(unix)]
#[test]
fn server_selection_succeeds_while_a_session_or_its_orphaned_child_holds_a_lease() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::start();
    for (alias, subject, account) in [
        ("first", "first-login", "first-seat"),
        ("second", "second-login", "second-seat"),
    ] {
        assert_eq!(
            server
                .import(&server.amir, alias, subject, account)
                .status(),
            200
        );
    }
    let home = server.connected_home();
    assert!(server.cli(home.path(), &["use", "first"]).status.success());
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(
        bin.join("codex"),
        "#!/bin/sh\nprintf 'ready\\n'\nread -r finish\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .arg("codex")
        .env("HOME", home.path())
        .env("PATH", &bin)
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(launcher.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");

    let selected = server.cli(home.path(), &["use", "second"]);
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    // The Codex child still owns its inherited descriptor after launcher death.
    let reselected = server.cli(home.path(), &["use", "second"]);
    drop(launcher.stdin.take());
    let mut end = String::new();
    assert_eq!(output.read_line(&mut end).unwrap(), 0);

    for result in [selected, reselected] {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let config: toml_edit::DocumentMut =
        std::fs::read_to_string(home.path().join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    let provider = &config["model_providers"]["codexctl-central"];
    assert_eq!(
        provider["http_headers"]["ChatGPT-Account-ID"].as_str(),
        Some("second-seat")
    );
    assert!(
        provider["auth"]["args"]
            .as_array()
            .unwrap()
            .get(2)
            .unwrap()
            .as_str()
            .unwrap()
            .ends_with("/second.json")
    );
    // A running session's saved helper command must still supply its first account.
    let first = home.path().join(".codexctl/central/first.json");
    let token = server.cli(
        home.path(),
        &["central-token", "--connection", first.to_str().unwrap()],
    );
    assert!(token.status.success());
    let payload: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(
                String::from_utf8(token.stdout)
                    .unwrap()
                    .trim()
                    .split('.')
                    .nth(1)
                    .unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        payload["https://api.openai.com/auth"]["chatgpt_account_id"],
        "first-seat"
    );
}

#[cfg(unix)]
#[test]
fn a_pending_local_login_blocks_remote_activation_until_it_finishes() {
    pending_local_login_scenario(false);
}

#[cfg(unix)]
#[test]
fn a_pending_login_child_keeps_ownership_after_its_parent_is_killed() {
    pending_local_login_scenario(true);
}

#[cfg(unix)]
fn pending_local_login_scenario(kill_parent: bool) {
    use std::os::unix::fs::symlink;
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    symlink(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pending_codex_login.py"),
        bin.join("codex"),
    )
    .unwrap();
    let gate = home.path().join("login-gate");
    let start = |alias: &str, gate: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .args(["login", alias])
            .env("HOME", home.path())
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("CENTRAL_TEST_LOGIN_GATE", gate)
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let wait_ready = |gate: &std::path::Path| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !gate.with_extension("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        gate.with_extension("ready").exists()
    };
    let mut login = start("pending", &gate);
    let ready = wait_ready(&gate);
    let killed_status = if kill_parent {
        login.kill().unwrap();
        Some(login.wait().unwrap())
    } else {
        None
    };
    let other_gate = home.path().join("other-login-gate");
    let mut other_login = (!kill_parent).then(|| start("other-pending", &other_gate));
    let other_ready = other_login.is_none() || wait_ready(&other_gate);
    let selected = server.cli(home.path(), &["use", "personal"]);
    std::fs::write(&gate, b"finish").unwrap();
    std::fs::write(&other_gate, b"finish").unwrap();
    let failed_login = killed_status.unwrap_or_else(|| login.wait().unwrap());
    if let Some(other) = other_login.as_mut() {
        assert!(!other.wait().unwrap().success());
    }
    assert!(ready, "the device login must be pending before selection");
    assert!(other_ready, "parallel local logins must remain possible");
    assert!(
        !selected.status.success(),
        "activation must not overtake a pending login"
    );
    assert!(!failed_login.success());
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );
}

#[test]
fn hq_rejected_alias_cannot_renew_an_account_owned_by_another_user() {
    let server = Server::start();
    let mut rejected = auth("same-login", "same-seat");
    rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    let response = server
        .http
        .post(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.amir)
        .json(&json!({"alias":"rejected", "auth":rejected}))
        .send()
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        server
            .import(&server.alex, "owner", "same-login", "same-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&server.amir, "start", "rejected", &"a".repeat(64))
            .status(),
        409
    );
    assert_eq!(server.token(&server.alex, "owner", None).status(), 200);
}

#[test]
fn hq_partial_native_write_does_not_poison_later_renewal_or_unrelated_import() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let interrupted = "b".repeat(64);
    server.login_request(&server.amir, "start", "personal", &interrupted);
    server.login_request(&server.amir, "cancel", "personal", &interrupted);
    server.await_login(&server.amir, "personal", &interrupted, "canceled");
    server.stop();
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    store::atomic_write(
        &account.join(format!("relogin/{interrupted}/home/auth.json")),
        b"{partial",
    )
    .unwrap();
    server.restart();
    assert_eq!(
        server
            .import(&server.alex, "other", "alex-login", "alex-seat")
            .status(),
        200
    );
    let retry = "c".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &retry)
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &retry, "completed");
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
    assert_eq!(server.token(&server.alex, "other", None).status(), 200);
}

#[test]
fn hq_permanent_rejection_allows_a_fresh_login_after_restart() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let first = "d".repeat(64);
    server.login_request(&server.amir, "start", "personal", &first);
    let mut rejected = auth("amir-login", "amir-seat");
    rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&rejected).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &first, "failed");
    server.stop();
    std::fs::remove_file(server.root.path().join("login-release")).unwrap();
    server.restart();
    let retry = "e".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &retry)
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &retry, "completed");
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

#[test]
fn hq_cli_cancellation_can_run_while_the_original_cli_polls() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    assert!(server.cli(home.path(), &["list"]).status.success());
    let mut cli = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["login", "personal", "--no-browser"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    server.await_login(&server.amir, "personal", "", "pending");
    let canceled = server.cli(home.path(), &["login", "personal", "--cancel"]);
    let stderr = String::from_utf8_lossy(&canceled.stderr);
    if !stderr.contains("login stopped") {
        cli.kill().unwrap();
    }
    cli.wait().unwrap();
    assert!(stderr.contains("login stopped"), "{stderr}");
    assert_eq!(
        server
            .login_request(&server.amir, "status", "personal", "")
            .json::<Value>()
            .unwrap()["status"],
        "canceled"
    );
}

#[test]
fn hq_remote_login_normalizes_alias_before_routing() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    let home = server.connected_home();
    assert!(server.cli(home.path(), &["list"]).status.success());
    let result = server.cli(home.path(), &["login", " personal ", "--no-browser"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!home.path().join(".codexctl/login-homes").exists());
}

#[test]
fn hq_login_resolves_the_owner_after_a_concurrent_import_replaces_it() {
    let server = Server::start();
    let mut rejected = auth("amir-login", "same-seat");
    rejected["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    assert_eq!(
        server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"personal","auth":rejected}))
            .send()
            .unwrap()
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    std::thread::scope(|scope| {
        let unrelated =
            scope.spawn(|| server.import(&server.alex, "other", "alex-login", "other-seat"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !server.root.path().join("refresh-started").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let imported =
            scope.spawn(|| server.import(&server.amir, "personal", "amir-login", "same-seat"));
        std::thread::sleep(Duration::from_millis(100));
        let login = scope
            .spawn(|| server.login_request(&server.amir, "start", "personal", &"f".repeat(64)));
        std::thread::sleep(Duration::from_millis(100));
        store::atomic_write(&server.root.path().join("release"), b"go").unwrap();
        assert_eq!(unrelated.join().unwrap().status(), 200);
        assert_eq!(imported.join().unwrap().status(), 200);
        assert_eq!(login.join().unwrap().status(), 200);
    });
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "same-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &"f".repeat(64), "completed");
}

#[test]
fn hq_late_worker_cannot_undo_another_accounts_retirement() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "same-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "work", "alex-login", "same-seat")
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"login-hold-after-save").unwrap();
    let wrong = "1".repeat(64);
    server.login_request(&server.amir, "start", "personal", &wrong);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("alex-login", "same-seat")).unwrap(),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !server.root.path().join("login-saved").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    let repair = "2".repeat(64);
    std::thread::scope(|scope| {
        let request = scope.spawn(|| server.login_request(&server.alex, "start", "work", &repair));
        while !server.root.path().join("refresh-started").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        store::atomic_write(&server.root.path().join("login-exit"), b"go").unwrap();
        // The native child has exited, but its worker waits behind verification.
        std::thread::sleep(Duration::from_millis(150));
        store::atomic_write(&server.root.path().join("release"), b"go").unwrap();
        assert_eq!(request.join().unwrap().status(), 200);
    });
    server.await_login(&server.alex, "work", &repair, "completed");
    server.await_login(&server.amir, "personal", &wrong, "failed");
    assert_eq!(server.token(&server.alex, "work", None).status(), 200);
}

#[test]
fn hq_relative_state_path_uses_an_absolute_native_home() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    server.stop();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .current_dir(server.root.path())
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args([
            "serve",
            "--state",
            "state",
            "--key-file",
            "key",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8787",
            "--codex-bin",
        ])
        .arg(fixture)
        .env("CENTRAL_TEST_MODE_FILE", server.root.path().join("mode"))
        .env(
            "CENTRAL_TEST_REFRESH_COUNTER",
            server.root.path().join("count"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    server.url = format!("http://{}", ready["listening"].as_str().unwrap());
    server.child = child;
    let id = "3".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &id, "completed");
}

#[cfg(target_os = "linux")]
#[test]
fn hq_linux_parent_death_covers_the_missing_child_pid_window() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let id = "4".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = account.join(format!("relogin/{id}/record.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let pid = record["child"]["process"]["pid"].as_u64().unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        if stat
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            || stat
                .as_ref()
                .is_ok_and(|s| s.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PDEATHSIG must stop the login child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Retain only the broker identity, as if the crash preceded the child PID write.
    record["child"] = json!({"status":"spawning"});
    store::atomic_write(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    server.restart();
    server.await_login(&server.amir, "personal", &id, "failed");
    assert_eq!(
        server
            .import(&server.alex, "other", "alex-login", "alex-seat")
            .status(),
        200
    );
}

#[test]
fn hq_corrupt_unspawned_record_fences_only_its_account() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "other", "alex-login", "alex-seat")
            .status(),
        200
    );
    let id = "5".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    server.login_request(&server.amir, "cancel", "personal", &id);
    server.await_login(&server.amir, "personal", &id, "canceled");
    server.stop();
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.join(format!("relogin/{id}")).exists())
        .unwrap();
    let dir = account.join(format!("relogin/{id}"));
    // Model a damaged published preparation with durable proof of no spawn.
    store::atomic_write(&dir.join("home/spawn-failed"), b"not-started").unwrap();
    store::atomic_write(&dir.join("record.json"), b"{partial").unwrap();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(server.token(&server.alex, "other", None).status(), 200);
    assert_eq!(
        server
            .import(&server.alex, "new", "other-login", "new-seat")
            .status(),
        200
    );
}

#[test]
fn hq_rejection_after_a_partial_verification_still_allows_fresh_login() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let id = "6".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(&server.root.path().join("mode"), b"billing-error").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status: Value = server
            .login_request(&server.amir, "status", "personal", &id)
            .json()
            .unwrap();
        if status["error"].is_string() {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    store::atomic_write(&server.root.path().join("mode"), b"error").unwrap();
    let retried: Value = server
        .login_request(&server.amir, "start", "personal", &id)
        .json()
        .unwrap();
    assert_eq!(retried["status"], "failed");
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let fresh = "7".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &fresh)
            .status(),
        200
    );
    server.await_login(&server.amir, "personal", &fresh, "completed");
}

#[test]
fn hq_verifier_missing_pid_crash_keeps_unrelated_accounts_available() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let id = "a".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &id, "completed");
    let account = std::fs::read_dir(server.root.path().join("state/accounts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = account.join(format!("relogin/{id}/record.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        record["verifier_broker"], record["broker"],
        "the real verifier must persist its own spawn intent"
    );
    #[cfg(not(target_os = "linux"))]
    server.stop();
    #[cfg(target_os = "linux")]
    {
        let process: Value =
            serde_json::from_slice(&std::fs::read(account.join("runtime/pid")).unwrap()).unwrap();
        let pid = process["pid"].as_u64().unwrap();
        server.child.kill().unwrap();
        server.child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if stat
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                || stat
                    .as_ref()
                    .is_ok_and(|s| s.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "PDEATHSIG must stop the verifier"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Crash after durable verifier spawn intent, before its PID is persisted.
    record["phase"] = json!("promoted");
    store::atomic_write(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::remove_file(account.join("runtime/pid")).unwrap();
    assert!(!account.join("runtime/spawn-failed").exists());
    server.restart();
    assert_eq!(
        server
            .import(&server.alex, "other", "alex-login", "alex-seat")
            .status(),
        200,
        "unknown verifier must not fence other accounts"
    );
    #[cfg(target_os = "linux")]
    {
        server.await_login(&server.amir, "personal", &id, "completed");
        assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
}

#[test]
fn hq_known_local_login_does_not_require_the_server_catalog() {
    use std::os::unix::fs::PermissionsExt;
    let mut server = Server::start();
    let home = server.connected_home();
    let profile = home.path().join(".codexctl/profiles/local");
    store::ensure_private_dir(&profile).unwrap();
    store::atomic_write(
        &profile.join("auth.json"),
        &serde_json::to_vec(&auth("local-login", "local-seat")).unwrap(),
    )
    .unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join("codex");
    std::fs::write(
        &executable,
        "#!/bin/sh\necho LOCAL_LOGIN_REACHED >&2\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    server.stop();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .args(["login", "local"])
            .env("HOME", home.path())
            .env("PATH", &bin)
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .output()
            .unwrap()
    };
    let result = run();
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("LOCAL_LOGIN_REACHED"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // Known remote names must never fall through to native local login.
    store::atomic_write(&profile.join(".central-transfer.json"), b"{}").unwrap();
    assert!(!String::from_utf8_lossy(&run().stderr).contains("LOCAL_LOGIN_REACHED"));
    std::fs::remove_file(profile.join(".central-transfer.json")).unwrap();
    store::atomic_write(&home.path().join(".codexctl/central/local.json"), b"{}").unwrap();
    assert!(!String::from_utf8_lossy(&run().stderr).contains("LOCAL_LOGIN_REACHED"));
}

#[test]
fn hq_renewal_refuses_a_live_conflicting_import_journal() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "different-login", "same-seat")
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let before = std::fs::read_to_string(server.root.path().join("count")).unwrap();
    let id = "d".repeat(64);
    let result = server.login_request(&server.amir, "start", "personal", &id);
    if result.status().is_success() {
        server.login_request(&server.amir, "cancel", "personal", &id);
        server.await_login(&server.amir, "personal", &id, "canceled");
    }
    assert_eq!(
        result.status(),
        409,
        "renewal must reserve the conflicting journal before device login"
    );
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        before
    );
}

#[test]
fn hq_startup_refuses_verification_against_a_conflicting_import_journal() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "different-login", "same-seat")
            .status(),
        200
    );
    let id = "c".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(&server.root.path().join("mode"), b"routing-error").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("different-login", "same-seat")).unwrap(),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response: Value = server
            .login_request(&server.amir, "status", "personal", &id)
            .json()
            .unwrap();
        if response["error"] == "relogin_failed" {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let before = std::fs::read_to_string(server.root.path().join("count")).unwrap();
    unsafe {
        libc::kill(server.child.id() as i32, libc::SIGTERM);
    }
    assert!(!server.child.wait().unwrap().success());
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        before,
        "startup must not launch a competing verifier"
    );
}

fn wait_for_renewal_error(server: &Server, alias: &str, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result: Value = server
            .login_request(&server.amir, "status", alias, id)
            .json()
            .unwrap();
        if result["error"] == "relogin_failed" {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "login renewal did not fail"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hq5_verifier_retry_does_not_launch_a_conflicting_selected_journal() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        200
    );
    let id = "1".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("same-login", "same-seat")).unwrap(),
    )
    .unwrap();
    wait_for_renewal_error(&server, "personal", &id);
    let before = std::fs::read_to_string(server.root.path().join("count")).unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"startup").unwrap();
    server.login_request(&server.amir, "start", "personal", &id);
    assert_eq!(
        std::fs::read_to_string(server.root.path().join("count")).unwrap(),
        before,
        "no refresh process may start against the conflicting journal"
    );
}

#[test]
fn hq5_wrong_grant_fences_a_refresh_process_identified_only_by_its_journal() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let id = "2".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(&server.root.path().join("mode"), b"identity").unwrap();
    assert_eq!(
        server
            .import(&server.alex, "pending", "same-login", "same-seat")
            .status(),
        503
    );
    let state = account_directory(&server, "alex", "pending");
    let pid: Value =
        serde_json::from_slice(&std::fs::read(state.join("runtime/pid")).unwrap()).unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("different-login", "same-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &id, "failed");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { libc::kill(pid["pid"].as_i64().unwrap() as i32, 0) } != 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "quarantined runtime refresh process is still alive"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hq5_completed_retirement_retry_does_not_repeat_verification() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let id = "3".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("amir-login", "amir-seat")).unwrap(),
    )
    .unwrap();
    server.await_login(&server.amir, "personal", &id, "completed");
    let path =
        account_directory(&server, "amir", "personal").join(format!("relogin/{id}/record.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["phase"] = json!("retiring");
    store::atomic_write(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"routing-error").unwrap();
    let result: Value = server
        .login_request(&server.amir, "start", "personal", &id)
        .json()
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert!(
        result["error"].is_null(),
        "completed recovery must not run a second failing verifier"
    );
}

#[test]
fn hq5_unauthenticated_renewal_refuses_before_waiting_for_migration_lock() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    std::thread::scope(|scope| {
        let migration =
            scope.spawn(|| server.import(&server.alex, "work", "alex-login", "alex-seat"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !server.root.path().join("refresh-started").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = reqwest::blocking::Client::new()
            .post(format!("{}/v1/relogin/start", server.url))
            .json(&json!({"alias":"work","id":"4".repeat(64)}))
            .timeout(Duration::from_millis(400))
            .send();
        store::atomic_write(&server.root.path().join("release"), b"go").unwrap();
        assert_eq!(migration.join().unwrap().status(), 200);
        assert_eq!(
            result
                .expect("authentication must not wait on migration")
                .status(),
            401
        );
    });
}

#[test]
fn hq5_migration_normalizes_the_alias_used_for_lookup() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, " personal ", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(server.accounts(&server.amir)[0]["alias"], "personal");
    assert_eq!(server.token(&server.amir, " personal ", None).status(), 200);
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

#[test]
fn hq6_legacy_whitespace_alias_keeps_its_disk_reservation_and_can_renew() {
    use aes_gcm::{
        Aes256Gcm,
        aead::{Aead, KeyInit},
    };
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "same-login", "same-seat")
            .status(),
        200
    );
    server.stop();
    let canonical = account_directory(&server, "amir", "personal");
    let legacy = account_directory(&server, "amir", " personal ");
    let mut saved = saved_vault(&server, &canonical);
    saved["alias"] = json!(" personal ");
    let key = std::fs::read(server.root.path().join("key")).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    // Synthetic fixture only; this nonce is used once with this test's random key.
    let nonce = [123_u8; 12];
    let encrypted = cipher
        .encrypt(
            (&nonce).into(),
            serde_json::to_vec(&saved).unwrap().as_slice(),
        )
        .unwrap();
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    store::atomic_write(&canonical.join("vault.enc"), &bytes).unwrap();
    std::fs::rename(&canonical, &legacy).unwrap();
    server.restart();
    assert_eq!(server.accounts(&server.amir)[0]["alias"], "personal");
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
    assert_eq!(
        server
            .import(&server.amir, " personal ", "same-login", "same-seat")
            .status(),
        200
    );
    assert!(!canonical.exists());
    let id = "b".repeat(64);
    assert_eq!(
        server
            .login_request(&server.amir, "start", "personal", &id)
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&server.amir, "status", " personal ", &id)
            .status(),
        200
    );
    assert_eq!(
        server
            .login_request(&server.amir, "cancel", "personal", &id)
            .status(),
        200
    );
    server.await_login(&server.amir, "personal", &id, "canceled");
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth("same-login", "same-seat")).unwrap(),
    )
    .unwrap();
    let machine = server.connected_home();
    assert!(server.cli(machine.path(), &["list"]).status.success());
    let renewed = server.cli(machine.path(), &["login", "personal", "--no-browser"]);
    assert!(
        renewed.status.success(),
        "{}",
        String::from_utf8_lossy(&renewed.stderr)
    );
    assert_eq!(server.token(&server.amir, "personal", None).status(), 200);
}

#[test]
fn hq6_slow_refresh_does_not_block_another_alias_of_the_same_company_user() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "slow", "alex-login", "slow-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.amir, "fast", "amir-login", "fast-seat")
            .status(),
        200
    );
    let initial: Value = server.token(&server.amir, "slow", None).json().unwrap();
    store::atomic_write(&server.root.path().join("mode"), b"hold").unwrap();
    let fast = std::thread::scope(|scope| {
        let slow = scope.spawn(|| server.token(&server.amir, "slow", initial["revision"].as_str()));
        wait_for_refresh(&server);
        let fast = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/v1/token", server.url))
            .bearer_auth(&server.amir)
            .json(&json!({"alias":"fast", "billing":true}))
            .send();
        store::atomic_write(&server.root.path().join("release"), b"released").unwrap();
        assert_eq!(slow.join().unwrap().status(), 200);
        fast
    });
    assert_eq!(
        fast.expect("unrelated alias waited for slow refresh")
            .status(),
        200
    );
}

#[test]
fn hq6_partial_journal_does_not_prevent_an_unrelated_verified_restart() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    assert_eq!(
        server
            .import(&server.alex, "personal", "alex-login", "alex-seat")
            .status(),
        200
    );
    server.stop();
    let journal = account_directory(&server, "amir", "personal").join("runtime/auth.json");
    store::atomic_write(&journal, b"{partial").unwrap();
    server.restart();
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    assert_eq!(server.token(&server.alex, "personal", None).status(), 200);
    assert_eq!(std::fs::read(&journal).unwrap(), b"{partial");
    assert_eq!(
        server
            .import(&server.alex, "new", "other-login", "other-seat")
            .status(),
        503
    );
}

#[test]
fn hq7_unstarted_migrations_do_not_permanently_block_each_other() {
    for cross_user in [false, true] {
        let mut server = Server::start();
        server.stop();
        let executable = server.root.path().join("missing-codex");
        let (child, url) = Server::spawn_binary(&server.root, &executable);
        server.child = child;
        server.url = url;
        assert_eq!(
            server
                .import(&server.amir, "personal", "same-login", "same-seat")
                .status(),
            503
        );
        let claimant = if cross_user {
            &server.alex
        } else {
            &server.amir
        };
        assert_eq!(
            server
                .import(claimant, "work", "same-login", "same-seat")
                .status(),
            503
        );
        std::os::unix::fs::symlink(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py"),
            &executable,
        )
        .unwrap();
        assert_eq!(
            server
                .import(claimant, "work", "same-login", "same-seat")
                .status(),
            200,
            "two proven unstarted migrations must not reserve each other's grant"
        );
        assert_eq!(server.token(claimant, "work", None).status(), 200);
        assert_eq!(
            server
                .import(&server.amir, "personal", "same-login", "same-seat")
                .status(),
            409
        );
        assert_eq!(
            std::fs::read_to_string(server.root.path().join("count")).unwrap(),
            "1"
        );
    }
}

#[test]
fn hq7_migration_can_repair_quarantine_without_an_existing_server_account() {
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "shared-seat")
            .status(),
        200
    );
    let id = "c".repeat(64);
    server.login_request(&server.amir, "start", "personal", &id);
    store::atomic_write(
        &server.root.path().join("login-release"),
        &serde_json::to_vec(&auth_with_uid("alex-login", "shared-seat", "alex-uid")).unwrap(),
    )
    .unwrap();
    let failed = server.await_login(&server.amir, "personal", &id, "failed");
    assert_eq!(failed["error"], "wrong_account");
    let quarantine =
        account_directory(&server, "amir", "personal").join(format!("relogin/{id}/record.json"));
    let original: Value = serde_json::from_slice(&std::fs::read(&quarantine).unwrap()).unwrap();
    assert_eq!(original["retired"], false);
    // Neither missing nor conflicting UID claims can retire this reservation.
    for grant in [
        auth("alex-login", "shared-seat"),
        auth_with_uid("alex-login", "shared-seat", "wrong-uid"),
    ] {
        let refused = server
            .http
            .post(format!("{}/v1/accounts", server.url))
            .bearer_auth(&server.alex)
            .json(&json!({"alias":"work", "auth":grant}))
            .send()
            .unwrap();
        assert_eq!(refused.status(), 409);
        assert_eq!(
            refused.json::<Value>().unwrap()["error"],
            "alias_identity_conflict"
        );
        let metrics = server
            .http
            .get(format!("{}/metrics", server.url))
            .bearer_auth("synthetic-monitoring-credential-only")
            .send()
            .unwrap()
            .text()
            .unwrap();
        assert!(
            metrics
                .contains("codexctl_central_failed_requests_total{reason=\"recovery_failed\"} 0\n"),
            "{metrics}"
        );
        assert!(
            metrics.contains(
                "codexctl_central_last_failure_timestamp_seconds{reason=\"recovery_failed\"} 0\n"
            ),
            "{metrics}"
        );
        let retained: Value = serde_json::from_slice(&std::fs::read(&quarantine).unwrap()).unwrap();
        assert_eq!(retained["retired"], false);
        assert!(!account_directory(&server, "alex", "work").exists());
    }
    let mut grant = auth_with_uid("alex-login", "shared-seat", "alex-uid");
    grant["tokens"]["refresh_token"] = json!("synthetic-rejected-refresh");
    let rejected = server
        .http
        .post(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.alex)
        .json(&json!({"alias":"work", "auth":grant}))
        .send()
        .unwrap();
    assert_eq!(rejected.status(), 503);
    let retained: Value = serde_json::from_slice(&std::fs::read(&quarantine).unwrap()).unwrap();
    assert_eq!(
        retained, original,
        "rejected verification must retain the reservation"
    );
    grant["tokens"]["refresh_token"] = json!("synthetic-fresh-bootstrap-grant");
    let migrated = server
        .http
        .post(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.alex)
        .json(&json!({"alias":"work", "auth":grant}))
        .send()
        .unwrap();
    assert_eq!(
        migrated.status(),
        200,
        "a verified fresh migration must provide a quarantine repair path"
    );
    let retired: Value = serde_json::from_slice(&std::fs::read(&quarantine).unwrap()).unwrap();
    assert_eq!(retired["retired"], true);
    assert_eq!(
        retired["candidate"], original["candidate"],
        "retirement preserves quarantine evidence"
    );
    assert_eq!(server.token(&server.alex, "work", None).status(), 200);
    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    server.stop();
    // Recreate a crash after the verified vault write but before retirement.
    store::atomic_write(&quarantine, &serde_json::to_vec(&original).unwrap()).unwrap();
    server.restart();
    assert_eq!(server.token(&server.alex, "work", None).status(), 503);
    let before = std::fs::read_to_string(server.root.path().join("count"))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let retried = server
        .http
        .post(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.alex)
        .json(&json!({"alias":"work", "auth":grant}))
        .send()
        .unwrap();
    assert_eq!(retried.status(), 200);
    let after = std::fs::read_to_string(server.root.path().join("count"))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(
        after,
        before + 1,
        "retry must verify again before retirement"
    );
    let retired: Value = serde_json::from_slice(&std::fs::read(&quarantine).unwrap()).unwrap();
    assert_eq!(retired["retired"], true);
    server.stop();
    server.restart();
    assert_eq!(server.token(&server.alex, "work", None).status(), 200);
}

#[test]
fn p3_new_local_login_survives_failed_discovery_but_known_server_aliases_stay_fenced() {
    use std::os::unix::fs::PermissionsExt;
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let home = server.connected_home();
    // Populate the last catalog through the production discovery path.
    assert!(server.cli(home.path(), &["list"]).status.success());
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join("codex");
    std::fs::write(
        &executable,
        "#!/bin/sh\necho LOCAL_LOGIN_REACHED >&2\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    server.stop();
    // A listening endpoint detects even an attempted discovery request.
    let listener =
        std::net::TcpListener::bind(server.url.strip_prefix("http://").unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let probe = std::thread::spawn(move || {
        use std::io::Write;
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    return true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("{e}"),
            }
        }
        false
    });
    let run = |alias: &str| {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .args(["login", alias])
            .env("HOME", home.path())
            .env("PATH", &bin)
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .output()
            .unwrap()
    };
    let result = run("new-profile");
    done.store(true, std::sync::atomic::Ordering::Release);
    let contacted_server = probe.join().unwrap();
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("LOCAL_LOGIN_REACHED"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        contacted_server,
        "login must try fresh discovery before falling back to local login"
    );
    let collision = home.path().join(".codexctl/profiles/personal");
    store::ensure_private_dir(&collision).unwrap();
    store::atomic_write(
        &collision.join("auth.json"),
        &serde_json::to_vec(&auth("local-login", "local-seat")).unwrap(),
    )
    .unwrap();
    for alias in ["personal", "PERSONAL"] {
        let result = run(alias);
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(!error.contains("LOCAL_LOGIN_REACHED"));
        assert!(
            error.contains("server account") && error.contains("local login"),
            "{error}"
        );
    }
    let profile = home.path().join(".codexctl/profiles/migrated");
    store::ensure_private_dir(&profile).unwrap();
    store::atomic_write(&profile.join(".central-transfer.json"), b"{}").unwrap();
    for alias in ["migrated", "MIGRATED"] {
        assert!(!String::from_utf8_lossy(&run(alias).stderr).contains("LOCAL_LOGIN_REACHED"));
    }
    store::atomic_write(&home.path().join(".codexctl/central/connected.json"), b"{}").unwrap();
    for alias in ["connected", "CONNECTED"] {
        assert!(!String::from_utf8_lossy(&run(alias).stderr).contains("LOCAL_LOGIN_REACHED"));
    }
    let cache = home.path().join(".codexctl/central/.catalog.json");
    let mut known: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    known["connection"]["user_id"] = json!("another-company-user");
    store::atomic_write(&cache, &serde_json::to_vec(&known).unwrap()).unwrap();
    assert!(
        String::from_utf8_lossy(&run("personal").stderr).contains("LOCAL_LOGIN_REACHED"),
        "aliases cached for another registration cannot route this login"
    );
    std::fs::remove_file(&cache).unwrap();
    assert!(
        String::from_utf8_lossy(&run("new-profile").stderr).contains("LOCAL_LOGIN_REACHED"),
        "a new machine does not need a cached catalog to start local login"
    );
    store::atomic_write(&cache, b"{").unwrap();
    let refused = run("new-profile");
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(!error.contains("LOCAL_LOGIN_REACHED"));
    assert!(
        error.contains("cannot read known server aliases"),
        "{error}"
    );
}

#[test]
fn p3_overlapping_discoveries_preserve_the_newer_server_alias() {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    fn accept(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Some(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }
    fn reply(mut stream: TcpStream, body: &str) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            request.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    }
    let mut server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    let newer = server
        .http
        .get(format!("{}/v1/accounts", server.url))
        .bearer_auth(&server.amir)
        .send()
        .unwrap()
        .text()
        .unwrap();
    let home = server.connected_home();
    server.stop();
    let listener = TcpListener::bind(server.url.strip_prefix("http://").unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .arg("list")
            .env("HOME", home.path())
            .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let old_request = accept(&listener, Duration::from_secs(5)).unwrap();
    let cleanup = server.cli(home.path(), &["disconnect"]);
    let second = spawn();
    // Force the newer response to complete first if discovery is not serialized.
    if let Some(new_request) = accept(&listener, Duration::from_secs(1)) {
        reply(new_request, &newer);
        assert!(second.wait_with_output().unwrap().status.success());
        reply(old_request, "[]");
        assert!(first.wait_with_output().unwrap().status.success());
    } else {
        reply(old_request, "[]");
        assert!(first.wait_with_output().unwrap().status.success());
        reply(accept(&listener, Duration::from_secs(5)).unwrap(), &newer);
        assert!(second.wait_with_output().unwrap().status.success());
    }
    drop(listener);
    assert!(
        cleanup.status.success(),
        "{}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
    let login = server.cli(home.path(), &["login", "personal", "--no-browser"]);
    let error = String::from_utf8_lossy(&login.stderr);
    assert!(
        error.contains("cannot renew server account personal; local login is disabled"),
        "{error}"
    );
}

#[test]
fn when_subscription_cap_is_closed_then_catalog_allows_automatic_selection() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    store::atomic_write(&server.root.path().join("mode"), b"closed-spend-cap").unwrap();

    let accounts = server.accounts(&server.amir);
    let selected = central::remote::select(
        &serde_json::from_value::<Vec<central::managed::Account>>(accounts).unwrap(),
    );

    assert_eq!(selected.unwrap(), "personal");
}

#[test]
fn when_subscription_has_included_headroom_then_catalog_allows_automatic_selection() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    store::atomic_write(&server.root.path().join("mode"), b"included-weekly").unwrap();

    let accounts = server.accounts(&server.amir);
    let selected = central::remote::select(
        &serde_json::from_value::<Vec<central::managed::Account>>(accounts).unwrap(),
    );

    assert_eq!(selected.unwrap(), "personal");
}

#[test]
fn reset_listing_requires_a_registered_machine() {
    let server = Server::start();

    let response = server
        .http
        .get(format!("{}/v1/resets", server.url))
        .send()
        .unwrap();

    assert_eq!(response.status(), 401);
}

fn reset_cli(response: Value) -> std::process::Output {
    use std::io::Write;
    let server = Server::start();
    let home = server.connected_home();
    let migrated = home.path().join(".codexctl/profiles/personal");
    store::ensure_private_dir(&migrated).unwrap();
    store::atomic_write(
        &migrated.join("meta.json"),
        br#"{"alias":"personal","saved_at":"2036-01-01"}"#,
    )
    .unwrap();
    store::atomic_write(&migrated.join(".central-transfer.json"), b"{}").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let registration = home.path().join(".codexctl/central/.server.json");
    let mut connection: Value =
        serde_json::from_slice(&std::fs::read(&registration).unwrap()).unwrap();
    connection["server"] = json!(format!("http://{}", listener.local_addr().unwrap()));
    store::atomic_write(&registration, &serde_json::to_vec(&connection).unwrap()).unwrap();
    let request = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut received = String::new();
                let mut reader = BufReader::new(&mut stream);
                while !received.ends_with("\r\n\r\n") {
                    if reader.read_line(&mut received).unwrap() == 0 {
                        break;
                    }
                }
                drop(reader);
                let body = response.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                return received;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        String::new()
    });
    let output = server.cli(home.path(), &["resets"]);
    let request = request.join().unwrap();
    assert!(request.starts_with("GET /v1/resets "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer ")
    );
    output
}

#[test]
fn reset_listing_on_a_connected_machine_needs_no_local_auth() {
    let reply = json!({"userId":"amir", "accounts":[{"alias":"personal", "available":2, "applicable":1,
        "credits":[{"id":"later","status":"available","expires_at":"2036-08-12T12:00:00Z"},
                   {"id":"sooner","status":"available","expires_at":"2036-07-26T12:00:00Z"}]}]});

    let output = reset_cli(reply);
    let text = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("2 banked, 1 redeemable now."), "{text}");
    assert!(text.contains("Jul 26, Aug 12"), "{text}");
}

#[test]
fn reset_listing_failure_does_not_report_a_complete_zero_total() {
    let reply =
        json!({"userId":"amir", "accounts":[{"alias":"personal", "error":"reset_read_failed"}]});

    let output = reset_cli(reply);
    let text = String::from_utf8_lossy(&output.stdout);

    assert!(!output.status.success());
    assert!(!text.contains("0 banked, 0 redeemable now."), "{text}");
    assert!(text.contains("reset_read_failed"), "{text}");
}

#[test]
fn reset_listing_rejects_another_company_users_response() {
    let reply = json!({"userId":"alex", "accounts":[]});

    let output = reset_cli(reply);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("server user identity changed"));
}

#[test]
fn b8_listing_cannot_disable_tokens_after_a_temporary_owner_error() {
    let server = Server::start();
    assert_eq!(
        server
            .import(&server.amir, "personal", "amir-login", "amir-seat")
            .status(),
        200
    );
    store::atomic_write(&server.root.path().join("mode"), b"billing-error").unwrap();

    let catalog = server.accounts(&server.amir);
    store::atomic_write(&server.root.path().join("mode"), b"").unwrap();
    let token = server.token(&server.amir, "personal", None);

    assert_eq!(catalog[0]["available"], true);
    assert_eq!(token.status(), 200);
}

#[test]
fn connected_status_json_preserves_server_usage_and_failed_local_duplicate() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"status-reset").unwrap();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    local_profile(home.path(), "personal");
    let retired = local_profile(home.path(), "retired");
    store::atomic_write(&retired.join(".central-transfer.json"), b"{}").unwrap();

    let output = server.cli(home.path(), &["status", "--json"]);

    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    let age = document["accounts"][0]["usage_age_seconds"]
        .as_u64()
        .unwrap();
    assert!(output.status.success() && age < 60, "{output:?}");
    assert_eq!(
        document,
        json!({
        "version":1,"accounts":[
            {"alias":"personal","label":"Personal","plan":"pro","source":"server","state":"server",
             "primary_used_percent":0.0,"secondary_used_percent":37.0,
             "primary_window_seconds":18000,"secondary_window_seconds":604800,
             "primary_resets_at":"2100-01-01T00:00:00Z","secondary_resets_at":"2100-01-01T00:00:00Z",
             "resets_at":"2100-01-01T00:00:00Z","billing_class":"rate_limited","error":null,"usage_age_seconds":age,"usage_stale":false},
            {"alias":"personal","label":null,"plan":null,"source":"local","state":"local",
             "primary_used_percent":null,"secondary_used_percent":null,
             "primary_window_seconds":null,"secondary_window_seconds":null,
             "primary_resets_at":null,"secondary_resets_at":null,
             "resets_at":null,"billing_class":"unknown","error":"credentials unavailable","usage_age_seconds":null,"usage_stale":null}
        ]})
    );
}

#[test]
fn status_json_from_an_old_server_keeps_window_durations_unknown() {
    // This protocol fixture omits duration fields, like an older account server.
    let (_, output) = automatic_reset_attempt(&["status", "--json"], "pro", 25.0, None);
    assert!(output.status.success(), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    let row = &document["accounts"][0];
    assert_eq!(row.get("primary_window_seconds"), Some(&Value::Null));
    assert_eq!(row.get("secondary_window_seconds"), Some(&Value::Null));
    assert_eq!(row.get("primary_resets_at"), Some(&Value::Null));
    assert_eq!(row["secondary_resets_at"], "2033-05-18T03:33:20Z");
    assert_eq!(row["resets_at"], row["secondary_resets_at"]);
    assert_eq!(row["primary_used_percent"], 25.0);
    assert_eq!(row["secondary_used_percent"], 10.0);
}

#[test]
fn connected_list_json_returns_empty_catalog_without_prose() {
    let server = Server::start();
    let home = server.connected_home();

    let output = server.cli(home.path(), &["list", "--json"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"version":1,"accounts":[]})
    );
}

#[test]
fn connected_status_json_keeps_unavailable_accounts() {
    let server = Server::start();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    store::atomic_write(&server.root.path().join("mode"), b"billing-error").unwrap();

    assert_eq!(server.token(&server.amir, "personal", None).status(), 503);
    let output = server.cli(home.path(), &["status", "--json"]);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(document["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(
        (
            &document["accounts"][0]["state"],
            &document["accounts"][0]["error"]
        ),
        (&json!("unavailable"), &json!("account unavailable"))
    );
}

#[test]
fn connected_status_json_filter_matches_the_table() {
    let server = Server::start();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    local_profile(home.path(), "broken");

    let output = server.cli(home.path(), &["status", "--json", "--usage-based"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"version":1,"accounts":[]})
    );
}

#[test]
fn b8_status_json_reports_stale_usage_without_disabling_the_account() {
    let mut server = Server::start();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    server.stop();
    server.restart();
    let home = server.connected_home();

    let output = server.cli(home.path(), &["status", "--json"]);
    let row = serde_json::from_slice::<Value>(&output.stdout).unwrap()["accounts"][0].clone();

    assert!(output.status.success());
    assert_eq!(
        (
            row["state"].clone(),
            row["usage_stale"].clone(),
            row["usage_age_seconds"].clone()
        ),
        (json!("server"), json!(true), Value::Null)
    );
    assert_eq!(row["error"], "catalog_usage_failed");
}

#[test]
fn statusline_status_json_populates_only_the_active_server_account() {
    let server = Server::start();
    store::atomic_write(&server.root.path().join("mode"), b"status-reset").unwrap();
    server.import(&server.amir, "personal", "server-login", "server-seat");
    let home = server.connected_home();
    assert!(
        server
            .cli(home.path(), &["use", "personal"])
            .status
            .success()
    );

    let status = server.cli(home.path(), &["status", "--json"]);
    let line = server.cli(home.path(), &["statusline"]);

    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&line.stdout).starts_with("Personal 63% wk · "));
    assert!(line.stderr.is_empty());
}

#[test]
fn reset_redemption_rejects_another_company_users_account() {
    let server = Server::start();
    server.import(&server.amir, "personal", "amir-login", "amir-seat");
    let response = server
        .http
        .post(format!("{}/v1/resets/redeem", server.url))
        .bearer_auth(&server.alex)
        .json(&json!({"alias":"personal","redeem_request_id":"wrong-company-user"}))
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        response.json::<Value>().unwrap(),
        json!({"error":"account_not_found"})
    );
}

#[test]
fn resets_redeem_command_accepts_an_alias_and_explicit_consent() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .args(["resets", "--redeem", "missing", "--yes"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("profile 'missing' not found"), "{error}");
}

#[test]
fn server_reset_cli_retries_an_ambiguous_response_with_the_same_request_id() {
    server_reset_cli_retry(false);
}

#[test]
fn server_reset_cli_starts_a_new_operation_after_a_terminal_rejection() {
    server_reset_cli_retry(true);
}

fn server_reset_cli_retry(terminal: bool) {
    use std::io::{Read, Write};
    let server = Server::start();
    let home = server.connected_home();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let registration = home.path().join(".codexctl/central/.server.json");
    let mut connection: Value =
        serde_json::from_slice(&std::fs::read(&registration).unwrap()).unwrap();
    connection["server"] = json!(format!("http://{}", listener.local_addr().unwrap()));
    store::atomic_write(&registration, &serde_json::to_vec(&connection).unwrap()).unwrap();
    let requests = std::thread::spawn(move || {
        let mut redemptions = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while std::time::Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            while !headers.ends_with("\r\n\r\n") {
                if reader.read_line(&mut headers).unwrap() == 0 {
                    break;
                }
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            let (status, response) = if headers.starts_with("GET /v1/resets ") {
                (
                    "200 OK",
                    json!({"userId":"amir","accounts":[{"alias":"personal","available":2,"applicable":1,"credits":[]}]}),
                )
            } else {
                assert!(headers.starts_with("POST /v1/resets/redeem "));
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["alias"], "personal");
                assert_eq!(body.as_object().unwrap().len(), 2);
                redemptions.push(body);
                if redemptions.len() == 1 && terminal {
                    (
                        "422 Unprocessable Entity",
                        json!({"error":"reset_rejected"}),
                    )
                } else if redemptions.len() == 1 {
                    ("502 Bad Gateway", json!({"error":"reset_redeem_failed"}))
                } else {
                    (
                        "200 OK",
                        json!({"code":"already_redeemed","windows_reset":1}),
                    )
                }
            };
            let body = response.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            if redemptions.len() == 2 {
                break;
            }
        }
        redemptions
    });
    let first = server.cli(home.path(), &["resets", "--redeem", "personal", "--yes"]);
    assert!(!first.status.success());
    let retry = server.cli(home.path(), &["resets", "--redeem", "personal", "--yes"]);
    let requests = requests.join().unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(requests.len(), 2);
    if terminal {
        assert_ne!(
            requests[0]["redeem_request_id"],
            requests[1]["redeem_request_id"]
        );
    } else {
        assert_eq!(requests[0], requests[1]);
    }
}

#[test]
fn server_automatic_reset_requires_reset_approval_and_never_runs_for_explicit_aliases() {
    for (args, plan, used, expected) in [
        (vec!["use", "--allow-resets"], "pro", 100.0, 1),
        (vec!["use"], "pro", 100.0, 0),
        (vec!["use", "--allow-billing"], "pro", 100.0, 0),
        (
            vec!["use", "personal", "--allow-resets", "--allow-billing"],
            "pro",
            100.0,
            0,
        ),
        (vec!["use", "--allow-resets"], "team_usage_based", 100.0, 0),
        (vec!["use", "--allow-resets"], "pro", 20.0, 0),
    ] {
        let (count, _) = automatic_reset_attempt(&args, plan, used, None);
        assert_eq!(count, expected, "{args:?}, {plan}, {used}");
    }
}

fn automatic_reset_attempt(
    args: &[&str],
    plan: &str,
    used: f64,
    fence: Option<&str>,
) -> (usize, std::process::Output) {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".codexctl/central");
    let credential = directory.join("device.token");
    store::atomic_write(&credential, b"synthetic-device").unwrap();
    let paths = codexctl::config::Paths::from_home(home.path().to_path_buf());
    let _store_lock = if fence == Some("store") {
        Some(store::lock(&paths).unwrap())
    } else {
        None
    };
    if fence == Some("daemon") {
        store::atomic_write(
            &home.path().join(".codex/app-server-daemon/daemon.pid"),
            &serde_json::to_vec(&json!({"pid": std::process::id()})).unwrap(),
        )
        .unwrap();
    }
    if fence == Some("profile") {
        store::atomic_write(
            &home.path().join(".codex/config.toml"),
            b"profile = 'blocked'\n[profiles.blocked]\nmodel_provider = 'other'\n",
        )
        .unwrap();
    }
    if fence == Some("provider") {
        store::atomic_write(
            &home.path().join(".codex/config.toml"),
            b"[model_providers.codexctl-central]\nname = 'reserved'\n",
        )
        .unwrap();
    }
    if fence == Some("handoff") {
        store::atomic_write(
            &home.path().join(".codex/auth.json"),
            &serde_json::to_vec(&auth("synthetic-login", "synthetic-seat")).unwrap(),
        )
        .unwrap();
    }
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = received.clone();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    store::atomic_write(
        &directory.join(".server.json"),
        &serde_json::to_vec(
            &json!({"server":server,"token_file":credential,"user_id":"synthetic-user"}),
        )
        .unwrap(),
    )
    .unwrap();
    let account = json!({"userId":"synthetic-user","alias":"personal","label":null,"accountId":"synthetic-seat","plan":plan,"billingClass":if used < 100.0 { "rate_limited" } else { "unknown" },"primaryUsed":used,"secondaryUsed":10.0,"resetsAt":2000000000,"available":true,"usageScore":if used < 100.0 { 20.0 } else { 600.0 },"usageStale":false});
    let token_auth = auth("synthetic-login", "synthetic-seat");
    let token = json!({"userId":"synthetic-user","accessToken":token_auth["tokens"]["access_token"],"chatgptAccountId":"synthetic-seat","chatgptPlanType":plan,"revision":"synthetic-revision","billingClass":"unknown","nativeRoutingSupported":true});
    let token_calls = Arc::new(Mutex::new(0));
    let settles = fence == Some("settles");
    let successful_reset = settles || fence == Some("never-settles");
    let app = Router::new()
            .route("/v1/token", post(move || async move {
                let mut count = token_calls.lock().unwrap();
                *count += 1;
                let mut token = token;
                if settles && *count >= 3 { token["billingClass"] = json!("rate_limited"); }
                Json(token)
            }))
            .route("/v1/accounts", get(move || async move { Json(json!([account])) }))
            .route("/v1/resets", get(|| async { Json(json!({"userId":"synthetic-user","accounts":[{"alias":"personal","available":1,"applicable":1,"credits":[{"id":"soon","status":"available","expires_at":"2036-07-26T00:00:00Z"}]}]})) }))
            .route("/v1/resets/redeem", post(move |Json(body): Json<Value>| async move {
                requests.lock().unwrap().push(body);
                use axum::response::IntoResponse;
                if successful_reset {
                    Json(json!({"code":"reset","windows_reset":1})).into_response()
                } else { axum::http::StatusCode::BAD_GATEWAY.into_response() }
            }));
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
    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("HOME", home.path())
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .args(args)
        .output()
        .unwrap();
    stop.send(()).unwrap();
    thread.join().unwrap();
    let count = received.lock().unwrap().len();
    (count, output)
}

#[test]
fn automatic_reset_waits_for_all_local_activation_fences() {
    for fence in ["profile", "store", "handoff", "provider", "daemon"] {
        let (count, output) =
            automatic_reset_attempt(&["use", "--allow-resets"], "pro", 100.0, Some(fence));
        assert!(!output.status.success());
        assert_eq!(
            count,
            0,
            "spent before {fence} refusal: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn local_reset_does_not_contact_a_configured_unavailable_account_server() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".codexctl");
    store::atomic_write(&directory.join("central/.server.json"), &serde_json::to_vec(&json!({"server":"http://127.0.0.1:1","token_file":directory.join("central/device.token"),"user_id":"synthetic-user"})).unwrap()).unwrap();
    store::atomic_write(&directory.join("central/device.token"), b"synthetic-device").unwrap();
    store::atomic_write(
        &directory.join("profiles/local/meta.json"),
        br#"{"alias":"local","saved_at":"2036-01-01"}"#,
    )
    .unwrap();
    store::atomic_write(&directory.join("profiles/local/auth.json"), b"{}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("HOME", home.path())
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .args(["reset", "local", "--yes"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("access_token"),
        "local auth validation should run: {error}"
    );
    assert!(!error.contains("account server"), "{error}");
}

#[test]
fn automatic_reset_rechecks_included_usage_and_reports_a_spend_before_a_lag_refusal() {
    for (mode, success) in [("settles", true), ("never-settles", false)] {
        let (count, output) =
            automatic_reset_attempt(&["use", "--allow-resets"], "pro", 100.0, Some(mode));
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(count, 1, "{error}");
        assert_eq!(output.status.success(), success, "{mode}: {error}");
        if !success {
            assert!(error.contains("a reset was redeemed"), "{error}");
            assert!(error.contains("without --allow-resets"), "{error}");
        }
    }
}

#[test]
fn dashboard_requires_company_browser_sign_in() {
    let issuer = EnrollmentServer::start(company_identity());
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for credential in [None, Some(issuer.server.amir.as_str())] {
        let mut request = http.get(format!("{}/accounts/data", issuer.server.url));
        if let Some(token) = credential {
            request = request.bearer_auth(token);
        }
        let response = request.send().unwrap();
        assert_eq!(response.status(), 401);
    }
    let page = http
        .get(format!("{}/accounts", issuer.server.url))
        .send()
        .unwrap();
    assert_eq!(page.status(), 303);
    assert_eq!(page.headers()["location"], "/accounts/sign-in");
}

impl EnrollmentServer {
    fn dashboard_cookie(&self) -> String {
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let start = http
            .get(format!("{}/accounts/sign-in", self.server.url))
            .send()
            .unwrap();
        assert_eq!(start.status(), 303);
        let binding = start.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let authorize = http
            .get(start.headers()["location"].to_str().unwrap())
            .send()
            .unwrap();
        let callback = http
            .get(authorize.headers()["location"].to_str().unwrap())
            .header("cookie", binding)
            .send()
            .unwrap();
        assert_eq!(callback.status(), 303);
        let cookie = callback
            .headers()
            .get_all("set-cookie")
            .iter()
            .find(|c| c.to_str().unwrap().contains("session="))
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("HttpOnly; SameSite=Lax; Max-Age=3600"));
        cookie.split(';').next().unwrap().to_owned()
    }
    fn enrolled_token(&self) -> String {
        let challenge = self.challenge();
        let approval = self
            .browser(challenge["verificationUrl"].as_str().unwrap())
            .text()
            .unwrap();
        assert_eq!(self.approve(&approval).status(), 200);
        self.poll(&challenge).json::<Value>().unwrap()["deviceToken"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[test]
fn dashboard_snapshot_is_scoped_secret_free_and_preserves_unknown_windows() {
    let issuer = EnrollmentServer::start(company_identity());
    let token = issuer.enrolled_token();
    store::atomic_write(
        &issuer.server.root.path().join("mode"),
        b"dashboard-unknown",
    )
    .unwrap();
    assert_eq!(
        issuer
            .server
            .import(&token, "mine", "my-login", "private-workspace")
            .status(),
        200
    );
    assert_eq!(
        issuer
            .server
            .import(
                &issuer.server.alex,
                "not-mine",
                "other-login",
                "other-workspace"
            )
            .status(),
        200
    );
    let cookie = issuer.dashboard_cookie();
    let response = issuer
        .server
        .http
        .get(format!("{}/accounts/data?user=alex", issuer.server.url))
        .header("cookie", &cookie)
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.text().unwrap();
    for secret in [
        &token,
        issuer.server.alex.as_str(),
        "synthetic-refresh",
        "private-workspace",
        "other-workspace",
        "not-mine",
        "alex-laptop",
        "access_token",
        "refresh_token",
        "token_hash",
        "synthetic-company-access",
    ] {
        assert!(!body.contains(secret));
    }
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["version"], 1);
    assert_eq!(data["accounts"].as_array().unwrap().len(), 1);
    let account = &data["accounts"][0];
    assert_eq!(account["alias"], "mine");
    assert_eq!(account["label"], "Personal");
    assert_eq!(account["state"], "available");
    assert_eq!(account["primary"]["used_percent"], 15.0);
    assert_eq!(account["primary"]["left_percent"], 85.0);
    assert!(account["primary"].get("window_seconds").unwrap().is_null());
    assert!(
        account["secondary"]
            .get("window_seconds")
            .unwrap()
            .is_null()
    );
    assert!(account["banked_resets"].get("count").unwrap().is_null());
    assert!(account["usage_age_seconds"].as_u64().unwrap() < 10);
    assert_eq!(data["machines"].as_array().unwrap().len(), 1);
    assert!(
        data["machines"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("Amir MacBook")
    );
    assert_eq!(data["machines"][0]["status"], "registered");
    assert!(data["machines"][0].get("last_seen_at").unwrap().is_null());
    assert!(data["machines"][0].get("last_used_alias").is_none());
    // Existing sessions lose access immediately when the company user is disabled.
    let user = central::managed::users(&issuer.server.root.path().join("state"))
        .unwrap()
        .into_iter()
        .find(|u| u.id != "amir" && u.id != "alex")
        .unwrap();
    central::managed::set_user(&issuer.server.root.path().join("state"), &user.id, false).unwrap();
    for path in ["/accounts", "/accounts/data"] {
        assert_eq!(
            issuer
                .server
                .http
                .get(format!("{}{path}", issuer.server.url))
                .header("cookie", &cookie)
                .send()
                .unwrap()
                .status(),
            403
        );
    }
}

#[test]
fn dashboard_connect_command_uses_configured_origin_not_request_headers() {
    let issuer = EnrollmentServer::start(company_identity());
    let without_sso = Server::start();
    for (server, configured) in [
        (&issuer.server, issuer.server.url.as_str()),
        (&without_sso, "http://127.0.0.1:8787"),
    ] {
        let response = server
            .http
            .get(format!("{}/", server.url))
            .header("host", "untrusted.example")
            .header("forwarded", "host=untrusted.example;proto=https")
            .header("x-forwarded-host", "untrusted.example")
            .send()
            .unwrap();
        assert_eq!(response.status(), 200);
        let html = response.text().unwrap();
        let command = html
            .split("id=\"connect-command\">")
            .nth(1)
            .unwrap()
            .split("</code>")
            .next()
            .unwrap();
        assert!(
            command.contains(configured),
            "connect command must target the configured server: {command}"
        );
        assert!(!command.contains("staging.plat.sm-svc.com"));
        assert!(!command.contains("untrusted.example"));
    }
}

#[test]
fn dashboard_root_is_public_without_account_data_and_redirects_signed_in_browsers() {
    let issuer = EnrollmentServer::start(company_identity());
    issuer.server.import(
        &issuer.server.amir,
        "private-alias",
        "private-login",
        "private-seat",
    );
    let response = issuer
        .server
        .http
        .get(format!("{}/", issuer.server.url))
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let html = response.text().unwrap();
    assert!(html.contains("Sign in with company SSO"));
    assert!(html.contains("brew install sawmills/tap/codexctl"));
    assert!(html.contains("sha256sum --check"));
    for private in [
        "private-alias",
        "private-login",
        "private-seat",
        "amir-laptop",
        "synthetic-refresh",
    ] {
        assert!(!html.contains(private));
    }
    let cookie = issuer.dashboard_cookie();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = http
        .get(format!("{}/", issuer.server.url))
        .header("cookie", cookie)
        .send()
        .unwrap();
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers()["location"], "/accounts");
}

#[test]
fn dashboard_disabled_session_can_view_public_landing_but_not_accounts() {
    let issuer = EnrollmentServer::start(company_identity());
    let cookie = issuer.dashboard_cookie();
    let state = issuer.server.root.path().join("state");
    let user = central::managed::users(&state)
        .unwrap()
        .into_iter()
        .find(|u| u.id != "amir" && u.id != "alex")
        .unwrap();
    central::managed::set_user(&state, &user.id, false).unwrap();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = http
        .get(format!("{}/", issuer.server.url))
        .header("cookie", &cookie)
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let html = response.text().unwrap();
    assert!(html.contains("Sign in with company SSO"));
    assert!(html.contains("id=\"connect-command\""));
    assert!(!html.contains(&user.email));
    for path in ["/accounts", "/accounts/data"] {
        assert_eq!(
            http.get(format!("{}{path}", issuer.server.url))
                .header("cookie", &cookie)
                .send()
                .unwrap()
                .status(),
            403
        );
    }
}

#[test]
fn dashboard_pages_pin_bundled_assets_and_contain_no_credentials() {
    use base64::engine::general_purpose::STANDARD;
    use sha2::{Digest, Sha256};
    let issuer = EnrollmentServer::start(company_identity());
    let cookie = issuer.dashboard_cookie();
    for (path, filename) in [("/", "landing.html"), ("/accounts", "accounts.html")] {
        let mut request = issuer
            .server
            .http
            .get(format!("{}{path}", issuer.server.url));
        if path == "/accounts" {
            request = request.header("cookie", &cookie);
        }
        let response = request.send().unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["referrer-policy"], "same-origin");
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        let html = response.text().unwrap();
        let footer = html
            .split("<footer")
            .nth(1)
            .unwrap()
            .split("</footer>")
            .next()
            .unwrap();
        assert!(
            footer.contains(&format!("codexctl v{}", env!("CARGO_PKG_VERSION"))),
            "footer must follow the binary build version"
        );
        assert!(!footer.contains("<!-- VERSION -->"));
        for tag in ["style", "script"] {
            let asset = html
                .split(&format!("<{tag}>"))
                .nth(1)
                .unwrap()
                .split(&format!("</{tag}>"))
                .next()
                .unwrap();
            assert!(csp.contains(&format!(
                "{tag}-src 'sha256-{}'",
                STANDARD.encode(Sha256::digest(asset))
            )));
        }
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("unsafe-inline"));
        for secret in [
            &cookie,
            "synthetic-company-client-secret",
            "synthetic-company-access",
            "synthetic-refresh",
            "token_hash",
        ] {
            assert!(!html.contains(secret));
        }
        if let Ok(directory) = std::env::var("B16_RENDER_DIR") {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(std::path::Path::new(&directory).join(filename), html).unwrap();
            std::fs::write(
                std::path::Path::new(&directory).join(format!("{filename}.csp")),
                csp,
            )
            .unwrap();
        }
    }
}

#[test]
fn dashboard_sign_in_refuses_a_callback_from_another_browser() {
    let issuer = EnrollmentServer::start(company_identity());
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let start = http
        .get(format!("{}/accounts/sign-in", issuer.server.url))
        .send()
        .unwrap();
    let authorize = http
        .get(start.headers()["location"].to_str().unwrap())
        .send()
        .unwrap();
    let url = authorize.headers()["location"].to_str().unwrap();
    let callback = http.get(url).send().unwrap();
    assert_eq!(callback.status(), 401);
    assert!(callback.headers().get("set-cookie").is_none());
    let replay = http
        .get(url)
        .header(
            "cookie",
            start.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
        )
        .send()
        .unwrap();
    assert_eq!(replay.status(), 400);
}

#[test]
fn dashboard_machine_activity_tracks_only_successful_token_delivery_with_user_isolation() {
    let issuer = EnrollmentServer::start(company_identity());
    let token = issuer.enrolled_token();
    assert_eq!(
        issuer
            .server
            .import(&token, "studio", "studio-login", "studio-seat")
            .status(),
        200
    );
    assert_eq!(
        issuer
            .server
            .import(&token, "build", "build-login", "build-seat")
            .status(),
        200
    );
    issuer.server.import(
        &issuer.server.alex,
        "alex-private",
        "alex-login",
        "alex-seat",
    );
    let cookie = issuer.dashboard_cookie();
    let data = || {
        issuer
            .server
            .http
            .get(format!("{}/accounts/data", issuer.server.url))
            .header("cookie", &cookie)
            .send()
            .unwrap()
            .json::<Value>()
            .unwrap()
    };
    assert!(data()["machines"][0]["last_seen_at"].is_null());
    let first = issuer.server.token(&token, "STUDIO", None);
    assert_eq!(first.status(), 200);
    let credentials: Value = first.json().unwrap();
    let delivered = data();
    assert_eq!(delivered["machines"][0]["last_used_alias"], "studio");
    assert!(
        delivered["machines"][0]["last_seen_at"].as_i64().unwrap()
            >= chrono::Utc::now().timestamp() - 10
    );
    issuer
        .server
        .token(&issuer.server.alex, "alex-private", None);
    assert_eq!(issuer.server.token(&token, "missing", None).status(), 404);
    assert_eq!(data()["machines"][0]["last_used_alias"], "studio");
    assert_eq!(issuer.server.token(&token, "build", None).status(), 200);
    let latest = data();
    assert_eq!(latest["machines"][0]["last_used_alias"], "build");
    assert_eq!(latest["machines"].as_array().unwrap().len(), 1);
    let text = latest.to_string();
    for secret in [
        credentials["accessToken"].as_str().unwrap(),
        &token,
        &issuer.server.alex,
        "alex-private",
        "alex-laptop",
        "studio-seat",
        "synthetic-refresh",
    ] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn dashboard_identity_and_sign_out_are_scoped_to_the_browser_session() {
    let issuer = EnrollmentServer::start(company_identity());
    let cookie = issuer.dashboard_cookie();
    let other_session = issuer.dashboard_cookie();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let snapshot = || {
        http.get(format!("{}/accounts/data", issuer.server.url))
            .header("cookie", &cookie)
            .send()
            .unwrap()
    };
    let data: Value = snapshot().json().unwrap();
    assert_eq!(data["identity"]["email"], "amir@sawmills.ai");
    let public = http
        .get(format!("{}/", issuer.server.url))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(!public.contains("amir@sawmills.ai"));
    assert_eq!(
        http.get(format!("{}/accounts/sign-out", issuer.server.url))
            .header("cookie", &cookie)
            .send()
            .unwrap()
            .status(),
        405
    );
    for origin in [None, Some("https://unrelated.example")] {
        let mut request = http
            .post(format!("{}/accounts/sign-out", issuer.server.url))
            .header("cookie", &cookie);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        assert_eq!(request.send().unwrap().status(), 403);
        assert_eq!(snapshot().status(), 200);
    }
    let signed_out = http
        .post(format!("{}/accounts/sign-out", issuer.server.url))
        .header("cookie", &cookie)
        .header("origin", &issuer.server.url)
        .send()
        .unwrap();
    assert_eq!(signed_out.status(), 303);
    assert_eq!(signed_out.headers()["location"], "/");
    assert_eq!(signed_out.headers()["cache-control"], "no-store");
    assert!(
        signed_out.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(snapshot().status(), 401);
    assert_eq!(
        http.get(format!("{}/accounts/data", issuer.server.url))
            .header("cookie", other_session)
            .send()
            .unwrap()
            .status(),
        200
    );
}
