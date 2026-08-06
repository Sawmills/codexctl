use anyhow::{Context, Result};
use serde::Deserialize;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
    /// Extra feature or model buckets returned alongside the main Codex limit.
    #[serde(default)]
    pub additional_rate_limits: Vec<AdditionalRateLimit>,
    /// Banked rate-limit reset credits, when the plan has any.
    pub rate_limit_reset_credits: Option<ResetCreditsSummary>,
}

impl RateLimitResponse {
    /// Classify the account before any automatic selection can spend credits.
    pub fn billing_class(&self) -> BillingClass {
        let plan = self.plan_type.as_deref();
        if plan.is_some_and(|plan| plan.contains("usage_based")) {
            return BillingClass::UsageBased;
        }
        let has_credit_billing = self.credits.as_ref().is_some_and(|credits| {
            credits.has_credits || credits.unlimited || credits.overage_limit_reached
        });
        if self.rate_limit.as_ref().is_some_and(RateLimit::has_window) {
            // A new plan name or mixed rate-limit and credit evidence is not
            // proof that automatic use is free. Keep it out of selection until
            // its contract is understood and added deliberately.
            if has_credit_billing || !plan.is_some_and(is_known_rate_limited_plan) {
                return BillingClass::Unknown;
            }
            return BillingClass::RateLimited;
        }
        if has_credit_billing {
            return BillingClass::UsageBased;
        }
        BillingClass::Unknown
    }

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

fn is_known_rate_limited_plan(plan: &str) -> bool {
    matches!(
        plan,
        "free" | "go" | "plus" | "pro" | "team" | "business" | "enterprise" | "edu"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingClass {
    RateLimited,
    UsageBased,
    Unknown,
}

#[derive(Deserialize)]
pub struct AdditionalRateLimit {
    pub limit_name: Option<String>,
    pub metered_feature: Option<String>,
    pub rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
pub struct RateLimit {
    // API returns both naming conventions depending on plan
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub primary_window: Option<RateLimitWindow>,
    pub secondary_window: Option<RateLimitWindow>,
}

/// A single window at or under this duration is the short-term window. A single
/// longer window is the long-term window. When the server returns two windows,
/// their declared durations determine which is shorter and which is longer.
const SHORT_WINDOW_MAX_SECONDS: u64 = 24 * 60 * 60;

impl RateLimit {
    fn has_window(&self) -> bool {
        self.first().is_some() || self.second().is_some()
    }

    fn first(&self) -> Option<&RateLimitWindow> {
        self.primary.as_ref().or(self.primary_window.as_ref())
    }

    fn second(&self) -> Option<&RateLimitWindow> {
        self.secondary.as_ref().or(self.secondary_window.as_ref())
    }

    /// Every server-returned window, with `0` for primary and `1` for
    /// secondary. Keep the slot before filtering so a secondary-only response
    /// cannot be mislabeled as primary.
    pub fn windows(&self) -> impl Iterator<Item = (usize, &RateLimitWindow)> {
        [(0, self.first()), (1, self.second())]
            .into_iter()
            .filter_map(|(position, window)| window.map(|window| (position, window)))
    }

    /// The shorter-term window.
    ///
    /// Windows are matched by their declared duration rather than by position:
    /// a weekly-only plan can return its window in the `primary_window` slot,
    /// while newer contracts can return two sub-day windows. Windows with no
    /// declared duration fall back to the historical positional reading.
    pub fn short_window(&self) -> Option<&RateLimitWindow> {
        match (self.first(), self.second()) {
            (Some(first), Some(second)) => {
                match (first.duration_seconds(), second.duration_seconds()) {
                    (Some(first_seconds), Some(second_seconds)) => {
                        Some(if first_seconds <= second_seconds {
                            first
                        } else {
                            second
                        })
                    }
                    (Some(seconds), None) => (seconds <= SHORT_WINDOW_MAX_SECONDS).then_some(first),
                    (None, Some(seconds)) => {
                        if seconds <= SHORT_WINDOW_MAX_SECONDS {
                            Some(second)
                        } else {
                            Some(first)
                        }
                    }
                    (None, None) => Some(first),
                }
            }
            (Some(window), None) => window.duration_seconds().map_or(Some(window), |seconds| {
                (seconds <= SHORT_WINDOW_MAX_SECONDS).then_some(window)
            }),
            (None, Some(window)) => window
                .duration_seconds()
                .and_then(|seconds| (seconds <= SHORT_WINDOW_MAX_SECONDS).then_some(window)),
            (None, None) => None,
        }
    }

    /// The longer-term window. See [`RateLimit::short_window`] for matching.
    pub fn long_window(&self) -> Option<&RateLimitWindow> {
        match (self.first(), self.second()) {
            (Some(first), Some(second)) => {
                match (first.duration_seconds(), second.duration_seconds()) {
                    (Some(first_seconds), Some(second_seconds)) => {
                        Some(if first_seconds > second_seconds {
                            first
                        } else {
                            second
                        })
                    }
                    (Some(seconds), None) => {
                        if seconds > SHORT_WINDOW_MAX_SECONDS {
                            Some(first)
                        } else {
                            Some(second)
                        }
                    }
                    (None, Some(seconds)) => (seconds > SHORT_WINDOW_MAX_SECONDS).then_some(second),
                    (None, None) => Some(second),
                }
            }
            (Some(window), None) => window
                .duration_seconds()
                .and_then(|seconds| (seconds > SHORT_WINDOW_MAX_SECONDS).then_some(window)),
            (None, Some(window)) => window.duration_seconds().map_or(Some(window), |seconds| {
                (seconds > SHORT_WINDOW_MAX_SECONDS).then_some(window)
            }),
            (None, None) => None,
        }
    }

    /// Availability score used by status sorting and automatic selection.
    /// Exhausting either returned window makes the account unavailable.
    pub fn availability_score(&self) -> f64 {
        let short = self
            .short_window()
            .map_or(0.0, |window| window.used_percent);
        let long = self.long_window().map_or(0.0, |window| window.used_percent);
        let score = if short >= 100.0 && long >= 100.0 {
            900.0
        } else if long >= 100.0 {
            700.0 + short
        } else if short >= 100.0 {
            500.0 + long
        } else {
            short * 2.0 + long
        };
        if self
            .windows()
            .any(|(_, window)| window.used_percent >= 100.0)
        {
            score.max(500.0)
        } else {
            score
        }
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

    /// A compact label derived from the server-declared window duration.
    pub fn duration_label(&self) -> Option<String> {
        self.duration_seconds().map(format_duration_label)
    }
}

fn format_duration_label(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if seconds.is_multiple_of(DAY) {
        format!("{}d", seconds / DAY)
    } else if seconds.is_multiple_of(HOUR) {
        format!("{}h", seconds / HOUR)
    } else if seconds.is_multiple_of(MINUTE) {
        format!("{}m", seconds / MINUTE)
    } else {
        format!("{seconds}s")
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

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

pub fn blocking_http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to build blocking HTTP client")
}

pub fn fetch_usage(access_token: &str, account_id: Option<&str>) -> Result<RateLimitResponse> {
    let client = blocking_http_client()?;
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

const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const PROFILE_CLAIM: &str = "https://api.openai.com/profile";

/// What a Codex access token asserts about the account behind it.
///
/// Every field is read from the token's own claims, so resolving an identity
/// needs no network call and still works once the token has expired — which is
/// precisely when a profile most needs to stay identifiable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    pub email: Option<String>,
    pub name: Option<String>,
    /// `chatgpt_account_id`: the workspace. Two profiles for one login differ here.
    pub account_id: Option<String>,
    /// `chatgpt_user_id`: the login. Two seats for one human share this.
    pub user_id: Option<String>,
    pub plan: Option<String>,
}

/// Read the identity claims out of an access token. `None` only when the token
/// is not a decodable JWT; a decodable token missing every claim yields an
/// empty identity so callers keep a single code path.
pub fn token_identity(token: &str) -> Option<TokenIdentity> {
    let value = decode_jwt_payload(token)?;
    let profile = value.get(PROFILE_CLAIM);
    let auth = value.get(AUTH_CLAIM);
    Some(TokenIdentity {
        email: string_claim(profile, "email"),
        name: string_claim(profile, "name"),
        account_id: string_claim(auth, "chatgpt_account_id")
            .or_else(|| string_claim(auth, "account_id")),
        user_id: string_claim(auth, "chatgpt_user_id"),
        plan: string_claim(auth, "chatgpt_plan_type"),
    })
}

fn string_claim(object: Option<&serde_json::Value>, key: &str) -> Option<String> {
    object?.get(key)?.as_str().map(str::to_string)
}

/// Extract account_id from JWT access_token claims when auth.json does not store it directly.
pub fn extract_account_id(token: &str) -> Option<String> {
    token_identity(token)?.account_id
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
