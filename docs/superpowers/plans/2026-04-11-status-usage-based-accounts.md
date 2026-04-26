# Usage-Based Account Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `codexctl status` into two tables — one for rate-limited accounts, one for usage-based accounts — each with columns meaningful to its billing model.

**Architecture:** Add `Credits` and `SpendControl` structs to `api.rs`, add `fetch_account_settings_async` for seat limits, split `AccountStatus` into a shared enum with rate-limited and usage-based variants, render each variant as its own table. CLI flags `--rate-limited` / `--usage-based` filter which tables are shown.

**Tech Stack:** Rust, clap (CLI args), comfy-table (rendering), reqwest (HTTP), serde (JSON), tokio (async)

---

### Task 1: Add Credits and SpendControl structs to api.rs

**Files:**

- Modify: `src/api.rs:27-31`
- Test: `tests/api_test.rs`

- [ ] **Step 1: Write test for parsing usage-based response with credits**

Add to `tests/api_test.rs`:

```rust
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
    assert_eq!(resp.plan_type.as_deref(), Some("self_serve_business_usage_based"));
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test api_test -- parse_usage_based parse_team_response_still_works 2>&1`
Expected: compilation error — `Credits` and `SpendControl` don't exist yet, and `RateLimitResponse` doesn't have those fields.

- [ ] **Step 3: Add Credits, SpendControl structs and update RateLimitResponse in api.rs**

In `src/api.rs`, add the new structs after the existing `RateLimitWindow` impl block (after line 72), and add fields to `RateLimitResponse`:

Replace the `RateLimitResponse` struct:

```rust
#[derive(Deserialize)]
pub struct RateLimitResponse {
    pub plan_type: Option<String>,
    pub rate_limit: Option<RateLimit>,
    pub credits: Option<Credits>,
    pub spend_control: Option<SpendControl>,
}
```

Add after `RateLimitWindow` impl:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test api_test 2>&1`
Expected: all tests pass, including the 3 new ones and all existing ones.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs tests/api_test.rs
git commit -m "feat: add Credits and SpendControl structs to api response"
```

---

### Task 2: Add account settings endpoint for seat credit limits

**Files:**

- Modify: `src/api.rs`
- Test: `tests/api_test.rs`

- [ ] **Step 1: Write test for parsing account settings response**

Add to `tests/api_test.rs`:

```rust
use codexctl::api::AccountSettings;

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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test api_test -- parse_account_settings 2>&1`
Expected: compilation error — `AccountSettings` doesn't exist.

- [ ] **Step 3: Add AccountSettings structs and fetch function to api.rs**

Add to `src/api.rs` after the `SpendControl` struct:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test api_test 2>&1`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs tests/api_test.rs
git commit -m "feat: add account settings endpoint for seat credit limits"
```

---

### Task 3: Add --rate-limited and --usage-based flags to CLI

**Files:**

- Modify: `src/main.rs`
- Modify: `src/commands/status.rs`
- Test: `tests/cli_test.rs`

- [ ] **Step 1: Write test for new CLI flags**

Add to `tests/cli_test.rs`:

```rust
#[test]
fn status_accepts_rate_limited_flag() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["status", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--rate-limited"));
    assert!(stdout.contains("--usage-based"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cli_test -- status_accepts_rate_limited_flag 2>&1`
Expected: FAIL — flags not in help output yet.

- [ ] **Step 3: Add flags to Status command in main.rs**

Replace the `Status` variant in the `Commands` enum in `src/main.rs`:

```rust
    /// Show rate limit status for all accounts
    Status {
        /// Show only rate-limited accounts
        #[arg(long, conflicts_with = "usage_based")]
        rate_limited: bool,
        /// Show only usage-based accounts
        #[arg(long, conflicts_with = "rate_limited")]
        usage_based: bool,
    },
```

Update the match arm in `main()`:

```rust
        Commands::Status { rate_limited, usage_based } => {
            let filter = if rate_limited {
                commands::status::Filter::RateLimited
            } else if usage_based {
                commands::status::Filter::UsageBased
            } else {
                commands::status::Filter::All
            };
            commands::status::run(filter)
        }
```

- [ ] **Step 4: Add Filter enum to status.rs and update run() signature**

At the top of `src/commands/status.rs`, add:

```rust
pub enum Filter {
    All,
    RateLimited,
    UsageBased,
}
```

Change `run()` signature to `pub fn run(filter: Filter) -> Result<()>`. For now, ignore the filter — just accept it so it compiles:

