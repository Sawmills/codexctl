use std::cmp::Ordering;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};

use crate::api;
use crate::commands::alias;
use crate::commands::resets;
use crate::commands::status;
use crate::config;
use crate::profile;

/// `rate_limit_score` returns at least this whenever a rate-limit window is at
/// 100% — i.e. the account has no usable headroom right now.
const RATE_LIMIT_EXHAUSTED: f64 = 500.0;

pub fn run(alias: Option<&str>, _allow_billing: bool, allow_resets: bool) -> Result<()> {
    run_to_auth_json(alias, &config::codex_auth_json()?, allow_resets)
}

pub fn run_to_auth_json(alias: Option<&str>, auth_json: &Path, allow_resets: bool) -> Result<()> {
    run_to_auth_json_excluding(alias, auth_json, None, allow_resets)
}

pub fn run_to_auth_json_excluding(
    alias: Option<&str>,
    auth_json: &Path,
    excluded_alias: Option<&str>,
    allow_resets: bool,
) -> Result<()> {
    match alias::optional(alias)? {
        // An explicit target is switched to as asked — never redeemed against,
        // since `codexctl reset <alias>` is the way to spend a credit on a
        // named account.
        Some(a) => {
            let email = profile::switch_to_auth_json(a, auth_json)?;
            println!("switched to {} ({})", a, email);
            println!();
            status::run_focused(a)?;
        }
        None => {
            let best = find_most_available_excluding(excluded_alias, allow_resets)?;
            let email = profile::switch_to_auth_json(&best, auth_json)?;
            println!("auto-selected most available: {} ({})", best, email);
            println!();
            status::run_focused(&best)?;
        }
    }
    Ok(())
}

fn find_most_available_excluding(
    excluded_alias: Option<&str>,
    allow_resets: bool,
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

    // Prefer existing headroom before spending a banked reset. A seat that can
    // bill after its hard window produces a warning before selection.
    if let Some(alias) = select_with_headroom(&scored, reset_aware()) {
        let candidate = scored
            .iter()
            .find(|candidate| candidate.alias == alias)
            .expect("selected candidate must exist");
        notify_billing_account(&candidate.alias, candidate.bills_credits);
        return Ok(alias.to_string());
    }

    // Nothing is usable as-is. Redeeming a banked reset is the only way to hand
    // back an account that actually works, so apply the same cost-ranked ladder
    // `codexctl codex` uses on recovery.
    if let Some(candidate) = best_candidate_from_usages(usages)?
        && let Some(plan) = candidate.reset_plan()
    {
        if !resets::approve_redemption(
            &candidate.alias,
            plan.expires_at,
            plan.expires_unused,
            allow_resets,
            &mut std::io::stdout(),
        ) {
            bail!(
                "every account is out of rate-limit headroom and redeeming a banked reset for {} was not approved",
                candidate.alias
            );
        }
        // Warn only after reset approval makes the switch actionable, but
        // before redeeming the scarce reset.
        notify_billing_account(&candidate.alias, candidate.bills_credits);
        let response = resets::redeem(&candidate.alias, plan.credit_id.as_deref())?;
        resets::report_outcome(&candidate.alias, &response);
        if response.code != api::ConsumeResetCode::Reset {
            bail!(
                "redeeming a banked reset for {} did not clear its rate limit",
                candidate.alias
            );
        }
        // Otherwise the status printed right after the switch still shows the
        // old 100% and the redemption looks like it did nothing.
        resets::settle_after_redeem(&candidate.alias);
        return Ok(candidate.alias);
    }

    // No reset can help either: fall back to the least-bad account, exactly as
    // before, so `use` still hands back something rather than failing outright.
    match select_most_available(&scored, reset_aware()) {
        Some(alias) => {
            let candidate = scored
                .iter()
                .find(|candidate| candidate.alias == alias)
                .expect("selected candidate must exist");
            notify_billing_account(&candidate.alias, candidate.bills_credits);
            Ok(alias.to_string())
        }
        None => bail!("no usable accounts found (all expired or errored)"),
    }
}

fn notify_billing_account(alias: &str, bills_credits: bool) {
    notify_billing_account_to(alias, bills_credits, &mut std::io::stderr());
}

