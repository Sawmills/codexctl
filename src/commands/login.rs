use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::api;
use crate::commands::{adopt, alias, status};
use crate::config::{self, Paths};
use crate::profile;
use crate::store;
use std::io::IsTerminal;

trait CodexLoginRunner {
    fn run_codex_login(&mut self, codex_home: &Path) -> Result<()>;

    /// Called once the store lock has been released and the operator is being
    /// asked to approve replacing a profile.
    ///
    /// Nothing happens here in production; it exists because the window is real
    /// — the lock is down, and another process may write to the store before the
    /// answer arrives — and a guard against that window can only be tested by
    /// something that can act inside it.
    fn while_awaiting_approval(&mut self) {}
}

struct CodexCliLoginRunner;

impl CodexLoginRunner for CodexCliLoginRunner {
    fn run_codex_login(&mut self, codex_home: &Path) -> Result<()> {
        store::ensure_private_dir(codex_home)?;

        let status = Command::new("codex")
            .arg("login")
            .arg("--device-auth")
            .env("CODEX_HOME", codex_home)
            .status()
            .context("failed to run `codex login --device-auth`")?;

        if !status.success() {
            bail!("codex login failed with status {status}");
        }

        Ok(())
    }
}

pub fn run(alias: &str, label: Option<&str>, allow_adopt: bool) -> Result<()> {
    let alias = alias::required(alias)?;
    let paths = config::default_paths()?;
    let mut runner = CodexCliLoginRunner;
    // The alias actually written may be label-qualified, so report that one.
    let saved = run_from_with_consent(&paths, alias, label, allow_adopt, &mut runner)?;

    println!("logged in and saved profile '{saved}'");
    println!();
    status::run_focused(&saved)?;
    Ok(())
}

/// `run_from` with adoption never pre-approved.
///
/// Every caller is a test, and tests have no terminal, so the consent prompt
/// declines on its own — this only spares them an argument that is always the
/// same.
#[cfg(test)]
fn run_from(
    paths: &Paths,
    alias: &str,
    label: Option<&str>,
    runner: &mut impl CodexLoginRunner,
) -> Result<String> {
    run_from_with_consent(paths, alias, label, false, runner)
}

fn run_from_with_consent(
    paths: &Paths,
    alias: &str,
    label: Option<&str>,
    allow_adopt: bool,
    runner: &mut impl CodexLoginRunner,
) -> Result<String> {
    let alias = alias::required(alias)?;
    // Validate before the device-auth flow starts. Failing after the operator
    // completed a browser login would read as a failed login even though the
    // profile was saved and made active.
    label.map(store::validate_label).transpose()?;
    let codex_home = create_isolated_login_home(paths, alias)?;
    let result = (|| {
        runner.run_codex_login(&codex_home)?;

        let auth_path = codex_home.join("auth.json");
        if !auth_path.exists() {
            bail!("codex login did not create {}", auth_path.display());
        }

        // The incoming account is only knowable once the login has produced a
        // token, so the target alias is resolved here rather than up front.
        // Workspace and login together: a team workspace is shared, so it
        // does not by itself say whose credentials these are.
        let incoming_identity = api::read_auth_json(&auth_path).ok();
        let incoming = incoming_identity
            .as_ref()
            .and_then(|a| a.account_id.clone());
        let incoming_user = incoming_identity
            .as_ref()
            .and_then(|a| api::token_login(&a.access_token));
        // Resolve and write under one lock. Which alias this login lands on is
        // read out of the store, so releasing the lock in between would let a
        // concurrent login change the answer before the write lands.
        let lock = store::lock(paths)?;
        let resolve = |_: &store::StoreLock| {
            resolve_target_alias(
                paths,
                alias,
                label,
                incoming.as_deref(),
                incoming_user.as_deref(),
            )
        };
        let (lock, target) = match resolve(&lock)? {
            Resolution::Ready(target) => (lock, target),
            Resolution::NeedsConsent {
                alias: pending,
                stored,
            } => {
                // Read before the question is asked: these are the bytes the
                // operator is agreeing to replace. The description alone cannot
                // stand in for them — two claimless tokens for one login
                // describe themselves identically.
                let shown = store::profile_dir(paths, &pending)
                    .ok()
                    .and_then(|dir| adopt::stored_credentials(&dir));
                // Never ask while holding the store lock: `store::lock` waits
                // without a timeout, so a question left unanswered would stall
                // every other codexctl process, not just this one.
                drop(lock);
                runner.while_awaiting_approval();
                // Either way nothing is saved, and the login has already
                // happened — so neither outcome is a success.
                match adopt::approve_adoption(
                    &pending,
                    stored.as_deref(),
                    incoming.as_deref(),
                    allow_adopt,
                    std::io::stdin().is_terminal(),
                    &mut std::io::stdin().lock(),
                    &mut std::io::stderr(),
                ) {
                    adopt::Approval::Granted => {}
                    adopt::Approval::Declined => {
                        bail!("not replacing profile '{pending}', so this login was not saved")
                    }
                    adopt::Approval::NoTerminal => {
                        return Err(adopt::refusal(&pending, stored.as_deref()));
                    }
                }
                let lock = store::lock(paths)?;
                // The answer approved one specific replacement. Resolving again
                // under the new lock keeps it from being applied to whatever the
                // store looks like now, which may be something nobody was asked
                // about.
                let unchanged = store::profile_dir(paths, &pending)
                    .ok()
                    .and_then(|dir| adopt::stored_credentials(&dir))
                    == shown;
                match resolve(&lock)? {
                    Resolution::Ready(target) => (lock, target),
                    Resolution::NeedsConsent {
                        alias: again,
                        stored: stored_again,
                    } if again == pending && stored_again == stored && unchanged => (lock, pending),
                    Resolution::NeedsConsent { .. } => bail!(
                        "the store changed while waiting for an answer, so the approval no \
                         longer describes what would be replaced. Re-run the login."
                    ),
                }
            }
        };

        // Only a fallback either way: a token carrying an email claim wins.
        //
        // When the login lands on the alias that was asked for, that alias is
        // the address. When it is redirected to a profile that already exists,
        // the address is whatever that profile already recorded — deriving one
        // from the requested alias would overwrite an established email with an
        // unrelated string, or with nothing at all.
        let email = profile::get_profile_from(paths, &target)
            .ok()
            .and_then(|existing| existing.meta.email)
            .or_else(|| email_from_alias(alias));
        profile::save_profile_and_activate_locked(
            &lock,
            paths,
            &target,
            email.as_deref(),
            &auth_path,
        )?;
        if let Some(label) = label {
            profile::set_label_locked(&lock, paths, &target, Some(label))?;
        }
        Ok(target)
    })();
    let cleanup = remove_isolated_login_home(&codex_home);

    match (result, cleanup) {
        (Ok(saved), Ok(())) => Ok(saved),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "also failed to remove isolated login home: {cleanup_error:#}"
        ))),
    }
}