```rust
pub fn run(filter: Filter) -> Result<()> {
    let _ = &filter; // used in task 4
    // ... rest of existing body unchanged
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test cli_test 2>&1`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/commands/status.rs tests/cli_test.rs
git commit -m "feat: add --rate-limited and --usage-based flags to status command"
```

---

### Task 4: Split status into two tables with usage-based rendering

**Files:**

- Modify: `src/commands/status.rs`

This is the main task — rewrite `status.rs` to:

1. Parse credits/spend data from the API response
2. Fetch seat limits for usage-based accounts
3. Split accounts into two groups
4. Render each group as its own table
5. Apply the filter flag

- [ ] **Step 1: Rewrite AccountStatus to support both account types**

Replace the entire `src/commands/status.rs` with:

```rust
use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use crate::api;
use crate::profile;

pub enum Filter {
    All,
    RateLimited,
    UsageBased,
}

enum CreditsStatus {
    Ok,
    Unlimited,
    None,
    Overage,
}

struct RateLimitedAccount {
    alias: String,
    plan: String,
    h5_pct: Option<f64>,
    d7_pct: Option<f64>,
    h5_reset: String,
    d7_reset: String,
    d7_used_note: String,
    is_active: bool,
    is_error: bool,
    error_msg: String,
}

struct UsageBasedAccount {
    alias: String,
    plan: String,
    credit_balance: Option<String>,
    seat_limit_cents: Option<u64>,
    credits_status: CreditsStatus,
    spend_control_reached: bool,
    is_active: bool,
    is_error: bool,
    error_msg: String,
}

impl RateLimitedAccount {
    fn availability_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        let h5 = self.h5_pct.unwrap_or(0.0);
        let d7 = self.d7_pct.unwrap_or(0.0);
        if h5 >= 100.0 && d7 >= 100.0 {
            return 900.0;
        }
        if d7 >= 100.0 {
            return 700.0 + h5;
        }
        if h5 >= 100.0 {
            return 500.0 + d7;
        }
        h5 * 2.0 + d7
    }
}

impl UsageBasedAccount {
    fn health_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        match self.credits_status {
            CreditsStatus::None => 300.0,
            CreditsStatus::Overage => 200.0,
            _ if self.spend_control_reached => 100.0,
            _ => 0.0,
        }
    }
}

