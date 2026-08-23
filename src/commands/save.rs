use anyhow::{Context, Result};

use crate::api;
use crate::commands::adopt;
use crate::commands::alias;
use crate::config;
use crate::profile;
use crate::store;
use std::io::IsTerminal;

pub fn run(alias: Option<&str>, label: Option<&str>, allow_adopt: bool) -> Result<()> {
    // Consent has to name what it consents to. Without an alias, `save` derives
    // one from the token's email claim, so the flag would pre-approve replacing
    // whichever profile that resolves to — and one login can hold seats in
    // several workspaces behind a single address. The operator would be
    // approving a target they never saw.
    if allow_adopt && alias.is_none() {
        anyhow::bail!(
            "--allow-adopt replaces a saved profile, so name the one you are approving: \
             codexctl save <alias> --allow-adopt"
        );
    }
    // Reject a bad label before the save switches the live auth file. Failing
    // afterwards would leave the active account changed under an error exit.
    // Validation also trims and reads a blank label as none, so keep its result
    // rather than storing the raw text.
    let label = label.map(store::validate_label).transpose()?.flatten();

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

    // One account, one profile. If this account is already saved, refresh that
    // profile rather than adding a second copy under whatever name was reached
    // for — a mistyped alias is free by definition, and the fork it leaves is
    // what every later lookup reports as ambiguous.
    let resolved_alias = match profile::existing_seat(
        &paths,
        auth.account_id.as_deref(),
        &api::token_logins(&auth.access_token),
    )
    .context("could not check whether this account is already saved")?
    {
        // Refused rather than redirected. Writing credentials into a profile
        // the operator did not name means deciding on their behalf what to do
        // about that profile's own state — whether its saved credential is
        // newer than the live one, which address its metadata should keep,
        // whether the approval they gave covers it. Naming the alias answers
        // all of that at once, and the error says which alias to name.
        profile::ExistingSeat::One(saved) if saved != resolved_alias => anyhow::bail!(
            "this account is already saved as '{saved}'. Save to that alias instead: \
             codexctl save {saved}"
        ),
        profile::ExistingSeat::Ambiguous(aliases)
            if !aliases.iter().any(|saved| saved == &resolved_alias) =>
        {
            anyhow::bail!(
                "this account is already saved under more than one alias ({}). \
                 Remove the duplicates, or save to one of them directly.",
                aliases.join(", ")
            )
        }
        _ => resolved_alias,
    };

    let existing = store::profile_dir(&paths, &resolved_alias)?;
    // What the operator is agreeing to replace, so the approval cannot be
    // applied to some other credential that lands there while they answer.
    let mut confirmed_state: Option<Option<Vec<u8>>> = None;
    let mut adoption_approved = false;
    // An address the profile established, kept only when the profile is settled
    // as this same account. A token carrying no email claim, with the `/me`
    // lookup unavailable, would otherwise rebuild metadata with none and erase
    // what `list` and `whoami` show. Never kept across an adoption: there the
    // occupant is a different or unidentifiable account, and its address would
    // label the arriving credential as somebody else.
    let mut keep_email: Option<String> = None;
    if existing.exists() {
        let adoption = classify_overwrite(
            &paths,
            &resolved_alias,
            auth.account_id.as_deref(),
            &api::token_logins(&auth.access_token),
            alias::optional(alias)?.is_some(),
        )?;
        // Read before the question is asked: this is the credential the operator
        // is being shown and agreeing to replace. Reading it afterwards would
        // silently adopt whatever landed there while they were deciding.
        let shown = adopt::stored_credentials(&existing);
        let adoption_kind = match &adoption {
            Adoption::Unneeded => Adoption::Unneeded,
            Adoption::AskOperator { stored } => Adoption::AskOperator {
                stored: stored.clone(),
            },
        };
        let approved = match adoption {
            Adoption::Unneeded => {
                keep_email = established_email(&paths, &resolved_alias, &adoption_kind);
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
                    std::io::stdin().is_terminal(),
                    &mut std::io::stdin().lock(),
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
    let email = email.or(keep_email);

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
    // The seat was resolved before the lock, because the prompt must not be held
    // under it. A concurrent save can create the profile this one should have
    // reused in that window, which would rebuild the duplicate this check
    // exists to prevent. Refuse rather than redirect: the operator already
    // answered about a specific profile, and this is not it.
    match profile::existing_seat(
        paths,
        live_now.account_id.as_deref(),
        &api::token_logins(&live_now.access_token),
    )
    .context("could not check whether this account is already saved")?
    {
        profile::ExistingSeat::One(saved) if saved != resolved_alias => anyhow::bail!(
            "this account was saved as '{saved}' while this command was preparing. \
             Re-run it to refresh that profile."
        ),
        profile::ExistingSeat::Ambiguous(aliases)
            if !aliases.iter().any(|saved| saved == resolved_alias) =>
        {
            anyhow::bail!(
                "this account is already saved under more than one alias ({}). \
                 Remove the duplicates, or save to one of them directly.",
                aliases.join(", ")
            )
        }
        _ => {}
    }
    if store::profile_dir(paths, resolved_alias)?.exists() {
        // Re-run under the lock against the store as it actually stands. An
        // adoption the operator already approved carries over; one that only
        // became necessary since does not, because nobody was asked about it.
        if let Adoption::AskOperator { stored } = classify_overwrite(
            paths,
            resolved_alias,
            live_now.account_id.as_deref(),
            &api::token_logins(&live_now.access_token),
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
            Some(approved) if *approved != adopt::stored_credentials(&dir) => anyhow::bail!(
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

/// What must be settled before this save may overwrite the target profile.
enum Adoption {
    /// Nothing. The profile is free, or it holds this same account.
    Unneeded,
    /// The profile neither matches nor contradicts this account, so the store
    /// has no way to decide. Only the operator can say whether replacing it is
    /// right, so this is a question rather than a refusal.
    AskOperator { stored: Option<String> },
}

/// The address to keep when the arriving token names none.
///
/// Only a profile settled as this same account has an address worth keeping.
/// Across an adoption the occupant is a different or unidentifiable account, and
/// its address would label the arriving credential as somebody else.
fn established_email(paths: &config::Paths, alias: &str, adoption: &Adoption) -> Option<String> {
    match adoption {
        Adoption::Unneeded => profile::get_profile_from(paths, alias)
            .ok()
            .and_then(|profile| profile.meta.email),
        Adoption::AskOperator { .. } => None,
    }
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
    incoming_user: &api::Logins,
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
         (stored {}, incoming {}). {remedy}",
        profile::describe_claim(&stored),
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

    /// The seat is resolved before the lock, so a concurrent save can create
    /// the profile this one should have reused while the prompt is open.
    /// Writing anyway rebuilds the duplicate the reuse check exists to prevent.
    #[test]
    fn refuses_when_another_alias_took_this_account_first() {
        let (_tmp, paths, snapshot) = setup(&token("legacy"));
        let arriving = workspace_token("acct-team");
        std::fs::write(&snapshot, auth_bytes(&arriving)).unwrap();

        // A concurrent save landed this very account under another alias.
        let other = paths.profiles_dir().join("winner");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("auth.json"), auth_bytes(&arriving)).unwrap();
        std::fs::write(
            other.join("meta.json"),
            r#"{"alias":"winner","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let approved = Some(Some(auth_bytes(&token("legacy")).into_bytes()));
        let lock = store::lock(&paths).unwrap();
        let verified = api::read_auth_json(&snapshot).unwrap();
        let error = save_verified_snapshot(
            &lock, &paths, "work", None, None, &snapshot, &verified, true, approved, true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("winner"), "{error}");
        assert!(
            stored(&paths).contains(&token("legacy")),
            "the duplicate was written anyway"
        );
    }

    /// The address a profile established survives a token that names none —
    /// but only where the profile is settled as this same account.
    #[test]
    fn an_established_email_is_kept_only_without_an_adoption() {
        let (_tmp, paths, _snapshot) = setup(&token("stored"));
        std::fs::write(
            paths.profiles_dir().join("work").join("meta.json"),
            r#"{"alias":"work","email":"amir@sawmills.ai","plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        assert_eq!(
            established_email(&paths, "work", &Adoption::Unneeded).as_deref(),
            Some("amir@sawmills.ai")
        );
        assert_eq!(
            established_email(&paths, "work", &Adoption::AskOperator { stored: None }),
            None,
            "an adopted account inherited the old occupant's address"
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
