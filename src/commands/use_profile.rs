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
    let scored: Vec<SelectionCandidate> = usages
        .iter()
        .map(|(alias, usage)| {
            let score = usage.as_ref().map_or(f64::MAX, selection_score);
            let bills_credits = usage.as_ref().is_none_or(selection_bills_credits);
            let reset = usage.as_ref().map_or(i64::MAX, secondary_reset_ts);
            SelectionCandidate {
                alias: alias.clone(),
                bills_credits,
                score,
                secondary_reset_ts: reset,
            }
        })
        .collect();

    match select_most_available(&scored, reset_aware()) {
        Some(alias) => Ok(alias.to_string()),
        None => bail!("no usable accounts found (all expired or errored)"),
    }
}

/// Whether selection prefers the soonest-resetting eligible account.
///
/// Reset-aware is the DEFAULT: among otherwise-eligible accounts it drains the
/// nearest-reset seat first and keeps fresher seats in reserve, de-synchronizing
/// the fleet's 7d windows so capacity refreshes gradually instead of filling and
/// resetting in a single cluster. Set `CODEXCTL_SELECT=most-available` (alias
/// `headroom`/`legacy`) to opt out and restore the legacy most-headroom-first pick.
fn reset_aware() -> bool {
    reset_aware_from(std::env::var("CODEXCTL_SELECT").ok().as_deref())
}

/// Pure form of [`reset_aware`] over the raw env value, for testing.
fn reset_aware_from(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "most-available" | "most_available" | "headroom" | "legacy" | "off" | "false"
        ),
        None => true,
    }
}