pub fn run(filter: Filter) -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

    let rt = tokio::runtime::Runtime::new()?;
    let (mut rate_limited, mut usage_based) =
        rt.block_on(fetch_and_split(&profiles, &active));

    rate_limited.sort_by(|a, b| {
        a.availability_score()
            .partial_cmp(&b.availability_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    usage_based.sort_by(|a, b| {
        a.health_score()
            .partial_cmp(&b.health_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let show_rl = matches!(filter, Filter::All | Filter::RateLimited);
    let show_ub = matches!(filter, Filter::All | Filter::UsageBased);

    if show_rl && !rate_limited.is_empty() {
        println!("Rate-Limited Accounts");
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec![
            "Account", "Plan", "5h Used", "5h Reset", "7d Used", "7d Reset", "Active",
        ]);
        for s in &rate_limited {
            table.add_row(render_rate_limited_row(s));
        }
        println!("{table}");
    }

    if show_rl && !rate_limited.is_empty() && show_ub && !usage_based.is_empty() {
        println!();
    }

    if show_ub && !usage_based.is_empty() {
        println!("Usage-Based Accounts");
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec![
            "Account", "Plan", "Balance", "Seat Limit", "Credits", "Spending", "Active",
        ]);
        for s in &usage_based {
            table.add_row(render_usage_based_row(s));
        }
        println!("{table}");
    }

    if (show_rl && rate_limited.is_empty() && !show_ub)
        || (show_ub && usage_based.is_empty() && !show_rl)
        || (rate_limited.is_empty() && usage_based.is_empty())
    {
        println!("no matching accounts found.");
    }

    Ok(())
}

fn is_usage_based_plan(plan: &str) -> bool {
    plan.contains("usage_based")
}

fn shorten_plan(plan: &str) -> String {
    match plan {
        "self_serve_business_usage_based" => "biz_usage".to_string(),
        "enterprise_cbp_usage_based" => "ent_usage".to_string(),
        other => other.to_string(),
    }
}

async fn fetch_and_split(
    profiles: &[profile::Profile],
    active: &Option<String>,
) -> (Vec<RateLimitedAccount>, Vec<UsageBasedAccount>) {
    let client = reqwest::Client::new();

    // Phase 1: fetch wham/usage for all accounts in parallel
    let usage_futures: Vec<_> = profiles
        .iter()
        .map(|p| {
            let client = client.clone();
            let alias = p.meta.alias.clone();
            let plan_from_meta = p.meta.plan.clone();
            let is_active = active.as_deref() == Some(&p.meta.alias);
            let auth = api::read_auth_json(&p.auth_json_path());

            async move {
                (alias, plan_from_meta, is_active, auth, {
                    match &auth {
                        Ok(a) => Some(api::fetch_usage_async(&client, &a.access_token).await),
                        Err(_) => None,
                    }
                })
            }
        })
        .collect();

    // Re-do: we need to handle auth outside the tuple because of borrow issues.
    // Simpler approach: collect (alias, is_active, Result<usage>) tuples.
    let futures: Vec<_> = profiles
        .iter()
        .map(|p| {
            let client = client.clone();
            let alias = p.meta.alias.clone();
            let plan_from_meta = p.meta.plan.clone();
            let is_active = active.as_deref() == Some(&p.meta.alias);
            let auth = api::read_auth_json(&p.auth_json_path());

            async move {
                let usage_result = match &auth {
                    Ok(a) => Some(api::fetch_usage_async(&client, &a.access_token).await),
                    Err(_) => None,
                };
                (alias, plan_from_meta, is_active, auth, usage_result)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    // Phase 2: classify and build account structs
    let mut rate_limited = Vec::new();
    let mut usage_based = Vec::new();
    // Collect usage-based accounts that need seat limit lookups
    let mut ub_needing_settings: Vec<(usize, String, String)> = Vec::new(); // (index, access_token, account_id)

    for (alias, plan_from_meta, is_active, auth, usage_result) in &results {
        // Auth failure
        let auth = match auth {
            Ok(a) => a,
            Err(_) => {
                let is_ub = plan_from_meta
                    .as_deref()
                    .is_some_and(|p| is_usage_based_plan(p));
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        plan: shorten_plan(plan_from_meta.as_deref().unwrap_or("-")),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: "bad auth.json".to_string(),
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        plan: "-".to_string(),
                        h5_pct: None,
                        d7_pct: None,
                        h5_reset: "-".to_string(),
                        d7_reset: "-".to_string(),
                        d7_used_note: String::new(),
                        is_active: *is_active,
                        is_error: true,
                        error_msg: "bad auth.json".to_string(),
                    });
                }
                continue;
            }
        };

        // Usage fetch failure
        let usage = match usage_result {
            Some(Ok(u)) => u,
            Some(Err(e)) => {
                let msg = if e.to_string().contains("expired") {
                    "expired"
                } else {
                    "error"
                };
                let is_ub = plan_from_meta
                    .as_deref()
                    .is_some_and(|p| is_usage_based_plan(p));
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        plan: shorten_plan(plan_from_meta.as_deref().unwrap_or("-")),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: msg.to_string(),
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        plan: "-".to_string(),
                        h5_pct: None,
                        d7_pct: None,
                        h5_reset: "-".to_string(),
                        d7_reset: "-".to_string(),
                        d7_used_note: String::new(),
                        is_active: *is_active,
                        is_error: true,
                        error_msg: msg.to_string(),
                    });
                }
                continue;
            }
            None => continue,
        };

        // Update plan in meta
        if let Some(plan) = &usage.plan_type {
            let _ = profile::update_meta_plan(alias, plan);
        }

        let plan_str = usage.plan_type.as_deref().unwrap_or("-");
        let is_ub = usage.rate_limit.is_none() && is_usage_based_plan(plan_str);

        if is_ub {
            let credits = &usage.credits;
            let credits_status = match credits {
                Some(c) if c.unlimited => CreditsStatus::Unlimited,
                Some(c) if c.overage_limit_reached => CreditsStatus::Overage,
                Some(c) if c.has_credits => CreditsStatus::Ok,
                _ => CreditsStatus::None,
            };
            let credit_balance = credits.as_ref().and_then(|c| c.balance.clone());
            let spend_control_reached = usage
                .spend_control
                .as_ref()
                .is_some_and(|sc| sc.reached);

            let idx = usage_based.len();
            usage_based.push(UsageBasedAccount {
                alias: alias.clone(),
                plan: shorten_plan(plan_str),
                credit_balance,
                seat_limit_cents: None, // filled in phase 3
                credits_status,
                spend_control_reached,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });

            // We need the account_id to fetch settings. Extract from JWT or use a known one.
            // For now, we'll try to extract account_id from the accounts/check endpoint.
            // Simpler: parse the JWT access_token to get chatgpt_account_id claim.
            if let Some(account_id) = extract_account_id(&auth.access_token) {
                ub_needing_settings.push((idx, auth.access_token.clone(), account_id));
            }
        } else {
            let primary = usage.rate_limit.as_ref().and_then(|r| r.primary());
            let secondary = usage.rate_limit.as_ref().and_then(|r| r.secondary());
            let h5_pct = primary.map(|w| w.used_percent);
            let d7_pct = secondary.map(|w| w.used_percent);
            let (_, h5_reset, _) = format_window(primary);
            let (_, d7_reset, d7_used_note) = format_window(secondary);

            rate_limited.push(RateLimitedAccount {
                alias: alias.clone(),
                plan: plan_str.to_string(),
                h5_pct,
                d7_pct,
                h5_reset,
                d7_reset,
                d7_used_note,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });
        }
    }

    // Phase 3: fetch seat limits for usage-based accounts (deduplicate by account_id)
    use std::collections::HashMap;
    let mut unique_account_ids: HashMap<String, (String, String)> = HashMap::new(); // account_id -> (access_token, account_id)
    for (_, token, account_id) in &ub_needing_settings {
        unique_account_ids
            .entry(account_id.clone())
            .or_insert_with(|| (token.clone(), account_id.clone()));
    }

    let settings_futures: Vec<_> = unique_account_ids
        .values()
        .map(|(token, account_id)| {
            let client = client.clone();
            let token = token.clone();
            let account_id = account_id.clone();
            async move {
                let result =
                    api::fetch_account_settings_async(&client, &token, &account_id).await;
                (account_id, result)
            }
        })
        .collect();

    let settings_results = futures::future::join_all(settings_futures).await;
    let mut settings_map: HashMap<String, u64> = HashMap::new();
    for (account_id, result) in settings_results {
        if let Ok(settings) = result {
            if let Some(limits) = settings.seat_type_credit_limits {
                if let Some(ub_limits) = limits.usage_based {
                    if let Some(first) = ub_limits.first() {
                        settings_map.insert(account_id, first.limit);
                    }
                }
            }
        }
    }

    // Apply seat limits to usage-based accounts
    for (idx, _, account_id) in &ub_needing_settings {
        if let Some(limit) = settings_map.get(account_id) {
            usage_based[*idx].seat_limit_cents = Some(*limit);
        }
    }

    (rate_limited, usage_based)
}

