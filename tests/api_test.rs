use codexctl::api::{AuthJson, RateLimitResponse};

#[test]
fn parse_auth_json() {
    let json = r#"{"access_token": "tok_abc", "refresh_token": "ref_123"}"#;
    let auth: AuthJson = serde_json::from_str(json).unwrap();
    assert_eq!(auth.access_token, "tok_abc");
}

#[test]
fn parse_auth_json_without_refresh() {
    let json = r#"{"access_token": "tok_abc"}"#;
    let auth: AuthJson = serde_json::from_str(json).unwrap();
    assert_eq!(auth.access_token, "tok_abc");
    assert!(auth.refresh_token.is_none());
}

#[test]
fn parse_rate_limit_response() {
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
    let primary = rl.primary.unwrap();
    assert!((primary.used_percent - 27.0).abs() < f64::EPSILON);
    assert_eq!(primary.window_minutes, 300);
    let secondary = rl.secondary.unwrap();
    assert!((secondary.used_percent - 46.0).abs() < f64::EPSILON);
}

#[test]
fn parse_rate_limit_response_missing_fields() {
    let json = r#"{"plan_type": null, "rate_limit": null}"#;
    let resp: RateLimitResponse = serde_json::from_str(json).unwrap();
    assert!(resp.plan_type.is_none());
    assert!(resp.rate_limit.is_none());
}
