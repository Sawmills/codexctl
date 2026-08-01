//! Banked rate-limit resets: grants that clear an exhausted usage window on
//! demand instead of waiting for it to lapse.
//!
//! Credits are scarce, per-account, and expire ~30 days after they are granted
//! with no refund, so every redemption here is deliberate: codexctl only spends
//! one when the backend reports it as applicable, and it always spends the
//! credit closest to lapsing first.

use anyhow::{Context, Result, bail};
use comfy_table::{Cell, Color, Table, presets::UTF8_FULL_CONDENSED};

use crate::api;
use crate::commands::status;
use crate::config;
use crate::profile;

/// `codexctl resets` — list banked resets across every saved profile.
pub fn run_list() -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;
    let rows = fetch_all(&profiles, &active)?;

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Account", "Banked", "Redeemable", "Expiries"]);
    let mut total = 0;
    let mut redeemable = 0;
    for row in &rows {
        total += row.available;
        redeemable += row.applicable;
        table.add_row(render_row(row));
    }
    println!("{table}");

    println!();
    println!("{total} banked, {redeemable} redeemable now.");
    if redeemable > 0 {
        println!("Redeem with `codexctl reset <alias>`.");
    } else if total > 0 {
        println!(
            "A reset only applies to an exhausted window, so none can be redeemed until an\naccount hits 100%."
        );
    }

    Ok(())
}

/// `codexctl resets --claim` — redeem every credit that is about to lapse.
///
/// A credit is only worth claiming when it is *applicable*: the account is
/// already at 100%, so the reset buys back capacity that is unavailable right
/// now. Combined with a near expiry, holding it back has almost no option value
/// left — the alternative is watching it evaporate.
pub fn run_claim(within_days: i64, assume_yes: bool) -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        println!("no profiles saved. Use 'codexctl save' to save the current account.");
        return Ok(());
    }

    let active = profile::get_active()?;
    let rows = fetch_all(&profiles, &active)?;
    let deadline = chrono::Utc::now().timestamp() + within_days * 24 * 60 * 60;

    let mut claimable: Vec<(&ResetsRow, &api::ResetCredit)> = Vec::new();
    for row in &rows {
        if row.applicable <= 0 {
            continue;
        }
        if let Some(credit) = row
            .credits
            .iter()
            .filter(|c| c.is_available())
            .filter(|c| c.expires_at_timestamp().is_some_and(|e| e <= deadline))
            .min_by_key(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
        {
            claimable.push((row, credit));
        }
    }

    if claimable.is_empty() {
        println!("no banked resets lapse within {within_days} day(s) on an exhausted account.");
        return Ok(());
    }

    println!(
        "{} banked reset(s) lapse within {within_days} day(s) on accounts that are already at 100%:",
        claimable.len()
    );
    for (row, credit) in &claimable {
        println!(
            "  {} — expires {}",
            row.alias,
            credit
                .expires_at
                .as_deref()
                .map(format_date)
                .unwrap_or_else(|| "-".to_string())
        );
    }
    println!("Redeeming them is not refundable.");

    if !assume_yes && !confirm("Redeem all of them?")? {
        bail!("claim cancelled");
    }

    let mut redeemed = 0;
    for (row, credit) in &claimable {
        match redeem(&row.alias, Some(&credit.id)) {
            Ok(response) => {
                report_outcome(&row.alias, &response);
                if response.code == api::ConsumeResetCode::Reset {
                    redeemed += 1;
                }
            }
            // One bad seat must not strand the rest of the sweep.
            Err(e) => eprintln!("{}: redemption failed: {e:#}", row.alias),
        }
    }

    println!();
    println!("{redeemed} of {} redeemed.", claimable.len());
    Ok(())
}

