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
    // What the operator is agreeing to replace, so the approval cannot be
    // applied to some other credential that lands there while they answer.
    let mut confirmed_state: Option<Option<String>> = None;
    if existing.exists() {
        refuse_a_different_account(
            &paths,
            &resolved_alias,
            auth.account_id.as_deref(),
            api::token_login(&auth.access_token).as_deref(),
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
        confirmed_state = Some(stored_access_token(&existing));
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
    // Snapshot the live file and work from the snapshot for the rest of this
    // command. A native `codex login` or refresh does not take this lock, so
    // re-reading the path later — as the profile writer would — could copy
    // different bytes than the ones checked here.
    let snapshot = paths.codexctl_dir().join(".save-snapshot.json");
    let live_bytes = std::fs::read(&auth_path)
        .with_context(|| format!("failed to read {}", auth_path.display()))?;
    store::atomic_write(&snapshot, &live_bytes)?;
    let saved = save_verified_snapshot(
        &lock,
        &paths,
        &resolved_alias,
        label,
        email.as_deref(),
        &snapshot,
        &auth,
        alias::optional(alias)?.is_some(),
        confirmed_state,
    );
    let _ = std::fs::remove_file(&snapshot);
    saved?;

    println!("saved profile '{}'", resolved_alias);
    Ok(())
}

/// The locked half of `save`, working only from the snapshot taken above.
#[allow(clippy::too_many_arguments)]
fn save_verified_snapshot(
    lock: &store::StoreLock,
    paths: &config::Paths,
    resolved_alias: &str,
    label: Option<&str>,
    email: Option<&str>,
    snapshot: &std::path::Path,
    verified: &api::AuthJson,
    alias_was_explicit: bool,
    confirmed_state: Option<Option<String>>,
) -> Result<()> {
    let live_now = api::read_auth_json(snapshot)?;
    // The workspace can change without the token changing: `auth.json` carries
    // an explicit `account_id` that `read_auth_json` prefers over the JWT claim,
    // so comparing tokens alone would let a switched workspace through under the
    // alias and email resolved for the previous one.
    if live_now.access_token != verified.access_token || live_now.account_id != verified.account_id
    {
        anyhow::bail!(
            "the active account changed while this save was preparing. \
             Re-run the command to save the account that is active now."
        );
    }
    if store::profile_dir(paths, resolved_alias)?.exists() {
        refuse_a_different_account(
            paths,
            resolved_alias,
            live_now.account_id.as_deref(),
            api::token_login(&live_now.access_token).as_deref(),
            alias_was_explicit,
        )?;
        // Re-prompting is not an option with the lock held, so an approval that
        // no longer describes what is stored is refused instead of reused.
        let dir = store::profile_dir(paths, resolved_alias)?;
        match &confirmed_state {
            // The profile appeared while this command was deciding, so nobody
            // approved overwriting it.
            None => anyhow::bail!(
                "profile '{resolved_alias}' was created by another process while this save was \
                 preparing. Re-run the command to confirm overwriting it."
            ),
            // It was approved, but something replaced its credentials since —
            // and the approval was for what used to be there.
            Some(approved) if *approved != stored_access_token(&dir) => anyhow::bail!(
                "profile '{resolved_alias}' changed while this save was preparing, so the \
                 confirmation no longer applies to what it holds. Re-run the command."
            ),
            Some(_) => {}
        }
    }
    profile::save_profile_and_activate_locked(lock, paths, resolved_alias, email, snapshot)?;
    if let Some(label) = label {
        profile::set_label_locked(lock, paths, resolved_alias, Some(label))?;
    }
    Ok(())
}

/// The access token a profile directory currently holds, if any is readable.
/// Used only to tell whether the thing an operator approved replacing is still
/// the thing about to be replaced.
fn stored_access_token(dir: &std::path::Path) -> Option<String> {
    api::read_auth_json(&dir.join("auth.json"))
        .ok()
        .map(|auth| auth.access_token)
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
    incoming_user: Option<&str>,
    alias_was_explicit: bool,
) -> Result<()> {
    let Some(stored) =
        profile::conflicting_workspace(paths, alias, incoming_account, incoming_user)
    else {
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
