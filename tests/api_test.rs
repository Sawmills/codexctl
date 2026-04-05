use codexctl::api::{self, RateLimitResponse};

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
