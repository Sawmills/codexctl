use anyhow::Result;
use dialoguer::FuzzySelect;

use crate::api;
use crate::config;
use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let paths = config::default_paths()?;
    let active = profile::get_active_from(&paths)?;

    let items: Vec<String> = profiles
        .iter()
        .map(|p| {
            let email = p.meta.email.as_deref().unwrap_or("-");
            let marker = if active.as_deref() == Some(&p.meta.alias) {
                " *"
            } else {
                ""
            };

            // Try to fetch usage for display — fall back gracefully
            let auth_path = profile::auth_json_path_for_profile_from(&paths, p, active.as_deref());
            let usage_info = api::read_auth_json(&auth_path)
                .ok()
                .and_then(|auth| {
                    api::fetch_usage(&auth.access_token, auth.account_id.as_deref()).ok()
                })
                .map(|usage| usage_summary(&usage))
                .unwrap_or_default();

            let plan = p.meta.plan.as_deref().unwrap_or("-");
            format!(
                "{} ({}) [{}]{}{}",
                p.meta.alias, email, plan, usage_info, marker
            )
        })
        .collect();

    let selection = FuzzySelect::new()
        .with_prompt("Select account")
        .items(&items)
        .default(0)
        .interact_opt()?;

    match selection {
        Some(idx) => {
            let alias = &profiles[idx].meta.alias;
            let email = profile::switch_to(alias)?;
            println!("switched to {} ({})", alias, email);
        }
        None => {
            println!("cancelled");
        }
    }
    Ok(())
}

fn usage_summary(usage: &api::RateLimitResponse) -> String {
    let Some(rate_limit) = &usage.rate_limit else {
        return String::new();
    };
    let mut windows = Vec::new();
    if let Some(short) = rate_limit.short_window() {
        windows.push(format!("5h: {:.0}%", short.used_percent));
    }
    if let Some(long) = rate_limit.long_window() {
        windows.push(format!("7d: {:.0}%", long.used_percent));
    }
    if windows.is_empty() {
        String::new()
    } else {
        format!(" — {}", windows.join(", "))
    }
}
