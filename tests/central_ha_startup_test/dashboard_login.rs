//! Browser Add through the public HTTP routes and controlled native child.
use super::*;

#[tokio::test]
async fn postgres_dashboard_sign_out_without_a_cookie_is_still_origin_guarded() {
    let f = BrowserFixture::start().await;
    for (origin, expected) in [
        (Some("http://127.0.0.1:8787"), 303),
        (Some("https://other.invalid"), 403),
        (None, 403),
    ] {
        let request = f.http.post(format!("{}/accounts/sign-out", f.first.url));
        let request = if let Some(origin) = origin {
            request.header("origin", origin)
        } else {
            request
        };
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            expected,
            "sign-out without a cookie is idempotent only at the configured Origin"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        if expected == 303 {
            assert!(
                response.headers()["set-cookie"]
                    .to_str()
                    .unwrap()
                    .contains("Max-Age=0")
            );
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_expiry_before_login_create_is_a_refusal_without_an_alert() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    f.control
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {}.central_login_operations IN ACCESS EXCLUSIVE MODE",
            f.schema
        ))
        .await
        .unwrap();
    let http = f.http.clone();
    let url = f.first.url.clone();
    let pending = tokio::spawn(async move {
        http.post(format!("{url}/accounts/add"))
            .header("cookie", session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[
                ("alias", "expired-before-create"),
                (
                    "id",
                    "dededededededededededededededededededededededededededededededede",
                ),
                ("form_token", &token),
            ])
            .send()
            .await
            .unwrap()
    });
    timeout(Duration::from_secs(8),async {
        loop {
            // This observer holds a transaction; refresh its activity snapshot.
            f.control.batch_execute("SELECT pg_stat_clear_snapshot()").await.unwrap();
            let waiting:bool=f.control.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock' AND query LIKE '%central_login_operations%')",&[&f.schema]).await.unwrap().get(0);
            if waiting {break;}
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("start passes browser guards and reaches held journal lookup");
    f.control.batch_execute(&format!("UPDATE {}.browser_sessions SET expires_at=clock_timestamp()-interval '1 second'; COMMIT",f.schema)).await.unwrap();
    let response = timeout(Duration::from_secs(8), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "authority ending before create is a refusal, not an operational outage"
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "login_authority_ended"
    );
    let metrics = f
        .http
        .get(format!("{}/metrics", f.first.url))
        .bearer_auth("synthetic-dashboard-metrics-token-32")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !metrics.contains("reason=\"relogin_failed\""),
        "benign expiry cannot trigger the credential-operation alert"
    );
    let result: String = f
        .control
        .query_one(
            &format!("SELECT result FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(result, "refused:login_authority_ended");
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_add_login_is_owned_by_its_browser_session() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("browser-test-machine", "synthetic-browser-test-token")
        .await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let response = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "browser-added"),
            (
                "id",
                "b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5",
            ),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        303,
        "enabled browser Add must start a login"
    );
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let view = f
        .http
        .get(format!("{}{location}", f.second.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(view.status(), 200, "receipt is shared across replicas");
    let html = view.text().await.unwrap();
    assert!(
        html.contains("TEST-LOGIN"),
        "owning browser sees the device code"
    );
    for operation in ["status", "cancel"] {
        let response = f.http.post(format!("{}/v1/accounts/login/{operation}", f.second.url))
            .bearer_auth("synthetic-browser-test-token")
            .header("x-codexctl-device", "browser-test-machine")
            .json(&json!({"alias":"browser-added", "id":"b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5b16b00b5"}))
            .send().await.unwrap();
        assert_eq!(
            response.status(),
            404,
            "machine cannot access a browser login"
        );
        assert!(!response.text().await.unwrap().contains("TEST-LOGIN"));
    }
    let collision=f.http.post(format!("{}/v1/accounts/login/start",f.second.url))
        .bearer_auth("synthetic-browser-test-token").header("x-codexctl-device","browser-test-machine")
        .json(&json!({"alias":"browser-added","id":"4545454545454545454545454545454545454545454545454545454545454545"}))
        .send().await.unwrap();
    assert_eq!(
        collision.status(),
        409,
        "an alias in use reports a generic conflict without receipt access"
    );
    assert!(!collision.text().await.unwrap().contains("TEST-LOGIN"));
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_browser_login_actions_are_audited_once() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("audit-test-machine", "synthetic-audit-test-token")
        .await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    for _ in 0..2 {
        let started = f
            .http
            .post(format!("{}/accounts/add", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[("alias", "audit-added"), ("id", id), ("form_token", &token)])
            .send()
            .await
            .unwrap();
        assert_eq!(started.status(), 303);
    }
    let canceled = f
        .http
        .post(format!("{}/accounts/login/cancel", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("id", id), ("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(canceled.status(), 303);
    timeout(Duration::from_secs(10), async {
        loop {
            let view = f
                .http
                .get(format!("{}/accounts/login?id={id}", f.second.url))
                .header("cookie", &session)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            if view.contains("role=\"status\">Login canceled</p>") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("browser cancellation settles");
    let rows = f
        .control
        .query(
            &format!(
                "SELECT actor,operation,target,result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    let outcomes: Vec<String> = rows.iter().map(|r| r.get(3)).collect();
    assert_eq!(
        outcomes,
        ["ok:started", "ok:canceled"],
        "one start and one terminal audit despite replay and polls"
    );
    for row in rows {
        assert_eq!(
            row.get::<_, String>(0),
            "dashboard:dashboard@example.invalid"
        );
        assert_eq!(row.get::<_, String>(1), "account_add");
        assert_eq!(row.get::<_, String>(2), "audit-added");
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_browser_login_starts_are_disabled_by_default() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("disabled-test-machine", "synthetic-disabled-token")
        .await;
    let token = f.form_token(&session).await;
    let response = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "disabled"),
            (
                "id",
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            ),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "disabled responses must not be cached"
    );
    assert!(
        !f.first.root.path().join("login-pid").exists(),
        "disabled starts launch no native login"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_login_errors_offer_html_navigation_without_secrets() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let response = f
        .http
        .get(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.headers().contains_key("content-security-policy"));
    let html = response.text().await.unwrap();
    assert!(html.contains("Back to accounts"));
    assert!(!html.contains(&session));
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_browser_add_completion_has_a_terminal_audit() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("complete-test-machine", "synthetic-complete-token")
        .await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "completed-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "dashboard-workspace",
            Some("dashboard-login"),
            Some("dashboard-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let view = f
                .http
                .get(format!("{}/accounts/login?id={id}", f.second.url))
                .header("cookie", &session)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            if view.contains("role=\"status\">Login completed</p>") {
                break;
            }
            assert!(
                !view.contains("role=\"status\">Login failed</p>")
                    && !view.contains("role=\"status\">Login failed</p>"),
                "browser Add failed: {view}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("browser Add completes with controlled credentials");
    let outcomes: Vec<String> = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        outcomes,
        ["ok:started", "ok:completed"],
        "successful publication and terminal audit commit together"
    );
    for _ in 0..2 {
        let status = f
            .http
            .post(format!("{}/accounts/login/status", f.second.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[("id", id), ("form_token", &token)])
            .send()
            .await
            .unwrap();
        assert_eq!(
            status.status(),
            200,
            "completed browser receipt remains replayable"
        );
        let view: Value = status.json().await.unwrap();
        assert_eq!(view["status"], "completed");
        assert!(view["userCode"].is_null());
    }
    let catalog = f
        .http
        .get(format!("{}/accounts/data", f.second.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(catalog.contains("completed-browser"));
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_out_requires_the_session_form_token() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let refused = f
        .http
        .post(format!("{}/accounts/sign-out", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("form_token", "wrong-session-form-token")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        403,
        "sign-out cannot cancel a browser login without the session token"
    );
    let still_signed_in = f
        .http
        .get(format!("{}/accounts/data", f.second.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(still_signed_in.status(), 200);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_out_clears_ended_or_disabled_sessions() {
    for ended in ["expired", "disabled", "missing"] {
        let f = BrowserFixture::start().await;
        let (session, _) = f.sign_in().await;
        let token = f.form_token(&session).await;
        let mutation = match ended {
            "expired" => format!(
                "UPDATE {}.browser_sessions SET expires_at=clock_timestamp()-interval '1 second'",
                f.schema
            ),
            "disabled" => format!("UPDATE {}.central_users SET enabled=false", f.schema),
            _ => format!("DELETE FROM {}.browser_sessions", f.schema),
        };
        f.control.batch_execute(&mutation).await.unwrap();
        let response = f
            .http
            .post(format!("{}/accounts/sign-out", f.second.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[("form_token", &token)])
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            303,
            "{ended} sessions can sign out with their original form token"
        );
        assert!(
            response.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        let sessions: i64 = f
            .control
            .query_one(
                &format!("SELECT count(*) FROM {}.browser_sessions", f.schema),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(sessions, 0);
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_dashboard_session_end_stops_its_native_login_without_browser_polls() {
    for end in ["sign-out", "expiry", "disabled-user"] {
        let mut f = BrowserFixture::start().await;
        let (session, _) = f.sign_in().await;
        let token = f.form_token(&session).await;
        f.first.stop().await;
        let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
        store::atomic_write(&root.path().join("browser-logins"), b"1").unwrap();
        f.first = Pod::spawn(&f.database(), &f._root.path().join("key"), root, "postgres").await;
        let id = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let started = f
            .http
            .post(format!("{}/accounts/add", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[
                ("alias", "ended-browser"),
                ("id", id),
                ("form_token", &token),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(started.status(), 303);
        let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            !central_child_dead(pid),
            "{end}: actual native polling child is live"
        );
        match end {
            "sign-out" => {
                let ended = f
                    .http
                    .post(format!("{}/accounts/sign-out", f.second.url))
                    .header("cookie", &session)
                    .header("origin", "http://127.0.0.1:8787")
                    .form(&[("form_token", &token)])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(ended.status(), 303);
            }
            "expiry" => {
                f.control.batch_execute(&format!("UPDATE {}.browser_sessions SET expires_at=clock_timestamp()-interval '1 second'", f.schema)).await.unwrap();
            }
            _ => {
                f.control
                    .batch_execute(&format!(
                        "UPDATE {}.central_users SET enabled=false",
                        f.schema
                    ))
                    .await
                    .unwrap();
            }
        }
        timeout(Duration::from_secs(12), async {
            loop {
                let row = f.control.query_one(&format!("SELECT phase,polling_clear FROM {}.central_login_operations WHERE id=$1", f.schema), &[&id]).await.unwrap();
                if row.get::<_, String>(0) == "canceled" && row.get::<_, bool>(1) && central_child_dead(pid) { break; }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap_or_else(|_| panic!("{end}: session end must durably cancel and stop polling without a browser request"));
        assert_eq!(f.first.launches(), 0, "{end}: no refresh owner was started");
        let row = f
            .control
            .query_one(
                &format!(
                    "SELECT count(*) FROM {}.dashboard_audit WHERE result='ok:canceled'",
                    f.schema
                ),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), 1);
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_dashboard_browser_cannot_read_machine_or_other_session_logins() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let (other_session, _) = f.sign_in().await;
    f.machine("isolation-machine", "synthetic-isolation-token")
        .await;
    let token = f.form_token(&session).await;
    let other_token = f.form_token(&other_session).await;
    f.enable_browser_logins().await;
    let machine_id = "1212121212121212121212121212121212121212121212121212121212121212";
    let machine_started = f
        .http
        .post(format!("{}/v1/accounts/login/start", f.first.url))
        .bearer_auth("synthetic-isolation-token")
        .header("x-codexctl-device", "isolation-machine")
        .json(&json!({"alias":"machine-only", "id":machine_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(machine_started.status(), 200);
    assert_eq!(
        machine_started.json::<Value>().await.unwrap()["userCode"],
        "TEST-LOGIN"
    );
    let browser_id = "3434343434343434343434343434343434343434343434343434343434343434";
    let browser_started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "browser-only"),
            ("id", browser_id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(browser_started.status(), 303);
    for (cookie, form_token, id) in [
        (&session, &token, machine_id),
        (&other_session, &other_token, browser_id),
    ] {
        let view = f
            .http
            .get(format!("{}/accounts/login?id={id}", f.second.url))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(view.status(), 404, "receipt belongs to its exact authority");
        assert!(!view.text().await.unwrap().contains("TEST-LOGIN"));
        let cancel = f
            .http
            .post(format!("{}/accounts/login/cancel", f.second.url))
            .header("cookie", cookie)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[("id", id), ("form_token", form_token.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), 404);
    }
    let machine_status = f
        .http
        .post(format!("{}/v1/accounts/login/status", f.second.url))
        .bearer_auth("synthetic-isolation-token")
        .header("x-codexctl-device", "isolation-machine")
        .json(&json!({"alias":"machine-only", "id":machine_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        machine_status.json::<Value>().await.unwrap()["userCode"],
        "TEST-LOGIN",
        "refused browser cancel leaves machine login intact"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_disabling_starts_settles_login_and_keeps_its_receipt() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "5656565656565656565656565656565656565656565656565656565656565656";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "rollback-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    let pid: u32 = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.first.stop().await;
    assert!(
        central_child_dead(pid),
        "disable/restart settles the actual polling child"
    );
    let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
    std::fs::remove_file(root.path().join("browser-logins")).unwrap();
    f.first = Pod::spawn(&f.database(), &f._root.path().join("key"), root, "postgres").await;
    let view = f
        .http
        .get(format!("{}/accounts/login?id={id}", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(
        view.status(),
        200,
        "old receipt stays accessible while starts are disabled"
    );
    let html = view.text().await.unwrap();
    assert!(html.contains("role=\"status\">Login canceled</p>"));
    assert!(
        !html.contains("TEST-LOGIN"),
        "terminal views drop device codes"
    );
    let refused = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "rollback-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 503);
    let count = f
        .control
        .query_one(
            &format!(
                "SELECT count(*) FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(
        count.get::<_, i64>(0),
        1,
        "rollback retains durable evidence"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_login_status_is_guarded_and_returns_a_safe_view() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "7878787878787878787878787878787878787878787878787878787878787878";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "status-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    for (origin, form_token, expected) in [
        ("http://127.0.0.1:8787", "wrong-form-token", 403),
        ("https://other.invalid", token.as_str(), 403),
        ("http://127.0.0.1:8787", token.as_str(), 200),
    ] {
        let status = f
            .http
            .post(format!("{}/accounts/login/status", f.second.url))
            .header("cookie", &session)
            .header("origin", origin)
            .form(&[("id", id), ("form_token", form_token)])
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), expected);
        assert_eq!(status.headers()["cache-control"], "no-store");
        if expected == 200 {
            let view: Value = status.json().await.unwrap();
            assert_eq!(view["status"], "pending");
            assert_eq!(view["userCode"], "TEST-LOGIN");
            let keys: Vec<_> = view
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "alias",
                    "error",
                    "id",
                    "landedAlias",
                    "status",
                    "userCode",
                    "verificationUrl"
                ]
            );
        }
    }
    let html = f
        .http
        .post(format!("{}/accounts/login/status", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .header("accept", "text/html")
        .form(&[("id", id), ("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(
        html.status(),
        303,
        "HTML status uses POST/redirect/GET so noscript refresh never GETs a POST-only route"
    );
    assert_eq!(
        html.headers()["location"],
        format!("/accounts/login?id={id}")
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_shows_add_only_when_browser_starts_are_enabled() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let page = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("href=\"/accounts/add\""));
    f.enable_browser_logins().await;
    let page = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("href=\"/accounts/add\""),
        "enabled dashboard offers Add"
    );
    let form = f
        .http
        .get(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 200);
    assert_eq!(form.headers()["cache-control"], "no-store");
    assert!(
        form.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self'")
    );
    let html = form.text().await.unwrap();
    for field in ["alias", "id", "form_token"] {
        assert!(html.contains(&format!("name=\"{field}\"")));
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_machine_add_does_not_depend_on_browser_session_storage() {
    let f = BrowserFixture::start().await;
    f.sign_in().await;
    f.machine("no-browser-table", "synthetic-no-browser-token")
        .await;
    f.control
        .batch_execute(&format!("DROP TABLE {}.browser_sessions", f.schema))
        .await
        .unwrap();
    let response = f.http.post(format!("{}/v1/accounts/login/start", f.first.url))
        .bearer_auth("synthetic-no-browser-token").header("x-codexctl-device", "no-browser-table")
        .json(&json!({"alias":"machine-independent", "id":"9090909090909090909090909090909090909090909090909090909090909090"}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["userCode"],
        "TEST-LOGIN",
        "machine login must work while browser session storage is unavailable"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_get_only_reads_an_expired_retained_login() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "abababababababababababababababababababababababababababababababab";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "retained-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    let pending: Vec<u8> = f
        .control
        .query_one(
            &format!(
                "SELECT encrypted_payload FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    f.first.stop().await;
    // A retained pending receipt whose native child is proven stopped. Reads
    // must not turn this evidence into a recovered operation without a POST.
    f.control.execute(&format!("UPDATE {}.central_login_operations SET phase='pending',expires_at=clock_timestamp()-interval '1 second',polling_clear=true,selected_reserved=true,encrypted_payload=$2 WHERE id=$1", f.schema), &[&id,&pending]).await.unwrap();
    // Restore the pre-exit audit snapshot as well: start is durable, the
    // terminal transition has not been committed by the lost parent.
    f.control
        .batch_execute(&format!(
            "DELETE FROM {}.dashboard_audit WHERE result<>'ok:started'",
            f.schema
        ))
        .await
        .unwrap();
    let read = f
        .http
        .get(format!("{}/accounts/login?id={id}", f.second.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 200);
    assert!(
        !read.text().await.unwrap().contains("TEST-LOGIN"),
        "expired receipts never redisplay a code"
    );
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT phase,selected_reserved FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, String>(0),
        "pending",
        "GET does not mutate the retained journal"
    );
    assert!(row.get::<_, bool>(1), "GET does not release a reservation");
    let polled = f
        .http
        .post(format!("{}/accounts/login/status", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("id", id), ("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(polled.status(), 200);
    assert_eq!(polled.json::<Value>().await.unwrap()["status"], "expired");
    let again = f
        .http
        .post(format!("{}/accounts/login/status", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("id", id), ("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200);
    let outcomes: Vec<String> = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        outcomes,
        ["ok:started", "failed:expired"],
        "recovery retains one terminal audit despite replay"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_refused_add_has_one_safe_audit_attempt() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let response = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "synthetic-private-input/invalid"),
            (
                "id",
                "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            ),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let rows = f
        .control
        .query(
            &format!(
                "SELECT actor,operation,target,result FROM {}.dashboard_audit",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "an action that passed its request guard retains its refusal"
    );
    assert_eq!(
        rows[0].get::<_, String>(0),
        "dashboard:dashboard@example.invalid"
    );
    assert_eq!(rows[0].get::<_, String>(1), "account_add");
    assert_eq!(rows[0].get::<_, String>(3), "refused:invalid_alias");
    let target: String = rows[0].get(2);
    assert!(target.starts_with("unresolved:"));
    assert!(!target.contains("synthetic-private-input"));
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_authority_migration_preserves_legacy_machine_receipts() {
    let mut f = BrowserFixture::start().await;
    f.sign_in().await;
    f.machine(
        "legacy-authority-machine",
        "synthetic-legacy-authority-token",
    )
    .await;
    let operation = json!({"alias":"legacy-authority", "id":"efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef"});
    let started = f
        .http
        .post(format!("{}/v1/accounts/login/start", f.first.url))
        .bearer_auth("synthetic-legacy-authority-token")
        .header("x-codexctl-device", "legacy-authority-machine")
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(
        started.json::<Value>().await.unwrap()["userCode"],
        "TEST-LOGIN"
    );
    f.first.stop().await;
    f.control
        .batch_execute(&format!(
            "ALTER TABLE {}.central_login_operations DROP COLUMN browser_session_hash",
            f.schema
        ))
        .await
        .unwrap();
    for _ in 0..2 {
        storage_fixture::migrate(
            &f.database(),
            &f._root.path().join("state"),
            &f._root.path().join("key"),
        )
        .await;
    }
    let status = f
        .http
        .post(format!("{}/v1/accounts/login/status", f.second.url))
        .bearer_auth("synthetic-legacy-authority-token")
        .header("x-codexctl-device", "legacy-authority-machine")
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(
        status.status(),
        200,
        "new decoder reads pre-authority machine receipts after repeat migration"
    );
    assert_eq!(status.json::<Value>().await.unwrap()["status"], "canceled");
    let row = f.control.query_one(&format!("SELECT browser_session_hash IS NULL, device_id FROM {}.central_login_operations WHERE id=$1", f.schema), &[&operation["id"].as_str().unwrap()]).await.unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(row.get::<_, String>(1), "legacy-authority-machine");
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_sign_out_during_verification_prevents_publication() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "2323232323232323232323232323232323232323232323232323232323232323";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "verification-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    store::atomic_write(
        &f.first.root.path().join("mode"),
        b"browser-verification-hold",
    )
    .unwrap();
    store::atomic_write(&f.first.root.path().join("forced-refresh-count"), b"0").unwrap();
    store::atomic_write(&f.first.root.path().join("limits-count"), b"0").unwrap();
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "verification-workspace",
            Some("verification-login"),
            Some("verification-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("native-started").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native credential verification reaches controlled hold");
    let phase: String = f
        .control
        .query_one(
            &format!(
                "SELECT phase FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        phase, "verifying",
        "this tests real verification, not only device polling"
    );
    let ended = f
        .http
        .post(format!("{}/accounts/sign-out", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(ended.status(), 303);
    store::atomic_write(&f.first.root.path().join("native-release"), b"1").unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let row = f.control.query_one(&format!("SELECT phase,completed_revision FROM {}.central_login_operations WHERE id=$1", f.schema), &[&id]).await.unwrap();
            let phase: String = row.get(0);
            assert_ne!(phase, "completed", "ended authority must not publish credentials");
            assert!(row.get::<_, Option<i64>>(1).is_none());
            if ["unresolved", "rejected", "failed", "canceled"].contains(&phase.as_str()) { break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("ended verification settles with retained evidence");
    assert_eq!(
        std::fs::read_to_string(f.first.root.path().join("forced-refresh-count")).unwrap(),
        "0",
        "browser Add never requests forced refresh"
    );
    assert_eq!(
        std::fs::read_to_string(f.first.root.path().join("limits-count")).unwrap(),
        "0",
        "browser verification omits billing reads"
    );
    let count: i64 = f
        .control
        .query_one(
            &format!(
                "SELECT count(*) FROM {}.dashboard_audit WHERE result='ok:completed'",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_add_validates_labels_before_starting_a_native_login() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let response = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "valid-alias"),
            ("label", "synthetic-private-label\ncontrol"),
            (
                "id",
                "4545454545454545454545454545454545454545454545454545454545454545",
            ),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        400,
        "browser labels follow the same rules as CLI labels"
    );
    assert!(!f.first.root.path().join("login-pid").exists());
    let row = f
        .control
        .query_one(
            &format!("SELECT result FROM {}.dashboard_audit", f.schema),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "refused:invalid_label");
    f.finish().await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_dashboard_parent_loss_without_a_grant_settles_after_lease_expiry() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "abababababababababababababababababababababababababababababababab";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "grantless-parent-loss"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    let pid = std::fs::read_to_string(f.first.root.path().join("login-pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.control.execute(&format!("UPDATE {}.central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",f.schema),&[&id]).await.unwrap();
    f.first.child.start_kill().unwrap();
    f.first.child.wait().await.unwrap();
    let ended = f
        .http
        .post(format!("{}/accounts/sign-out", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(ended.status(), 303);
    wait_dead(pid).await;
    timeout(Duration::from_secs(8), async {
        loop {
            let clear: bool = f
                .control
                .query_one(
                    &format!(
                        "SELECT polling_clear FROM {}.central_login_operations WHERE id=$1",
                        f.schema
                    ),
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            if clear {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("independent supervisor records awaited grantless native exit");
    let row = f
        .control
        .query_one(
            &format!(
                "SELECT phase,selected_reserved FROM {}.central_login_operations WHERE id=$1",
                f.schema
            ),
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, String>(0),
        "failed",
        "grantless parent loss reaches a terminal receipt without any browser poll"
    );
    assert!(
        !row.get::<_, bool>(1),
        "proven grant absence releases the Add alias reservation"
    );
    let results: Vec<String> = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(results, ["ok:started", "failed:login_failed"]);
    f.finish().await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn postgres_dashboard_parent_loss_keeps_the_late_grant_terminal_audit() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    store::atomic_write(&f.first.root.path().join("mode"), b"login-hold-after-save").unwrap();
    f.control.batch_execute(&format!(
        "CREATE FUNCTION {0}.reject_capture() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase IN ('candidate','rejected') THEN RAISE EXCEPTION 'synthetic candidate publication failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_capture BEFORE UPDATE ON {0}.central_login_operations FOR EACH ROW EXECUTE FUNCTION {0}.reject_capture()",f.schema
    )).await.unwrap();
    let id = "8989898989898989898989898989898989898989898989898989898989898989";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "late-browser"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "late-browser-workspace",
            Some("late-login"),
            Some("late-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        while !f.first.root.path().join("login-saved").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native login retains a grant before its controlled exit");
    // Kill only the serving parent. Its independent polling supervisor must
    // stop the native child and commit its own terminal audit without a poll.
    f.first.child.start_kill().unwrap();
    f.first.child.wait().await.unwrap();
    let signed_out = f
        .http
        .post(format!("{}/accounts/sign-out", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("form_token", &token)])
        .send()
        .await
        .unwrap();
    assert_eq!(signed_out.status(), 303);
    timeout(Duration::from_secs(8), async {
        loop {
            let phase: String = f
                .control
                .query_one(
                    &format!(
                        "SELECT phase FROM {}.central_login_operations WHERE id=$1",
                        f.schema
                    ),
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            if phase == "unresolved" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("independent supervisor retains late grant as unresolved");
    let results: Vec<String> = f
        .control
        .query(
            &format!(
                "SELECT result FROM {}.dashboard_audit ORDER BY id",
                f.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        results,
        ["ok:started", "failed:unresolved"],
        "parent loss and ended session cannot lose the terminal audit"
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_session_expiry_during_publication_rolls_back_credentials() {
    browser_expiry_during_credential_write(false).await;
}

#[tokio::test]
async fn postgres_dashboard_session_expiry_during_admission_rolls_back_credentials() {
    browser_expiry_during_credential_write(true).await;
}

async fn browser_expiry_during_credential_write(admission: bool) {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    let token = f.form_token(&session).await;
    f.first.stop().await;
    // Advance only this disposable schema's SQL clock, after observing the real
    // credential UPDATE blocked in its trigger. No production clock override.
    let write = if admission { "INSERT" } else { "UPDATE" };
    f.control.batch_execute(&format!(r#"
        CREATE TABLE {0}.test_clock(seconds integer NOT NULL);
        INSERT INTO {0}.test_clock VALUES(0);
        CREATE FUNCTION {0}.clock_timestamp() RETURNS timestamptz LANGUAGE sql VOLATILE AS
        $$ SELECT pg_catalog.clock_timestamp()+seconds*interval '1 second' FROM {0}.test_clock $$;
        CREATE FUNCTION {0}.hold_publication() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
            IF TG_OP='INSERT' OR NEW.revision>OLD.revision THEN PERFORM pg_advisory_xact_lock(hashtextextended('{0}:publish',0)); END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER hold_publication BEFORE {write} ON {0}.central_accounts
            FOR EACH ROW EXECUTE FUNCTION {0}.hold_publication();
        SELECT pg_advisory_lock(hashtextextended('{0}:publish',0));
        UPDATE {0}.browser_sessions SET expires_at=pg_catalog.clock_timestamp()+interval '10 seconds';
    "#,f.schema)).await.unwrap();
    let root = std::mem::replace(&mut f.first.root, tempfile::tempdir().unwrap());
    store::atomic_write(&root.path().join("browser-logins"), b"1").unwrap();
    let mut database = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    database
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={},pg_catalog", f.schema))
        .append_pair("application_name", &f.schema);
    f.first = Pod::spawn(
        database.as_str(),
        &f._root.path().join("key"),
        root,
        "postgres",
    )
    .await;
    let id = "6767676767676767676767676767676767676767676767676767676767676767";
    let started = f
        .http
        .post(format!("{}/accounts/add", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "expiry-at-publication"),
            ("id", id),
            ("form_token", &token),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&add_grant(
            "expiry-at-publication-workspace",
            Some("expiry-login"),
            Some("expiry-uid"),
        ))
        .unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let waiting: bool = f.control.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event='advisory' AND pg_backend_pid()=ANY(pg_blocking_pids(pid)))", &[&f.schema]
            ).await.unwrap().get(0);
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("credential publication reaches controlled advisory hold");
    f.control.batch_execute(&format!(
        "UPDATE {0}.test_clock SET seconds=15; SELECT pg_advisory_unlock(hashtextextended('{0}:publish',0));",f.schema
    )).await.unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            let phase: String = f
                .control
                .query_one(
                    &format!(
                        "SELECT phase FROM {}.central_login_operations WHERE id=$1",
                        f.schema
                    ),
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            assert_ne!(
                phase, "completed",
                "a session that expires during publication cannot complete Add"
            );
            if ["unresolved", "rejected", "failed", "canceled"].contains(&phase.as_str()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expired publication settles with retained evidence");
    let row=f.control.query_one(&format!(
        "SELECT completed_revision,EXISTS(SELECT 1 FROM {0}.dashboard_audit WHERE result='ok:completed') FROM {0}.central_login_operations WHERE id=$1",f.schema),&[&id]).await.unwrap();
    assert!(row.get::<_, Option<i64>>(0).is_none());
    assert!(
        !row.get::<_, bool>(1),
        "rolled-back completion has no success audit"
    );
    if admission {
        let count: i64 = f
            .control
            .query_one(
                &format!(
                    "SELECT count(*) FROM {}.central_accounts WHERE alias='expiry-at-publication'",
                    f.schema
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            count, 0,
            "expired admission never persists even preliminary credentials"
        );
    }
    f.finish().await;
}
