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
    }

    profile::save_profile_and_activate(&resolved_alias, email.as_deref(), &auth_path)?;
    if let Some(label) = label {
        profile::set_label_from(&paths, &resolved_alias, Some(label))?;
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
    let Some(incoming) = incoming_account else {
        return Ok(());
    };
    let Ok(existing) = profile::get_profile_from(paths, alias) else {
        return Ok(());
    };
    let Some(stored) = existing.meta.account_id.as_deref() else {
        return Ok(());
    };
    if stored == incoming {
        return Ok(());
    }
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
        short_workspace(stored),
        short_workspace(incoming)
    )
}

fn short_workspace(account_id: &str) -> String {
    match account_id.char_indices().nth(8) {
        Some((index, _)) => format!("{}…", &account_id[..index]),
        None => account_id.to_string(),
    }
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
