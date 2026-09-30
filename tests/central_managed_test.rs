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
fn auth(subject: &str, account: &str) -> Value {
    let payload=URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":subject,"iat":2000000000_u64,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}})).unwrap());
    json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"synthetic-refresh","account_id":account}})
}
impl Server {
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
                },
                central::managed::User {
                    id: "alex".into(),
                    email: "alex@sawmills.ai".into(),
                    enabled: true,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
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
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
    let migration = server.cli(home.path(), &["migrate", "--all", "--exclusive-owner"]);
    let usage = server.cli(home.path(), &["use", "personal"]);
    assert!(
        migration.status.success(),
        "{}",
        String::from_utf8_lossy(&migration.stderr)
    );
    assert!(
        usage.status.success(),
        "{}",
        String::from_utf8_lossy(&usage.stderr)
    );
    assert!(api_read_fails(&profile.join("auth.json")));
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
        let mut server = Server::start();
        server.stop();
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
        let configuration = json!({"issuer":ready["issuer"],"client_id":"codexctl-test","client_secret_file":secret,"allowed_domains":["sawmills.ai"]});
        let sso = server.root.path().join("sso.json");
        store::atomic_write(&sso, &serde_json::to_vec(&configuration).unwrap()).unwrap();
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let url = format!("http://{address}");
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .args(["serve", "--state"])
            .arg(server.root.path().join("state"))
            .arg("--key-file")
            .arg(server.root.path().join("key"))
            .arg("--listen")
            .arg(address.to_string())
            .arg("--public-url")
            .arg(&url)
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        line.clear();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let _: Value = serde_json::from_str(&line).expect("SSO server must start");
        server.child = child;
        server.url = url;
        Self { server, issuer }
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
    let output = server
        .http
        .get(format!("{}/metrics", server.url))
        .bearer_auth(metrics)
        .send()
        .unwrap()
        .text()
        .unwrap();
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
