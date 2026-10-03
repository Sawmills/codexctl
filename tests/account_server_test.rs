#![cfg(feature = "central-prototype")]
use codexctl::{central, store};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

#[test]
fn claude_only_server_starts_without_codex_and_does_not_expand_old_machine_access() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let key = root.path().join("key");
    central::managed::setup(&state, &key).unwrap();
    store::atomic_write(
        &state.join("users.json"),
        &serde_json::to_vec(&json!([
            {"id":"company-person","email":"test@example.com","enabled":true}
        ]))
        .unwrap(),
    )
    .unwrap();
    let token_path = root.path().join("machine.token");
    central::register(
        &state,
        "test-machine",
        "sawmills",
        "company-person",
        &token_path,
    )
    .unwrap();
    let token = std::fs::read_to_string(token_path).unwrap();
    // A disabled provider's damaged inventory must not be read or repaired.
    let broken = state.join("accounts");
    std::fs::remove_dir(&broken).unwrap();
    std::fs::write(&broken, b"synthetic-invalid-vault").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_account-server"))
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .args(["serve", "--state"])
        .arg(&state)
        .arg("--key-file")
        .arg(&key)
        .args([
            "--providers",
            "anthropic",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8787",
            "--codex-bin",
            "/does-not-exist",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address: Value = serde_json::from_str(&line).unwrap();
    let url = format!("http://{}", address["listening"].as_str().unwrap());
    let probe = || {
        Command::new(env!("CARGO_BIN_EXE_account-server"))
            .args([
                "health-check",
                "--address",
                address["listening"].as_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let running_probe = probe();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let ready = http.get(format!("{url}/ready")).send().unwrap().status();
    let old_machine = http
        .get(format!("{url}/v2/anthropic/accounts"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .status();
    let disabled_provider = http
        .get(format!("{url}/v1/accounts"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .status();
    central::providers::grant(
        &state,
        "test-machine",
        vec![central::providers::Provider::Anthropic],
    )
    .unwrap();
    let granted = http
        .get(format!("{url}/v2/anthropic/accounts"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .status();
    central::revoke(&state, "test-machine").unwrap();
    let revoked = http
        .get(format!("{url}/v2/anthropic/accounts"))
        .bearer_auth(&token)
        .send()
        .unwrap()
        .status();
    child.kill().unwrap();
    child.wait().unwrap();
    let stopped_probe = probe();
    assert!(running_probe.status.success());
    assert!(!stopped_probe.status.success());
    assert_eq!(ready, 200);
    assert_eq!(old_machine, 403);
    assert_eq!(granted, 200);
    assert_eq!(revoked, 401);
    assert_eq!(std::fs::read(&broken).unwrap(), b"synthetic-invalid-vault");
    assert_eq!(disabled_provider, 403);
}

#[test]
fn openai_enrollment_retains_the_request_shape_understood_by_old_servers() {
    let request = central::enrollment::Start {
        name: "laptop".into(),
        providers: central::providers::legacy(),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"name":"laptop"})
    );
}