/// Extract account_id from JWT access_token claims.
/// JWT is base64url(header).base64url(payload).signature
/// The payload contains "https://api.openai.com/auth" -> "chatgpt_account_id"
fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // base64url decode the payload
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = engine.decode(parts[1]).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(|s| s.to_string())
}

fn render_rate_limited_row(s: &RateLimitedAccount) -> Vec<Cell> {
    let active_marker = if s.is_active { "*" } else { "" };

    if s.is_error {
        return vec![
            Cell::new(&s.alias),
            Cell::new("-"),
            Cell::new(&s.error_msg).fg(Color::Red),
            Cell::new("-"),
            Cell::new(&s.error_msg).fg(Color::Red),
            Cell::new("-"),
            Cell::new(active_marker),
        ];
    }

    let h5_str = s
        .h5_pct
        .map(|p| format!("{:.0}%", p))
        .unwrap_or_else(|| "-".to_string());
    let d7_str = s
        .d7_pct
        .map(|p| format!("{:.0}%", p))
        .unwrap_or_else(|| "-".to_string());

    let d7_reset_str = if s.d7_used_note.is_empty() {
        s.d7_reset.clone()
    } else {
        format!("{} ({})", s.d7_reset, s.d7_used_note)
    };

    vec![
        Cell::new(&s.alias),
        Cell::new(&s.plan),
        colorize_usage(&h5_str),
        Cell::new(&s.h5_reset),
        colorize_usage(&d7_str),
        Cell::new(&d7_reset_str),
        Cell::new(active_marker),
    ]
}