/// Where a login should land, once the store has said everything it can.
enum Resolution {
    /// Settled. Write here.
    Ready(String),
    /// This alias holds something the store can neither match to this account
    /// nor rule out, so only the operator can settle it. See `commands::adopt`.
    NeedsConsent {
        alias: String,
        stored: Option<String>,
    },
}

/// Where this login should land.
///
/// The requested alias wins whenever it is free or already holds this same
/// account. When it holds a *different* account the label qualifies it, so a
/// second seat on one address lands beside the first rather than replacing it.
/// Without a label there is nothing to qualify with — and nothing the store can
/// decide either, so the operator is asked rather than refused.
fn resolve_target_alias(
    paths: &Paths,
    alias: &str,
    label: Option<&str>,
    incoming_account: Option<&str>,
    incoming_user: Option<&str>,
) -> Result<Resolution> {
    let conflict = profile::conflicting_workspace(paths, alias, incoming_account, incoming_user);
    let Some(conflict) = conflict else {
        // The requested alias is usable. When a label was given, this seat may
        // still be saved under a label-derived alias from an earlier run — and
        // saving it here too would fork one account across two profiles, which
        // is what makes ownership ambiguous later.
        if label.is_some() {
            match profile::existing_seat(paths, incoming_account, incoming_user)
                .context("could not check whether this account is already saved")?
            {
                profile::ExistingSeat::One(existing) if existing != alias => {
                    return Ok(Resolution::Ready(existing));
                }
                // Already ambiguous. A free alias is room for a third copy,
                // not permission to make one — unless the operator named one of
                // the duplicates, which is refreshing an existing profile and
                // the very remedy this error recommends.
                profile::ExistingSeat::Ambiguous(aliases) => {
                    if aliases.iter().any(|existing| existing == alias) {
                        return Ok(Resolution::Ready(alias.to_string()));
                    }
                    bail!(
                        "this account is already saved under more than one alias ({}). \
                         Remove the duplicates, or log in with one of them directly.",
                        aliases.join(", ")
                    )
                }
                _ => {}
            }
        }
        return Ok(Resolution::Ready(alias.to_string()));
    };
    let arriving = incoming_account
        .map(profile::short_workspace)
        .unwrap_or_default();
    let stored = conflict.stored().map(str::to_string);

    let Some(label) = label else {
        // A claim that positively disagrees is never this account, so no answer
        // could make replacing it right. One that simply cannot be compared is a
        // question, and the operator is the only one who can answer it.
        let profile::AccountConflict::Different(different) = &conflict else {
            return Ok(Resolution::NeedsConsent {
                alias: alias.to_string(),
                stored,
            });
        };
        bail!(
            "profile '{alias}' holds a different account \
             (stored workspace {}, this login {arriving}). \
             Re-run with --label <name> to save it alongside, choose another \
             alias, or remove it first: codexctl remove {alias}",
            profile::short_workspace(different)
        );
    };

    // About to derive an alias — but this seat may already be saved under
    // another one. Refreshing that profile keeps a re-login stable and avoids a
    // second profile for one account, which is what makes ownership ambiguous
    // later. A store that cannot be scanned is not an answer of "no".
    match profile::existing_seat(paths, incoming_account, incoming_user)
        .context("could not check whether this account is already saved")?
    {
        profile::ExistingSeat::One(existing) => return Ok(Resolution::Ready(existing)),
        profile::ExistingSeat::Ambiguous(aliases) => {
            // Naming one of the duplicates is refreshing it, not adding to them.
            if aliases.iter().any(|existing| existing == alias) {
                return Ok(Resolution::Ready(alias.to_string()));
            }
            bail!(
                "this account is already saved under more than one alias ({}). \
                 Remove the duplicates, or log in with one of them directly.",
                aliases.join(", ")
            )
        }
        profile::ExistingSeat::None => {}
    }

    let slug = alias_safe(label);
    if slug.is_empty() {
        bail!("label '{label}' has no characters usable in an alias; choose another alias");
    }
    let qualified = format!("{alias}+{slug}");
    store::validate_alias(&qualified).with_context(|| {
        format!("label '{label}' does not produce a usable alias for '{alias}'")
    })?;

    if let Some(also_taken) =
        profile::conflicting_workspace(paths, &qualified, incoming_account, incoming_user)
    {
        if let profile::AccountConflict::Different(also_taken) = &also_taken {
            bail!(
                "'{alias}' and '{qualified}' both hold other accounts \
                 ({} and {}, this login {arriving}). Choose another alias.",
                stored
                    .as_deref()
                    .map(profile::short_workspace)
                    .unwrap_or_default(),
                profile::short_workspace(also_taken)
            );
        }
        return Ok(Resolution::NeedsConsent {
            stored: also_taken.stored().map(str::to_string),
            alias: qualified,
        });
    }
    // The agreement rule above has vouched for this pair, including two sides
    // that both declare nothing — which is a claimless profile being refreshed
    // by the command that created it. What it cannot vouch for is a profile
    // whose credentials will not read at all. The operator never named this
    // alias, so replacing its occupant is not something the store may decide on
    // its own — but the only remedy left otherwise is `remove`, which destroys
    // the metadata that identifies it and then replaces it anyway. Ask instead.
    if profile::credentials_unreadable(paths, &qualified) {
        return Ok(Resolution::NeedsConsent {
            stored: profile::workspace_of_profile(paths, &qualified),
            alias: qualified,
        });
    }
    Ok(Resolution::Ready(qualified))
}

