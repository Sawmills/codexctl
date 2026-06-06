use std::path::Path;

use anyhow::{Result, bail};

use crate::api;
use crate::commands::alias;
use crate::commands::status;
use crate::config;
use crate::profile;

/// `rate_limit_score` returns at least this whenever a rate-limit window is at
/// 100% — i.e. the account has no usable headroom right now.
const RATE_LIMIT_EXHAUSTED: f64 = 500.0;

pub fn run(alias: Option<&str>) -> Result<()> {
    run_to_auth_json(alias, &config::codex_auth_json()?)
}

pub fn run_to_auth_json(alias: Option<&str>, auth_json: &Path) -> Result<()> {
    run_to_auth_json_excluding(alias, auth_json, None)
}

pub fn run_to_auth_json_excluding(
    alias: Option<&str>,
    auth_json: &Path,
    excluded_alias: Option<&str>,
) -> Result<()> {
    match alias::optional(alias) {
        Some(a) => {
            let email = profile::switch_to_auth_json(a, auth_json)?;
            println!("switched to {} ({})", a, email);
            println!();
            status::run_focused(a)?;
        }
        None => {
            let best = find_most_available_excluding(excluded_alias)?;
            let email = profile::switch_to_auth_json(&best, auth_json)?;
            println!("auto-selected most available: {} ({})", best, email);
            println!();
            status::run_focused(&best)?;
        }
    }
    Ok(())
}

fn find_most_available_excluding(excluded_alias: Option<&str>) -> Result<String> {
    let all_profiles = profile::list_profiles()?;
    let had_profiles = !all_profiles.is_empty();
    let profiles = profiles_after_excluding(all_profiles, excluded_alias);
    if profiles.is_empty() {
        if had_profiles {
            bail!("no alternate profiles saved. Use 'codexctl save' to save another account.");
        }
        bail!("no profiles saved. Use 'codexctl save' to save the current account.");
    }

    let usages = fetch_usages(&profiles)?;
    let best = usages
        .iter()
        .map(|(alias, usage)| (alias, usage.as_ref().map_or(f64::MAX, selection_score)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(_, score)| *score < f64::MAX);

    match best {
        Some((alias, _)) => Ok(alias.clone()),
        None => bail!("no usable accounts found (all expired or errored)"),
    }
}

fn selection_score(usage: &api::RateLimitResponse) -> f64 {
    let plan = usage.plan_type.as_deref().unwrap_or("");
    if plan.contains("usage_based") {
        // Never auto-select usage-based accounts: they bill real credits.
        return f64::MAX;
    }
    rate_limit_score(usage)
}

/// A spend-cap recovery candidate plus whether using it can bill credits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCandidate {
    pub alias: String,
    /// True when overage is open (spend cap NOT reached), so the account draws
    /// credits ($) once it passes 100% of its rate limit. These require consent.
    pub bills_credits: bool,
}

/// Pick the next spend-cap recovery account, excluding any `tried` aliases.
///
/// Prefers accounts that will not bill credits — spend cap already reached, so
/// they hard-stop at 100% rather than drawing credits — over ones that can.
/// Usage-based accounts are never selected, and accounts without rate-limit
/// headroom (a window already at 100%) are skipped, since switching to them
/// would immediately re-trigger the cap.
pub fn find_recovery_candidate(tried: &[String]) -> Result<Option<RecoveryCandidate>> {
    let profiles: Vec<profile::Profile> = profile::list_profiles()?
        .into_iter()
        .filter(|p| !tried.iter().any(|t| t.as_str() == p.meta.alias.as_str()))
        .collect();
    if profiles.is_empty() {
        return Ok(None);
    }

    let usages = fetch_usages(&profiles)?;
    let best = usages
        .into_iter()
        .filter_map(|(alias, usage)| {
            let (bills_credits, score) = recovery_score(&usage?)?;
            Some((alias, bills_credits, score))
        })
        // No-bill (bills_credits == false) wins over billing; then most headroom.
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        });

    Ok(best.map(|(alias, bills_credits, _)| RecoveryCandidate {
        alias,
        bills_credits,
    }))
}

