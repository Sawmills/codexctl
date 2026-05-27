use std::collections::HashMap;

use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use crate::api;
use crate::config;
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
    h5_pct: Option<f64>,
    d7_pct: Option<f64>,
    h5_reset: String,
    d7_reset: String,
    token_expiry: Option<i64>,
    is_active: bool,
    is_error: bool,
    error_msg: String,
}

struct UsageBasedAccount {
    alias: String,
    credit_balance: Option<String>,
    seat_limit_cents: Option<u64>,
    credits_status: CreditsStatus,
    spend_control_reached: bool,
    token_expiry: Option<i64>,
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
    let (rate_limited, usage_based, fetched_at) = load_sorted_statuses()?;

    let show_rl = matches!(filter, Filter::All | Filter::RateLimited);
    let show_ub = matches!(filter, Filter::All | Filter::UsageBased);
    let has_rows = (show_rl && !rate_limited.is_empty()) || (show_ub && !usage_based.is_empty());

    if has_rows {
        print_live_fetched_at(fetched_at);
    }

    if show_rl {
        let rate_limited_refs: Vec<&RateLimitedAccount> = rate_limited.iter().collect();
        print_rate_limited_table("Rate-Limited Accounts", &rate_limited_refs);
    }

    if show_rl && !rate_limited.is_empty() && show_ub && !usage_based.is_empty() {
        println!();
    }

    if show_ub {
        let usage_based_refs: Vec<&UsageBasedAccount> = usage_based.iter().collect();
        print_usage_based_table("Usage-Based Accounts", &usage_based_refs);
    }

    if (show_rl && rate_limited.is_empty() && !show_ub)
        || (show_ub && usage_based.is_empty() && !show_rl)
        || (rate_limited.is_empty() && usage_based.is_empty())
    {
        println!("no matching accounts found.");
    }

    Ok(())
}

pub fn run_focused(focused_alias: &str) -> Result<()> {
    let (rate_limited, usage_based, fetched_at) = load_sorted_statuses()?;
    if !rate_limited.is_empty() || !usage_based.is_empty() {
        print_live_fetched_at(fetched_at);
    }

    let selected_rate_limited: Vec<&RateLimitedAccount> = rate_limited
        .iter()
        .filter(|account| account.alias == focused_alias)
        .collect();
    let selected_usage_based: Vec<&UsageBasedAccount> = usage_based
        .iter()
        .filter(|account| account.alias == focused_alias)
        .collect();

    let mut printed_selected =
        print_rate_limited_table("Selected Rate-Limited Account", &selected_rate_limited);
    if printed_selected && !selected_usage_based.is_empty() {
        println!();
    }
    printed_selected |=
        print_usage_based_table("Selected Usage-Based Account", &selected_usage_based);

    if !printed_selected {
        println!("selected account status unavailable: {focused_alias}");
    }

    let other_rate_limited: Vec<&RateLimitedAccount> = rate_limited
        .iter()
        .filter(|account| account.alias != focused_alias)
        .collect();
    let other_usage_based: Vec<&UsageBasedAccount> = usage_based
        .iter()
        .filter(|account| account.alias != focused_alias)
        .collect();

    if !other_rate_limited.is_empty() || !other_usage_based.is_empty() {
        println!();
        println!("Other Accounts");
        let printed_rate_limited =
            print_rate_limited_table("Rate-Limited Accounts", &other_rate_limited);
        if printed_rate_limited && !other_usage_based.is_empty() {
            println!();
        }
        print_usage_based_table("Usage-Based Accounts", &other_usage_based);
    }

    Ok(())
}