/// Unix timestamp when the 7d (secondary) window resets; `i64::MAX` if unknown,
/// so accounts with no known reset are never preferred by reset-aware sorting.
fn secondary_reset_ts(usage: &api::RateLimitResponse) -> i64 {
    usage
        .rate_limit
        .as_ref()
        .and_then(|r| r.secondary())
        .and_then(|w| w.reset_timestamp())
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
struct SelectionCandidate {
    alias: String,
    /// True when overage is open (spend cap NOT reached), so this account can
    /// draw credits once it crosses the hard rate-limit windows.
    bills_credits: bool,
    score: f64,
    secondary_reset_ts: i64,
}

/// Pick the most-available alias from scored candidates.
///
/// Default (`reset_aware == false`): lowest `selection_score` (most headroom),
/// exactly as before. Reset-aware: among accounts with usable headroom
/// (`score < RATE_LIMIT_EXHAUSTED`) pick no-bill accounts first, then the
/// soonest 7d reset, breaking ties by most headroom; if none have headroom, fall
/// back to the default pick so the behavior degrades identically to today.
fn select_most_available(scored: &[SelectionCandidate], reset_aware: bool) -> Option<&str> {
    if reset_aware {
        let staggered = scored
            .iter()
            .filter(|candidate| candidate.score < RATE_LIMIT_EXHAUSTED)
            .min_by(|a, b| {
                a.bills_credits
                    .cmp(&b.bills_credits)
                    .then(a.secondary_reset_ts.cmp(&b.secondary_reset_ts))
                    .then(
                        a.score
                            .partial_cmp(&b.score)
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
            });
        if let Some(candidate) = staggered {
            return Some(candidate.alias.as_str());
        }
    }
    scored
        .iter()
        .min_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .filter(|candidate| candidate.score < f64::MAX)
        .map(|candidate| candidate.alias.as_str())
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
    let candidates: Vec<(String, bool, f64, i64)> = usages
        .into_iter()
        .filter_map(|(alias, usage)| {
            let usage = usage?;
            let (bills_credits, score) = recovery_score(&usage)?;
            Some((alias, bills_credits, score, secondary_reset_ts(&usage)))
        })
        .collect();

    Ok(select_recovery(candidates, reset_aware()))
}

/// Choose the recovery candidate. No-bill accounts (`bills_credits == false`)
/// always win over billing ones — preserved in both modes. Within a bill class,
/// default picks most headroom; reset-aware picks the soonest 7d reset first,
/// then most headroom.
fn select_recovery(
    candidates: Vec<(String, bool, f64, i64)>,
    reset_aware: bool,
) -> Option<RecoveryCandidate> {
    candidates
        .into_iter()
        .min_by(|a, b| {
            let by_bill = a.1.cmp(&b.1);
            if reset_aware {
                by_bill
                    .then(a.3.cmp(&b.3))
                    .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            } else {
                by_bill.then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            }
        })
        .map(|(alias, bills_credits, _, _)| RecoveryCandidate {
            alias,
            bills_credits,
        })
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

fn selection_bills_credits(usage: &api::RateLimitResponse) -> bool {
    !usage.spend_control.as_ref().is_some_and(|s| s.reached)
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

    fn selection_candidate(
        alias: &str,
        bills_credits: bool,
        score: f64,
        secondary_reset_ts: i64,
    ) -> SelectionCandidate {
        SelectionCandidate {
            alias: alias.to_string(),
            bills_credits,
            score,
            secondary_reset_ts,
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

    #[test]
    fn reset_aware_is_default_on() {
        assert!(reset_aware_from(None), "unset must default to reset-aware");
        assert!(reset_aware_from(Some("reset-aware")));
        assert!(reset_aware_from(Some("anything-unrecognized")));
    }

    #[test]
    fn reset_aware_opt_out_values_disable_it() {
        assert!(!reset_aware_from(Some("most-available")));
        assert!(!reset_aware_from(Some("headroom")));
        assert!(!reset_aware_from(Some("LEGACY")));
        assert!(!reset_aware_from(Some("off")));
    }

    #[test]
    fn default_selection_picks_most_headroom_ignoring_reset() {
        // a has more headroom (lower score) but a later reset than b.
        let scored = vec![
            selection_candidate("a", false, 20.0, 2000),
            selection_candidate("b", false, 60.0, 1000),
        ];
        assert_eq!(select_most_available(&scored, false), Some("a"));
    }

    #[test]
    fn legacy_selection_picks_most_headroom_even_when_it_bills() {
        // `CODEXCTL_SELECT=most-available` is the explicit legacy opt-out from
        // reset-aware selection; it must preserve pure most-headroom ordering.
        let scored = vec![
            selection_candidate("billing", true, 20.0, 1000),
            selection_candidate("no-bill", false, 60.0, 2000),
        ];
        assert_eq!(select_most_available(&scored, false), Some("billing"));
    }

    #[test]
    fn reset_aware_prefers_soonest_7d_reset_among_headroom() {
        // Same accounts; reset-aware drains the soonest-resetting one first even
        // though it has less headroom — this is what de-synchronizes the fleet.
        let scored = vec![
            selection_candidate("a", false, 20.0, 2000),
            selection_candidate("b", false, 60.0, 1000),
        ];
        assert_eq!(select_most_available(&scored, true), Some("b"));
    }

    #[test]
    fn reset_aware_selection_prefers_no_bill_before_billing() {
        // Plain `codexctl use` must keep the same billing guard as recovery:
        // a credit-billing account with an earlier reset must not beat a
        // no-bill account that still has usable headroom.
        let billing = team_response(5.0, 10.0, false);
        let no_bill = team_response(20.0, 30.0, true);
        let scored = vec![
            selection_candidate(
                "billing",
                selection_bills_credits(&billing),
                selection_score(&billing),
                secondary_reset_ts(&billing),
            ),
            selection_candidate(
                "no-bill",
                selection_bills_credits(&no_bill),
                selection_score(&no_bill),
                secondary_reset_ts(&no_bill),
            ),
        ];

        assert_eq!(select_most_available(&scored, true), Some("no-bill"));
    }

    #[test]
    fn reset_aware_falls_back_to_score_when_no_headroom() {
        // Both windows exhausted (score >= RATE_LIMIT_EXHAUSTED): reset-aware must
        // NOT pick by reset, it falls back to the default lowest-score pick.
        let scored = vec![
            selection_candidate("a", false, 700.0, 2000),
            selection_candidate("b", false, 550.0, 1000),
        ];
        assert_eq!(select_most_available(&scored, true), Some("b"));
    }

    #[test]
    fn select_most_available_skips_usage_based_in_both_modes() {
        // Usage-based -> score f64::MAX -> never usable.
        let scored = vec![selection_candidate("u", false, f64::MAX, 100)];
        assert_eq!(select_most_available(&scored, true), None);
        assert_eq!(select_most_available(&scored, false), None);
    }

    #[test]
    fn reset_aware_recovery_keeps_no_bill_priority() {
        // A billing account resets soonest; a no-bill account resets later.
        // No-bill must still win — reset is only a tiebreak within a bill class.
        let candidates = vec![
            ("bills".to_string(), true, 10.0, 1000),
            ("nobill".to_string(), false, 50.0, 5000),
        ];
        assert_eq!(select_recovery(candidates, true).unwrap().alias, "nobill");
    }

    #[test]
    fn reset_aware_recovery_breaks_ties_by_soonest_reset() {
        let candidates = vec![
            ("late".to_string(), false, 10.0, 5000),
            ("soon".to_string(), false, 40.0, 1000),
        ];
        assert_eq!(
            select_recovery(candidates.clone(), true).unwrap().alias,
            "soon"
        );
        // Default ignores reset and keeps the most-headroom pick.
        assert_eq!(select_recovery(candidates, false).unwrap().alias, "late");
    }
}
