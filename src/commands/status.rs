use anyhow::Result;
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use crate::api;
use crate::profile;

struct AccountStatus {
    alias: String,
    plan: String,
    h5_pct: Option<f64>,
    d7_pct: Option<f64>,
    h5_reset: String,
    d7_reset: String,
    is_active: bool,
    is_error: bool,
    error_msg: String,
}

impl AccountStatus {
    /// Sort score: lower = more available. Errors/expired go to bottom.
    fn availability_score(&self) -> f64 {
        if self.is_error {
            return 1000.0;
        }
        let h5 = self.h5_pct.unwrap_or(0.0);
        let d7 = self.d7_pct.unwrap_or(0.0);
        // 5h limit matters more (it's the one that blocks you right now)
        // 100% on 5h = unusable regardless of 7d
        if h5 >= 100.0 {
            return 500.0 + d7;
        }
        h5 * 2.0 + d7
    }
}

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

    let mut statuses: Vec<AccountStatus> =
        profiles.iter().map(|p| fetch_status(p, &active)).collect();

    statuses.sort_by(|a, b| {
        a.availability_score()
            .partial_cmp(&b.availability_score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec![
        "Account", "Plan", "5h Used", "5h Reset", "7d Used", "7d Reset", "Active",
    ]);

    for s in &statuses {
        table.add_row(render_row(s));
    }

    println!("{table}");
    Ok(())
}

fn fetch_status(p: &profile::Profile, active: &Option<String>) -> AccountStatus {
    let is_active = active.as_deref() == Some(&p.meta.alias);

    let auth = match api::read_auth_json(&p.auth_json_path()) {
        Ok(a) => a,
        Err(_) => {
            return AccountStatus {
                alias: p.meta.alias.clone(),

                plan: "-".to_string(),
                h5_pct: None,
                d7_pct: None,
                h5_reset: "-".to_string(),
                d7_reset: "-".to_string(),
                is_active,
                is_error: true,
                error_msg: "bad auth.json".to_string(),
            };
        }
    };

    match api::fetch_usage(&auth.access_token) {
        Ok(usage) => {
            if let Some(plan) = &usage.plan_type {
                let _ = profile::update_meta_plan(&p.meta.alias, plan);
            }
            let plan = usage.plan_type.as_deref().unwrap_or("-").to_string();

            let primary = usage.rate_limit.as_ref().and_then(|r| r.primary());
            let secondary = usage.rate_limit.as_ref().and_then(|r| r.secondary());

            let h5_pct = primary.map(|w| w.used_percent);
            let d7_pct = secondary.map(|w| w.used_percent);
            let (_, h5_reset) = format_window(primary);
            let (_, d7_reset) = format_window(secondary);

            AccountStatus {
                alias: p.meta.alias.clone(),

                plan,
                h5_pct,
                d7_pct,
                h5_reset,
                d7_reset,
                is_active,
                is_error: false,
                error_msg: String::new(),
            }
        }
        Err(e) => {
            let msg = if e.to_string().contains("expired") {
                "expired"
            } else {
                "error"
            };
            AccountStatus {
                alias: p.meta.alias.clone(),

                plan: "-".to_string(),
                h5_pct: None,
                d7_pct: None,
                h5_reset: "-".to_string(),
                d7_reset: "-".to_string(),
                is_active,
                is_error: true,
                error_msg: msg.to_string(),
            }
        }
    }
}

fn render_row(s: &AccountStatus) -> Vec<Cell> {
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

    vec![
        Cell::new(&s.alias),
        Cell::new(&s.plan),
        colorize_usage(&h5_str),
        Cell::new(&s.h5_reset),
        colorize_usage(&d7_str),
        Cell::new(&s.d7_reset),
        Cell::new(active_marker),
    ]
}

fn format_window(window: Option<&api::RateLimitWindow>) -> (String, String) {
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
                    (used, format!("in {reset}"))
                }
                None => (used, "-".to_string()),
            }
        }
        None => ("-".to_string(), "-".to_string()),
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