/// Classify an account for spend-cap recovery as `(bills_credits, score)`, or
/// `None` when it must not be used (usage-based, or no rate-limit headroom).
fn recovery_score(usage: &api::RateLimitResponse) -> Option<(bool, f64)> {
    let plan = usage.plan_type.as_deref().unwrap_or("");
    if plan.contains("usage_based") {
        return None;
    }
    let score = rate_limit_score(usage);
    if score >= RATE_LIMIT_EXHAUSTED {
        // A rate-limit window is already at 100% — not usable right now.
        return None;
    }
    // Spend cap reached => overage closed => the account hard-stops at 100% and
    // never bills. Spend cap NOT reached => overage open => it draws credits ($).
    let bills_credits = !usage.spend_control.as_ref().is_some_and(|s| s.reached);
    Some((bills_credits, score))
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

fn fetch_usages(
    profiles: &[profile::Profile],
) -> Result<Vec<(String, Option<api::RateLimitResponse>)>> {
    let rt = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::new();

    Ok(rt.block_on(async {
        let futs: Vec<_> = profiles
            .iter()
            .map(|p| {
                let client = client.clone();
                let alias = p.meta.alias.clone();
                let auth = api::read_auth_json(&p.auth_json_path());
                async move {
                    let usage = match auth {
                        Ok(a) => api::fetch_usage_async(
                            &client,
                            &a.access_token,
                            a.account_id.as_deref(),
                        )
                        .await
                        .ok(),
                        Err(_) => None,
                    };
                    (alias, usage)
                }
            })
            .collect();
        futures::future::join_all(futs).await
    }))
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

    fn window(used_percent: f64) -> api::RateLimitWindow {
        api::RateLimitWindow {
            used_percent,
            window_minutes: None,
            limit_window_seconds: None,
            resets_at: None,
            reset_at: None,
            reset_after_seconds: None,
        }
    }

    fn team_response(h5: f64, d7: f64, spend_reached: bool) -> api::RateLimitResponse {
        api::RateLimitResponse {
            plan_type: Some("team".to_string()),
            rate_limit: Some(api::RateLimit {
                primary: Some(window(h5)),
                secondary: Some(window(d7)),
                primary_window: None,
                secondary_window: None,
            }),
            credits: None,
            spend_control: Some(api::SpendControl {
                reached: spend_reached,
            }),
        }
    }

    fn usage_based_response() -> api::RateLimitResponse {
        api::RateLimitResponse {
            plan_type: Some("self_serve_business_usage_based".to_string()),
            rate_limit: None,
            credits: Some(api::Credits {
                has_credits: true,
                unlimited: false,
                overage_limit_reached: false,
                balance: None,
            }),
            spend_control: Some(api::SpendControl { reached: false }),
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
    fn selection_never_picks_usage_based_accounts() {
        assert_eq!(selection_score(&usage_based_response()), f64::MAX);
    }

    #[test]
    fn recovery_skips_usage_based_accounts() {
        assert_eq!(recovery_score(&usage_based_response()), None);
    }

    #[test]
    fn recovery_skips_accounts_without_rate_limit_headroom() {
        // 5h window already at 100% -> not usable right now, even though the
        // spend cap is reached (no-bill).
        assert_eq!(recovery_score(&team_response(100.0, 20.0, true)), None);
    }

    #[test]
    fn recovery_treats_spend_capped_account_as_no_bill() {
        // Spend cap reached + headroom -> usable and hard-stops, so won't bill.
        let (bills_credits, _) = recovery_score(&team_response(10.0, 30.0, true)).unwrap();
        assert!(
            !bills_credits,
            "spend-cap-reached accounts hard-stop at 100% and must be treated as no-bill"
        );
    }

    #[test]
    fn recovery_treats_overage_open_account_as_billing() {
        // Spend cap NOT reached -> overage open -> draws credits past 100%.
        let (bills_credits, _) = recovery_score(&team_response(5.0, 71.0, false)).unwrap();
        assert!(
            bills_credits,
            "overage-open accounts can draw credits and must require consent"
        );
    }
}
