use anyhow::Result;
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Color, Table};

use crate::api;
use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec![
        "Account", "Email", "Plan", "5h Used", "5h Reset", "7d Used", "7d Reset", "Active",
    ]);

    for p in &profiles {
        let auth = api::read_auth_json(&p.auth_json_path());
        let row = match auth {
            Ok(auth) => build_row(p, &auth.access_token, &active),
            Err(_) => build_error_row(p, "bad auth.json", &active),
        };
        table.add_row(row);
    }

    println!("{table}");
    Ok(())
}

fn build_row(p: &profile::Profile, access_token: &str, active: &Option<String>) -> Vec<Cell> {
    let is_active = active.as_deref() == Some(&p.meta.alias);
    let active_marker = if is_active { "*" } else { "" };
    let email = p.meta.email.as_deref().unwrap_or("-");

    match api::fetch_usage(access_token) {
        Ok(usage) => {
            if let Some(plan) = &usage.plan_type {
                let _ = profile::update_meta_plan(&p.meta.alias, plan);
            }
            let plan = usage.plan_type.as_deref().unwrap_or("-");
            let (h5_used, h5_reset) =
                format_window(usage.rate_limit.as_ref().and_then(|r| r.primary.as_ref()));
            let (d7_used, d7_reset) =
                format_window(usage.rate_limit.as_ref().and_then(|r| r.secondary.as_ref()));

            vec![
                Cell::new(&p.meta.alias),
                Cell::new(email),
                Cell::new(plan),
                colorize_usage(&h5_used),
                Cell::new(&h5_reset),
                colorize_usage(&d7_used),
                Cell::new(&d7_reset),
                Cell::new(active_marker),
            ]
        }
        Err(e) => {
            let msg = if e.to_string().contains("expired") {
                "expired"
            } else {
                "error"
            };
            build_error_row(p, msg, active)
        }
    }
}

fn build_error_row(p: &profile::Profile, msg: &str, active: &Option<String>) -> Vec<Cell> {
    let is_active = active.as_deref() == Some(&p.meta.alias);
    let active_marker = if is_active { "*" } else { "" };
    let email = p.meta.email.as_deref().unwrap_or("-");

    vec![
        Cell::new(&p.meta.alias),
        Cell::new(email),
        Cell::new("-"),
        Cell::new(msg).fg(Color::Red),
        Cell::new("-"),
        Cell::new(msg).fg(Color::Red),
        Cell::new("-"),
        Cell::new(active_marker),
    ]
}

fn format_window(window: Option<&api::RateLimitWindow>) -> (String, String) {
    match window {
        Some(w) => {
            let used = format!("{:.0}%", w.used_percent);
            let now = chrono::Utc::now().timestamp();
            let diff_secs = w.resets_at - now;
            let reset = if diff_secs <= 0 {
                "now".to_string()
            } else {
                format_duration(diff_secs)
            };
            (used, format!("in {reset}"))
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
