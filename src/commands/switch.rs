use anyhow::Result;
use dialoguer::FuzzySelect;

use crate::api;
use crate::profile;

pub fn run() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;

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
            let usage_info = api::read_auth_json(&p.auth_json_path())
                .ok()
                .and_then(|auth| api::fetch_usage(&auth.access_token).ok())
                .map(|u| {
                    let h5 = u
                        .rate_limit
                        .as_ref()
                        .and_then(|r| r.primary())
                        .map(|w| format!("{:.0}%", w.used_percent))
                        .unwrap_or_else(|| "-".to_string());
                    let d7 = u
                        .rate_limit
                        .as_ref()
                        .and_then(|r| r.secondary())
                        .map(|w| format!("{:.0}%", w.used_percent))
                        .unwrap_or_else(|| "-".to_string());
                    format!(" — 5h: {h5}, 7d: {d7}")
                })
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