/// Reduce a display label to something usable as one path component.
///
/// Labels permit spaces and path syntax that an alias cannot carry, so anything
/// outside a conservative set collapses to `-`.
fn alias_safe(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for character in label.trim().chars() {
        let keep = character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-');
        match (keep, out.ends_with('-')) {
            (true, _) => out.push(character.to_ascii_lowercase()),
            (false, false) => out.push('-'),
            (false, true) => {}
        }
    }
    out.trim_matches(['-', '.']).to_string()
}

fn create_isolated_login_home(paths: &Paths, alias: &str) -> Result<PathBuf> {
    let _lock = store::lock(paths)?;
    let alias_home = store::login_home(paths, alias)?;
    store::ensure_private_dir(&alias_home)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..16 {
        let home = alias_home.join(format!("session-{}-{nonce}-{attempt}", std::process::id()));
        match std::fs::create_dir(&home) {
            Ok(()) => {
                store::ensure_private_dir(&home)?;
                return Ok(home);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to create {}", home.display()));
            }
        }
    }

    bail!("failed to allocate a unique isolated login home")
}

fn remove_isolated_login_home(codex_home: &Path) -> Result<()> {
    std::fs::remove_dir_all(codex_home)
        .with_context(|| format!("failed to remove {}", codex_home.display()))
}

