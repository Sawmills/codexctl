use std::path::Path;

use anyhow::{Result, bail};

use crate::api;
use crate::commands::alias;
use crate::commands::status;
use crate::config;
use crate::profile;

pub fn run(alias: Option<&str>) -> Result<()> {
    run_to_auth_json(alias, &config::codex_auth_json()?)
}

pub fn run_to_auth_json(alias: Option<&str>, auth_json: &Path) -> Result<()> {
    run_to_auth_json_excluding(alias, auth_json, None)
}

pub fn run_recovery_to_auth_json(auth_json: &Path, excluded_alias: Option<&str>) -> Result<()> {
    run_to_auth_json_with_mode(
        None,
        auth_json,
        excluded_alias,
        AutoSelectMode::SpendCapRecovery,
    )
}

pub fn run_to_auth_json_excluding(
    alias: Option<&str>,
    auth_json: &Path,
    excluded_alias: Option<&str>,
) -> Result<()> {
    run_to_auth_json_with_mode(alias, auth_json, excluded_alias, AutoSelectMode::RateLimit)
}

fn run_to_auth_json_with_mode(
    alias: Option<&str>,
    auth_json: &Path,
    excluded_alias: Option<&str>,
    mode: AutoSelectMode,
) -> Result<()> {
    match alias::optional(alias) {
        Some(a) => {
            let email = profile::switch_to_auth_json(a, auth_json)?;
            println!("switched to {} ({})", a, email);
            println!();
            status::run_focused(a)?;
        }
        None => {
            let best = find_most_available_excluding(excluded_alias, mode)?;
            let email = profile::switch_to_auth_json(&best, auth_json)?;
            println!("auto-selected most available: {} ({})", best, email);
            println!();
            status::run_focused(&best)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoSelectMode {
    RateLimit,
    SpendCapRecovery,
}

fn find_most_available_excluding(
    excluded_alias: Option<&str>,
    mode: AutoSelectMode,
) -> Result<String> {
    let all_profiles = profile::list_profiles()?;
    let had_profiles = !all_profiles.is_empty();
    let profiles = profiles_after_excluding(all_profiles, excluded_alias);
    if profiles.is_empty() {
        if had_profiles {
            bail!("no alternate profiles saved. Use 'codexctl save' to save another account.");
        }
        bail!("no profiles saved. Use 'codexctl save' to save the current account.");
    }

    let rt = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::new();

    let results = rt.block_on(async {
        let futs: Vec<_> = profiles
            .iter()
            .map(|p| {
                let client = client.clone();
                let alias = p.meta.alias.clone();
                let auth = api::read_auth_json(&p.auth_json_path());
                async move {
                    let auth = match auth {
                        Ok(a) => a,
                        Err(_) => return (alias, f64::MAX),
                    };
                    match api::fetch_usage_async(
                        &client,
                        &auth.access_token,
                        auth.account_id.as_deref(),
                    )
                    .await
                    {
                        Ok(usage) => (alias, selection_score(&usage, mode)),
                        Err(_) => (alias, f64::MAX),
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await
    });

    let best = results
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(_, score)| *score < f64::MAX);

    match best {
        Some((alias, _)) => Ok(alias.clone()),
        None => bail!("no usable accounts found (all expired or errored)"),
    }
}

fn selection_score(usage: &api::RateLimitResponse, mode: AutoSelectMode) -> f64 {
    if mode == AutoSelectMode::SpendCapRecovery
        && usage.spend_control.as_ref().is_some_and(|s| s.reached)
    {
        return f64::MAX;
    }

    let plan = usage.plan_type.as_deref().unwrap_or("");
    if plan.contains("usage_based") {
        // Never auto-select usage-based accounts: they bill real credits. Both
        // rate-limit switching and spend-cap recovery stay on rate-limited
        // (subscription) accounts only.
        return f64::MAX;
    }

    rate_limit_score(usage)
}

fn rate_limit_score(usage: &api::RateLimitResponse) -> f64 {
    let h5 = usage
        .rate_limit
        .as_ref()
        .and_then(|r| r.primary())
        .map(|w| w.used_percent)
        .unwrap_or(0.0);
    let d7 = usage
        .rate_limit
        .as_ref()
        .and_then(|r| r.secondary())
        .map(|w| w.used_percent)
        .unwrap_or(0.0);
    if h5 >= 100.0 && d7 >= 100.0 {
        900.0
    } else if d7 >= 100.0 {
        700.0 + h5
    } else if h5 >= 100.0 {
        500.0 + d7
    } else {
        h5 * 2.0 + d7
    }
}

fn profiles_after_excluding(
    mut profiles: Vec<profile::Profile>,
    excluded_alias: Option<&str>,
) -> Vec<profile::Profile> {
    if let Some(excluded_alias) = excluded_alias {
        profiles.retain(|p| p.meta.alias != excluded_alias);
    }
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile(alias: &str) -> profile::Profile {
        profile::Profile {
            meta: profile::Meta {
                alias: alias.to_string(),
                email: None,
                plan: None,
                saved_at: "2026-01-01T00:00:00Z".to_string(),
            },
            dir: Path::new("/tmp").join(alias),
        }
    }

    fn usage_based_response(
        credits: Option<api::Credits>,
        spend_control: Option<api::SpendControl>,
    ) -> api::RateLimitResponse {
        api::RateLimitResponse {
            plan_type: Some("usage_based".to_string()),
            rate_limit: None,
            credits,
            spend_control,
        }
    }

    #[test]
    fn profiles_after_excluding_removes_failed_active_alias() {
        let profiles = vec![test_profile("failed@test"), test_profile("next@test")];

        let aliases: Vec<_> = profiles_after_excluding(profiles, Some("failed@test"))
            .into_iter()
            .map(|profile| profile.meta.alias)
            .collect();

        assert_eq!(aliases, vec!["next@test"]);
    }

    #[test]
    fn rate_limit_mode_ignores_usage_based_profiles() {
        let usage = usage_based_response(
            Some(api::Credits {
                has_credits: true,
                unlimited: false,
                overage_limit_reached: false,
                balance: Some("10.00".to_string()),
            }),
            Some(api::SpendControl { reached: false }),
        );

        assert_eq!(selection_score(&usage, AutoSelectMode::RateLimit), f64::MAX);
    }

    #[test]
    fn recovery_mode_rejects_usage_based_profiles() {
        let usage = usage_based_response(
            Some(api::Credits {
                has_credits: true,
                unlimited: false,
                overage_limit_reached: false,
                balance: Some("10.00".to_string()),
            }),
            Some(api::SpendControl { reached: false }),
        );

        assert_eq!(
            selection_score(&usage, AutoSelectMode::SpendCapRecovery),
            f64::MAX,
            "usage-based profiles must never be auto-selected; recovery uses rate-limited accounts only"
        );
    }

    #[test]
    fn recovery_mode_rejects_spend_capped_usage_based_profiles() {
        let usage = usage_based_response(
            Some(api::Credits {
                has_credits: true,
                unlimited: false,
                overage_limit_reached: false,
                balance: Some("10.00".to_string()),
            }),
            Some(api::SpendControl { reached: true }),
        );

        assert_eq!(
            selection_score(&usage, AutoSelectMode::SpendCapRecovery),
            f64::MAX
        );
    }

    #[test]
    fn recovery_mode_rejects_spend_capped_profiles_without_usage_plan_label() {
        let usage = api::RateLimitResponse {
            plan_type: Some("team".to_string()),
            rate_limit: None,
            credits: None,
            spend_control: Some(api::SpendControl { reached: true }),
        };

        assert_eq!(
            selection_score(&usage, AutoSelectMode::SpendCapRecovery),
            f64::MAX
        );
    }
}
