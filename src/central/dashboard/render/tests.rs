use super::*;

fn fixture() -> Snapshot {
    serde_json::from_str(include_str!("../../../../tests/fixtures/dashboard-v3.json")).unwrap()
}

#[test]
fn renders_every_account_and_machine_state_with_the_right_action() {
    let mut data = fixture();
    data.accounts[3].billing_class = crate::api::BillingClass::UsageBased;
    let html = overview(&data);
    for text in [
        "Use Everyday building",
        "codexctl use studio",
        "Nearly exhausted",
        "Exhausted",
        "Redeemable now",
        "codexctl reset sprint",
        "Stale usage",
        "Last observed 7 min ago",
        "Renewal pending",
        "codexctl login weekend --cancel",
        "Login needs attention",
        "codexctl login night",
        "Routing refused",
        "Usage-based · bills credits",
        "In use",
        "Idle",
        "revoked machine",
    ] {
        assert!(html.contains(text), "missing {text}");
    }
    assert!(!html.contains("codexctl login routing"));
    assert!(!html.contains("data-value=\"show\" open"));
    assert!(html.contains("Banked reset expires"));
    assert!(html.contains("value=\"76\""), "meters show left, not used");
}

#[test]
fn recommendation_follows_no_bill_threshold_reset_and_score_order() {
    let base = fixture().accounts[0].clone();
    let mut accounts = vec![base.clone(); 5];
    for (i, a) in accounts.iter_mut().enumerate() {
        a.alias = format!("account-{i}");
        a.secondary.resets_at = Some(100 + i as i64);
    }
    accounts[0].billing_class = crate::api::BillingClass::UsageBased;
    accounts[1].billing_class = crate::api::BillingClass::Unknown;
    accounts[2].primary.used_percent = Some(95.0);
    assert_eq!(recommendation(&accounts).unwrap().alias, "account-3");
    accounts[3].secondary.resets_at = Some(104);
    accounts[4].primary.used_percent = Some(1.0);
    assert_eq!(recommendation(&accounts).unwrap().alias, "account-4");
    accounts[4].primary.used_percent = Some(100.0);
    assert_eq!(recommendation(&accounts).unwrap().alias, "account-3");
    accounts[3].usage_age_seconds = Some(60);
    assert_eq!(recommendation(&accounts).unwrap().alias, "account-2");
    accounts[2].routing_refused = true;
    assert!(recommendation(&accounts).is_none());
}

#[test]
fn weekly_only_qualifies_but_no_telemetry_never_does() {
    let mut data = fixture();
    data.accounts.truncate(1);
    data.accounts[0].primary = Window {
        used_percent: None,
        left_percent: None,
        window_seconds: None,
        resets_at: None,
    };
    let html = overview(&data);
    assert!(html.contains("Use Everyday building"));
    assert!(html.contains("No limit"));
    data.accounts[0].primary.window_seconds = Some(18000);
    assert!(
        recommendation(&data.accounts).is_none(),
        "reported window without usage is unknown, not absent"
    );
    data.accounts[0].primary.window_seconds = None;
    data.accounts[0].secondary = data.accounts[0].primary.clone();
    let html = overview(&data);
    assert!(!html.contains("Use Everyday building"));
    assert!(html.contains("Unknown"));
    assert!(!html.contains("No limit"));
}

#[test]
fn reset_fallback_requires_fresh_exhaustion_and_inventory() {
    let mut data = fixture();
    data.accounts = vec![data.accounts[1].clone()];
    assert!(overview(&data).contains("codexctl reset sprint\ncodexctl use sprint"));
    data.accounts[0].banked_resets.stale = true;
    assert!(!overview(&data).contains("codexctl reset sprint"));
    data.accounts[0].banked_resets.stale = false;
    data.accounts[0].banked_resets.nearest_expiry = Some(data.server_time);
    assert!(!overview(&data).contains("codexctl reset sprint"));
    data.accounts[0].banked_resets.nearest_expiry = None;
    data.accounts[0].usage_stale = true;
    assert!(!overview(&data).contains("codexctl reset sprint"));
    assert!(overview(&data).contains("Cannot confirm headroom"));
}

#[test]
fn stale_unknown_billing_still_withdraws_the_recommendation() {
    let mut data = fixture();
    data.accounts.truncate(1);
    data.accounts[0].billing_class = crate::api::BillingClass::Unknown;
    data.accounts[0].usage_stale = true;
    let html = overview(&data);
    assert!(html.contains("Cannot confirm headroom"));
    assert!(!html.contains("No included usage available"));
}

#[test]
fn stale_included_account_blocks_reset_advice_for_another_account() {
    let mut data = fixture();
    let mut stale = data.accounts[3].clone();
    stale.billing_class = crate::api::BillingClass::Unknown;
    data.accounts = vec![data.accounts[1].clone(), stale];
    let html = overview(&data);
    assert!(html.contains("Cannot confirm headroom"));
    assert!(!html.contains("id=\"cmd-reset-use\""));
    assert!(!html.contains("codexctl reset sprint"));
    assert!(!html.contains("id=\"cmd-live-current\""));
    assert!(!html.contains("id=\"cmd-live\""));
}

