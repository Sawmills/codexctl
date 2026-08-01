use anyhow::{Context, Result};

use crate::api;
use crate::commands::alias;
use crate::config;
use crate::profile;
use crate::store;

pub fn run(alias: Option<&str>) -> Result<()> {
    let auth_path = config::codex_auth_json()?;
    if !auth_path.exists() {
        anyhow::bail!(
            "no auth.json found at {}. Log in with Codex CLI first.",
            auth_path.display()
        );
    }

    let auth = api::read_auth_json(&auth_path)?;

    let email = fetch_email(&auth.access_token);
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

    let existing = store::profile_dir(&config::default_paths()?, &resolved_alias)?;
    if existing.exists() {
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

    println!("saved profile '{}'", resolved_alias);
    Ok(())
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