fn load_sorted_statuses() -> Result<(
    Vec<RateLimitedAccount>,
    Vec<UsageBasedAccount>,
    chrono::DateTime<chrono::Utc>,
)> {
    let profiles = profile::list_profiles()?;
    let fetched_at = chrono::Utc::now();
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok((Vec::new(), Vec::new(), fetched_at));
    }

    let active = profile::get_active()?;
    let codex_auth = config::codex_auth_json()?;

    let rt = tokio::runtime::Runtime::new()?;
    let (mut rate_limited, mut usage_based) =
        rt.block_on(fetch_and_split(&profiles, &active, &codex_auth));

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

    Ok((rate_limited, usage_based, fetched_at))
}

fn print_live_fetched_at(fetched_at: chrono::DateTime<chrono::Utc>) {
    let local = fetched_at.with_timezone(&chrono::Local);
    println!(
        "Live status fetched at {}",
        local.format("%a %b %d %H:%M:%S")
    );
    println!();
}

fn print_rate_limited_table(title: &str, accounts: &[&RateLimitedAccount]) -> bool {
    if accounts.is_empty() {
        return false;
    }

    println!("{title}");
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Account", "5h", "5h Reset", "7d", "7d Reset", "Token"]);
    for account in accounts {
        table.add_row(render_rate_limited_row(account));
    }
    println!("{table}");
    true
}

fn print_usage_based_table(title: &str, accounts: &[&UsageBasedAccount]) -> bool {
    if accounts.is_empty() {
        return false;
    }

    println!("{title}");
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec![
        "Account", "Balance", "Seat", "Credits", "Spend", "Token",
    ]);
    for account in accounts {
        table.add_row(render_usage_based_row(account));
    }
    println!("{table}");
    true
}

fn is_usage_based_plan(plan: &str) -> bool {
    plan.contains("usage_based")
}

