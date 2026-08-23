use anyhow::{Context, Result};

use crate::api;
use crate::commands::adopt;
use crate::commands::alias;
use crate::config;
use crate::profile;
use crate::store;

pub fn run(alias: Option<&str>, label: Option<&str>, allow_adopt: bool) -> Result<()> {
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
    let mut confirmed_state: Option<Option<Vec<u8>>> = None;
    let mut adoption_approved = false;
    if existing.exists() {
        let adoption = classify_overwrite(
            &paths,
            &resolved_alias,
            auth.account_id.as_deref(),
            api::token_login(&auth.access_token).as_deref(),
            alias::optional(alias)?.is_some(),
        )?;
        // Read before the question is asked: this is the credential the operator
        // is being shown and agreeing to replace. Reading it afterwards would
        // silently adopt whatever landed there while they were deciding.
        let shown = stored_credentials(&existing);
        let approved = match adoption {
            Adoption::Unneeded => {
                eprint!(
                    "profile '{}' already exists. Overwrite? [y/N] ",
                    resolved_alias
                );
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input.trim().eq_ignore_ascii_case("y")
            }
            // The adoption question already asks to replace these credentials,
            // so it stands in for the overwrite prompt rather than following it.
            Adoption::AskOperator { stored } => {
                match adopt::approve_adoption(
                    &resolved_alias,
                    stored.as_deref(),
                    auth.account_id.as_deref(),
                    allow_adopt,
                    &mut std::io::stderr(),
                ) {
                    adopt::Approval::Granted => {
                        adoption_approved = true;
                        true
                    }
                    adopt::Approval::Declined => false,
                    // Not the operator declining — nobody was there to ask. A
                    // quiet exit 0 here would report a save that did not happen.
                    adopt::Approval::NoTerminal => {
                        return Err(adopt::refusal(&resolved_alias, stored.as_deref()));
                    }
                }
            }
        };
        if !approved {
            println!("aborted");
            return Ok(());
        }
        confirmed_state = Some(shown);
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
        adoption_approved,
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
    confirmed_state: Option<Option<Vec<u8>>>,
    adoption_approved: bool,
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
        // Re-run under the lock against the store as it actually stands. An
        // adoption the operator already approved carries over; one that only
        // became necessary since does not, because nobody was asked about it.
        if let Adoption::AskOperator { stored } = classify_overwrite(
            paths,
            resolved_alias,
            live_now.account_id.as_deref(),
            api::token_login(&live_now.access_token).as_deref(),
            alias_was_explicit,
        )? && !adoption_approved
        {
            return Err(adopt::refusal(resolved_alias, stored.as_deref()));
        }
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
            Some(approved) if *approved != stored_credentials(&dir) => anyhow::bail!(
                "profile '{resolved_alias}' changed while this save was preparing, so the \
                 confirmation no longer applies to what it holds. Re-run the command."
            ),
            Some(_) => {}
        }
    }
    profile::save_live_profile_locked(lock, paths, resolved_alias, email, snapshot)?;
    if let Some(label) = label {
        profile::set_label_locked(lock, paths, resolved_alias, Some(label))?;
    }
    Ok(())
}

/// Exactly what a profile directory currently stores, if anything readable.
///
/// The raw bytes rather than one parsed field: a refresh can rotate the refresh
/// token while the access token stays put, and an approval given for the old
/// credential must not be honoured against the new one.
fn stored_credentials(dir: &std::path::Path) -> Option<Vec<u8>> {
    std::fs::read(dir.join("auth.json")).ok()
}

/// What must be settled before this save may overwrite the target profile.
enum Adoption {
    /// Nothing. The profile is free, or it holds this same account.
    Unneeded,
    /// The profile neither matches nor contradicts this account, so the store
    /// has no way to decide. Only the operator can say whether replacing it is
    /// right, so this is a question rather than a refusal.
    AskOperator { stored: Option<String> },
}