#[test]
fn earliest_recovery_requires_both_exhausted_windows_and_never_promises_unknown_reset() {
    let mut data = fixture();
    data.accounts.truncate(1);
    let a = &mut data.accounts[0];
    a.primary.used_percent = Some(100.0);
    a.secondary.used_percent = Some(100.0);
    a.primary.resets_at = Some(data.server_time + 60);
    a.secondary.resets_at = Some(data.server_time + 7200);
    let html = answer(&data.accounts, None, data.server_time);
    assert!(html.contains("Resets in 2h 0m"));
    assert!(!html.contains("Resets in 1m"));
    data.accounts[0].secondary.resets_at = None;
    assert!(!answer(&data.accounts, None, data.server_time).contains("Resets in"));
}

#[test]
fn stale_page_uses_one_notice_and_keeps_actionable_login_problems() {
    let mut data = fixture();
    for a in &mut data.accounts {
        a.usage_stale = true;
    }
    let html = overview(&data);
    assert!(html.contains("All usage observations are stale"));
    assert!(!html.contains("Stale usage"));
    assert!(!html.contains("codexctl reset sprint"));
    assert!(html.contains("codexctl login night"));
    assert!(!html.contains("All clear"));
}

#[test]
fn renders_untrusted_text_as_text_and_quotes_shell_aliases() {
    let mut data = fixture();
    data.identity.email = "<img src=x onerror=alert(1)>".into();
    data.accounts[0].label = Some("<script>alert(1)</script>".into());
    data.accounts[0].alias = "seat;$(touch /tmp/unsafe)'".into();
    data.machines[0].name = "<img src=x>".into();
    let html = overview(&data);
    assert!(!html.contains("<img"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("codexctl use &#39;seat;$(touch /tmp/unsafe)&#39;\\&#39;&#39;&#39;"));
}

// Export the actual Rust renderer and CSP for browser assertions/screenshots.
// This test hook is absent from production routes.
#[tokio::test]
async fn render_fixture_pages() {
    let Some(directory) = std::env::var_os("B22_RENDER_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    std::fs::create_dir_all(directory).unwrap();
    let data = fixture();
    let mut pages = vec![("accounts", data.clone())];
    let mut healthy = data.clone();
    healthy.accounts.truncate(1);
    pages.push(("healthy", healthy));
    let mut blocked = data.clone();
    blocked.accounts = vec![data.accounts[1].clone(), data.accounts[7].clone()];
    pages.push(("blocked", blocked));
    let mut stale = data.clone();
    for a in &mut stale.accounts {
        a.usage_stale = true;
    }
    pages.push(("stale", stale));
    let mut empty = data;
    empty.accounts.clear();
    empty.machines.clear();
    pages.push(("empty", empty));
    for (name, data) in pages {
        let response = super::super::document(&overview(&data));
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        std::fs::write(directory.join(format!("{name}.html")), body).unwrap();
        std::fs::write(directory.join(format!("{name}.html.csp")), csp).unwrap();
    }
}

#[tokio::test]
async fn snapshot_error_page_retries_without_javascript() {
    let response = super::super::document(include_str!("../error.html"));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains(r#"<noscript><meta http-equiv="refresh" content="60"></noscript>"#));
}

#[test]
fn expiring_reset_never_suggests_spending_before_exhaustion() {
    let mut data = fixture();
    data.accounts.truncate(1);
    data.accounts[0].banked_resets = Resets {
        count: Some(2),
        redeemable_now: Some(1),
        nearest_expiry: Some(data.server_time + 3600),
        stale: false,
    };
    let html = overview(&data);
    assert!(html.contains("Reset expiring"));
    assert!(!html.contains("codexctl reset studio"));
}

#[test]
fn option_like_aliases_are_passed_as_positional_arguments() {
    let mut data = fixture();
    data.accounts.truncate(1);
    data.accounts[0].alias = "--help".into();
    assert!(overview(&data).contains("codexctl use -- --help"));
    data.accounts[0].state = "renewal_pending".into();
    assert!(overview(&data).contains("codexctl login --cancel -- --help"));
}

#[test]
fn page_validity_ignores_accounts_that_cannot_refresh_the_answer() {
    for (state, refused) in [
        ("available", true),
        ("unavailable", true),
        ("unavailable", false),
        ("renewal_pending", false),
    ] {
        for age in [30, 59, 60] {
            let mut data = fixture();
            data.accounts.truncate(1);
            data.accounts[0].usage_age_seconds = Some(5);
            let mut unavailable = data.accounts[0].clone();
            unavailable.alias = "cannot-refresh".into();
            unavailable.state = state.into();
            unavailable.routing_refused = refused;
            unavailable.usage_age_seconds = Some(age);
            data.accounts.push(unavailable);
            let html = overview(&data);
            assert!(html.contains("codexctl use studio"));
            assert!(
                html.contains("data-valid-for=\"55\""),
                "{state}, routing refused {refused}, age {age} must not shorten the ready account's validity"
            );
        }
    }
}

#[test]
fn validity_uses_only_refreshable_accounts_and_empty_pages_do_not_expire() {
    let mut data = fixture();
    data.accounts.truncate(1);
    data.accounts[0].usage_age_seconds = Some(5);
    let mut routing = data.accounts[0].clone();
    routing.alias = "routing".into();
    routing.state = "available".into();
    routing.routing_refused = true;
    routing.usage_age_seconds = Some(59);
    data.accounts.push(routing);
    assert!(overview(&data).contains("data-valid-for=\"55\""));
    let mut empty = data;
    empty.accounts.clear();
    let html = overview(&empty);
    assert!(html.contains("data-has-accounts=\"false\""));
    assert!(!html.contains("data-valid-for=\"0\""));
}

#[test]
fn account_commands_identify_posix_shell_syntax() {
    let html = overview(&fixture());
    assert!(html.matches("POSIX shell syntax.").count() >= 4);
}
