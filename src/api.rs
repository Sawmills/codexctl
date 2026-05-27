use anyhow::{Context, Result};
use serde::Deserialize;

/// Supports both formats:
/// - Codex CLI: `{"auth_mode": "chatgpt", "tokens": {"access_token": "...", "refresh_token": "..."}}`
/// - Simple: `{"access_token": "...", "refresh_token": "..."}`
pub struct AuthJson {
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthJson {
    tokens: Option<CodexTokens>,
    // Flat fallback fields
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
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

#[derive(Deserialize)]
pub struct AccountSettings {
    pub seat_type_credit_limits: Option<SeatTypeCreditLimits>,
}

#[derive(Deserialize)]
pub struct SeatTypeCreditLimits {
    pub usage_based: Option<Vec<CreditLimit>>,
}

#[derive(Deserialize)]
pub struct CreditLimit {
    #[allow(dead_code)]
    pub enforcement_mode: String,
    pub limit: u64,
}

const ACCOUNT_SETTINGS_URL: &str = "https://chatgpt.com/backend-api/accounts";

pub async fn fetch_account_settings_async(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<AccountSettings> {
    let url = format!("{ACCOUNT_SETTINGS_URL}/{account_id}/settings");
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", account_id)
        .send()
        .await
        .context("failed to reach account settings API")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("account settings API returned {status}");
    }

    resp.json::<AccountSettings>()
        .await
        .context("failed to parse account settings response")
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub fn fetch_usage(access_token: &str, account_id: Option<&str>) -> Result<RateLimitResponse> {
    let client = reqwest::blocking::Client::new();
    let mut request = client.get(USAGE_URL).bearer_auth(access_token);
    if let Some(account_id) = account_id {
        request = request.header("chatgpt-account-id", account_id);
    }

    let resp = request.send().context("failed to reach rate limit API")?;

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
    account_id: Option<&str>,
) -> Result<RateLimitResponse> {
    let mut request = client.get(USAGE_URL).bearer_auth(access_token);
    if let Some(account_id) = account_id {
        request = request.header("chatgpt-account-id", account_id);
    }

    let resp = request
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

    if let Some(tokens) = raw.tokens {
        let account_id = tokens
            .account_id
            .or(tokens.chatgpt_account_id)
            .or_else(|| extract_account_id(&tokens.access_token));
        Ok(AuthJson {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id,
        })
    } else if let Some(access_token) = raw.access_token {
        let account_id = raw
            .account_id
            .or(raw.chatgpt_account_id)
            .or_else(|| extract_account_id(&access_token));
        Ok(AuthJson {
            access_token,
            refresh_token: raw.refresh_token,
            account_id,
        })
    } else {
        anyhow::bail!("no access_token found in {}", path.display())
    }
}

/// Decode the (unverified) claims payload of a JWT access token.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    parts.next()?;

    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let bytes = engine.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Extract account_id from JWT access_token claims when auth.json does not store it directly.
pub fn extract_account_id(token: &str) -> Option<String> {
    let value = decode_jwt_payload(token)?;
    let auth = value.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .or_else(|| auth.get("account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The `sub` (subject) claim — identifies the individual seat/user behind a token.
/// Distinct per seat even when many seats share one `chatgpt_account_id`.
pub fn token_subject(token: &str) -> Option<String> {
    decode_jwt_payload(token)?
        .get("sub")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The `exp` (expiry) claim as a unix timestamp, if present.
pub fn token_expiry(token: &str) -> Option<i64> {
    decode_jwt_payload(token)?
        .get("exp")
        .and_then(|v| v.as_i64())
}

/// True only when the token's `exp` claim is in the past. An unreadable/absent
/// `exp` returns false so a rotated-but-not-time-expired token isn't mislabeled.
pub fn is_token_expired(token: &str) -> bool {
    token_expiry(token).is_some_and(|exp| exp < chrono::Utc::now().timestamp())
}