/// Decide how the target profile stands against the account being saved.
///
/// A profile whose claims positively disagree is refused outright: without an
/// explicit alias `save` defaults to the detected email, so a second account on
/// one address lands on the first account's profile, where the destructive
/// answer is a single keystroke. No answer makes those the same account.
///
/// A profile that declares nothing is a different case and is returned for the
/// operator to settle. See `commands::adopt`.
fn classify_overwrite(
    paths: &config::Paths,
    alias: &str,
    incoming_account: Option<&str>,
    incoming_user: Option<&str>,
    alias_was_explicit: bool,
) -> Result<Adoption> {
    let Some(conflict) =
        profile::conflicting_workspace(paths, alias, incoming_account, incoming_user)
    else {
        return Ok(Adoption::Unneeded);
    };
    let stored = conflict.stored().map(str::to_string);
    let profile::AccountConflict::Different(stored) = conflict else {
        return Ok(Adoption::AskOperator { stored });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

    fn token(jti: &str) -> String {
        use base64::Engine;
        let claims = format!(r#"{{"sub":"seatA","jti":"{jti}"}}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        format!("{JWT_HDR}.{payload}.sig")
    }

    fn auth_bytes(access: &str) -> String {
        format!(r#"{{"access_token":"{access}"}}"#)
    }

    /// A store with `alias` holding `access`, plus a snapshot of the live file.
    fn setup(access: &str) -> (tempfile::TempDir, Paths, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let dir = paths.profiles_dir().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), auth_bytes(access)).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"alias":"work","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let snapshot = tmp.path().join("snapshot.json");
        std::fs::write(&snapshot, auth_bytes(&token("live"))).unwrap();
        (tmp, paths, snapshot)
    }

    fn stored(paths: &Paths) -> String {
        std::fs::read_to_string(paths.profiles_dir().join("work").join("auth.json")).unwrap()
    }

    /// A token that names a workspace, unlike `token`.
    fn workspace_token(account: &str) -> String {
        use base64::Engine;
        let claims = format!(
            r#"{{"sub":"seatA","https://api.openai.com/auth":{{"chatgpt_account_id":"{account}"}}}}"#
        );
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        format!("{JWT_HDR}.{payload}.sig")
    }

    /// The stored profile declares no workspace, so it can be neither matched
    /// to the arriving account nor ruled out. Without an answer the save keeps
    /// what is there, and says which flag supplies one.
    #[test]
    fn refuses_an_unidentifiable_profile_without_approval() {
        let (_tmp, paths, snapshot) = setup(&token("legacy"));
        let arriving = auth_bytes(&workspace_token("acct-team"));
        std::fs::write(&snapshot, &arriving).unwrap();
        let approved = Some(Some(auth_bytes(&token("legacy")).into_bytes()));

        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        let error = save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("--allow-adopt"), "{error}");
        assert!(
            stored(&paths).contains(&token("legacy")),
            "the unidentifiable profile was replaced without approval"
        );
    }

    /// The same situation with the answer given. The refusal above is a
    /// default, not a wall: `remove` followed by a fresh save would perform this
    /// very replacement after destroying the metadata that describes it.
    #[test]
    fn adopts_an_unidentifiable_profile_when_approved() {
        let (_tmp, paths, snapshot) = setup(&token("legacy"));
        let incoming = workspace_token("acct-team");
        std::fs::write(&snapshot, auth_bytes(&incoming)).unwrap();
        let approved = Some(Some(auth_bytes(&token("legacy")).into_bytes()));

        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, true,
        )
        .unwrap();

        assert!(
            stored(&paths).contains(&incoming),
            "the approved save did not land"
        );
    }

    /// The operator approved replacing one credential. Another process replaced
    /// it while they were answering, so the approval no longer describes what
    /// is there and the save must not proceed on it.
    #[test]
    fn refuses_when_the_profile_changed_after_confirmation() {
        let (_tmp, paths, snapshot) = setup(&token("approved"));
        let approved = Some(Some(auth_bytes(&token("approved")).into_bytes()));
        // A concurrent write lands between the prompt and the lock.
        let newer = auth_bytes(&token("newer"));
        std::fs::write(paths.profiles_dir().join("work").join("auth.json"), &newer).unwrap();

        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        let error = save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("changed while this save"),
            "{error}"
        );
        assert_eq!(stored(&paths), newer, "the newer credential was replaced");
    }

    /// The profile did not exist when the command decided, so nobody approved
    /// overwriting it.
    #[test]
    fn refuses_an_unconfirmed_profile_that_appeared() {
        let (_tmp, paths, snapshot) = setup(&token("existing"));

        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        let error = save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, None, false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("created by another process"),
            "{error}"
        );
    }

    /// The live file moved to another account while the prompt was open, so the
    /// alias and email resolved earlier no longer describe it.
    #[test]
    fn refuses_when_the_live_credential_changed() {
        let (_tmp, paths, snapshot) = setup(&token("approved"));
        let approved = Some(Some(auth_bytes(&token("approved")).into_bytes()));
        // `verified` describes what was read before the prompt; the snapshot
        // holds what is there now.
        let before = _tmp.path().join("before.json");
        std::fs::write(&before, auth_bytes(&token("before"))).unwrap();
        let verified = api::read_auth_json(&before).unwrap();

        let lock = store::lock(&paths).unwrap();
        let error = save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("active account changed"),
            "{error}"
        );
    }

    /// Nothing changed: the approval still describes the profile, so the save
    /// goes through.
    #[test]
    fn saves_when_nothing_changed_after_confirmation() {
        let (_tmp, paths, snapshot) = setup(&token("approved"));
        let approved = Some(Some(auth_bytes(&token("approved")).into_bytes()));

        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, false,
        )
        .unwrap();

        assert!(
            stored(&paths).contains(&token("live")),
            "the snapshot was not saved"
        );
    }
}
