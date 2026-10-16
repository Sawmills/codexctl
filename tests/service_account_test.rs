#![cfg(all(feature = "central-prototype", target_os = "linux"))]

//! Shared service credentials through the public CLI and machine HTTP routes.
//! Synthetic loopback only; no subscription credential or provider request.
use axum::{Json, Router, http::HeaderMap, routing::get};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_credentials_cannot_redeem_banked_resets() {
    let redemptions = Arc::new(AtomicUsize::new(0));
    let posts = redemptions.clone();
    let app = Router::new()
        .route(
            "/v1/me",
            get(|| async { Json(json!({"id":"company-user"})) }),
        )
        .route(
            "/v1/resets",
            get(|| async {
                Json(json!({"userId":"company-user",
            "accounts":[{"alias":"lane","available":1,"applicable":1,"credits":[]}]}))
            }),
        )
        .route(
            "/v1/resets/redeem",
            axum::routing::post(move || {
                let posts = posts.clone();
                async move {
                    posts.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"code":"reset","windows_reset":1}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .args(["reset", "lane", "--yes"])
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEXCTL_PINNED_CONNECTION")
        .env("CODEXCTL_SERVICE_TOKEN", "synthetic-service")
        .env("CODEXCTL_SERVER_URL", origin)
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .output()
        .unwrap();
    server.abort();
    assert_eq!(
        redemptions.load(Ordering::SeqCst),
        0,
        "service credential spent a reset"
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("service jobs cannot redeem"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_service_exec_never_starts_a_local_child() {
    use std::os::unix::fs::PermissionsExt;
    let app = Router::new().route(
        "/v1/me",
        get(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let binary = home.path().join("codex");
    std::fs::write(
        &binary,
        b"#!/bin/sh\nprintf 'UNAUTHENTICATED-CHILD-STARTED\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .args(["codex", "exec", "synthetic prompt"])
        .env("HOME", home.path())
        .env(
            "PATH",
            std::env::join_paths(
                std::iter::once(home.path().to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        )
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEXCTL_PINNED_CONNECTION")
        .env("CODEXCTL_SERVICE_TOKEN", "synthetic-refused-service")
        .env("CODEXCTL_SERVER_URL", origin)
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .output()
        .unwrap();
    server.abort();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("service credential rejected"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("UNAUTHENTICATED-CHILD-STARTED"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-refused-service"));
}

#[test]
fn service_jobs_cannot_approve_billing_or_banked_resets() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let binary = home.path().join("codex");
    std::fs::write(&binary, b"#!/bin/sh\nprintf 'SPENDING-CHILD-STARTED\\n'\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    for flag in ["--allow-billing", "--allow-resets"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .args(["codex", flag, "exec", "synthetic prompt"])
            .env("HOME", home.path())
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(home.path().to_path_buf())
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env_remove("CODEX_HOME")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .env_remove("CODEXCTL_PINNED_CONNECTION")
            .env("CODEXCTL_SERVICE_TOKEN", "synthetic-service")
            .env("CODEXCTL_SERVER_URL", "https://account-server.invalid")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("service jobs cannot approve billing or banked resets")
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SPENDING-CHILD-STARTED"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_token_connects_without_a_saved_human_login() {
    let reads = Arc::new(AtomicUsize::new(0));
    let identity_reads = reads.clone();
    let machine_reads = reads.clone();
    let app = Router::new()
        .route("/v1/me", get(move |headers: HeaderMap| {
            let reads = identity_reads.clone();
            async move {
                assert!(headers.get("authorization").is_some_and(|h| h == "Bearer synthetic-shared-service"));
                reads.fetch_add(1, Ordering::SeqCst);
                Json(json!({"id":"company-user", "email":"automation@example.test", "enabled":true}))
            }
        }))
        .route("/v1/devices", get(move |headers: HeaderMap| {
            let reads = machine_reads.clone();
            async move {
                assert!(headers.get("authorization").is_some_and(|h| h == "Bearer synthetic-shared-service"));
                reads.fetch_add(1, Ordering::SeqCst);
                Json(json!([{"id":"shared-job-machine", "revoked":false}]))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .arg("devices")
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env("CODEXCTL_SERVICE_TOKEN", "synthetic-shared-service")
        .env("CODEXCTL_SERVER_URL", origin)
        .env("CODEXCTL_ALLOW_INSECURE_LOOPBACK", "1")
        .output()
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "service-only CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("shared-job-machine"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-shared-service"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-shared-service"));
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}
