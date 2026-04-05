use anyhow::{Context, Result};
use serde::Deserialize;

/// Supports both formats:
/// - Codex CLI: `{"auth_mode": "chatgpt", "tokens": {"access_token": "...", "refresh_token": "..."}}`
/// - Simple: `{"access_token": "...", "refresh_token": "..."}`
pub struct AuthJson {
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthJson {
    tokens: Option<CodexTokens>,
    // Flat fallback fields
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
pub struct RateLimitResponse {
    pub plan_type: Option<String>,
    pub rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
pub struct RateLimit {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
}

#[derive(Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    #[allow(dead_code)]
    pub window_minutes: u64,
    pub resets_at: i64,
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub fn fetch_usage(access_token: &str) -> Result<RateLimitResponse> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .send()
        .context("failed to reach rate limit API")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("expired");
    }
    if !status.is_success() {
        anyhow::bail!("API returned {status}");
    }

    resp.json::<RateLimitResponse>()
        .context("failed to parse rate limit response")
}

pub fn read_auth_json(path: &std::path::Path) -> Result<AuthJson> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let raw: CodexAuthJson = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    // Prefer nested tokens format, fall back to flat
    if let Some(tokens) = raw.tokens {
        Ok(AuthJson {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    } else if let Some(access_token) = raw.access_token {
        Ok(AuthJson {
            access_token,
            refresh_token: raw.refresh_token,
        })
    } else {
        anyhow::bail!("no access_token found in {}", path.display())
    }
}
