//! Browser Renew through the same public routes and native lifecycle as Add.
use super::*;

#[tokio::test]
async fn postgres_dashboard_renew_confirmation_does_not_decrypt_credentials() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("confirmation-renew", "synthetic-confirmation-renew-token")
        .await;
    let alias = "Renew & Metadata";
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let seeded = f.http.post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-confirmation-renew-token")
        .json(&json!({"alias":alias,"auth":add_grant("confirmation-workspace",Some("confirmation-login"),None)}))
        .send().await.unwrap();
    assert_eq!(seeded.status(), 200);
    f.enable_browser_logins().await;
    // Keep the server key valid for session/form guards, but make this account's
    // synthetic vault impossible to decrypt. A confirmation needs metadata only.
    assert_eq!(
        f.control
            .execute(
                &format!(
                    "UPDATE {}.central_accounts SET encrypted_vault=$2 WHERE alias=$1",
                    f.schema
                ),
                &[&alias, &vec![0u8]],
            )
            .await
            .unwrap(),
        1
    );
    let page = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", "renew & metadata")])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        200,
        "confirmation must not decrypt vault bytes"
    );
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = page.text().await.unwrap();
    assert!(
        html.contains("Renew &amp; Metadata"),
        "use the stored alias"
    );
    assert!(html.contains("name=\"form_token\""));
    f.control.batch_execute(&format!(
        "INSERT INTO {s}.central_users(id,email,enabled) VALUES('foreign-confirmation','foreign@example.invalid',true); \
         UPDATE {s}.central_accounts SET user_id='foreign-confirmation' WHERE alias='Renew & Metadata'", s=f.schema
    )).await.unwrap();
    let foreign = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(
        foreign.status(),
        404,
        "another company user's account stays hidden"
    );
    f.control.batch_execute(&format!(
        "UPDATE {s}.central_accounts SET user_id=(SELECT id FROM {s}.central_users WHERE email='dashboard@example.invalid'), \
         deleted_at=clock_timestamp() WHERE alias='Renew & Metadata'", s=f.schema
    )).await.unwrap();
    let deleted = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 404, "a deleted account stays hidden");
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}

