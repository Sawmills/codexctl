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
    pub credits: Option<Credits>,
    pub spend_control: Option<SpendControl>,
}

#[derive(Deserialize)]
pub struct RateLimit {
    // API returns both naming conventions depending on plan
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub primary_window: Option<RateLimitWindow>,
    pub secondary_window: Option<RateLimitWindow>,
}

impl RateLimit {
    pub fn primary(&self) -> Option<&RateLimitWindow> {
        self.primary.as_ref().or(self.primary_window.as_ref())
    }
    pub fn secondary(&self) -> Option<&RateLimitWindow> {
        self.secondary.as_ref().or(self.secondary_window.as_ref())
    }
}

#[derive(Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    // Supports both field names from API
    #[allow(dead_code)]
    pub window_minutes: Option<u64>,
    #[allow(dead_code)]
    pub limit_window_seconds: Option<u64>,
    pub resets_at: Option<i64>,
    pub reset_at: Option<i64>,
    pub reset_after_seconds: Option<i64>,
}

impl RateLimitWindow {
    /// Get reset timestamp, preferring absolute time, falling back to relative
    pub fn reset_timestamp(&self) -> Option<i64> {
        self.resets_at.or(self.reset_at).or_else(|| {
            self.reset_after_seconds
                .map(|s| chrono::Utc::now().timestamp() + s)
        })
    }
}

#[derive(Deserialize)]
pub struct Credits {
    pub has_credits: bool,
    #[serde(default)]
    pub unlimited: bool,
    #[serde(default)]
    pub overage_limit_reached: bool,
    pub balance: Option<String>,
}

#[derive(Deserialize)]
pub struct SpendControl {
    pub reached: bool,
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

pub async fn fetch_usage_async(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<RateLimitResponse> {
    let resp = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .context("failed to reach rate limit API")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("expired");
    }
    if !status.is_success() {
        anyhow::bail!("API returned {status}");
    }

    resp.json::<RateLimitResponse>()
        .await
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
