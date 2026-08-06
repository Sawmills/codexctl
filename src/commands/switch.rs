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
            // Try to fetch usage for display — fall back gracefully
            let auth_path = profile::auth_json_path_for_profile_from(&paths, p, active.as_deref());
            let usage_info = api::read_auth_json(&auth_path)
                .ok()
                .and_then(|auth| {
                    api::fetch_usage(&auth.access_token, auth.account_id.as_deref()).ok()
                })
                .map(|usage| usage_summary(&usage))
                .unwrap_or_default();

            picker_row(p, &usage_info, active.as_deref() == Some(&p.meta.alias))
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

/// One row of the fuzzy picker.
///
/// The label goes in the text because the picker matches on it: typing `team`
/// has to find the profile labeled `team`. This is the only place a label
/// influences selection, and the operator still confirms the highlighted row.
fn picker_row(p: &profile::Profile, usage_info: &str, is_active: bool) -> String {
    let email = p.meta.email.as_deref().unwrap_or("-");
    let plan = p.meta.plan.as_deref().unwrap_or("-");
    let label = p
        .meta
        .label
        .as_deref()
        .map(|label| format!(" — {label}"))
        .unwrap_or_default();
    let marker = if is_active { " *" } else { "" };
    format!(
        "{}{label} ({email}) [{plan}]{usage_info}{marker}",
        p.meta.alias
    )
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

    fn profile_with(alias: &str, label: Option<&str>) -> profile::Profile {
        profile::Profile {
            meta: profile::Meta {
                alias: alias.to_string(),
                label: label.map(str::to_string),
                email: Some("amir@sawmills.ai".to_string()),
                plan: Some("business".to_string()),
                saved_at: "2026-01-01T00:00:00Z".to_string(),
                ..profile::Meta::default()
            },
            dir: std::path::PathBuf::from("/tmp").join(alias),
        }
    }

    /// The picker is the one place a label reaches selection: typing it has to
    /// match, which means it has to be in the row text.
    #[test]
    fn picker_row_carries_the_label_for_fuzzy_matching() {
        let row = picker_row(&profile_with("amir-2", Some("sawmills seat")), "", false);

        assert!(row.contains("sawmills seat"), "{row}");
        assert!(row.contains("amir-2"), "{row}");
    }

    #[test]
    fn picker_row_omits_the_label_segment_when_unset() {
        let row = picker_row(&profile_with("amir-2", None), "", false);

        assert_eq!(row, "amir-2 (amir@sawmills.ai) [business]");
    }

    #[test]
    fn picker_row_marks_the_active_profile() {
        let row = picker_row(&profile_with("amir-2", None), "", true);

        assert!(row.ends_with(" *"), "{row}");
    }

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
