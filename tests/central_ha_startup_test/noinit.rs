use super::*;

async fn canary_fixture() -> LoginFixture {
    login_fixture_in_database(false, false, false, 4102444800, true).await
}

async fn vault_fast_path(mut f: LoginFixture) -> LoginFixture {
    f.first.stop().await;
    store::atomic_write(&f.first.root.path().join("vault-fast-path"), b"1").unwrap();
    f.first = Pod::spawn(&f.database, &f.key, f.first.root, "postgres").await;
    f
}

#[tokio::test]
async fn a_cold_valid_token_request_never_initializes_or_refreshes_native() {
    let f = vault_fast_path(canary_fixture().await).await;
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let token: Value = response.json().await.unwrap();
    let initializes = f.first.launches();
    let refreshes: u32 = std::fs::read_to_string(f.first.root.path().join("count"))
        .unwrap()
        .parse()
        .unwrap();
    let unchanged = token["accessToken"] == f.seed_token["accessToken"];
    stop_fixture(f).await;

    assert_eq!(status, 200);
    assert_eq!((initializes, refreshes), (0, 0));
    assert!(
        unchanged,
        "token bytes must match the original HTTP response"
    );
    assert_eq!(
        generation(&token),
        1,
        "return the saved credential unchanged"
    );
    assert_eq!(token["nativeRoutingSupported"], false);
}

#[tokio::test]
async fn a_token_inside_the_refresh_margin_still_initializes_and_refreshes() {
    let f = vault_fast_path(
        login_fixture_in_database(
            false,
            false,
            false,
            chrono::Utc::now().timestamp() + 1800,
            true,
        )
        .await,
    )
    .await;
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let token: Value = response.json().await.unwrap();
    let initializes = f.first.launches();
    let refreshes: u32 = std::fs::read_to_string(f.first.root.path().join("count"))
        .unwrap()
        .parse()
        .unwrap();
    stop_fixture(f).await;

    assert_eq!(status, 200);
    assert_eq!((initializes, refreshes), (1, 1));
    assert_eq!(generation(&token), 2);
    assert_eq!(token["nativeRoutingSupported"], true);
}