fn notify_billing_account_to(alias: &str, bills_credits: bool, output: &mut impl Write) {
    if bills_credits {
        let _ = writeln!(
            output,
            "codexctl: {alias} can use paid credits after its included usage; no billing confirmation is required"
        );
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
        .and_then(|r| r.long_window())
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
/// Every mode keeps no-bill accounts ahead of billing accounts. Default
/// (`reset_aware == false`) then uses the lowest `selection_score` (most
/// headroom). Reset-aware then uses the soonest 7d reset, breaking ties by most
/// headroom. If none have headroom, fall back to the least-bad safe class.
fn select_most_available(scored: &[SelectionCandidate], reset_aware: bool) -> Option<&str> {
    if let Some(alias) = select_with_headroom(scored, reset_aware) {
        return Some(alias);
    }
    scored
        .iter()
        .min_by(|a, b| {
            a.bills_credits.cmp(&b.bills_credits).then(
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
        .filter(|candidate| candidate.score < f64::MAX)
        .map(|candidate| candidate.alias.as_str())
}

/// The best account that still has usable rate-limit headroom, or `None` when
/// every account is exhausted (or usage-based, which is never auto-selected).
///
/// Both modes put no-bill accounts first. Reset-aware then uses the soonest 7d
/// reset and most headroom. Default uses most headroom within the billing class.
fn select_with_headroom(scored: &[SelectionCandidate], reset_aware: bool) -> Option<&str> {
    scored
        .iter()
        .filter(|candidate| candidate.score < RATE_LIMIT_EXHAUSTED)
        .min_by(|a, b| {
            let by_score = a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal);
            if reset_aware {
                a.bills_credits
                    .cmp(&b.bills_credits)
                    .then(a.secondary_reset_ts.cmp(&b.secondary_reset_ts))
                    .then(by_score)
            } else {
                a.bills_credits.cmp(&b.bills_credits).then(by_score)
            }
        })
        .map(|candidate| candidate.alias.as_str())
}

fn selection_score(usage: &api::RateLimitResponse) -> f64 {
    if usage.billing_class() != api::BillingClass::RateLimited {
        // Never auto-select usage-based or unknown accounts. Unknown billing
        // metadata is not evidence that an account is free to use.
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
    /// What it costs to put this account to work.
    pub cost: RecoveryCost,
}

impl RecoveryCandidate {
    /// The banked reset that has to be redeemed before this account is usable,
    /// if any.
    pub fn reset_plan(&self) -> Option<&ResetPlan> {
        match &self.cost {
            RecoveryCost::Headroom => None,
            RecoveryCost::ResetCredit(plan) => Some(plan),
        }
    }
}

/// What switching to a recovery candidate costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryCost {
    /// The account still has rate-limit headroom; switching to it costs nothing.
    Headroom,
    /// The account is exhausted, but a banked reset credit can clear its window.
    ResetCredit(ResetPlan),
}

/// The banked reset codexctl would redeem to make an exhausted account usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPlan {
    /// The soonest-expiring redeemable credit. `None` lets the backend pick,
    /// which is the safe fallback when the credit listing could not be read.
    pub credit_id: Option<String>,
    pub expires_at: Option<i64>,
    /// The credit expires before this account's window would reset on its own.
    /// Holding it back cannot pay off — it would lapse unredeemed — so spending
    /// it now is strictly free and needs no consent.
    pub expires_unused: bool,
}

/// Pick the next spend-cap recovery account, excluding any `tried` aliases.
///
/// Candidates are ranked cheapest-first: an account with rate-limit headroom
/// that will not bill, then one whose banked reset would expire unused anyway,
/// then one that must spend a bankable reset, and only then accounts that draw
/// credits ($). Usage-based accounts are never selected, and an exhausted
/// account is skipped unless a banked reset can actually clear its window.
pub fn find_recovery_candidate(tried: &[String]) -> Result<Option<RecoveryCandidate>> {
    let profiles: Vec<profile::Profile> = profile::list_profiles()?
        .into_iter()
        .filter(|p| !tried.iter().any(|t| t.as_str() == p.meta.alias.as_str()))
        .collect();
    if profiles.is_empty() {
        return Ok(None);
    }

    let usages = fetch_usages(&profiles)?;
    best_candidate_from_usages(usages)
}

/// Rank already-fetched accounts cheapest-first and return the winner. Shared
/// by `codexctl codex` recovery and by `codexctl use`, so both spend resources
/// in the same order.
fn best_candidate_from_usages(
    usages: Vec<(String, Option<api::RateLimitResponse>)>,
) -> Result<Option<RecoveryCandidate>> {
    let classified: Vec<(String, api::RateLimitResponse, RecoveryClass)> = usages
        .into_iter()
        .filter_map(|(alias, usage)| {
            let usage = usage?;
            let class = recovery_class(&usage)?;
            Some((alias, usage, class))
        })
        .collect();

    // Only accounts that must redeem need their credit listing read, so the
    // common case (someone has headroom) stays at one request per profile.
    let needs_credits: Vec<String> = classified
        .iter()
        .filter(|(_, _, class)| class.needs_reset)
        .map(|(alias, _, _)| alias.clone())
        .collect();
    let credit_details = fetch_reset_credits(&needs_credits)?;

    let candidates: Vec<ScoredRecovery> = classified
        .into_iter()
        .map(|(alias, usage, class)| {
            let cost = if class.needs_reset {
                let details = credit_details
                    .iter()
                    .find(|(a, _)| *a == alias)
                    .and_then(|(_, details)| details.as_ref());
                RecoveryCost::ResetCredit(reset_plan(details, natural_reset_ts(&usage)))
            } else {
                RecoveryCost::Headroom
            };
            let tiebreak_ts = match &cost {
                // Spend the credit closest to lapsing first.
                RecoveryCost::ResetCredit(plan) => plan.expires_at.unwrap_or(i64::MAX),
                RecoveryCost::Headroom => secondary_reset_ts(&usage),
            };
            ScoredRecovery {
                alias,
                bills_credits: class.bills_credits,
                score: class.score,
                tiebreak_ts,
                cost,
            }
        })
        .collect();

    Ok(select_recovery(candidates, reset_aware()))
}

#[derive(Debug, Clone)]
struct ScoredRecovery {
    alias: String,
    bills_credits: bool,
    score: f64,
    /// Soonest-first tiebreak: the 7d window reset for headroom candidates, the
    /// credit expiry for reset candidates.
    tiebreak_ts: i64,
    cost: RecoveryCost,
}

impl ScoredRecovery {
    /// Cheapest first. A banked reset costs no money, so redeeming one beats
    /// any account that would draw credits; a reset that would lapse unredeemed
    /// costs nothing at all, so it even beats one worth keeping in the bank.
    fn rank(&self) -> u8 {
        match (&self.cost, self.bills_credits) {
            (RecoveryCost::Headroom, false) => 0,
            (RecoveryCost::ResetCredit(plan), false) if plan.expires_unused => 1,
            (RecoveryCost::ResetCredit(_), false) => 2,
            // A lapsing credit is free even on an account that can bill later,
            // and salvaging it leaves that account no worse off than simply
            // switching to one — so it comes first among the billing options.
            (RecoveryCost::ResetCredit(plan), true) if plan.expires_unused => 3,
            (RecoveryCost::Headroom, true) => 4,
            (RecoveryCost::ResetCredit(_), true) => 5,
        }
    }
}

/// Choose the recovery candidate. Cheapest [`ScoredRecovery::rank`] always wins
/// — so no-bill accounts still beat billing ones in both modes. Within a rank,
/// default picks most headroom; reset-aware picks the soonest 7d reset (or
/// soonest-lapsing credit) first, then most headroom.
fn select_recovery(
    candidates: Vec<ScoredRecovery>,
    reset_aware: bool,
) -> Option<RecoveryCandidate> {
    candidates
        .into_iter()
        .min_by(|a, b| {
            let by_rank = a.rank().cmp(&b.rank());
            if reset_aware {
                by_rank
                    .then(a.tiebreak_ts.cmp(&b.tiebreak_ts))
                    .then(a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal))
            } else {
                by_rank.then(a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal))
            }
        })
        .map(|candidate| RecoveryCandidate {
            alias: candidate.alias,
            bills_credits: candidate.bills_credits,
            cost: candidate.cost,
        })
}

#[derive(Debug, Clone, PartialEq)]
struct RecoveryClass {
    bills_credits: bool,
    score: f64,
    /// The account is out of rate-limit headroom, so only redeeming a banked
    /// reset can make it usable.
    needs_reset: bool,
}

/// Classify an account for spend-cap recovery, or `None` when it must not be
/// used: usage-based (bills real credits), or out of headroom with no banked
/// reset to clear the window.
fn recovery_class(usage: &api::RateLimitResponse) -> Option<RecoveryClass> {
    if usage.billing_class() != api::BillingClass::RateLimited {
        return None;
    }
    // Spend cap reached => overage closed => the account hard-stops at 100% and
    // never bills. Spend cap NOT reached => overage open => it draws credits ($).
    let bills_credits = !usage.spend_control.as_ref().is_some_and(|s| s.reached);
    let score = rate_limit_score(usage);
    if score >= RATE_LIMIT_EXHAUSTED {
        // A rate-limit window is already at 100%. The backend only reports a
        // credit as applicable once there is a window to clear, so this is the
        // authoritative test for "a reset would actually make this usable".
        if usage.reset_credits_applicable() <= 0 {
            return None;
        }
        return Some(RecoveryClass {
            bills_credits,
            score,
            needs_reset: true,
        });
    }
    Some(RecoveryClass {
        bills_credits,
        score,
        needs_reset: false,
    })
}

/// When this account's exhausted window(s) would clear without spending a
/// credit — the moment a banked reset stops being worth anything here.
fn natural_reset_ts(usage: &api::RateLimitResponse) -> Option<i64> {
    // Reset credits and general recovery use the main Codex bucket. Additional
    // buckets are model- or feature-specific and cannot be applied without a
    // requested-model-to-bucket mapping.
    let rate_limit = usage.rate_limit.as_ref()?;
    [rate_limit.short_window(), rate_limit.long_window()]
        .into_iter()
        .flatten()
        .filter(|w| w.used_percent >= 100.0)
        .filter_map(|w| w.reset_timestamp())
        .max()
}

/// Plan which banked reset to spend: the soonest-expiring redeemable one.
///
/// A missing or unreadable listing still yields a plan with no credit id — the
/// usage response already confirmed a credit is applicable, so the backend can
/// pick one itself.
fn reset_plan(
    details: Option<&api::ResetCreditsDetails>,
    natural_reset_ts: Option<i64>,
) -> ResetPlan {
    let credit = details.and_then(|d| {
        d.credits
            .iter()
            .filter(|c| c.is_available())
            .min_by_key(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
    });
    let Some(credit) = credit else {
        return ResetPlan {
            credit_id: None,
            expires_at: None,
            expires_unused: false,
        };
    };
    let expires_at = credit.expires_at_timestamp();
    let expires_unused = match (expires_at, natural_reset_ts) {
        (Some(expiry), Some(reset)) => expiry < reset,
        _ => false,
    };
    ResetPlan {
        credit_id: Some(credit.id.clone()),
        expires_at,
        expires_unused,
    }
}

fn rate_limit_score(usage: &api::RateLimitResponse) -> f64 {
    // General account selection uses the backward-compatible main Codex
    // bucket. Treating any model-specific additional bucket as account-wide
    // exhaustion would hide capacity that other models can still use.
    usage
        .rate_limit
        .as_ref()
        .map_or(0.0, api::RateLimit::availability_score)
}

fn selection_bills_credits(usage: &api::RateLimitResponse) -> bool {
    !usage.spend_control.as_ref().is_some_and(|s| s.reached)
}

fn fetch_usages(
    profiles: &[profile::Profile],
) -> Result<Vec<(String, Option<api::RateLimitResponse>)>> {
    let paths = config::default_paths()?;
    let active = profile::get_active_from(&paths)?;
    let rt = tokio::runtime::Runtime::new()?;
    let client = api::http_client()?;

    Ok(rt.block_on(async {
        let futs: Vec<_> = profiles
            .iter()
            .map(|p| {
                let client = client.clone();
                let alias = p.meta.alias.clone();
                let auth_path =
                    profile::auth_json_path_for_profile_from(&paths, p, active.as_deref());
                let auth = api::read_auth_json(&auth_path);
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

/// Read the banked-reset listing for the given aliases, in parallel. A profile
/// whose listing cannot be read yields `None` rather than failing the pick.
fn fetch_reset_credits(
    aliases: &[String],
) -> Result<Vec<(String, Option<api::ResetCreditsDetails>)>> {
    if aliases.is_empty() {
        return Ok(Vec::new());
    }

    let rt = tokio::runtime::Runtime::new()?;
    let client = api::http_client()?;
    let paths = config::default_paths()?;
    let active = profile::get_active_from(&paths)?;

    Ok(rt.block_on(async {
        let futs: Vec<_> = aliases
            .iter()
            .map(|alias| {
                let client = client.clone();
                let alias = alias.clone();
                let auth = profile::get_profile_from(&paths, &alias).and_then(|p| {
                    let path =
                        profile::auth_json_path_for_profile_from(&paths, &p, active.as_deref());
                    api::read_auth_json(&path)
                });
                async move {
                    let details = match auth {
                        Ok(a) => api::fetch_reset_credits_async(
                            &client,
                            &a.access_token,
                            a.account_id.as_deref(),
                        )
                        .await
                        .ok(),
                        Err(_) => None,
                    };
                    (alias, details)
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

    fn window_resetting_at(used_percent: f64, reset_at: i64) -> api::RateLimitWindow {
        api::RateLimitWindow {
            used_percent,
            window_minutes: None,
            limit_window_seconds: None,
            resets_at: Some(reset_at),
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
            additional_rate_limits: Vec::new(),
            rate_limit_reset_credits: None,
        }
    }

    /// A team account with `applicable` banked resets redeemable right now.
    fn team_response_with_resets(
        h5: f64,
        d7: f64,
        spend_reached: bool,
        applicable: i64,
    ) -> api::RateLimitResponse {
        api::RateLimitResponse {
            rate_limit_reset_credits: Some(api::ResetCreditsSummary {
                available_count: applicable,
                applicable_available_count: applicable,
            }),
            ..team_response(h5, d7, spend_reached)
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
            additional_rate_limits: Vec::new(),
            rate_limit_reset_credits: None,
        }
    }

    fn unknown_billing_response() -> api::RateLimitResponse {
        api::RateLimitResponse {
            plan_type: Some("new_subscription_tier".to_string()),
            rate_limit: None,
            credits: None,
            spend_control: None,
            additional_rate_limits: Vec::new(),
            rate_limit_reset_credits: None,
        }
    }

    fn credit(id: &str, status: &str, expires_at: Option<&str>) -> api::ResetCredit {
        api::ResetCredit {
            id: id.to_string(),
            status: status.to_string(),
            reset_type: Some("codex_rate_limits".to_string()),
            granted_at: None,
            expires_at: expires_at.map(|e| e.to_string()),
            title: None,
            description: None,
        }
    }

    #[test]
    fn unknown_billing_is_never_auto_selected_or_recovered() {
        let usage = unknown_billing_response();

        assert_eq!(usage.billing_class(), api::BillingClass::Unknown);
        assert_eq!(selection_score(&usage), f64::MAX);
        assert_eq!(recovery_class(&usage), None);

        let empty_rate_limit = api::RateLimitResponse {
            rate_limit: Some(api::RateLimit {
                primary: None,
                secondary: None,
                primary_window: None,
                secondary_window: None,
            }),
            ..unknown_billing_response()
        };
        assert_eq!(empty_rate_limit.billing_class(), api::BillingClass::Unknown);
        assert_eq!(selection_score(&empty_rate_limit), f64::MAX);
        assert_eq!(recovery_class(&empty_rate_limit), None);
    }

    #[test]
    fn credits_without_plan_metadata_are_treated_as_usage_based() {
        let usage = api::RateLimitResponse {
            credits: Some(api::Credits {
                has_credits: true,
                unlimited: false,
                overage_limit_reached: false,
                balance: Some("10".to_string()),
            }),
            ..unknown_billing_response()
        };

        assert_eq!(usage.billing_class(), api::BillingClass::UsageBased);
        assert_eq!(selection_score(&usage), f64::MAX);
        assert_eq!(recovery_class(&usage), None);
    }

    fn scored(
        alias: &str,
        bills_credits: bool,
        score: f64,
        tiebreak_ts: i64,
        cost: RecoveryCost,
    ) -> ScoredRecovery {
        ScoredRecovery {
            alias: alias.to_string(),
            bills_credits,
            score,
            tiebreak_ts,
            cost,
        }
    }

    fn headroom(alias: &str, bills_credits: bool, score: f64, ts: i64) -> ScoredRecovery {
        scored(alias, bills_credits, score, ts, RecoveryCost::Headroom)
    }

    fn with_reset(
        alias: &str,
        bills_credits: bool,
        score: f64,
        ts: i64,
        expires_unused: bool,
    ) -> ScoredRecovery {
        scored(
            alias,
            bills_credits,
            score,
            ts,
            RecoveryCost::ResetCredit(ResetPlan {
                credit_id: Some(format!("credit-{alias}")),
                expires_at: Some(ts),
                expires_unused,
            }),
        )
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
    fn selection_never_picks_unknown_or_mixed_billing_accounts() {
        let mut unknown = team_response(5.0, 10.0, true);
        unknown.plan_type = Some("new_plan".to_string());
        assert_eq!(selection_score(&unknown), f64::MAX);
        assert_eq!(recovery_class(&unknown), None);

        let mut mixed = team_response(5.0, 10.0, true);
        mixed.credits = Some(api::Credits {
            has_credits: true,
            unlimited: false,
            overage_limit_reached: false,
            balance: None,
        });
        assert_eq!(selection_score(&mixed), f64::MAX);
        assert_eq!(recovery_class(&mixed), None);
    }

    #[test]
    fn recovery_skips_usage_based_accounts() {
        assert_eq!(recovery_class(&usage_based_response()), None);
    }

    #[test]
    fn recovery_skips_exhausted_accounts_without_a_redeemable_reset() {
        // 5h window already at 100% and no banked reset -> nothing can make it
        // usable right now, even though the spend cap is reached (no-bill).
        assert_eq!(recovery_class(&team_response(100.0, 20.0, true)), None);
    }

    #[test]
    fn recovery_keeps_exhausted_account_that_can_redeem_a_reset() {
        // Same exhausted account, but a banked reset is applicable: it becomes
        // usable again at the cost of one credit.
        let class = recovery_class(&team_response_with_resets(100.0, 20.0, true, 2)).unwrap();
        assert!(class.needs_reset);
        assert!(!class.bills_credits);
    }

    #[test]
    fn recovery_ignores_banked_resets_that_are_not_applicable_yet() {
        // Credits held but none applicable — the backend reports zero until a
        // window is actually exhausted, so redeeming would waste one.
        let usage = api::RateLimitResponse {
            rate_limit_reset_credits: Some(api::ResetCreditsSummary {
                available_count: 3,
                applicable_available_count: 0,
            }),
            ..team_response(100.0, 20.0, true)
        };
        assert_eq!(recovery_class(&usage), None);
    }

    #[test]
    fn recovery_treats_spend_capped_account_as_no_bill() {
        // Spend cap reached + headroom -> usable and hard-stops, so won't bill.
        let class = recovery_class(&team_response(10.0, 30.0, true)).unwrap();
        assert!(
            !class.bills_credits,
            "spend-cap-reached accounts hard-stop at 100% and must be treated as no-bill"
        );
        assert!(!class.needs_reset);
    }

    #[test]
    fn recovery_treats_overage_open_account_as_billing() {
        // Spend cap NOT reached -> overage open -> draws credits past 100%.
        let class = recovery_class(&team_response(5.0, 71.0, false)).unwrap();
        assert!(
            class.bills_credits,
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
    fn most_available_selection_keeps_no_bill_first() {
        // The reset-order opt-out cannot disable the billing safety order.
        let scored = vec![
            selection_candidate("billing", true, 20.0, 1000),
            selection_candidate("no-bill", false, 60.0, 2000),
        ];
        assert_eq!(select_most_available(&scored, false), Some("no-bill"));
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

    /// The gate that lets `codexctl use` reach for a banked reset: it must
    /// report "nothing usable" rather than handing back an exhausted account.
    #[test]
    fn select_with_headroom_returns_nothing_when_every_account_is_exhausted() {
        let scored = vec![
            selection_candidate("a", false, 700.0, 2000),
            selection_candidate("b", false, 900.0, 1000),
        ];
        for mode in [true, false] {
            assert_eq!(
                select_with_headroom(&scored, mode),
                None,
                "reset_aware={mode}"
            );
        }
        // ...while the legacy fallback still yields the least-bad account.
        assert_eq!(select_most_available(&scored, true), Some("a"));
    }

    /// Most-headroom mode must not bypass the no-bill safety order.
    #[test]
    fn select_with_headroom_keeps_no_bill_first_in_most_available_mode() {
        let scored = vec![
            selection_candidate("billing", true, 20.0, 1000),
            selection_candidate("no-bill", false, 60.0, 2000),
            selection_candidate("exhausted", false, 700.0, 500),
        ];
        assert_eq!(select_with_headroom(&scored, false), Some("no-bill"));
        assert_eq!(select_most_available(&scored, false), Some("no-bill"));
    }

    #[test]
    fn billing_selection_warns_and_does_not_ask_for_permission() {
        let mut output = Vec::new();
        notify_billing_account_to("billing", true, &mut output);

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "codexctl: billing can use paid credits after its included usage; no billing confirmation is required\n"
        );
    }

    #[test]
    fn no_bill_selection_does_not_warn() {
        let mut output = Vec::new();
        notify_billing_account_to("no-bill", false, &mut output);

        assert!(output.is_empty());
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
            headroom("bills", true, 10.0, 1000),
            headroom("nobill", false, 50.0, 5000),
        ];
        assert_eq!(select_recovery(candidates, true).unwrap().alias, "nobill");
    }

    #[test]
    fn reset_aware_recovery_breaks_ties_by_soonest_reset() {
        let candidates = vec![
            headroom("late", false, 10.0, 5000),
            headroom("soon", false, 40.0, 1000),
        ];
        assert_eq!(
            select_recovery(candidates.clone(), true).unwrap().alias,
            "soon"
        );
        // Default ignores reset and keeps the most-headroom pick.
        assert_eq!(select_recovery(candidates, false).unwrap().alias, "late");
    }

    #[test]
    fn recovery_prefers_headroom_over_spending_a_banked_reset() {
        // Redeeming is free in money but spends a scarce, expiring asset, so an
        // account that just works must always win.
        let candidates = vec![
            with_reset("exhausted", false, 700.0, 1000, false),
            headroom("free", false, 90.0, 5000),
        ];
        for mode in [true, false] {
            assert_eq!(
                select_recovery(candidates.clone(), mode).unwrap().alias,
                "free",
                "reset_aware={mode}"
            );
        }
    }

    #[test]
    fn recovery_prefers_a_banked_reset_over_billing_credits() {
        // A reset costs no money; a billing account eventually does. When no
        // free-and-ready account is left, redeem before reaching for credits.
        let candidates = vec![
            headroom("billing", true, 10.0, 1000),
            with_reset("reset", false, 700.0, 5000, false),
        ];
        for mode in [true, false] {
            let picked = select_recovery(candidates.clone(), mode).unwrap();
            assert_eq!(picked.alias, "reset", "reset_aware={mode}");
            assert!(picked.reset_plan().is_some());
        }
    }

    #[test]
    fn recovery_spends_a_lapsing_reset_before_a_bankable_one() {
        // A credit that expires before its window would reset anyway is
        // use-it-or-lose-it: spending it costs nothing.
        let candidates = vec![
            with_reset("bankable", false, 700.0, 1000, false),
            with_reset("lapsing", false, 900.0, 5000, true),
        ];
        for mode in [true, false] {
            assert_eq!(
                select_recovery(candidates.clone(), mode).unwrap().alias,
                "lapsing",
                "reset_aware={mode}"
            );
        }
    }

    #[test]
    fn recovery_salvages_a_lapsing_credit_before_switching_to_a_billing_account() {
        // Both options leave a billing-capable account in hand, but one also
        // rescues a credit that would otherwise evaporate.
        let candidates = vec![
            headroom("billing", true, 10.0, 1000),
            with_reset("lapsing", true, 700.0, 5000, true),
        ];
        for mode in [true, false] {
            assert_eq!(
                select_recovery(candidates.clone(), mode).unwrap().alias,
                "lapsing",
                "reset_aware={mode}"
            );
        }
    }

    #[test]
    fn recovery_still_prefers_billing_headroom_over_spending_a_bankable_credit() {
        let candidates = vec![
            headroom("billing", true, 10.0, 1000),
            with_reset("bankable", true, 700.0, 5000, false),
        ];
        assert_eq!(
            select_recovery(candidates, true).unwrap().alias,
            "billing",
            "a credit worth keeping must not be spent to avoid a billing switch"
        );
    }

    #[test]
    fn reset_aware_recovery_spends_the_soonest_expiring_credit_first() {
        let candidates = vec![
            with_reset("later", false, 700.0, 5000, false),
            with_reset("sooner", false, 700.0, 1000, false),
        ];
        assert_eq!(
            select_recovery(candidates, true).unwrap().alias,
            "sooner",
            "drain the credit closest to lapsing"
        );
    }

    #[test]
    fn natural_reset_ts_uses_the_latest_exhausted_window() {
        // Only exhausted windows gate the account; the 7d one clears last.
        let usage = api::RateLimitResponse {
            rate_limit: Some(api::RateLimit {
                primary: Some(window_resetting_at(100.0, 1_000)),
                secondary: Some(window_resetting_at(100.0, 9_000)),
                primary_window: None,
                secondary_window: None,
            }),
            ..team_response(100.0, 100.0, true)
        };
        assert_eq!(natural_reset_ts(&usage), Some(9_000));
    }

    #[test]
    fn natural_reset_ts_ignores_windows_with_headroom() {
        let usage = api::RateLimitResponse {
            rate_limit: Some(api::RateLimit {
                primary: Some(window_resetting_at(100.0, 1_000)),
                secondary: Some(window_resetting_at(40.0, 9_000)),
                primary_window: None,
                secondary_window: None,
            }),
            ..team_response(100.0, 40.0, true)
        };
        assert_eq!(natural_reset_ts(&usage), Some(1_000));
    }

    #[test]
    fn reset_plan_picks_the_soonest_expiring_available_credit() {
        let details = api::ResetCreditsDetails {
            credits: vec![
                credit("late", "available", Some("2026-08-12T00:00:00Z")),
                credit("soon", "available", Some("2026-07-26T00:00:00Z")),
                // Already spent, and expiring earliest — must be ignored.
                credit("spent", "redeemed", Some("2026-07-01T00:00:00Z")),
            ],
            available_count: 2,
        };
        let plan = reset_plan(Some(&details), None);
        assert_eq!(plan.credit_id.as_deref(), Some("soon"));
    }

    #[test]
    fn reset_plan_flags_a_credit_that_would_lapse_before_the_window_resets() {
        let expires_at = chrono::DateTime::parse_from_rfc3339("2026-07-26T00:00:00Z")
            .unwrap()
            .timestamp();
        let details = api::ResetCreditsDetails {
            credits: vec![credit("c", "available", Some("2026-07-26T00:00:00Z"))],
            available_count: 1,
        };

        // Window clears after the credit lapses -> the credit is worthless if
        // held, so spending it is free.
        assert!(reset_plan(Some(&details), Some(expires_at + 60)).expires_unused);
        // Window clears first -> the credit stays bankable for a later crunch.
        assert!(!reset_plan(Some(&details), Some(expires_at - 60)).expires_unused);
        // Unknown reset time -> never assume the credit is free to burn.
        assert!(!reset_plan(Some(&details), None).expires_unused);
    }

    #[test]
    fn reset_plan_without_a_readable_listing_defers_the_pick_to_the_backend() {
        // The usage response already confirmed a credit is applicable, so a
        // failed listing must not block recovery.
        let plan = reset_plan(None, Some(1_000));
        assert_eq!(plan.credit_id, None);
        assert!(!plan.expires_unused);
    }
}