fn render_usage_based_row(s: &UsageBasedAccount) -> Vec<Cell> {
    let active_marker = if s.is_active { "*" } else { "" };

    if s.is_error {
        return vec![
            Cell::new(&s.alias),
            Cell::new(&s.plan),
            Cell::new(&s.error_msg).fg(Color::Red),
            Cell::new("-"),
            Cell::new(&s.error_msg).fg(Color::Red),
            Cell::new("-"),
            Cell::new(active_marker),
        ];
    }

    let balance_str = s
        .credit_balance
        .as_deref()
        .map(|b| format!("${b}"))
        .unwrap_or_else(|| "-".to_string());

    let seat_limit_str = s
        .seat_limit_cents
        .map(|c| format!("${}", c / 100))
        .unwrap_or_else(|| "-".to_string());

    let (credits_str, credits_color) = match s.credits_status {
        CreditsStatus::Ok => ("ok", Color::Green),
        CreditsStatus::Unlimited => ("unlimited", Color::Cyan),
        CreditsStatus::None => ("none", Color::Red),
        CreditsStatus::Overage => ("overage", Color::Red),
    };

    let (spend_str, spend_color) = if s.spend_control_reached {
        ("limit", Color::Red)
    } else {
        ("ok", Color::Green)
    };

    vec![
        Cell::new(&s.alias),
        Cell::new(&s.plan),
        Cell::new(&balance_str),
        Cell::new(&seat_limit_str),
        Cell::new(credits_str).fg(credits_color),
        Cell::new(spend_str).fg(spend_color),
        Cell::new(active_marker),
    ]
}

fn format_window(window: Option<&api::RateLimitWindow>) -> (String, String, String) {
    match window {
        Some(w) => {
            let used = format!("{:.0}%", w.used_percent);
            match w.reset_timestamp() {
                Some(reset_ts) => {
                    let now = chrono::Utc::now().timestamp();
                    let diff_secs = reset_ts - now;
                    let reset = if diff_secs <= 0 {
                        "now".to_string()
                    } else {
                        format_duration(diff_secs)
                    };
                    let dt = chrono::DateTime::from_timestamp(reset_ts, 0)
                        .map(|dt| {
                            let local = dt.with_timezone(&chrono::Local);
                            local.format("%a %b %d %H:%M").to_string()
                        })
                        .unwrap_or_default();
                    (used, format!("in {reset}"), dt)
                }
                None => (used, "-".to_string(), String::new()),
            }
        }
        None => ("-".to_string(), "-".to_string(), String::new()),
    }
}

fn format_duration(secs: i64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn colorize_usage(usage_str: &str) -> Cell {
    let pct: f64 = usage_str.trim_end_matches('%').parse().unwrap_or(0.0);
    let color = if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    Cell::new(usage_str).fg(color)
}
```

- [ ] **Step 2: Add base64 dependency to Cargo.toml**

Add `base64 = "0.22"` to `[dependencies]` in `Cargo.toml`.

- [ ] **Step 3: Build and verify compilation**

Run: `cargo build 2>&1`
Expected: successful compilation with no errors.

- [ ] **Step 4: Run all existing tests**

Run: `cargo test 2>&1`
Expected: all tests pass (existing api_test, cli_test, profile_test, config_test).

- [ ] **Step 5: Manual smoke test**

Run: `cargo run -- status 2>&1`
Expected: two tables printed. Rate-limited table has all team accounts. Usage-based table has `amir+ezra` and `amir+reviewer` with `biz_usage` plan, credits status, seat limit.

Run: `cargo run -- status --rate-limited 2>&1`
Expected: only the rate-limited table.

Run: `cargo run -- status --usage-based 2>&1`
Expected: only the usage-based table.

- [ ] **Step 6: Commit**

```bash
git add src/commands/status.rs Cargo.toml Cargo.lock
git commit -m "feat: split status into rate-limited and usage-based tables"
```

---

### Task 5: Cleanup and final verification

**Files:**

- All modified files

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1`
Expected: all tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy 2>&1`
Expected: no warnings.

- [ ] **Step 3: Run rustfmt**

Run: `cargo fmt --check 2>&1`
Expected: no formatting issues.

- [ ] **Step 4: Final smoke test of all three modes**

Run: `cargo run -- status 2>&1`
Run: `cargo run -- status --rate-limited 2>&1`
Run: `cargo run -- status --usage-based 2>&1`
Run: `cargo run -- status --rate-limited --usage-based 2>&1` (should error: conflicting flags)

- [ ] **Step 5: Commit any fixes**

Only if steps 1-3 required changes:

```bash
git add -A
git commit -m "fix: clippy and fmt fixes for status command"
```