async fn fetch_and_split(
    profiles: &[profile::Profile],
    active: &Option<String>,
    codex_auth_path: &std::path::Path,
) -> (Vec<RateLimitedAccount>, Vec<UsageBasedAccount>) {
    let client = reqwest::Client::new();

    // Phase 1: fetch wham/usage for all accounts in parallel
    let futures: Vec<_> = profiles
        .iter()
        .map(|p| {
            let client = client.clone();
            let alias = p.meta.alias.clone();
            let plan_from_meta = p.meta.plan.clone();
            let is_active = active.as_deref() == Some(&p.meta.alias);
            // The active profile's live, Codex-maintained tokens are the source of
            // truth; the stored snapshot can be stale until the next switch/save.
            let auth_path = if is_active {
                codex_auth_path.to_path_buf()
            } else {
                p.auth_json_path()
            };
            let auth = api::read_auth_json(&auth_path);

            async move {
                let usage_result = match &auth {
                    Ok(a) => Some(
                        api::fetch_usage_async(&client, &a.access_token, a.account_id.as_deref())
                            .await,
                    ),
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
    let mut ub_needing_settings: Vec<(usize, String, String)> = Vec::new();

    for (alias, plan_from_meta, is_active, auth, usage_result) in &results {
        let account_id = auth.as_ref().ok().and_then(|a| a.account_id.clone());
        let auth = match auth {
            Ok(a) => a,
            Err(_) => {
                let is_ub = plan_from_meta.as_deref().is_some_and(is_usage_based_plan);
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        token_expiry: None,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: "bad auth.json".to_string(),
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        h5_pct: None,
                        d7_pct: None,
                        h5_reset: "-".to_string(),
                        d7_reset: "-".to_string(),
                        token_expiry: None,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: "bad auth.json".to_string(),
                    });
                }
                continue;
            }
        };

        let token_expiry = api::token_expiry(&auth.access_token);

        let usage = match usage_result {
            Some(Ok(u)) => u,
            Some(Err(e)) => {
                let msg = if e.to_string().contains("expired") {
                    auth_failure_label(&auth.access_token)
                } else {
                    "error"
                };
                let is_ub = plan_from_meta.as_deref().is_some_and(is_usage_based_plan);
                if is_ub {
                    usage_based.push(UsageBasedAccount {
                        alias: alias.clone(),
                        credit_balance: None,
                        seat_limit_cents: None,
                        credits_status: CreditsStatus::None,
                        spend_control_reached: false,
                        token_expiry,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: msg.to_string(),
                    });
                } else {
                    rate_limited.push(RateLimitedAccount {
                        alias: alias.clone(),
                        h5_pct: None,
                        d7_pct: None,
                        h5_reset: "-".to_string(),
                        d7_reset: "-".to_string(),
                        token_expiry,
                        is_active: *is_active,
                        is_error: true,
                        error_msg: msg.to_string(),
                    });
                }
                continue;
            }
            None => continue,
        };

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
            let spend_control_reached = usage.spend_control.as_ref().is_some_and(|sc| sc.reached);

            let idx = usage_based.len();
            usage_based.push(UsageBasedAccount {
                alias: alias.clone(),
                credit_balance,
                seat_limit_cents: None,
                credits_status,
                spend_control_reached,
                token_expiry,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });

            if let Some(account_id) =
                account_id.or_else(|| api::extract_account_id(&auth.access_token))
            {
                ub_needing_settings.push((idx, auth.access_token.clone(), account_id));
            }
        } else {
            let primary = usage.rate_limit.as_ref().and_then(|r| r.primary());
            let secondary = usage.rate_limit.as_ref().and_then(|r| r.secondary());
            let h5_pct = primary.map(|w| w.used_percent);
            let d7_pct = secondary.map(|w| w.used_percent);
            let h5_reset = format_window_reset(primary);
            let d7_reset = format_window_reset(secondary);

            rate_limited.push(RateLimitedAccount {
                alias: alias.clone(),
                h5_pct,
                d7_pct,
                h5_reset,
                d7_reset,
                token_expiry,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });
        }
    }

    // Phase 3: fetch seat limits for usage-based accounts (deduplicate by account_id)
    let mut unique_account_ids: HashMap<String, (String, String)> = HashMap::new();
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
                let result = api::fetch_account_settings_async(&client, &token, &account_id).await;
                (account_id, result)
            }
        })
        .collect();

    let settings_results = futures::future::join_all(settings_futures).await;
    let mut settings_map: HashMap<String, u64> = HashMap::new();
    for (account_id, result) in settings_results {
        if let Ok(settings) = result
            && let Some(limits) = settings.seat_type_credit_limits
            && let Some(ub_limits) = limits.usage_based
            && let Some(first) = ub_limits.first()
        {
            settings_map.insert(account_id, first.limit);
        }
    }

    for (idx, _, account_id) in &ub_needing_settings {
        if let Some(limit) = settings_map.get(account_id) {
            usage_based[*idx].seat_limit_cents = Some(*limit);
        }
    }

    (rate_limited, usage_based)
}