#[tokio::test]
async fn a_valid_token_rerun_does_not_invent_billing_or_routing_evidence() {
    let f = vault_fast_path(canary_fixture().await).await;
    let first: Value = request(&f.http, &f.first, &f.token)
        .await
        .json()
        .await
        .unwrap();
    let response = f
        .http
        .post(format!("{}/v1/token", f.first.url))
        .bearer_auth(&f.token)
        .json(&json!({"alias":"seat","billing":true,"previousRevision":first["revision"]}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let token: Value = response.json().await.unwrap();
    let initializes = f.first.launches();
    let refreshes = std::fs::read_to_string(f.first.root.path().join("count")).unwrap();
    stop_fixture(f).await;

    assert_eq!(status, 200);
    assert_eq!((initializes, refreshes.trim()), (0, "0"));
    assert_eq!(token["revision"], first["revision"]);
    assert_eq!(token["nativeRoutingSupported"], false);
    assert_eq!(token["billingClass"], "unknown");
    assert!(token.get("statuslineUsage").is_none());
}

#[tokio::test]
async fn a_vault_read_failure_refuses_without_initializing_native() {
    let f = vault_fast_path(canary_fixture().await).await;
    f.control
        .batch_execute(&format!(
            "ALTER TABLE {}.central_token_evidence RENAME TO unavailable_evidence",
            f.schema
        ))
        .await
        .unwrap();
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let initializes = f.first.launches();
    stop_fixture(f).await;

    assert_eq!(status, 503);
    assert_eq!(initializes, 0);
}

#[tokio::test]
async fn a_login_fence_refuses_without_initializing_native() {
    let f = vault_fast_path(canary_fixture().await).await;
    let account = account_key("test", "seat");
    // A previous native owner has not settled. Keep the login candidate fenced.
    f.control.execute(&format!("INSERT INTO {}.account_refresh_leases(account_id,holder_id,epoch,expires_at,released) VALUES($1,'foreign-settling',41,clock_timestamp()-interval '1 second',false)", f.schema), &[&account]).await.unwrap();
    let id = "a9".repeat(32);
    login_request(&f, &f.second, "start", &id).await;
    store::atomic_write(
        &f.second.root.path().join("login-release"),
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
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let initializes = f.first.launches();
    login_request(&f, &f.second, "cancel", &id).await;
    stop_fixture(f).await;

    assert_eq!(status, 503);
    assert_eq!(initializes, 0);
}

#[tokio::test]
async fn the_opt_in_refuses_an_ordinary_database_without_initializing_native() {
    let f = vault_fast_path(login_fixture().await).await;
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let initializes = f.first.launches();
    stop_fixture(f).await;

    assert_eq!(status, 503);
    assert_eq!(initializes, 0);
}

#[tokio::test]
async fn default_mode_keeps_native_initialization_on_a_cold_canary_database() {
    let f = canary_fixture().await;
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let token: Value = response.json().await.unwrap();
    let initializes = f.first.launches();
    let refreshes = std::fs::read_to_string(f.first.root.path().join("count")).unwrap();
    stop_fixture(f).await;

    assert_eq!(status, 200);
    assert_eq!((initializes, refreshes.trim()), (1, "1"));
    assert_eq!(generation(&token), 2);
    assert_eq!(token["nativeRoutingSupported"], true);
    assert_ne!(token["billingClass"], "unknown");
}

#[tokio::test]
async fn fresh_unknown_billing_cannot_reach_native_client_approval() {
    let f = vault_fast_path(canary_fixture().await).await;
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    // Supply a fresh observation of unknown billing for the committed token.
    f.control
        .execute(
            &format!(
                "UPDATE {}.central_token_evidence SET billing_class='unknown',usage_present=false",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let token: Value = response.json().await.unwrap();
    let initializes = f.first.launches();
    stop_fixture(f).await;

    assert_eq!(status, 200);
    assert_eq!(initializes, 0);
    assert_eq!(token["billingClass"], "unknown");
    assert_eq!(token["nativeRoutingSupported"], false);
}

#[tokio::test]
async fn fresh_high_usage_is_a_refusal_instead_of_unknown_billing() {
    let f = vault_fast_path(canary_fixture().await).await;
    store::atomic_write(&f.second.root.path().join("mode"), b"used-95").unwrap();
    assert_eq!(request(&f.http, &f.second, &f.token).await.status(), 200);
    let response = request(&f.http, &f.first, &f.token).await;
    let status = response.status();
    let initializes = f.first.launches();
    stop_fixture(f).await;

    assert_eq!(status, 503);
    assert_eq!(initializes, 0);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn queued_requests_preserve_a_new_local_routing_fence_after_rotation() {
    let f = vault_fast_path(
        login_fixture_in_database(
            false,
            false,
            false,
            chrono::Utc::now().timestamp() + 1800,
            true,
        )
        .await,
    )
    .await;
    store::atomic_write(&f.first.root.path().join("mode"), b"canary-routing-fence").unwrap();
    // Hold import admission while both token requests inspect the old expiring
    // token. The fake only gates this unrelated account's initialization.
    let importer = tokio::spawn({
        let http = f.http.clone();
        let url = f.first.url.clone();
        let bearer = f.token.clone();
        async move {
            http.post(format!("{url}/v1/accounts"))
                .bearer_auth(bearer)
                .json(&json!({"alias":"control","auth":add_grant("control-seat",Some("control-login"),Some("control-uid"))}))
                .send().await.unwrap()
        }
    });
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("initialize-started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let requests: Vec<_> = (0..2)
        .map(|_| {
            let http = f.http.clone();
            let url = f.first.url.clone();
            let bearer = f.token.clone();
            tokio::spawn(async move {
                http.post(format!("{url}/v1/token"))
                    .bearer_auth(bearer)
                    .json(&json!({"alias":"seat","billing":true}))
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();
    timeout(Duration::from_secs(8), async {
        while metric(
            &f,
            &f.first,
            "codexctl_central_token_fast_path_total{result=\"expiring\"}",
        )
        .await
            < 2
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    store::atomic_write(&f.first.root.path().join("release-initialize"), b"go").unwrap();
    let imported = importer.await.unwrap().status();
    let mut statuses = Vec::new();
    for request in requests {
        statuses.push(request.await.unwrap());
    }
    stop_fixture(f).await;

    assert_eq!(imported, 200);
    assert!(
        statuses.iter().all(|status| !status.is_success()),
        "a waiting request bypassed the new routing fence: {statuses:?}"
    );
}
