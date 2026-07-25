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
fn parse_auth_json_codex_account_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"auth_mode": "chatgpt", "tokens": {"access_token": "tok_nested", "refresh_token": "ref_nested", "account_id": "acc_123"}}"#,
    )
    .unwrap();
    let auth = api::read_auth_json(&path).unwrap();
    assert_eq!(auth.account_id.as_deref(), Some("acc_123"));
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
    let primary = rl.short_window().unwrap();
    assert!((primary.used_percent - 27.0).abs() < f64::EPSILON);
    assert_eq!(primary.reset_timestamp(), Some(1743789600));
    let secondary = rl.long_window().unwrap();
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
    let primary = rl.short_window().unwrap();
    assert!((primary.used_percent - 0.0).abs() < f64::EPSILON);
    assert_eq!(primary.reset_timestamp(), Some(1775369763));
    let secondary = rl.long_window().unwrap();
    assert!((secondary.used_percent - 25.0).abs() < f64::EPSILON);
}

/// Plans that publish only a weekly window return it in the `primary_window`
/// slot. Reading windows positionally would report that weekly limit as a 5h
/// one and leave the 7d reset unknown, which silently disables reset-aware
/// selection.
#[test]
fn parse_rate_limit_response_weekly_only_maps_to_long_window() {
    let json = r#"{
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 92,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 506456,
                "reset_at": 1785453503
            },
            "secondary_window": null
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    let rl = resp.rate_limit.unwrap();
    assert!(rl.short_window().is_none());
    let long = rl.long_window().unwrap();
    assert!((long.used_percent - 92.0).abs() < f64::EPSILON);
    assert_eq!(long.reset_timestamp(), Some(1785453503));
}

#[test]
fn parse_rate_limit_reset_credits() {
    let json = r#"{
        "plan_type": "pro",
        "rate_limit": null,
        "rate_limit_reset_credits": {
            "available_count": 3,
            "applicable_available_count": 2
        }
    }"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.reset_credits_available(), 3);
    assert_eq!(resp.reset_credits_applicable(), 2);
}

/// Responses predating banked resets omit the field entirely.
#[test]
fn parse_rate_limit_reset_credits_absent() {
    let json = r#"{"plan_type": "pro", "rate_limit": null}"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.reset_credits_available(), 0);
    assert_eq!(resp.reset_credits_applicable(), 0);
}

#[test]
fn parse_reset_credits_details() {
    let json = r#"{
        "credits": [
            {
                "id": "RateLimitResetCredit_abc",
                "reset_type": "codex_rate_limits",
                "is_supported_by_plan": true,
                "status": "available",
                "granted_at": "2026-06-26T23:59:32.757458Z",
                "expires_at": "2026-07-26T23:59:32.757458Z",
                "redeemed_at": null,
                "title": "Full reset",
                "description": "Thanks for using Codex!"
            },
            {
                "id": "RateLimitResetCredit_def",
                "reset_type": "codex_rate_limits",
                "status": "redeemed",
                "granted_at": "2026-06-01T00:00:00Z",
                "expires_at": null,
                "title": null,
                "description": null
            }
        ],
        "available_count": 1
    }"#;
    let details: api::ResetCreditsDetails = serde_json::from_str(json).unwrap();
    assert_eq!(details.available_count, 1);
    assert_eq!(details.credits.len(), 2);
    assert!(details.credits[0].is_available());
    assert!(!details.credits[1].is_available());
    assert_eq!(
        details.credits[0].expires_at_timestamp(),
        Some(1785110372),
        "expiry parses as an RFC3339 timestamp"
    );
    assert_eq!(details.credits[1].expires_at_timestamp(), None);
}

#[test]
fn parse_consume_reset_response_codes() {
    let reset: api::ConsumeResetResponse =
        serde_json::from_str(r#"{"code": "reset", "windows_reset": 2}"#).unwrap();
    assert_eq!(reset.code, api::ConsumeResetCode::Reset);
    assert_eq!(reset.windows_reset, 2);

    for (raw, expected) in [
        ("nothing_to_reset", api::ConsumeResetCode::NothingToReset),
        ("no_credit", api::ConsumeResetCode::NoCredit),
        ("already_redeemed", api::ConsumeResetCode::AlreadyRedeemed),
        ("something_new", api::ConsumeResetCode::Unknown),
    ] {
        let resp: api::ConsumeResetResponse =
            serde_json::from_str(&format!(r#"{{"code": "{raw}"}}"#)).unwrap();
        assert_eq!(resp.code, expected, "code {raw}");
        assert_eq!(resp.windows_reset, 0);
    }
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

// Fake JWT header `{"alg":"none"}`; only the (unverified) claims payload is read.
const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

#[test]
fn token_subject_reads_sub_claim() {
    // payload {"sub":"seatA"}
    let tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.sig");
    assert_eq!(api::token_subject(&tok).as_deref(), Some("seatA"));
    assert_eq!(api::token_subject("not-a-jwt"), None);
}

#[test]
fn is_token_expired_distinguishes_exp_claim() {
    let future = format!("{JWT_HDR}.eyJleHAiOjk5OTk5OTk5OTl9.sig"); // exp 9999999999
    let past = format!("{JWT_HDR}.eyJleHAiOjEwMDAwMDAwMDB9.sig"); // exp 1000000000 (year 2001)
    assert!(!api::is_token_expired(&future));
    assert!(api::is_token_expired(&past));
    // A token whose grant was rotated/revoked still has a valid (or unreadable)
    // exp — it must not be reported as time-expired.
    assert!(!api::is_token_expired("opaque-token"));
    assert!(!api::is_token_expired(&format!(
        "{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.sig"
    )));
}