fn render_rate_limited_row(s: &RateLimitedAccount) -> Vec<Cell> {
    let alias = display_alias(&s.alias, s.is_active);

    if s.is_error {
        return vec![
            Cell::new(alias),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            token_cell(s.token_expiry, true, &s.error_msg),
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

    vec![
        Cell::new(alias),
        colorize_usage(&h5_str),
        Cell::new(&s.h5_reset),
        colorize_usage(&d7_str),
        Cell::new(&s.d7_reset),
        token_cell(s.token_expiry, false, &s.error_msg),
    ]
}

fn render_usage_based_row(s: &UsageBasedAccount) -> Vec<Cell> {
    let alias = display_alias(&s.alias, s.is_active);

    if s.is_error {
        return vec![
            Cell::new(alias),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new("-"),
            token_cell(s.token_expiry, true, &s.error_msg),
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
        Cell::new(alias),
        Cell::new(&balance_str),
        Cell::new(&seat_limit_str),
        Cell::new(credits_str).fg(credits_color),
        Cell::new(spend_str).fg(spend_color),
        token_cell(s.token_expiry, false, &s.error_msg),
    ]
}

/// The "Token" column: how long the stored access token is good for without a
/// re-login, or — for an errored row — what went wrong. An `invalidated` value
/// means the JWT still looks valid but OpenAI revoked the grant server-side (a
/// sibling seat was logged in), so the remaining lifetime would be misleading.
fn token_cell(token_expiry: Option<i64>, is_error: bool, error_msg: &str) -> Cell {
    if is_error {
        return Cell::new(error_msg).fg(Color::Red);
    }
    match token_expiry {
        None => Cell::new("-"),
        Some(exp) => {
            let diff = exp - chrono::Utc::now().timestamp();
            if diff <= 0 {
                return Cell::new("expired").fg(Color::Red);
            }
            let color = if diff >= 86400 {
                Color::Green
            } else if diff >= 3600 {
                Color::Yellow
            } else {
                Color::Red
            };
            Cell::new(format_duration(diff)).fg(color)
        }
    }
}

fn auth_failure_label(access_token: &str) -> &'static str {
    if api::is_token_expired(access_token) {
        "expired"
    } else {
        "invalidated"
    }
}

fn display_alias(alias: &str, is_active: bool) -> String {
    if is_active {
        format!("* {alias}")
    } else {
        alias.to_string()
    }
}

fn format_window_reset(window: Option<&api::RateLimitWindow>) -> String {
    match window {
        Some(w) => match w.reset_timestamp() {
            Some(reset_ts) => {
                let now = chrono::Utc::now().timestamp();
                let diff_secs = reset_ts - now;
                if diff_secs <= 0 {
                    "now".to_string()
                } else if diff_secs >= 86400 {
                    format!(
                        "in {} ({})",
                        format_duration(diff_secs),
                        format_reset_timestamp(reset_ts)
                    )
                } else {
                    format!("in {}", format_duration(diff_secs))
                }
            }
            None => "-".to_string(),
        },
        None => "-".to_string(),
    }
}

fn format_reset_timestamp(reset_ts: i64) -> String {
    chrono::DateTime::from_timestamp(reset_ts, 0)
        .map(|dt| {
            let local = dt.with_timezone(&chrono::Local);
            local.format("%a %b %d %H:%M").to_string()
        })
        .unwrap_or_else(|| "-".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

    #[test]
    fn auth_failure_label_reports_invalidated_when_token_is_not_time_expired() {
        let token = format!("{JWT_HDR}.eyJleHAiOjk5OTk5OTk5OTl9.sig");

        assert_eq!(auth_failure_label(&token), "invalidated");
    }

    #[test]
    fn auth_failure_label_reports_expired_when_exp_claim_is_past() {
        let token = format!("{JWT_HDR}.eyJleHAiOjEwMDAwMDAwMDB9.sig");

        assert_eq!(auth_failure_label(&token), "expired");
    }

    #[test]
    fn render_rate_limited_row_has_expected_column_count() {
        let account = RateLimitedAccount {
            alias: "amir+8@sawmills.ai".to_string(),
            h5_pct: Some(10.0),
            d7_pct: Some(20.0),
            h5_reset: "in 1h 00m".to_string(),
            d7_reset: "in 1d 00h".to_string(),
            token_expiry: None,
            is_active: false,
            is_error: false,
            error_msg: String::new(),
        };

        assert_eq!(render_rate_limited_row(&account).len(), 6);
    }

    #[test]
    fn render_usage_based_row_has_expected_column_count() {
        let account = UsageBasedAccount {
            alias: "amir+11@sawmills.ai".to_string(),
            credit_balance: Some("10.00".to_string()),
            seat_limit_cents: Some(2000),
            credits_status: CreditsStatus::Ok,
            spend_control_reached: false,
            token_expiry: None,
            is_active: false,
            is_error: false,
            error_msg: String::new(),
        };

        assert_eq!(render_usage_based_row(&account).len(), 6);
    }
}