/// `codexctl reset [alias]` — redeem one banked reset.
pub fn run_redeem(alias: Option<&str>, assume_yes: bool, credit_id: Option<&str>) -> Result<()> {
    let alias = match alias {
        Some(alias) => alias.to_string(),
        None => profile::get_active()?
            .context("no active profile; pass an alias: codexctl reset <alias>")?,
    };
    // Fail early on an unknown alias rather than after a round trip.
    profile::get_profile(&alias)?;

    let account = fetch_one(&alias, is_active(&alias)?)?;
    if account.applicable <= 0 {
        if account.available <= 0 {
            bail!("{alias} has no banked resets to redeem");
        }
        bail!(
            "{alias} has {} banked reset(s) but none apply right now — a reset only clears an \
             already-exhausted window, so redeeming would waste one. Check `codexctl status`.",
            account.available
        );
    }

    let chosen = match credit_id {
        Some(id) => {
            let credit = account
                .credits
                .iter()
                .find(|c| c.id == id)
                .with_context(|| format!("{alias} has no reset credit {id}"))?;
            if !credit.is_available() {
                bail!(
                    "reset credit {id} is not available (status: {})",
                    credit.status
                );
            }
            Some(credit.clone())
        }
        // Spend the credit closest to lapsing first.
        None => account
            .credits
            .iter()
            .filter(|c| c.is_available())
            .min_by_key(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
            .cloned(),
    };

    println!(
        "{alias}: {} banked reset(s), {} redeemable now.",
        account.available, account.applicable
    );
    if let Some(credit) = &chosen {
        println!(
            "Redeeming {}{}.",
            credit.title.as_deref().unwrap_or("reset credit"),
            match credit.expires_at.as_deref() {
                Some(expiry) => format!(" (expires {})", format_date(expiry)),
                None => String::new(),
            }
        );
    }
    println!("This is not refundable.");

    if !assume_yes && !confirm(&format!("Redeem a banked reset for {alias}?"))? {
        bail!("redemption cancelled");
    }

    let response = redeem(&alias, chosen.as_ref().map(|c| c.id.as_str()))?;
    report_outcome(&alias, &response);

    if response.code == api::ConsumeResetCode::Reset {
        settle_after_redeem(&alias);
        println!();
        status::run_focused(&alias)?;
    }

    Ok(())
}

/// Redeem one banked reset for `alias`, spending `credit_id` when given.
pub fn redeem(alias: &str, credit_id: Option<&str>) -> Result<api::ConsumeResetResponse> {
    let auth = auth_for(alias, is_active(alias)?)?;
    let request_id = api::new_redeem_request_id(alias);
    let rt = tokio::runtime::Runtime::new()?;
    let client = api::http_client()?;
    rt.block_on(api::consume_reset_credit_async(
        &client,
        &auth.access_token,
        auth.account_id.as_deref(),
        &request_id,
        credit_id,
    ))
}

/// Decide whether to spend a banked reset for `alias`, reporting the reasoning
/// to `out` (stdout for interactive commands, stderr for the Codex wrapper).
///
/// A credit that would lapse before its window resets anyway is approved with
/// no prompt: holding it back cannot pay off, so there is nothing to weigh.
/// Anything else needs `assume_yes` or a human on a terminal — a credit is
/// scarce and non-refundable, so it is never spent unattended by default.
pub fn approve_redemption(
    alias: &str,
    expires_at: Option<i64>,
    expires_unused: bool,
    assume_yes: bool,
    out: &mut impl std::io::Write,
) -> bool {
    use std::io::IsTerminal;

    if expires_unused {
        let _ = writeln!(
            out,
            "codexctl: {alias}'s banked reset expires before its window resets; redeeming it now (it would otherwise lapse unused)"
        );
        return true;
    }
    if assume_yes {
        let _ = writeln!(out, "codexctl: redeeming a banked reset for {alias}");
        return true;
    }
    if !std::io::stdin().is_terminal() {
        let _ = writeln!(
            out,
            "codexctl: not redeeming a banked reset for {alias} (no terminal to approve; pass --allow-resets to allow)"
        );
        return false;
    }

    let expiry = expires_at
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| {
            format!(
                " (expires {})",
                dt.with_timezone(&chrono::Local).format("%b %d")
            )
        })
        .unwrap_or_default();
    let _ = write!(
        out,
        "codexctl: redeem a banked reset for {alias}{expiry}? It is not refundable. [y/N] "
    );
    let _ = out.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Wait, briefly, for a redemption to show up on the usage endpoint.
///
/// The consume call returns before `wham/usage` reflects the cleared window, so
/// reading status immediately after a redeem reports the old 100% and makes a
/// successful redemption look like it did nothing. Poll until the account stops
/// reporting a redeemable credit — that flips only once the window is clear —
/// and give up quietly, since this is cosmetic.
pub fn settle_after_redeem(alias: &str) {
    const ATTEMPTS: u32 = 6;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(500);

    let Ok(is_active) = is_active(alias) else {
        return;
    };
    let Ok(auth) = auth_for(alias, is_active) else {
        return;
    };
    let Ok(rt) = tokio::runtime::Runtime::new() else {
        return;
    };
    let Ok(client) = api::http_client() else {
        return;
    };

    for _ in 0..ATTEMPTS {
        std::thread::sleep(DELAY);
        let usage = rt.block_on(api::fetch_usage_async(
            &client,
            &auth.access_token,
            auth.account_id.as_deref(),
        ));
        if let Ok(usage) = usage
            && usage.reset_credits_applicable() == 0
        {
            return;
        }
    }
}

/// Print what the backend did with the redemption.
pub fn report_outcome(alias: &str, response: &api::ConsumeResetResponse) {
    match response.code {
        api::ConsumeResetCode::Reset => {
            let windows = response.windows_reset;
            if windows > 0 {
                println!("{alias}: reset redeemed — {windows} window(s) cleared.");
            } else {
                println!("{alias}: reset redeemed.");
            }
        }
        api::ConsumeResetCode::NothingToReset => {
            println!("{alias}: nothing to reset — no credit was spent.");
        }
        api::ConsumeResetCode::NoCredit => {
            println!("{alias}: no banked reset available.");
        }
        api::ConsumeResetCode::AlreadyRedeemed => {
            println!("{alias}: this redemption was already applied.");
        }
        api::ConsumeResetCode::Unknown => {
            println!("{alias}: unrecognized redemption outcome.");
        }
    }
}

struct ResetsRow {
    alias: String,
    is_active: bool,
    available: i64,
    applicable: i64,
    credits: Vec<api::ResetCredit>,
    error: Option<String>,
}

fn fetch_all(profiles: &[profile::Profile], active: &Option<String>) -> Result<Vec<ResetsRow>> {
    let paths = config::default_paths()?;
    let rt = tokio::runtime::Runtime::new()?;
    let client = api::http_client()?;

    Ok(rt.block_on(async {
        let futs: Vec<_> = profiles
            .iter()
            .map(|p| {
                let client = client.clone();
                let alias = p.meta.alias.clone();
                let is_active = active.as_deref() == Some(&p.meta.alias);
                let auth_path =
                    profile::auth_json_path_for_profile_from(&paths, p, active.as_deref());
                async move {
                    match api::read_auth_json(&auth_path) {
                        Ok(auth) => fetch_row(&client, alias, is_active, &auth).await,
                        Err(_) => ResetsRow {
                            alias,
                            is_active,
                            available: 0,
                            applicable: 0,
                            credits: Vec::new(),
                            error: Some("bad auth.json".to_string()),
                        },
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await
    }))
}

async fn fetch_row(
    client: &reqwest::Client,
    alias: String,
    is_active: bool,
    auth: &api::AuthJson,
) -> ResetsRow {
    let account_id = auth.account_id.as_deref();
    // The usage response carries the applicable count — whether a credit can be
    // spent right now — which the credit listing alone does not report.
    let usage = api::fetch_usage_async(client, &auth.access_token, account_id).await;
    let details = api::fetch_reset_credits_async(client, &auth.access_token, account_id).await;

    match (usage, details) {
        (Ok(usage), Ok(details)) => ResetsRow {
            alias,
            is_active,
            available: usage.reset_credits_available().max(details.available_count),
            applicable: usage.reset_credits_applicable(),
            credits: details.credits,
            error: None,
        },
        (usage, details) => {
            let err = usage
                .err()
                .or_else(|| details.err())
                .map(|e| e.to_string())
                .unwrap_or_else(|| "error".to_string());
            ResetsRow {
                alias,
                is_active,
                available: 0,
                applicable: 0,
                credits: Vec::new(),
                error: Some(if err.contains("expired") {
                    "expired".to_string()
                } else {
                    "error".to_string()
                }),
            }
        }
    }
}

fn fetch_one(alias: &str, is_active: bool) -> Result<ResetsRow> {
    let auth = auth_for(alias, is_active)?;
    let rt = tokio::runtime::Runtime::new()?;
    let client = api::http_client()?;
    let row = rt.block_on(fetch_row(&client, alias.to_string(), is_active, &auth));
    if let Some(error) = &row.error {
        bail!("could not read reset credits for {alias}: {error}");
    }
    Ok(row)
}

/// The active profile's live tokens are Codex-maintained and authoritative; the
/// stored snapshot can be stale until the next switch or save.
fn auth_for(alias: &str, is_active: bool) -> Result<api::AuthJson> {
    let paths = config::default_paths()?;
    let profile = profile::get_profile_from(&paths, alias)?;
    let active = is_active.then_some(alias);
    let path = profile::auth_json_path_for_profile_from(&paths, &profile, active);
    api::read_auth_json(&path)
}

fn is_active(alias: &str) -> Result<bool> {
    Ok(profile::get_active()?.as_deref() == Some(alias))
}

fn render_row(row: &ResetsRow) -> Vec<Cell> {
    let alias = if row.is_active {
        format!("* {}", row.alias)
    } else {
        row.alias.clone()
    };

    if let Some(error) = &row.error {
        return vec![
            Cell::new(alias),
            Cell::new("-"),
            Cell::new("-"),
            Cell::new(error).fg(Color::Red),
        ];
    }

    let redeemable = if row.applicable > 0 {
        Cell::new(row.applicable.to_string()).fg(Color::Green)
    } else {
        Cell::new("0")
    };

    vec![
        Cell::new(alias),
        Cell::new(row.available.to_string()),
        redeemable,
        expiries_cell(&row.credits),
    ]
}

/// Expiry dates of the redeemable credits, soonest first. A credit lapsing
/// within [`EXPIRY_WARN_SECONDS`] is called out — an unspent credit is simply
/// lost, which is the whole reason to surface this column.
fn expiries_cell(credits: &[api::ResetCredit]) -> Cell {
    let mut expiries: Vec<(Option<i64>, String)> = credits
        .iter()
        .filter(|c| c.is_available())
        .map(|c| {
            (
                c.expires_at_timestamp(),
                c.expires_at
                    .as_deref()
                    .map(format_date)
                    .unwrap_or_else(|| "-".to_string()),
            )
        })
        .collect();
    if expiries.is_empty() {
        return Cell::new("-");
    }
    expiries.sort_by_key(|(ts, _)| ts.unwrap_or(i64::MAX));

    let soonest = expiries.first().and_then(|(ts, _)| *ts);
    let text = expiries
        .iter()
        .map(|(_, label)| label.clone())
        .collect::<Vec<_>>()
        .join(", ");

    match soonest {
        Some(ts) if ts - chrono::Utc::now().timestamp() <= EXPIRY_WARN_SECONDS => {
            Cell::new(text).fg(Color::Red)
        }
        _ => Cell::new(text),
    }
}

/// A credit this close to lapsing is effectively use-it-or-lose-it.
pub const EXPIRY_WARN_SECONDS: i64 = 3 * 24 * 60 * 60;

fn format_date(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%b %d").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::IsTerminal;
    // A credit is scarce and non-refundable, so never spend one that a human
    // could not have approved.
    if !std::io::stdin().is_terminal() {
        bail!("no terminal to confirm on; pass --yes to redeem unattended");
    }
    Ok(dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credit(id: &str, status: &str, expires_at: Option<&str>) -> api::ResetCredit {
        api::ResetCredit {
            id: id.to_string(),
            status: status.to_string(),
            reset_type: None,
            granted_at: None,
            expires_at: expires_at.map(|e| e.to_string()),
            title: None,
            description: None,
        }
    }

    // Expiries render in local time, so these compare against `format_date`
    // rather than hard-coded dates.
    const SOON: &str = "2036-07-26T00:00:00Z";
    const LATE: &str = "2036-08-12T00:00:00Z";

    #[test]
    fn expiries_list_available_credits_soonest_first() {
        let credits = vec![
            credit("late", "available", Some(LATE)),
            credit("soon", "available", Some(SOON)),
        ];
        let rendered = expiries_cell(&credits).content().to_string();
        let soon = rendered
            .find(&format_date(SOON))
            .expect("soonest expiry listed");
        let late = rendered
            .find(&format_date(LATE))
            .expect("later expiry listed");
        assert!(soon < late, "soonest expiry must come first: {rendered}");
    }

    #[test]
    fn expiries_ignore_credits_that_cannot_be_redeemed() {
        let credits = vec![
            credit("spent", "redeemed", Some(SOON)),
            credit("live", "available", Some(LATE)),
        ];
        let rendered = expiries_cell(&credits).content().to_string();
        assert!(
            !rendered.contains(&format_date(SOON)),
            "redeemed credit listed: {rendered}"
        );
        assert!(rendered.contains(&format_date(LATE)));
    }

    #[test]
    fn expiries_render_a_dash_when_nothing_is_redeemable() {
        let credits = vec![credit("spent", "redeemed", Some(SOON))];
        assert_eq!(expiries_cell(&credits).content().to_string(), "-");
    }
}