#[tokio::test]
async fn postgres_dashboard_renew_is_owned_by_its_browser_session() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("renew-machine", "synthetic-renew-machine-token")
        .await;
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let seeded = f.http.post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-renew-machine-token")
        .json(&json!({"alias":"browser-existing", "auth":add_grant("renew-workspace",Some("renew-login"),Some("renew-uid"))}))
        .send().await.unwrap();
    assert_eq!(
        seeded.status(),
        200,
        "seed an existing owned server account"
    );
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let id = "7171717171717171717171717171717171717171717171717171717171717171";
    let started = f
        .http
        .post(format!("{}/accounts/renew", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "browser-existing"),
            ("id", id),
            ("form_token", token.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        started.status(),
        303,
        "a browser can start Renew for its existing server account"
    );
    let location = started.headers()["location"].to_str().unwrap();
    let page = f
        .http
        .get(format!("{}{location}", f.second.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("TEST-LOGIN"));
    for action in ["status", "cancel"] {
        let response = f
            .http
            .post(format!("{}/v1/relogin/{action}", f.second.url))
            .bearer_auth("synthetic-renew-machine-token")
            .header("x-codexctl-device", "renew-machine")
            .json(&json!({"alias":"browser-existing","id":id}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "machine must not access browser Renew"
        );
        assert!(!response.text().await.unwrap().contains("TEST-LOGIN"));
    }
    let canceled = f
        .http
        .post(format!("{}/accounts/login/cancel", f.second.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[("id", id), ("form_token", token.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(canceled.status(), 303);
    f.finish().await;
}

#[tokio::test]
async fn postgres_browser_start_leaves_a_lost_machine_receipt_to_its_machine() {
    for (phase, same_id) in [
        ("replica_lost", false),
        ("pending", false),
        ("pending", true),
    ] {
        let mut f = BrowserFixture::start().await;
        let (session, _) = f.sign_in().await;
        f.machine("lost-machine", "synthetic-lost-machine-token")
            .await;
        let token = f.form_token(&session).await;
        f.enable_browser_logins().await;
        let old_id = "8181818181818181818181818181818181818181818181818181818181818181";
        let new_id = "8282828282828282828282828282828282828282828282828282828282828282";
        let old = f
            .http
            .post(format!("{}/v1/accounts/login/start", f.second.url))
            .bearer_auth("synthetic-lost-machine-token")
            .header("x-codexctl-device", "lost-machine")
            .json(&json!({"alias":"lost-machine-alias","id":old_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(old.status(), 200);
        let pending: Vec<u8> = f
            .control
            .query_one(
                &format!(
                    "SELECT encrypted_payload FROM {}.central_login_operations WHERE id=$1",
                    f.schema
                ),
                &[&old_id],
            )
            .await
            .unwrap()
            .get(0);
        f.second.stop().await;
        // Restore retained polling evidence after its native child has exited.
        f.control.execute(&format!("UPDATE {}.central_login_operations SET phase=$3,polling_clear=true,selected_reserved=true,expires_at=clock_timestamp()-interval '1 second',encrypted_payload=$2 WHERE id=$1",f.schema),&[&old_id,&pending,&phase]).await.unwrap();
        f.control.execute(&format!("UPDATE {s}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {s}.central_login_operations WHERE id=$1)",s=f.schema),&[&old_id]).await.unwrap();
        let before = f
            .control
            .query_one(
                &format!(
                    "SELECT sequence,epoch,holder_id FROM {}.central_login_operations WHERE id=$1",
                    f.schema
                ),
                &[&old_id],
            )
            .await
            .unwrap();
        let refused = f
            .http
            .post(format!("{}/accounts/add", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[
                ("alias", "lost-machine-alias"),
                ("id", if same_id { old_id } else { new_id }),
                ("form_token", token.as_str()),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            if same_id { 404 } else { 409 },
            "browser start must leave lost machine recovery to that authority kind"
        );
        let after=f.control.query_one(&format!("SELECT sequence,epoch,holder_id,selected_reserved,encrypted_payload FROM {}.central_login_operations WHERE id=$1",f.schema),&[&old_id]).await.unwrap();
        assert_eq!(
            after.get::<_, i64>(0),
            before.get::<_, i64>(0),
            "{phase} same_id={same_id}: foreign receipt must not expire during lookup"
        );
        assert_eq!(after.get::<_, i64>(1), before.get::<_, i64>(1));
        assert_eq!(after.get::<_, String>(2), before.get::<_, String>(2));
        assert!(after.get::<_, bool>(3));
        assert_eq!(after.get::<_, Vec<u8>>(4), pending);
        let recovered = f
            .http
            .post(format!("{}/v1/accounts/login/status", f.first.url))
            .bearer_auth("synthetic-lost-machine-token")
            .header("x-codexctl-device", "lost-machine")
            .json(&json!({"alias":"lost-machine-alias","id":old_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            recovered.status(),
            200,
            "the machine kind retains its recovery path"
        );
        let reserved: bool = f
            .control
            .query_one(
                &format!(
                    "SELECT selected_reserved FROM {}.central_login_operations WHERE id=$1",
                    f.schema
                ),
                &[&old_id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!reserved);
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_browser_renew_completes_without_forced_refresh_and_logs_only_a_new_audit() {
    for existing_terminal in [false, true] {
        let mut f = BrowserFixture::start().await;
        let (session, _) = f.sign_in().await;
        f.machine("renew-complete-machine", "synthetic-renew-complete-token")
            .await;
        store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
        let grant = add_grant(
            "renew-complete-workspace",
            Some("renew-complete-login"),
            Some("renew-complete-uid"),
        );
        let seeded = f
            .http
            .post(format!("{}/v1/accounts", f.first.url))
            .bearer_auth("synthetic-renew-complete-token")
            .json(&json!({"alias":"renew-complete","auth":grant}))
            .send()
            .await
            .unwrap();
        assert_eq!(seeded.status(), 200);
        let before=f.control.query_one(&format!("SELECT account_id,revision FROM {}.central_accounts WHERE alias='renew-complete'",f.schema),&[]).await.unwrap();
        let token = f.form_token(&session).await;
        f.enable_browser_logins().await;
        let id = "9191919191919191919191919191919191919191919191919191919191919191";
        let started = f
            .http
            .post(format!("{}/accounts/renew", f.first.url))
            .header("cookie", &session)
            .header("origin", "http://127.0.0.1:8787")
            .form(&[
                ("alias", "renew-complete"),
                ("id", id),
                ("form_token", token.as_str()),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(started.status(), 303);
        if existing_terminal {
            // Recovery may already have committed the receipt's immutable terminal audit.
            f.control.execute(&format!("INSERT INTO {s}.dashboard_audit(at,actor,operation,target,result,coalesce_key) SELECT 1,'fixture','account_renew','renew-complete','failed:expired','login:'||user_id||':'||id||':terminal' FROM {s}.central_login_operations WHERE id=$1",s=f.schema),&[&id]).await.unwrap();
        }
        for name in ["forced-refresh-count", "limits-count"] {
            store::atomic_write(&f.first.root.path().join(name), b"0").unwrap();
        }
        store::atomic_write(
            &f.first.root.path().join("login-release"),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        let completed = timeout(Duration::from_secs(15), async {
            loop {
                let response = f
                    .http
                    .post(format!("{}/accounts/login/status", f.second.url))
                    .header("cookie", &session)
                    .header("origin", "http://127.0.0.1:8787")
                    .form(&[("id", id), ("form_token", token.as_str())])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                let view: Value = response.json().await.unwrap();
                assert_ne!(
                    view["status"], "failed",
                    "Renew must finish the matching login: {view}"
                );
                if view["status"] == "completed" {
                    break view;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("matching browser Renew completes without a forced refresh");
        assert_eq!(
            completed.as_object().unwrap().len(),
            7,
            "safe receipt fields stay bounded"
        );
        assert!(completed["userCode"].is_null());
        let after=f.control.query_one(&format!("SELECT account_id,revision FROM {}.central_accounts WHERE alias='renew-complete'",f.schema),&[]).await.unwrap();
        assert_eq!(after.get::<_, String>(0), before.get::<_, String>(0));
        assert!(after.get::<_, i64>(1) > before.get::<_, i64>(1));
        for name in ["forced-refresh-count", "limits-count"] {
            assert_eq!(
                std::fs::read_to_string(f.first.root.path().join(name)).unwrap(),
                "0",
                "browser Renew must not request forced refresh or billing reads"
            );
        }
        let rows=f.control.query(&format!("SELECT result FROM {}.dashboard_audit WHERE operation='account_renew' ORDER BY id",f.schema),&[]).await.unwrap();
        let results: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(
            results,
            vec![
                "ok:started",
                if existing_terminal {
                    "failed:expired"
                } else {
                    "ok:completed"
                }
            ]
        );
        timeout(Duration::from_secs(8), async {
            loop {
                let released: bool = f.control.query_one(
                    &format!("SELECT NOT EXISTS(SELECT 1 FROM {s}.account_refresh_leases l JOIN {s}.central_login_operations o ON l.holder_id=o.holder_id||':'||o.id WHERE o.id=$1 AND NOT l.released)", s=f.schema), &[&id]
                ).await.unwrap().get(0);
                if released { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("Renew releases its lease after the completion log decision");
        let logs = std::fs::read_to_string(f.first.root.path().join("dashboard.log")).unwrap();
        let completed_logs = logs
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["operation"] == "account_renew" && v["result"] == "ok:completed")
            .count();
        assert_eq!(
            completed_logs,
            usize::from(!existing_terminal),
            "only a newly inserted terminal audit emits its completion log"
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_dashboard_renew_controls_are_scoped_guarded_and_default_off() {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("renew-controls", "synthetic-renew-controls-token")
        .await;
    let alias = "renew & quoted";
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let seeded=f.http.post(format!("{}/v1/accounts",f.first.url))
        .bearer_auth("synthetic-renew-controls-token")
        .json(&json!({"alias":alias,"auth":add_grant("renew-controls-workspace",Some("renew-controls-login"),None)}))
        .send().await.unwrap();
    assert_eq!(seeded.status(), 200);
    let page = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 503, "Renew starts stay disabled by default");
    assert_eq!(page.headers()["cache-control"], "no-store");
    let overview = f
        .http
        .get(format!("{}/accounts", f.first.url))
        .header("cookie", &session)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!overview.contains("action=\"/accounts/renew\""));
    f.enable_browser_logins().await;
    let page = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", alias)])
        .header("cookie", &session)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        200,
        "owned Renew has a no-JS confirmation page"
    );
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert_eq!(page.headers()["referrer-policy"], "same-origin");
    let html = page.text().await.unwrap();
    assert!(html.contains("renew &amp; quoted"));
    assert!(html.contains("action=\"/accounts/renew\""));
    for field in ["form_token", "id", "alias"] {
        assert!(html.contains(&format!("name=\"{field}\"")));
    }
    let overview = f
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
        overview.contains("action=\"/accounts/renew\""),
        "own account row offers Renew"
    );
    let missing = f
        .http
        .get(format!("{}/accounts/renew", f.first.url))
        .query(&[("alias", "missing-owned-account")])
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let token = f.form_token(&session).await;
    for (origin, form_token) in [
        (None, token.as_str()),
        (Some("https://other.invalid"), token.as_str()),
        (Some("null"), token.as_str()),
        (Some("http://127.0.0.1:8787"), "wrong-form-token"),
    ] {
        let request = f
            .http
            .post(format!("{}/accounts/renew", f.first.url))
            .header("cookie", &session);
        let request = if let Some(origin) = origin {
            request.header("origin", origin)
        } else {
            request
        };
        let refused = request
            .form(&[
                ("alias", alias),
                (
                    "id",
                    "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
                ),
                ("form_token", form_token),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            403,
            "Renew keeps the exact-Origin and session HMAC guard"
        );
        assert_eq!(refused.headers()["cache-control"], "no-store");
    }
    assert!(!f.first.root.path().join("login-pid").exists());
    f.finish().await;
}

async fn pending_verification_renew() -> (BrowserFixture, String, String, Value) {
    let mut f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.machine("verification-renew", "synthetic-verification-renew-token")
        .await;
    let grant = add_grant(
        "verification-renew-workspace",
        Some("verification-renew-login"),
        Some("verification-renew-uid"),
    );
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let seeded = f
        .http
        .post(format!("{}/v1/accounts", f.first.url))
        .bearer_auth("synthetic-verification-renew-token")
        .json(&json!({"alias":"verification-renew","auth":grant}))
        .send()
        .await
        .unwrap();
    assert_eq!(seeded.status(), 200);
    let token = f.form_token(&session).await;
    f.enable_browser_logins().await;
    let started = f
        .http
        .post(format!("{}/accounts/renew", f.first.url))
        .header("cookie", &session)
        .header("origin", "http://127.0.0.1:8787")
        .form(&[
            ("alias", "verification-renew"),
            (
                "id",
                "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1",
            ),
            ("form_token", token.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 303);
    for name in ["forced-refresh-count", "limits-count"] {
        store::atomic_write(&f.first.root.path().join(name), b"0").unwrap();
    }
    (f, session, token, grant)
}

#[tokio::test]
async fn postgres_browser_renew_rejects_a_different_login_without_replacing_the_account() {
    let (f, session, token, _) = pending_verification_renew().await;
    let before=f.control.query_one(&format!("SELECT account_id,revision,encrypted_vault FROM {}.central_accounts WHERE alias='verification-renew'",f.schema),&[]).await.unwrap();
    store::atomic_write(&f.first.root.path().join("mode"), b"").unwrap();
    let wrong = add_grant(
        "verification-renew-workspace",
        Some("different-login"),
        Some("different-uid"),
    );
    store::atomic_write(
        &f.first.root.path().join("login-release"),
        &serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            let response = f
                .http
                .post(format!("{}/accounts/login/status", f.second.url))
                .header("cookie", &session)
                .header("origin", "http://127.0.0.1:8787")
                .form(&[
                    (
                        "id",
                        "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1",
                    ),
                    ("form_token", token.as_str()),
                ])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let view: Value = response.json().await.unwrap();
            assert_ne!(view["status"], "completed");
            if view["status"] == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("wrong-login Renew refuses with retained evidence");
    let after=f.control.query_one(&format!("SELECT account_id,revision,encrypted_vault FROM {}.central_accounts WHERE alias='verification-renew'",f.schema),&[]).await.unwrap();
    assert_eq!(after.get::<_, String>(0), before.get::<_, String>(0));
    assert_eq!(after.get::<_, i64>(1), before.get::<_, i64>(1));
    assert_eq!(after.get::<_, Vec<u8>>(2), before.get::<_, Vec<u8>>(2));
    for name in ["forced-refresh-count", "limits-count"] {
        assert_eq!(
            std::fs::read_to_string(f.first.root.path().join(name)).unwrap(),
            "0"
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_browser_renew_cannot_publish_after_its_session_authority_ends() {
    for ended in ["sign-out", "expiry", "disabled"] {
        let (f, session, token, grant) = pending_verification_renew().await;
        let before: i64 = f
            .control
            .query_one(
                &format!(
                    "SELECT revision FROM {}.central_accounts WHERE alias='verification-renew'",
                    f.schema
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        store::atomic_write(
            &f.first.root.path().join("mode"),
            b"browser-verification-hold",
        )
        .unwrap();
        store::atomic_write(
            &f.first.root.path().join("login-release"),
            &serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        timeout(Duration::from_secs(8), async {
            while !f.first.root.path().join("native-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Renew native verification reaches the controlled hold");
        let phase:String=f.control.query_one(&format!("SELECT phase FROM {}.central_login_operations WHERE alias='verification-renew'",f.schema),&[]).await.unwrap().get(0);
        assert_eq!(phase, "verifying");
        match ended {
            "sign-out"=>{
                let ended=f.http.post(format!("{}/accounts/sign-out",f.second.url))
                    .header("cookie",&session).header("origin","http://127.0.0.1:8787")
                    .form(&[("form_token",token.as_str())]).send().await.unwrap();
                assert_eq!(ended.status(),303);
            }
            "expiry"=>f.control.batch_execute(&format!("UPDATE {}.browser_sessions SET expires_at=clock_timestamp()-interval '1 second'",f.schema)).await.unwrap(),
            _=>f.control.batch_execute(&format!("UPDATE {}.central_users SET enabled=false",f.schema)).await.unwrap(),
        }
        store::atomic_write(&f.first.root.path().join("native-release"), b"1").unwrap();
        timeout(Duration::from_secs(12),async {
            loop {
                let row=f.control.query_one(&format!("SELECT phase,completed_revision FROM {}.central_login_operations WHERE alias='verification-renew'",f.schema),&[]).await.unwrap();
                let phase:String=row.get(0);
                assert_ne!(phase,"completed","ended {ended} authority must not publish");
                assert!(row.get::<_,Option<i64>>(1).is_none());
                if ["unresolved","rejected","failed","canceled"].contains(&phase.as_str()) {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("ended Renew verification settles without publication");
        let after: i64 = f
            .control
            .query_one(
                &format!(
                    "SELECT revision FROM {}.central_accounts WHERE alias='verification-renew'",
                    f.schema
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(after, before);
        for name in ["forced-refresh-count", "limits-count"] {
            assert_eq!(
                std::fs::read_to_string(f.first.root.path().join(name)).unwrap(),
                "0"
            );
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn postgres_machine_renew_recovers_a_lost_browser_receipt_with_browser_starts_disabled() {
    for phase in ["replica_lost", "pending"] {
        let (mut f, _, _, _) = pending_verification_renew().await;
        let old_id = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
        let pending: Vec<u8> = f
            .control
            .query_one(
                &format!(
                    "SELECT encrypted_payload FROM {}.central_login_operations WHERE id=$1",
                    f.schema
                ),
                &[&old_id],
            )
            .await
            .unwrap()
            .get(0);
        f.first.stop().await;
        f.control.execute(&format!("UPDATE {}.central_login_operations SET phase=$3,expires_at=clock_timestamp()-interval '1 second',polling_clear=true,selected_reserved=true,encrypted_payload=$2 WHERE id=$1",f.schema),&[&old_id,&pending,&phase]).await.unwrap();
        f.control.execute(&format!("UPDATE {s}.central_login_holders SET expires_at=clock_timestamp()-interval '1 second' WHERE holder_id=(SELECT holder_id FROM {s}.central_login_operations WHERE id=$1)",s=f.schema),&[&old_id]).await.unwrap();
        // Pod B keeps its default-off gate; no surviving browser worker owns this receipt.
        let started=f.http.post(format!("{}/v1/relogin/start",f.second.url))
            .bearer_auth("synthetic-verification-renew-token").header("x-codexctl-device","verification-renew")
            .json(&json!({"alias":"verification-renew","id":"c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1"}))
            .send().await.unwrap();
        assert_eq!(
            started.status(),
            200,
            "machine renewal must recover stale {phase} browser polling during rollback"
        );
        let view: Value = started.json().await.unwrap();
        assert_eq!(view["status"], "pending");
        assert_ne!(
            view["id"], old_id,
            "machine receives its new receipt, never the browser receipt"
        );
        let reserved: bool = f
            .control
            .query_one(
                &format!(
                    "SELECT selected_reserved FROM {}.central_login_operations WHERE id=$1",
                    f.schema
                ),
                &[&old_id],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!reserved);
        f.finish().await;
    }
}
