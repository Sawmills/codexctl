use std::collections::HashMap;

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
    let (mut rate_limited, mut usage_based) = rt.block_on(fetch_and_split(&profiles, &active));

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
    let mut ub_needing_settings: Vec<(usize, String, String)> = Vec::new();

    for (alias, plan_from_meta, is_active, auth, usage_result) in &results {
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
                seat_limit_cents: None,
                credits_status,
                spend_control_reached,
                is_active: *is_active,
                is_error: false,
                error_msg: String::new(),
            });

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

    for (idx, _, account_id) in &ub_needing_settings {
        if let Some(limit) = settings_map.get(account_id) {
            usage_based[*idx].seat_limit_cents = Some(*limit);
        }
    }

    (rate_limited, usage_based)
}

/// Extract account_id from JWT access_token claims.
fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
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