/// Fallback address only. The saved profile prefers the token's own profile
/// claim, so this matters solely for a token that carries no claim at all —
/// where an alias that looks like an address is the best guess available.
fn email_from_alias(alias: &str) -> Option<String> {
    if alias.contains('@') {
        Some(alias.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLoginRunner {
        auth_json: String,
        seen_home: Option<PathBuf>,
        /// Applied while the approval prompt is open, standing in for another
        /// process writing to the store.
        concurrent_write: Option<(PathBuf, String)>,
    }

    impl FakeLoginRunner {
        fn new(auth_json: &str) -> Self {
            Self {
                auth_json: auth_json.to_string(),
                seen_home: None,
                concurrent_write: None,
            }
        }

        fn writing_during_approval(mut self, path: PathBuf, contents: String) -> Self {
            self.concurrent_write = Some((path, contents));
            self
        }
    }

    impl CodexLoginRunner for FakeLoginRunner {
        fn run_codex_login(&mut self, codex_home: &Path) -> Result<()> {
            self.seen_home = Some(codex_home.to_path_buf());
            std::fs::create_dir_all(codex_home)?;
            std::fs::write(codex_home.join("auth.json"), &self.auth_json)?;
            Ok(())
        }

        fn while_awaiting_approval(&mut self) {
            if let Some((path, contents)) = self.concurrent_write.take() {
                std::fs::write(path, contents).unwrap();
            }
        }
    }

    #[derive(Default)]
    struct FailingLoginRunner {
        seen_home: Option<PathBuf>,
    }

    impl CodexLoginRunner for FailingLoginRunner {
        fn run_codex_login(&mut self, codex_home: &Path) -> Result<()> {
            self.seen_home = Some(codex_home.to_path_buf());
            std::fs::write(
                codex_home.join("auth.json"),
                r#"{"access_token":"partial"}"#,
            )?;
            bail!("simulated login failure")
        }
    }

    /// Unsigned JWT declaring a workspace. Synthetic claims only.
    fn synthetic_token(account_id: &str) -> String {
        use base64::Engine;
        let claims = format!(
            r#"{{"sub":"seatA","https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
        );
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        format!("eyJhbGciOiJub25lIn0.{payload}.sig")
    }

    fn setup_test_env() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::write(paths.codex_auth_json(), r#"{"access_token":"active_tok"}"#).unwrap();
        (tmp, paths)
    }

    #[test]
    fn isolated_login_home_is_unique_and_profile_scoped() {
        let (_tmp, paths) = setup_test_env();
        let first = create_isolated_login_home(&paths, "amir+8@sawmills.ai").unwrap();
        let second = create_isolated_login_home(&paths, "amir+8@sawmills.ai").unwrap();

        assert_ne!(first, second);
        let alias_home = paths
            .codexctl_dir()
            .join("login-homes")
            .join("amir+8@sawmills.ai");
        assert_eq!(first.parent(), Some(alias_home.as_path()));
        assert_eq!(second.parent(), Some(alias_home.as_path()));
        assert!(first.is_dir());
        assert!(second.is_dir());

        remove_isolated_login_home(&first).unwrap();
        remove_isolated_login_home(&second).unwrap();
    }

    #[test]
    fn concurrent_login_homes_for_same_alias_do_not_overlap() {
        use std::sync::{Arc, Barrier};

        let (_tmp, paths) = setup_test_env();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let paths = paths.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    create_isolated_login_home(&paths, "amir+8@sawmills.ai").unwrap()
                })
            })
            .collect();

        barrier.wait();
        let homes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_ne!(homes[0], homes[1]);
        assert!(homes.iter().all(|home| home.is_dir()));
        for home in homes {
            remove_isolated_login_home(&home).unwrap();
        }
    }

    #[test]
    fn run_from_uses_isolated_home_and_imports_auth() {
        let (_tmp, paths) = setup_test_env();
        let mut runner = FakeLoginRunner::new(r#"{"access_token":"new_tok"}"#);

        run_from(&paths, "  amir+8@sawmills.ai  ", None, &mut runner).unwrap();

        let seen_home = runner.seen_home.as_deref().unwrap();
        assert_eq!(
            seen_home.parent(),
            Some(paths.login_homes_dir().join("amir+8@sawmills.ai").as_path())
        );
        assert!(!seen_home.exists());
        let saved = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir+8@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(saved.contains("new_tok"));
        let active = std::fs::read_to_string(paths.codex_auth_json()).unwrap();
        assert!(active.contains("new_tok"));
        assert_eq!(
            profile::get_active_from(&paths).unwrap().as_deref(),
            Some("amir+8@sawmills.ai")
        );
    }

    #[test]
    fn run_from_relogging_active_alias_keeps_new_auth() {
        let (_tmp, paths) = setup_test_env();
        profile::save_profile_to(
            &paths,
            "amir+8@sawmills.ai",
            Some("amir+8@sawmills.ai"),
            &paths.codex_auth_json(),
        )
        .unwrap();
        profile::set_active_from(&paths, "amir+8@sawmills.ai").unwrap();
        std::fs::write(
            paths.codex_auth_json(),
            r#"{"access_token":"old_active_tok"}"#,
        )
        .unwrap();
        let mut runner = FakeLoginRunner::new(r#"{"access_token":"new_active_tok"}"#);

        run_from(&paths, "amir+8@sawmills.ai", None, &mut runner).unwrap();

        let saved = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir+8@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(saved.contains("new_active_tok"));
        assert!(!saved.contains("old_active_tok"));
        let active = std::fs::read_to_string(paths.codex_auth_json()).unwrap();
        assert!(active.contains("new_active_tok"));
        assert!(!active.contains("old_active_tok"));
    }

    /// A derived alias whose workspace is known but whose login is not does not
    /// identify an account: workspaces hold many people. The operator never
    /// named this alias, so "not proven different" is not enough to replace it.
    #[test]
    fn run_from_refuses_a_derived_alias_with_a_workspace_but_no_login() {
        let (_tmp, paths) = setup_test_env();
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // The derived alias declares the workspace in metadata only, with a
        // token that yields no login identity at all.
        let dir = paths.profiles_dir().join("amir@sawmills.ai+team");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"access_token":"opaque-not-a-jwt"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"alias":"amir@sawmills.ai+team","email":null,"plan":null,"account_id":"acct-team","saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap_err();

        // Either guard may speak first — the workspace guard now refuses an
        // unproven owner outright — but the credentials must survive.
        let message = error.to_string();
        assert!(
            message.contains("--allow-adopt"),
            "unhelpful refusal: {message}"
        );
        assert!(
            std::fs::read_to_string(dir.join("auth.json"))
                .unwrap()
                .contains("opaque-not-a-jwt"),
            "the derived alias was overwritten"
        );
    }

    /// The approval names one specific credential, and the lock is down while
    /// the operator answers. Another process replacing the profile in that
    /// window produces a profile that still *describes* itself identically —
    /// same login, still no workspace — so nothing but the stored bytes can tell
    /// the two apart. The approval must not carry over to the new one.
    #[test]
    fn an_approval_does_not_survive_a_concurrent_replacement() {
        let (_tmp, paths) = setup_test_env();
        let legacy = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSJ9.sig";
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{legacy}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // A different credential that describes itself exactly as the first
        // does: same login claim, still no workspace.
        let usurper = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSIsImp0aSI6Im90aGVyIn0.sig";
        let profile_auth = paths
            .profiles_dir()
            .join("amir@sawmills.ai")
            .join("auth.json");

        use base64::Engine;
        let claims =
            r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        let incoming = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#))
            .writing_during_approval(
                profile_auth.clone(),
                format!(r#"{{"access_token":"{usurper}"}}"#),
            );

        let error =
            run_from_with_consent(&paths, "amir@sawmills.ai", None, true, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("changed while waiting"),
            "approval was reused after the profile changed: {error}"
        );
        let kept = std::fs::read_to_string(&profile_auth).unwrap();
        assert!(
            kept.contains(usurper),
            "a credential nobody approved replacing was overwritten"
        );
    }

    /// The other half of the headline case: the refusal above is a default, not
    /// a wall. A legacy profile that declares no workspace is exactly what an
    /// upgrade leaves behind, and the operator is the one who knows whether the
    /// account arriving is the one it held. With that answer given, the login
    /// lands and the profile stops being claimless — which is what makes a
    /// re-login the documented way to bring an old profile forward.
    #[test]
    fn run_from_adopts_a_claimless_profile_when_approved() {
        let (_tmp, paths) = setup_test_env();
        let legacy = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSJ9.sig";
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{legacy}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        use base64::Engine;
        let claims =
            r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        let incoming = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let saved =
            run_from_with_consent(&paths, "amir@sawmills.ai", None, true, &mut runner).unwrap();

        assert_eq!(saved, "amir@sawmills.ai");
        let dir = paths.profiles_dir().join("amir@sawmills.ai");
        assert!(
            std::fs::read_to_string(dir.join("auth.json"))
                .unwrap()
                .contains(&incoming),
            "the approved login did not land"
        );
        // The profile can now answer for itself, so the next login needs no
        // approval at all. Without this the adoption would have to be repeated
        // forever, which is the circularity the refusal alone created.
        assert_eq!(
            profile::workspace_of_profile(&paths, "amir@sawmills.ai").as_deref(),
            Some("acct-team"),
            "the adopted profile still records no workspace"
        );
    }

    /// Approval settles what the store could not work out. It does not overrule
    /// what the store worked out and got a negative answer to: two workspaces
    /// that positively disagree are not one account, and no flag makes them one.
    #[test]
    fn allow_adopt_does_not_override_a_proven_different_account() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error =
            run_from_with_consent(&paths, "amir@sawmills.ai", None, true, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("different account"),
            "--allow-adopt bypassed a proven conflict: {error}"
        );
        assert!(
            std::fs::read_to_string(
                paths
                    .profiles_dir()
                    .join("amir@sawmills.ai")
                    .join("auth.json"),
            )
            .unwrap()
            .contains(&stored),
            "a proven different account was overwritten"
        );
    }

    /// The headline case, from the other side: a legacy profile whose stored
    /// token declares no workspace, and its own owner signing into a second
    /// one. The logins match, so a rule that reads "stored declares nothing" as
    /// "same account" hands the personal profile to the team seat.
    #[test]
    fn run_from_refuses_a_second_workspace_for_one_login_on_a_claimless_profile() {
        let (_tmp, paths) = setup_test_env();
        // Stored: a readable token with a subject but no workspace claim.
        let legacy = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSJ9.sig";
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{legacy}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // The same human, now authenticating into a workspace.
        use base64::Engine;
        let claims =
            r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
        let incoming = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("--allow-adopt"),
            "unhelpful refusal: {error}"
        );
        let kept = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(kept.contains(legacy), "the legacy profile was overwritten");
    }

    /// Two aliases already holding one account is an ambiguous store. Deriving
    /// a third copy would deepen exactly the ambiguity that later stops tokens
    /// being attributed at all, so the login stops and says so.
    #[test]
    fn run_from_refuses_when_the_seat_is_already_saved_twice() {
        let (_tmp, paths) = setup_test_env();
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // The same team seat saved under two aliases already.
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+work", None, &source).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+team-old", None, &source).unwrap();

        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{team}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("more than one alias"),
            "unhelpful refusal: {error}"
        );
        assert!(
            !paths.profiles_dir().join("amir@sawmills.ai+team").exists(),
            "a third copy of one account was created"
        );
    }

    /// A free alias is room for a third copy, not permission to make one: if a
    /// seat is already saved twice, the store is ambiguous and adding to it is
    /// what stops tokens being attributed at all.
    #[test]
    fn run_from_refuses_an_ambiguous_seat_even_when_the_base_alias_is_free() {
        let (_tmp, paths) = setup_test_env();
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+work", None, &source).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+old", None, &source).unwrap();

        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{team}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("more than one alias"),
            "unhelpful refusal: {error}"
        );
        assert!(
            !paths.profiles_dir().join("amir@sawmills.ai").exists(),
            "a third copy of one account was created"
        );
    }

    /// The base alias being free is not proof the seat is unsaved. Removing the
    /// personal profile leaves the team seat under its label-derived alias, and
    /// saving it again under the freed base alias would fork one account.
    #[test]
    fn run_from_reuses_an_existing_seat_when_the_base_alias_is_free() {
        let (_tmp, paths) = setup_test_env();
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        // Only the label-derived alias exists; the base alias is free.
        profile::save_profile_to(&paths, "amir@sawmills.ai+work", None, &source).unwrap();

        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{team}"}}"#));
        let target = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap();

        assert_eq!(target, "amir@sawmills.ai+work", "the seat was forked");
        assert!(
            !paths.profiles_dir().join("amir@sawmills.ai").exists(),
            "a second profile was created for one account"
        );
    }

    /// A seat already saved under one label is refreshed, not duplicated, when
    /// the operator logs in again with a different one. Two profiles for one
    /// account are what make ownership ambiguous later.
    #[test]
    fn run_from_reuses_an_existing_seat_when_the_label_changes() {
        let (_tmp, paths) = setup_test_env();
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // The team seat is already saved under an earlier label.
        let team = synthetic_token("acct-team");
        let existing = paths.home.join("team-auth.json");
        std::fs::write(&existing, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+work", None, &existing).unwrap();

        // Same seat, new label.
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{team}"}}"#));
        let target = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap();

        assert_eq!(target, "amir@sawmills.ai+work", "the seat was duplicated");
        assert!(
            !paths.profiles_dir().join("amir@sawmills.ai+team").exists(),
            "a second profile was created for one account"
        );
    }

    /// Damaged metadata is not an empty profile. The stored token still proves
    /// which account these credentials belong to, so an alias the operator did
    /// name is still protected from a different login.
    #[test]
    fn run_from_refuses_a_different_account_behind_unreadable_metadata() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-personal");
        let dir = paths.profiles_dir().join("amir@sawmills.ai");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        // A valid token behind metadata that will not parse.
        std::fs::write(dir.join("meta.json"), "{ truncated").unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("different account"),
            "unhelpful refusal: {error}"
        );
        let kept = std::fs::read_to_string(dir.join("auth.json")).unwrap();
        assert!(
            kept.contains(&stored),
            "credentials behind damaged metadata were destroyed"
        );
    }

    /// A token that names no login at all cannot prove ownership by agreeing on
    /// the workspace: workspaces are shared, and the profile does name someone.
    #[test]
    fn run_from_refuses_a_login_with_no_identity_against_a_named_profile() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-team");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(&paths, "team", None, &paths.codex_auth_json().clone()).unwrap();

        // Same workspace declared in the file, but the token names nobody.
        let mut runner =
            FakeLoginRunner::new(r#"{"access_token":"opaque-not-a-jwt","account_id":"acct-team"}"#);

        let error = run_from(&paths, "team", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("--allow-adopt"),
            "unhelpful refusal: {error}"
        );
        let kept =
            std::fs::read_to_string(paths.profiles_dir().join("team").join("auth.json")).unwrap();
        assert!(kept.contains(&stored), "stored credentials were replaced");
    }

    /// A readable token that simply names nobody is an intact profile, not a
    /// broken one. Only credentials that cannot be read at all get the repair
    /// exception; anything else needs the same proof as any other overwrite.
    #[test]
    fn run_from_refuses_a_known_login_over_a_readable_but_anonymous_profile() {
        let (_tmp, paths) = setup_test_env();
        // Readable auth whose token yields neither workspace nor login.
        let dir = paths.profiles_dir().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"access_token":"opaque-not-a-jwt"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"alias":"work","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "work", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("--allow-adopt"),
            "unhelpful refusal: {error}"
        );
        assert!(
            std::fs::read_to_string(dir.join("auth.json"))
                .unwrap()
                .contains("opaque-not-a-jwt"),
            "an intact profile was overwritten"
        );
    }

    /// Naming one of the duplicates is refreshing it, not adding a third — and
    /// it is the remedy the ambiguity error itself recommends.
    #[test]
    fn run_from_refreshes_a_named_alias_from_an_ambiguous_set() {
        let (_tmp, paths) = setup_test_env();
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "work", None, &source).unwrap();
        profile::save_profile_to(&paths, "work-copy", None, &source).unwrap();

        let refreshed = format!("{}refreshed", synthetic_token("acct-team"));
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{refreshed}"}}"#));

        let target = run_from(&paths, "work", Some("team"), &mut runner).unwrap();

        assert_eq!(target, "work", "a named duplicate could not be refreshed");
        assert!(
            !paths.profiles_dir().join("work+team").exists(),
            "a third copy was created"
        );
    }

    /// A login redirected to an existing profile keeps the address that profile
    /// already recorded, rather than replacing it from an unrelated alias.
    #[test]
    fn run_from_keeps_the_existing_email_when_redirected_to_a_seat() {
        let (_tmp, paths) = setup_test_env();
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir-team", Some("amir@sawmills.ai"), &source).unwrap();

        // Requested under an unrelated alias; the token carries no email claim.
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{team}"}}"#));
        let target = run_from(&paths, "temporary", Some("team"), &mut runner).unwrap();

        assert_eq!(target, "amir-team", "the existing seat was not reused");
        let meta =
            std::fs::read_to_string(paths.profiles_dir().join("amir-team").join("meta.json"))
                .unwrap();
        assert!(
            meta.contains("amir@sawmills.ai"),
            "the established email was discarded: {meta}"
        );
    }

    /// Re-logging the same alias keeps its recorded address when the new token
    /// carries no email claim — `list` and `whoami` should not lose established
    /// identity to a token that simply says less than the last one.
    #[test]
    fn run_from_keeps_the_stored_email_on_a_same_alias_relogin() {
        let (_tmp, paths) = setup_test_env();
        let team = synthetic_token("acct-team");
        let source = paths.home.join("team-auth.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{team}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir-team", Some("amir@sawmills.ai"), &source).unwrap();

        let refreshed = format!("{}refreshed", synthetic_token("acct-team"));
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{refreshed}"}}"#));

        run_from(&paths, "amir-team", None, &mut runner).unwrap();

        let meta =
            std::fs::read_to_string(paths.profiles_dir().join("amir-team").join("meta.json"))
                .unwrap();
        assert!(
            meta.contains("amir@sawmills.ai"),
            "a re-login erased the stored email: {meta}"
        );
    }

    /// A labelled profile for an account that declares no workspace must be
    /// refreshable by the same command that created it. Both sides declaring
    /// nothing is agreement, not missing evidence.
    #[test]
    fn run_from_refreshes_a_claimless_derived_profile() {
        let (_tmp, paths) = setup_test_env();
        // The base alias holds another account.
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // A claimless account already saved under the derived alias.
        let claimless = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0WiJ9.first";
        let source = paths.home.join("claimless.json");
        std::fs::write(&source, format!(r#"{{"access_token":"{claimless}"}}"#)).unwrap();
        profile::save_profile_to(&paths, "amir@sawmills.ai+team", None, &source).unwrap();

        // The same claimless account logging in again.
        let refreshed = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0WiJ9.second";
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{refreshed}"}}"#));

        let target = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap();

        assert_eq!(target, "amir@sawmills.ai+team");
        assert!(
            std::fs::read_to_string(
                paths
                    .profiles_dir()
                    .join("amir@sawmills.ai+team")
                    .join("auth.json")
            )
            .unwrap()
            .contains(refreshed),
            "the claimless profile was not refreshed"
        );
    }

    /// A team workspace holds many people. Two colleagues therefore agree on
    /// `chatgpt_account_id` and are still different accounts, so the workspace
    /// alone cannot say whose credentials an alias holds.
    #[test]
    fn run_from_refuses_a_different_login_in_the_same_workspace() {
        let (_tmp, paths) = setup_test_env();
        let colleague = |user: &str| {
            use base64::Engine;
            let claims = format!(
                r#"{{"sub":"{user}","https://api.openai.com/auth":{{"chatgpt_account_id":"acct-team","chatgpt_user_id":"{user}"}}}}"#
            );
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
            format!("eyJhbGciOiJub25lIn0.{payload}.sig")
        };
        let stored = colleague("user-a");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(&paths, "team", None, &paths.codex_auth_json().clone()).unwrap();

        // Same workspace, different person.
        let incoming = colleague("user-b");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "team", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("different account"),
            "unhelpful refusal: {error}"
        );
        let kept =
            std::fs::read_to_string(paths.profiles_dir().join("team").join("auth.json")).unwrap();
        assert!(
            kept.contains(&stored),
            "a colleague's login replaced these credentials"
        );
    }

    /// An interrupted save or a damaged metadata file leaves a directory that
    /// cannot be read. That is the strongest reason to leave it alone, not a
    /// reason to treat the alias as free.
    #[test]
    fn run_from_refuses_a_derived_alias_whose_profile_cannot_be_read() {
        let (_tmp, paths) = setup_test_env();
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // The derived alias exists on disk but its metadata is unreadable.
        let dir = paths.profiles_dir().join("amir@sawmills.ai+team");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), r#"{"access_token":"salvageable"}"#).unwrap();
        std::fs::write(dir.join("meta.json"), "{ this is not json").unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap_err();

        // Several guards can speak first here — an unprovable workspace, an
        // unidentifiable occupant, an unreadable store. The invariant is that
        // the login is refused and the credentials survive, not which one
        // answers, so this pins that rather than one wording.
        assert!(
            !format!("{error:#}").is_empty(),
            "refusal carried no explanation"
        );
        assert!(
            std::fs::read_to_string(dir.join("auth.json"))
                .unwrap()
                .contains("salvageable"),
            "credentials behind unreadable metadata were destroyed"
        );
    }

    /// The operator names the base alias; codexctl derives the qualified one.
    /// Whichever guard fires — a positively different login, or an identity
    /// that cannot be read at all — overwriting a profile nobody named is not
    /// an approval anyone gave.
    #[test]
    fn run_from_refuses_a_derived_alias_holding_an_unidentifiable_profile() {
        let (_tmp, paths) = setup_test_env();
        let personal = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{personal}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // `amir@sawmills.ai+team` exists but declares no workspace anywhere:
        // no metadata claim, and a stored token that carries none either.
        let dir = paths.profiles_dir().join("amir@sawmills.ai+team");
        std::fs::create_dir_all(&dir).unwrap();
        let opaque = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0WiJ9.sig";
        std::fs::write(
            dir.join("auth.json"),
            format!(r#"{{"access_token":"{opaque}"}}"#),
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"alias":"amir@sawmills.ai+team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", Some("team"), &mut runner).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("Choose another alias") || message.contains("cannot be identified"),
            "unhelpful refusal: {message}"
        );
        let kept = std::fs::read_to_string(dir.join("auth.json")).unwrap();
        assert!(kept.contains(opaque), "the derived alias was overwritten");
    }

    /// Every profile written before this release has no `account_id` in its
    /// metadata, so a guard reading metadata alone is inert for exactly the
    /// profiles an upgrade brings with it. The stored token still carries the
    /// claim, so the guard has to read that.
    #[test]
    fn run_from_refuses_a_second_workspace_against_a_legacy_profile() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();
        // Rewrite meta.json the way an older codexctl left it: no workspace.
        let meta_path = paths
            .profiles_dir()
            .join("amir@sawmills.ai")
            .join("meta.json");
        std::fs::write(
            &meta_path,
            r#"{"alias":"amir@sawmills.ai","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("different account"),
            "unhelpful refusal: {error}"
        );
        let kept = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(
            kept.contains(&stored),
            "a legacy profile's credentials were replaced"
        );
    }

    /// A token that declares no workspace cannot prove it belongs to the seat
    /// already stored, so it must not be allowed to write over it. Treating
    /// "no claim" as "same account" is what let a second seat replace the
    /// first's credentials.
    #[test]
    fn run_from_refuses_a_claimless_login_against_a_stored_workspace() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-team");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        // No `chatgpt_account_id` claim at all.
        let claimless = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSJ9.sig";
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{claimless}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("--allow-adopt"),
            "unhelpful refusal: {error}"
        );
        let kept = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(kept.contains(&stored), "stored credentials were replaced");
    }

    /// Logging a second workspace into an alias that already holds another
    /// account must not replace its stored credentials. `save` already refuses
    /// this; `login` reaches the same store by a different path.
    #[test]
    fn run_from_refuses_to_overwrite_a_profile_holding_a_different_account() {
        let (_tmp, paths) = setup_test_env();
        let stored = synthetic_token("acct-personal");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{stored}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(
            &paths,
            "amir@sawmills.ai",
            None,
            &paths.codex_auth_json().clone(),
        )
        .unwrap();

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let error = run_from(&paths, "amir@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("different account"),
            "unhelpful refusal: {error}"
        );
        let kept = std::fs::read_to_string(
            paths
                .profiles_dir()
                .join("amir@sawmills.ai")
                .join("auth.json"),
        )
        .unwrap();
        assert!(kept.contains(&stored), "stored credentials were replaced");
        assert!(!kept.contains(&incoming));
    }

    fn save_existing(paths: &Paths, alias: &str, account: &str) {
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{}"}}"#, synthetic_token(account)),
        )
        .unwrap();
        profile::save_profile_to(paths, alias, None, &paths.codex_auth_json().clone()).unwrap();
    }

    fn stored_auth(paths: &Paths, alias: &str) -> String {
        std::fs::read_to_string(paths.profiles_dir().join(alias).join("auth.json")).unwrap()
    }

    /// The email names the account; the label separates its seats. A second
    /// workspace on one address lands beside the first instead of on top of it.
    #[test]
    fn run_from_derives_an_alias_from_the_label_when_the_alias_holds_another_account() {
        let (_tmp, paths) = setup_test_env();
        save_existing(&paths, "amir@sawmills.ai", "acct-personal");

        let incoming = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{incoming}"}}"#));

        let saved = run_from(&paths, "amir@sawmills.ai", Some("work"), &mut runner).unwrap();

        assert_eq!(saved, "amir@sawmills.ai+work");
        assert!(stored_auth(&paths, "amir@sawmills.ai+work").contains(&incoming));
        assert!(
            !stored_auth(&paths, "amir@sawmills.ai").contains(&incoming),
            "the first account was overwritten"
        );
    }

    /// Logging the same seat again refreshes the alias the label already
    /// produced, rather than inventing another one.
    #[test]
    fn run_from_refreshes_the_derived_alias_on_a_later_login() {
        let (_tmp, paths) = setup_test_env();
        save_existing(&paths, "amir@sawmills.ai", "acct-personal");
        save_existing(&paths, "amir@sawmills.ai+work", "acct-team");

        let refreshed = format!("{}refreshed", synthetic_token("acct-team"));
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{refreshed}"}}"#));

        let saved = run_from(&paths, "amir@sawmills.ai", Some("work"), &mut runner).unwrap();

        assert_eq!(saved, "amir@sawmills.ai+work");
        assert!(stored_auth(&paths, "amir@sawmills.ai+work").contains(&refreshed));
    }

    /// A label is display text and may hold characters an alias cannot.
    #[test]
    fn run_from_makes_a_label_alias_safe_before_deriving() {
        let (_tmp, paths) = setup_test_env();
        save_existing(&paths, "amir@sawmills.ai", "acct-personal");

        let mut runner = FakeLoginRunner::new(&format!(
            r#"{{"access_token":"{}"}}"#,
            synthetic_token("acct-team")
        ));

        let saved = run_from(&paths, "amir@sawmills.ai", Some("Team / Prod"), &mut runner).unwrap();

        assert_eq!(saved, "amir@sawmills.ai+team-prod");
    }

    /// Both the requested alias and the derived one already hold other
    /// accounts, so there is no safe place to land.
    #[test]
    fn run_from_refuses_when_the_derived_alias_also_holds_another_account() {
        let (_tmp, paths) = setup_test_env();
        save_existing(&paths, "amir@sawmills.ai", "acct-personal");
        save_existing(&paths, "amir@sawmills.ai+work", "acct-other");

        let mut runner = FakeLoginRunner::new(&format!(
            r#"{{"access_token":"{}"}}"#,
            synthetic_token("acct-team")
        ));

        let error = run_from(&paths, "amir@sawmills.ai", Some("work"), &mut runner).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("both hold other accounts"), "{message}");
        // Both candidate aliases must be named so the operator knows what to avoid.
        assert!(message.contains("amir@sawmills.ai+work"), "{message}");
        assert!(!stored_auth(&paths, "amir@sawmills.ai+work").contains("acct-team"));
    }

    /// Re-logging the same account into its own alias is the normal refresh
    /// path and must keep working.
    #[test]
    fn run_from_allows_relogin_of_the_same_account() {
        let (_tmp, paths) = setup_test_env();
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{}"}}"#, synthetic_token("acct-team")),
        )
        .unwrap();
        profile::save_profile_to(&paths, "team", None, &paths.codex_auth_json().clone()).unwrap();

        let refreshed = format!("{}x", synthetic_token("acct-team"));
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{refreshed}"}}"#));

        run_from(&paths, "team", None, &mut runner).unwrap();

        let saved =
            std::fs::read_to_string(paths.profiles_dir().join("team").join("auth.json")).unwrap();
        assert!(saved.contains(&refreshed));
    }

    /// A damaged profile that still records *which account* it holds is not
    /// open to anyone who names a login. Its own login cannot be recovered from
    /// the broken token, so a colleague in the same workspace would otherwise
    /// replace it. Repair means removing it first, which the error says.
    #[test]
    fn run_from_refuses_to_replace_an_identified_profile_with_unreadable_auth() {
        let (_tmp, paths) = setup_test_env();
        let live = synthetic_token("acct-team");
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{live}"}}"#),
        )
        .unwrap();
        profile::save_profile_to(&paths, "team", None, &paths.codex_auth_json().clone()).unwrap();
        profile::set_active_from(&paths, "team").unwrap();
        // Metadata still records the workspace; only the token is corrupt.
        std::fs::write(
            paths.profiles_dir().join("team").join("auth.json"),
            "{ not json",
        )
        .unwrap();

        let fresh = format!("{}fresh", synthetic_token("acct-team"));
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{fresh}"}}"#));

        let error = run_from(&paths, "team", None, &mut runner).unwrap_err();

        assert!(
            error.to_string().contains("--allow-adopt"),
            "the refusal does not name the remedy: {error}"
        );
        let saved =
            std::fs::read_to_string(paths.profiles_dir().join("team").join("auth.json")).unwrap();
        assert!(!saved.contains(&fresh), "the damaged profile was replaced");
    }

    /// A profile with nothing left to identify it — no readable token and no
    /// recorded account — is the one an operator is sent back to `login` to
    /// repair, and that still works.
    #[test]
    fn run_from_repairs_a_profile_with_no_identity_at_all() {
        let (_tmp, paths) = setup_test_env();
        let dir = paths.profiles_dir().join("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), "{ not json").unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"alias":"broken","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let fresh = synthetic_token("acct-team");
        let mut runner = FakeLoginRunner::new(&format!(r#"{{"access_token":"{fresh}"}}"#));

        run_from(&paths, "broken", None, &mut runner).unwrap();

        assert!(
            std::fs::read_to_string(dir.join("auth.json"))
                .unwrap()
                .contains(&fresh),
            "an unidentifiable profile could not be repaired"
        );
    }

    /// A bad label must cost nothing. Failing after the device-auth flow would
    /// make a completed login read as a failure.
    #[test]
    fn run_from_rejects_an_invalid_label_before_running_login() {
        let (_tmp, paths) = setup_test_env();
        let mut runner = FakeLoginRunner::new(r#"{"access_token":"new_tok"}"#);

        let error = run_from(
            &paths,
            "amir+8@sawmills.ai",
            Some("two\nlines"),
            &mut runner,
        )
        .unwrap_err();

        assert!(error.to_string().contains("label"), "{error}");
        assert!(runner.seen_home.is_none(), "login ran anyway");
        assert!(!paths.profiles_dir().join("amir+8@sawmills.ai").exists());
    }

    #[test]
    fn run_from_stores_a_valid_label() {
        let (_tmp, paths) = setup_test_env();
        let mut runner = FakeLoginRunner::new(r#"{"access_token":"new_tok"}"#);

        run_from(&paths, "amir-team", Some("  team  "), &mut runner).unwrap();

        assert_eq!(
            profile::get_profile_from(&paths, "amir-team")
                .unwrap()
                .meta
                .label
                .as_deref(),
            Some("team")
        );
    }

    #[test]
    fn run_from_removes_isolated_home_after_login_failure() {
        let (_tmp, paths) = setup_test_env();
        let mut runner = FailingLoginRunner::default();

        let error = run_from(&paths, "amir+8@sawmills.ai", None, &mut runner).unwrap_err();

        assert!(error.to_string().contains("simulated login failure"));
        assert!(!runner.seen_home.unwrap().exists());
    }
}
