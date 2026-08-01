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
    let windows: Vec<_> = rate_limit
        .windows()
        .map(|(position, window)| {
            let label = window.duration_label().unwrap_or_else(|| {
                if position == 0 {
                    "primary".to_string()
                } else {
                    "secondary".to_string()
                }
            });
            format!("{label}: {:.0}%", window.used_percent)
        })
        .collect();
    if windows.is_empty() {
        String::new()
    } else {
        format!(" — {}", windows.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_summary_uses_declared_durations() {
        let usage: api::RateLimitResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {"used_percent": 25, "window_minutes": 15},
                    "secondary_window": {"used_percent": 42, "window_minutes": 60}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(usage_summary(&usage), " — 15m: 25%, 1h: 42%");
    }

    #[test]
    fn usage_summary_keeps_secondary_only_position() {
        let usage: api::RateLimitResponse =
            serde_json::from_str(r#"{"rate_limit": {"secondary_window": {"used_percent": 42}}}"#)
                .unwrap();

        assert_eq!(usage_summary(&usage), " — secondary: 42%");
    }
}
