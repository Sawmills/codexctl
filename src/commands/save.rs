use anyhow::{Context, Result};

use crate::api;
use crate::commands::alias;
use crate::config;
use crate::profile;
use crate::store;

pub fn run(alias: Option<&str>, label: Option<&str>) -> Result<()> {
    // Reject a bad label before the save switches the live auth file. Failing
    // afterwards would leave the active account changed under an error exit.
    label.map(store::validate_label).transpose()?;

    let paths = config::default_paths()?;
    let auth_path = paths.codex_auth_json();
    if !auth_path.exists() {
        anyhow::bail!(
            "no auth.json found at {}. Log in with Codex CLI first.",
            auth_path.display()
        );
    }

    let auth = api::read_auth_json(&auth_path)?;
    let identity = api::token_identity(&auth.access_token).unwrap_or_default();

    // The token's own claim is authoritative and costs no network call. Only
    // ask the API when the token carries no profile claim at all.
    let email = identity
        .email
        .clone()
        .or_else(|| fetch_email(&auth.access_token));
    let resolved_alias = match alias::optional(alias)? {
        Some(a) => a.to_string(),
        None => match &email {
            Some(e) => store::validate_alias(e)
                .with_context(|| {
                    format!(
                        "detected email '{e}' is not a usable alias; provide one: codexctl save <alias>"
                    )
                })?
                .to_string(),
            None => {
                anyhow::bail!(
                    "could not detect email (token may be expired). Provide an alias: codexctl save <alias>"
                );
            }
        },
    };

    let existing = store::profile_dir(&paths, &resolved_alias)?;
    let mut overwrite_confirmed = false;
    if existing.exists() {
        refuse_a_different_account(
            &paths,
            &resolved_alias,
            auth.account_id.as_deref(),
            alias::optional(alias)?.is_some(),
        )?;
        eprint!(
            "profile '{}' already exists. Overwrite? [y/N] ",
            resolved_alias
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return Ok(());
        }
        overwrite_confirmed = true;
    }

    // The lock is taken only now: holding it across the prompt above would
    // block every other codexctl process for as long as the operator takes to
    // answer. That makes the checks so far advisory, so they are re-run here
    // against the store as it actually stands at write time.
    let lock = store::lock(&paths)?;
    // Everything above was decided from the auth file as it read at the start,
    // and `codexctl use` can rewrite that file while the prompt waits. The alias
    // and email were derived from that token, so a changed file invalidates the
    // decision itself, not just the checks — copying it now would store one
    // account's credentials under another's alias and email.
    let live_now = api::read_auth_json(&auth_path)?;
    if live_now.access_token != auth.access_token {
        anyhow::bail!(
            "the active account changed while this save was preparing. \
             Re-run the command to save the account that is active now."
        );
    }
    if store::profile_dir(&paths, &resolved_alias)?.exists() {
        refuse_a_different_account(
            &paths,
            &resolved_alias,
            auth.account_id.as_deref(),
            alias::optional(alias)?.is_some(),
        )?;
        // The profile appeared while this command was deciding, so nobody
        // approved overwriting it. Re-prompting is not an option with the lock
        // held, so stop and let the operator run the command again against the
        // store as it now stands.
        if !overwrite_confirmed {
            anyhow::bail!(
                "profile '{resolved_alias}' was created by another process while this save was \
                 preparing. Re-run the command to confirm overwriting it."
            );
        }
    }
    profile::save_profile_and_activate_locked(
        &lock,
        &paths,
        &resolved_alias,
        email.as_deref(),
        &auth_path,
    )?;
    if let Some(label) = label {
        profile::set_label_locked(&lock, &paths, &resolved_alias, Some(label))?;
    }

    println!("saved profile '{}'", resolved_alias);
    Ok(())
}

/// Stop before the overwrite prompt when the target profile holds a *different*
/// workspace.
///
/// Without an explicit alias `save` defaults to the detected email, so a second
/// account on one address lands on the first account's profile. There the
/// destructive answer is a single keystroke, and the right action is always to
/// pick another alias — so this is an error rather than another prompt.
///
/// A refusal needs positive evidence of a different account. When either side
/// has no workspace identifier the command falls through to the usual prompt.
fn refuse_a_different_account(
    paths: &config::Paths,
    alias: &str,
    incoming_account: Option<&str>,
    alias_was_explicit: bool,
) -> Result<()> {
    let Some(stored) = profile::conflicting_workspace(paths, alias, incoming_account) else {
        return Ok(());
    };
    // Naming the remedy matters: an operator who already chose this alias
    // cannot act on "pass an explicit alias".
    let remedy = if alias_was_explicit {
        format!("Choose another alias, or remove it first: codexctl remove {alias}")
    } else {
        "Pass an explicit alias: codexctl save <alias>".to_string()
    };
    anyhow::bail!(
        "profile '{alias}' holds a different account \
         (stored workspace {}, incoming {}). {remedy}",
        profile::short_workspace(&stored),
        incoming_account
            .map(profile::short_workspace)
            .unwrap_or_default()
    )
}

fn fetch_email(access_token: &str) -> Option<String> {
    let client = api::blocking_http_client().ok()?;
    let resp = client
        .get("https://chatgpt.com/backend-api/me")
        .bearer_auth(access_token)
        .send()
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct MeResponse {
        email: Option<String>,
    }

    let me: MeResponse = resp.json().ok()?;
    me.email
}
