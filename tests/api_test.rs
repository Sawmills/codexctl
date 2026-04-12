use codexctl::api::{self, AccountSettings, RateLimitResponse};

#[test]
fn parse_auth_json_flat_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"access_token": "tok_abc", "refresh_token": "ref_123"}"#,
    )
    .unwrap();
    let auth = api::read_auth_json(&path).unwrap();
    assert_eq!(auth.access_token, "tok_abc");
    assert_eq!(auth.refresh_token.as_deref(), Some("ref_123"));
}

#[test]
fn parse_auth_json_codex_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"auth_mode": "chatgpt", "tokens": {"access_token": "tok_nested", "refresh_token": "ref_nested"}}"#,
    )
    .unwrap();
    let auth = api::read_auth_json(&path).unwrap();
    assert_eq!(auth.access_token, "tok_nested");
    assert_eq!(auth.refresh_token.as_deref(), Some("ref_nested"));
}

#[test]
fn parse_auth_json_without_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(&path, r#"{"access_token": "tok_abc"}"#).unwrap();
    let auth = api::read_auth_json(&path).unwrap();
    assert_eq!(auth.access_token, "tok_abc");
    assert!(auth.refresh_token.is_none());
}

#[test]
fn parse_rate_limit_response_old_format() {
    let json = r#"{
        "plan_type": "pro",
        "rate_limit": {
            "limit_id": "codex",
            "limit_name": "Codex",
            "primary": {
                "used_percent": 27.0,
                "window_minutes": 300,
                "resets_at": 1743789600
            },
            "secondary": {
                "used_percent": 46.0,
                "window_minutes": 10080,
                "resets_at": 1744137600
            }
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.plan_type.as_deref(), Some("pro"));
    let rl = resp.rate_limit.unwrap();
    let primary = rl.primary().unwrap();
    assert!((primary.used_percent - 27.0).abs() < f64::EPSILON);
    assert_eq!(primary.reset_timestamp(), Some(1743789600));
    let secondary = rl.secondary().unwrap();
    assert!((secondary.used_percent - 46.0).abs() < f64::EPSILON);
}

#[test]
fn parse_rate_limit_response_team_format() {
    let json = r#"{
        "plan_type": "team",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 0,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 18000,
                "reset_at": 1775369763
            },
            "secondary_window": {
                "used_percent": 25,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 414415,
                "reset_at": 1775766178
            }
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.plan_type.as_deref(), Some("team"));
    let rl = resp.rate_limit.unwrap();
    let primary = rl.primary().unwrap();
    assert!((primary.used_percent - 0.0).abs() < f64::EPSILON);
    assert_eq!(primary.reset_timestamp(), Some(1775369763));
    let secondary = rl.secondary().unwrap();
    assert!((secondary.used_percent - 25.0).abs() < f64::EPSILON);
}

#[test]
fn parse_rate_limit_response_missing_fields() {
    let json = r#"{"plan_type": null, "rate_limit": null}"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert!(resp.plan_type.is_none());
    assert!(resp.rate_limit.is_none());
}

#[test]
fn parse_usage_based_response_with_credits() {
    let json = r#"{
        "plan_type": "self_serve_business_usage_based",
        "rate_limit": null,
        "credits": {
            "has_credits": true,
            "unlimited": false,
            "overage_limit_reached": false,
            "balance": null,
            "approx_local_messages": null,
            "approx_cloud_messages": null
        },
        "spend_control": {
            "reached": false
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(
        resp.plan_type.as_deref(),
        Some("self_serve_business_usage_based")
    );
    assert!(resp.rate_limit.is_none());
    let credits = resp.credits.unwrap();
    assert!(credits.has_credits);
    assert!(!credits.unlimited);
    assert!(!credits.overage_limit_reached);
    assert!(credits.balance.is_none());
    let spend = resp.spend_control.unwrap();
    assert!(!spend.reached);
}

#[test]
fn parse_usage_based_response_with_balance() {
    let json = r#"{
        "plan_type": "self_serve_business_usage_based",
        "rate_limit": null,
        "credits": {
            "has_credits": true,
            "unlimited": false,
            "overage_limit_reached": false,
            "balance": "1234.56",
            "approx_local_messages": null,
            "approx_cloud_messages": null
        },
        "spend_control": {
            "reached": false
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    let credits = resp.credits.unwrap();
    assert_eq!(credits.balance.as_deref(), Some("1234.56"));
}

#[test]
fn parse_team_response_still_works_with_new_fields() {
    let json = r#"{
        "plan_type": "team",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 5,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 17000,
                "reset_at": 1775369763
            },
            "secondary_window": {
                "used_percent": 20,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 400000,
                "reset_at": 1775766178
            }
        },
        "credits": {
            "has_credits": false,
            "unlimited": false,
            "overage_limit_reached": false,
            "balance": null,
            "approx_local_messages": null,
            "approx_cloud_messages": null
        },
        "spend_control": {
            "reached": false
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.plan_type.as_deref(), Some("team"));
    assert!(resp.rate_limit.is_some());
    let credits = resp.credits.unwrap();
    assert!(!credits.has_credits);
}

#[test]
fn parse_account_settings_with_credit_limits() {
    let json = r#"{
        "seat_type_credit_limits": {
            "default": [],
            "usage_based": [
                {"enforcement_mode": "HARD_CAP", "limit": 20000}
            ]
        }
    }"#;
    let settings: AccountSettings = serde_json::from_str(json).unwrap();
    let limits = settings.seat_type_credit_limits.unwrap();
    let usage_based = limits.usage_based.unwrap();
    assert_eq!(usage_based.len(), 1);
    assert_eq!(usage_based[0].limit, 20000);
    assert_eq!(usage_based[0].enforcement_mode, "HARD_CAP");
}

#[test]
fn parse_account_settings_empty_limits() {
    let json = r#"{
        "seat_type_credit_limits": {
            "default": [],
            "usage_based": []
        }
    }"#;
    let settings: AccountSettings = serde_json::from_str(json).unwrap();
    let limits = settings.seat_type_credit_limits.unwrap();
    assert!(limits.usage_based.unwrap().is_empty());
}

#[test]
fn parse_account_settings_missing_limits() {
    let json = r#"{}"#;
    let settings: AccountSettings = serde_json::from_str(json).unwrap();
    assert!(settings.seat_type_credit_limits.is_none());
}
