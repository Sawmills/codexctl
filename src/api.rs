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
    /// Banked rate-limit reset credits, when the plan has any.
    pub rate_limit_reset_credits: Option<ResetCreditsSummary>,
}

impl RateLimitResponse {
    /// How many banked resets are held, redeemable or not.
    pub fn reset_credits_available(&self) -> i64 {
        self.rate_limit_reset_credits
            .as_ref()
            .map_or(0, |c| c.available_count)
    }

    /// How many banked resets can be redeemed *right now*. The backend only
    /// counts a credit as applicable once a window is actually exhausted, so
    /// this is the authoritative "would a redeem do anything" signal.
    pub fn reset_credits_applicable(&self) -> i64 {
        self.rate_limit_reset_credits
            .as_ref()
            .map_or(0, |c| c.applicable_available_count)
    }
}

#[derive(Deserialize)]
pub struct RateLimit {
    // API returns both naming conventions depending on plan
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub primary_window: Option<RateLimitWindow>,
    pub secondary_window: Option<RateLimitWindow>,
}

/// Windows at or under this duration are the short (5h) window; longer ones are
/// the long (7d) window.
const SHORT_WINDOW_MAX_SECONDS: u64 = 24 * 60 * 60;

impl RateLimit {
    fn first(&self) -> Option<&RateLimitWindow> {
        self.primary.as_ref().or(self.primary_window.as_ref())
    }

    fn second(&self) -> Option<&RateLimitWindow> {
        self.secondary.as_ref().or(self.secondary_window.as_ref())
    }

    /// The short (5h) window.
    ///
    /// Windows are matched by their declared duration rather than by position:
    /// plans that no longer publish a 5h window return their weekly window in
    /// the `primary_window` slot, and reading that positionally would report a
    /// weekly limit as a 5h one. Windows with no declared duration fall back to
    /// the historical positional reading.
    pub fn short_window(&self) -> Option<&RateLimitWindow> {
        self.window_matching(|seconds| seconds <= SHORT_WINDOW_MAX_SECONDS, Self::first)
    }

    /// The long (7d) window. See [`RateLimit::short_window`] for how windows are
    /// matched.
    pub fn long_window(&self) -> Option<&RateLimitWindow> {
        self.window_matching(|seconds| seconds > SHORT_WINDOW_MAX_SECONDS, Self::second)
    }

    fn window_matching(
        &self,
        matches: impl Fn(u64) -> bool,
        positional: impl Fn(&Self) -> Option<&RateLimitWindow>,
    ) -> Option<&RateLimitWindow> {
        for window in [self.first(), self.second()].into_iter().flatten() {
            if window.duration_seconds().is_some_and(&matches) {
                return Some(window);
            }
        }
        // Nothing declares a duration in this class. Fall back to the historical
        // positional reading only when that window's duration is unknown, so a
        // response that does publish durations is never misread.
        positional(self).filter(|w| w.duration_seconds().is_none())
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

    /// How long this window spans, from whichever field the API published.
    pub fn duration_seconds(&self) -> Option<u64> {
        self.limit_window_seconds
            .or_else(|| self.window_minutes.map(|m| m * 60))
    }
}

/// Banked rate-limit reset credits, as summarized on the usage response.
#[derive(Deserialize)]
pub struct ResetCreditsSummary {
    pub available_count: i64,
    /// Credits redeemable right now. The backend reports this as zero until a
    /// rate-limit window is actually exhausted — there is nothing to reset
    /// before that, and redeeming would waste the credit.
    #[serde(default)]
    pub applicable_available_count: i64,
}

#[derive(Deserialize)]
pub struct ResetCreditsDetails {
    #[serde(default)]
    pub credits: Vec<ResetCredit>,
    #[serde(default)]
    pub available_count: i64,
}

#[derive(Deserialize, Clone)]
pub struct ResetCredit {
    pub id: String,
    pub status: String,
    #[allow(dead_code)]
    pub reset_type: Option<String>,
    #[allow(dead_code)]
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
    pub title: Option<String>,
    #[allow(dead_code)]
    pub description: Option<String>,
}

impl ResetCredit {
    /// Only `available` credits can be redeemed; the rest are already spent,
    /// in flight, or cooling down.
    pub fn is_available(&self) -> bool {
        self.status == "available"
    }

    pub fn expires_at_timestamp(&self) -> Option<i64> {
        parse_rfc3339(self.expires_at.as_deref()?)
    }
}

fn parse_rfc3339(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Outcome of redeeming a banked reset.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeResetCode {
    /// The credit was spent and the window(s) cleared.
    Reset,
    /// No exhausted window to clear, so no credit was spent.
    NothingToReset,
    /// The account holds no redeemable credit.
    NoCredit,
    /// This redemption request was already applied.
    AlreadyRedeemed,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub struct ConsumeResetResponse {
    pub code: ConsumeResetCode,
    #[serde(default)]
    pub windows_reset: i64,
}

const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

pub async fn fetch_reset_credits_async(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<ResetCreditsDetails> {
    let mut request = client.get(RESET_CREDITS_URL).bearer_auth(access_token);
    if let Some(account_id) = account_id {
        request = request.header("chatgpt-account-id", account_id);
    }

    let resp = request
        .send()
        .await
        .context("failed to reach reset credits API")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("expired");
    }
    if !status.is_success() {
        anyhow::bail!("reset credits API returned {status}");
    }

    resp.json::<ResetCreditsDetails>()
        .await
        .context("failed to parse reset credits response")
}

/// Redeem one banked reset, clearing the account's exhausted rate-limit window.
///
/// `redeem_request_id` is the idempotency key: retrying a timed-out redemption
/// with the same key returns `AlreadyRedeemed` instead of spending a second
/// credit, so callers must reuse it across retries of the *same* redemption.
pub async fn consume_reset_credit_async(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    redeem_request_id: &str,
    credit_id: Option<&str>,
) -> Result<ConsumeResetResponse> {
    let mut body = serde_json::json!({ "redeem_request_id": redeem_request_id });
    if let Some(credit_id) = credit_id {
        body["credit_id"] = serde_json::Value::String(credit_id.to_string());
    }

    let mut request = client
        .post(format!("{RESET_CREDITS_URL}/consume"))
        .bearer_auth(access_token)
        .json(&body);
    if let Some(account_id) = account_id {
        request = request.header("chatgpt-account-id", account_id);
    }

    let resp = request
        .send()
        .await
        .context("failed to reach reset credits API")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("expired");
    }
    if !status.is_success() {
        anyhow::bail!("reset credits API returned {status}");
    }

    resp.json::<ConsumeResetResponse>()
        .await
        .context("failed to parse reset redemption response")
}

/// A unique idempotency key for one redemption attempt.
pub fn new_redeem_request_id(alias: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("codexctl-{alias}-{nanos}")
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
