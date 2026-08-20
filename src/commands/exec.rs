use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::api;
use crate::commands::alias;
use crate::config::{self, Paths};
use crate::profile;
use crate::store;

const AUTH_FILE: &str = "auth.json";

/// Names the account a pinned launch selected, for codexctl processes inside it.
///
/// A nested `codexctl codex` otherwise has to infer that account from the token
/// in the pinned home, which one seat saved under two aliases makes ambiguous.
/// Guessing wrong lets recovery hand back the account that just failed.
pub const PINNED_ALIAS_ENV: &str = "CODEXCTL_PINNED_ALIAS";

pub fn run(account: &str, args: &[String]) -> Result<i32> {
    refuse_inherited_codex_home(std::env::var_os("CODEX_HOME").as_deref())?;
    run_from(&config::default_paths()?, account, args)
}

/// Refuse to launch from inside somebody else's Codex home.
///
/// A pinned launch owns `CODEX_HOME`, so an inherited one would be replaced
/// without a word: the child would read the shared `~/.codex` settings instead
/// of the caller's, and write its rollouts where the caller's `codex resume`
/// never looks. Refusing keeps that surprise out of the launch path.
fn refuse_inherited_codex_home(codex_home: Option<&OsStr>) -> Result<()> {
    // Presence is the test, not usefulness. An empty value is still a caller
    // who touched the variable, and guessing what they meant by it is the
    // silent substitution this refusal exists to prevent.
    let Some(codex_home) = codex_home else {
        return Ok(());
    };
    bail!(
        "CODEX_HOME is already set to '{}'. `codexctl exec` sets CODEX_HOME itself and will not run inside another Codex home; unset it to pin a launch.",
        Path::new(codex_home).display()
    )
}

/// Run a command with its Codex credentials pinned to one saved profile.
///
/// The pinning is per child process: `CODEX_HOME` points the child at a home
/// that holds that account's `auth.json`. The live Codex home and the active
/// profile marker are never written, so two pinned launches cannot take each
/// other's account, and neither can a concurrent `codexctl use`.
fn run_from(paths: &Paths, account: &str, args: &[String]) -> Result<i32> {
    let alias = alias::required(account)?;
    let Some((program, arguments)) = args.split_first() else {
        bail!("no command to run. Use 'codexctl exec --account <alias> -- <command> [args...]'");
    };

    let home = provision_exec_home(paths, alias)?;
    let status = Command::new(program)
        .args(arguments)
        .env("CODEX_HOME", &home)
        .env(PINNED_ALIAS_ENV, alias)
        .status()
        .with_context(|| format!("failed to run `{program}`"))?;

    // Codex rotates its token in place, so the refreshed copy lives in the
    // pinned home. Fold it back, or `status` and the next launch keep reporting
    // the token this run started with.
    if let Err(error) = profile::capture_exec_auth_from(paths, &home.join(AUTH_FILE), alias) {
        eprintln!("warning: failed to capture tokens for profile '{alias}': {error:#}");
    }

    Ok(exit_code(status))
}

/// Prepare the pinned Codex home for `alias` and return its path.
fn provision_exec_home(paths: &Paths, alias: &str) -> Result<PathBuf> {
    // Resolve the profile first so an unknown alias fails before anything is
    // created on its behalf.
    profile::get_profile_from(paths, alias)?;

    store::ensure_private_dir(&paths.exec_homes_dir())?;
    let home = store::exec_home(paths, alias)?;
    store::ensure_private_dir(&home)?;
    let exec_auth = home.join(AUTH_FILE);
    profile::seed_exec_auth_from(paths, alias, &exec_auth)?;
    // Judge the token the child will actually use. Seeding can fold a fresher
    // token left behind by a killed run back over the saved copy, and warning
    // before that runs cries stale about a token that is live.
    warn_when_token_expired(alias, &exec_auth);
    link_shared_codex_entries(paths, &home)?;
    Ok(home)
}

/// Stay offline like `codexctl use`: report the stale token and launch anyway,
/// since Codex refreshes it from the stored refresh token.
fn warn_when_token_expired(alias: &str, auth_json: &Path) {
    let Ok(auth) = api::read_auth_json(auth_json) else {
        return;
    };
    if api::is_token_expired(&auth.access_token) {
        eprintln!(
            "warning: profile '{alias}' has an expired access token; Codex will try to refresh it"
        );
    }
}

/// Share everything except credentials with the live Codex home.
///
/// Config, global instructions, and the sessions directory are symlinked, so a
/// pinned run reads the same settings and writes its rollouts where `codex
/// resume` and the `codexctl codex` wrapper already look for them. Only
/// `auth.json` is a real per-alias file. An entry that already exists is never
/// replaced, so a link the child turned into a real file stays as it left it.
fn link_shared_codex_entries(paths: &Paths, home: &Path) -> Result<()> {
    let codex_home = paths.codex_home();
    let entries = match std::fs::read_dir(&codex_home) {
        Ok(entries) => entries,
        // No live Codex home to share: the pinned home holds credentials only.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", codex_home.display()));
        }
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", codex_home.display()))?;
        let name = entry.file_name();
        if name == OsStr::new(AUTH_FILE) || is_atomic_replace_temp(&name) {
            continue;
        }
        let link = home.join(&name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        symlink_shared_entry(&entry.path(), &link)?;
    }

    Ok(())
}

/// Whether a name is one of the temporary files `store::atomic_replace` holds
/// for the moment before its rename.
///
/// Linking one leaves a symlink that dangles as soon as the rename lands, and
/// nothing replaces an existing entry afterwards — so a `codexctl use` running
/// during the scan would leave permanent litter in the pinned home.
fn is_atomic_replace_temp(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
}

#[cfg(unix)]
fn symlink_shared_entry(target: &Path, link: &Path) -> Result<()> {
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => Ok(()),
        // A concurrent launch of the same alias won the race and linked the
        // same target, so the entry is already shared.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to link {}", link.display())),
    }
}

#[cfg(not(unix))]
fn symlink_shared_entry(_target: &Path, link: &Path) -> Result<()> {
    // Silently keeping the entry unshared would send session rollouts somewhere
    // `codex resume` never looks, so say so instead of pretending it worked.
    bail!(
        "cannot share {} with the pinned Codex home: this platform has no symbolic links",
        link.display()
    )
}

/// Map the child status onto an exit code the way a shell does, so a signal
/// death stays distinguishable from a clean exit with the same number.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake JWT header `{"alg":"none"}`; only the claims payload is read.
    const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";
    // `{"sub":"seatA","exp":2000000000}`
    const SEAT_A: &str = "eyJzdWIiOiJzZWF0QSIsImV4cCI6MjAwMDAwMDAwMH0";
    // `{"sub":"seatB","exp":2000000000}`
    const SEAT_B: &str = "eyJzdWIiOiJzZWF0QiIsImV4cCI6MjAwMDAwMDAwMH0";

    fn setup(aliases: &[(&str, &str)]) -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        for (alias, token) in aliases {
            write_profile(&paths, alias, token);
        }
        (tmp, paths)
    }

    fn write_profile(paths: &Paths, alias: &str, access_token: &str) {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(AUTH_FILE),
            format!(r#"{{"access_token":"{access_token}"}}"#),
        )
        .unwrap();
        let meta = profile::Meta {
            alias: alias.to_string(),
            email: None,
            plan: None,
            saved_at: "2026-01-01T00:00:00Z".to_string(),
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    fn token_of(path: &Path) -> String {
        api::read_auth_json(path).unwrap().access_token
    }

    #[test]
    fn an_inherited_codex_home_is_refused() {
        assert!(refuse_inherited_codex_home(None).is_ok());
        // Set-but-empty is still set.
        assert!(refuse_inherited_codex_home(Some(OsStr::new(""))).is_err());

        let error = refuse_inherited_codex_home(Some(OsStr::new("/tmp/work-codex"))).unwrap_err();

        assert!(error.to_string().contains("/tmp/work-codex"), "{error:#}");
    }

    #[test]
    fn atomic_replace_temporaries_are_not_shared() {
        assert!(is_atomic_replace_temp(OsStr::new(
            ".auth.json.codexctl-1-2-3.tmp"
        )));
        assert!(!is_atomic_replace_temp(OsStr::new("config.toml")));
        assert!(!is_atomic_replace_temp(OsStr::new("sessions")));
    }

    #[test]
    fn unknown_alias_fails_before_creating_an_exec_home() {
        let (_tmp, paths) = setup(&[]);

        let error = run_from(&paths, "missing", &["true".to_string()]).unwrap_err();

        assert!(error.to_string().contains("missing"), "{error:#}");
        assert!(!paths.exec_homes_dir().exists());
    }

    #[test]
    fn missing_command_is_rejected() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);

        assert!(run_from(&paths, "a", &[]).is_err());
    }

    #[test]
    fn provisioning_seeds_the_pinned_auth_without_touching_global_state() {
        let token = format!("{JWT_HDR}.{SEAT_A}.sig");
        let (_tmp, paths) = setup(&[("a", &token)]);
        let live = format!("{JWT_HDR}.{SEAT_B}.live");
        std::fs::create_dir_all(paths.codex_home()).unwrap();
        std::fs::write(
            paths.codex_auth_json(),
            format!(r#"{{"access_token":"{live}"}}"#),
        )
        .unwrap();

        let home = provision_exec_home(&paths, "a").unwrap();

        assert_eq!(home, paths.exec_homes_dir().join("a"));
        assert_eq!(token_of(&home.join(AUTH_FILE)), token);
        assert_eq!(token_of(&paths.codex_auth_json()), live);
        assert!(!paths.active_file().exists());
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_shares_every_codex_entry_except_credentials() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
        let codex_home = paths.codex_home();
        std::fs::create_dir_all(codex_home.join("sessions")).unwrap();
        std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5\"").unwrap();
        std::fs::write(codex_home.join(AUTH_FILE), r#"{"access_token":"live"}"#).unwrap();

        let home = provision_exec_home(&paths, "a").unwrap();

        for shared in ["config.toml", "sessions"] {
            let link = home.join(shared);
            assert!(
                link.symlink_metadata().unwrap().file_type().is_symlink(),
                "{shared} is not shared"
            );
            assert_eq!(std::fs::read_link(&link).unwrap(), codex_home.join(shared));
        }
        assert!(
            !home
                .join(AUTH_FILE)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "credentials must stay pinned per account"
        );
    }

    /// A `codexctl use` mid-`atomic_replace` leaves a temporary file in the
    /// live home for an instant. Linking it would dangle the moment the rename
    /// lands, and nothing replaces an existing entry afterwards.
    #[cfg(unix)]
    #[test]
    fn provisioning_skips_a_temporary_file_from_a_concurrent_switch() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
        std::fs::create_dir_all(paths.codex_home()).unwrap();
        let temp_name = ".auth.json.codexctl-123-456-0.tmp";
        std::fs::write(paths.codex_home().join(temp_name), "in flight").unwrap();

        let home = provision_exec_home(&paths, "a").unwrap();

        assert!(
            home.join(temp_name).symlink_metadata().is_err(),
            "a transient file was shared into the pinned home"
        );
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_keeps_an_entry_the_child_replaced() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
        std::fs::create_dir_all(paths.codex_home()).unwrap();
        std::fs::write(paths.codex_home().join("config.toml"), "shared").unwrap();
        let home = provision_exec_home(&paths, "a").unwrap();
        std::fs::remove_file(home.join("config.toml")).unwrap();
        std::fs::write(home.join("config.toml"), "pinned").unwrap();

        provision_exec_home(&paths, "a").unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "pinned"
        );
        assert_eq!(
            std::fs::read_to_string(paths.codex_home().join("config.toml")).unwrap(),
            "shared"
        );
    }

    #[test]
    fn provisioning_works_without_a_live_codex_home() {
        let token = format!("{JWT_HDR}.{SEAT_A}.sig");
        let (_tmp, paths) = setup(&[("a", &token)]);

        let home = provision_exec_home(&paths, "a").unwrap();

        assert_eq!(token_of(&home.join(AUTH_FILE)), token);
    }

    #[cfg(unix)]
    #[test]
    fn a_refreshed_pinned_token_is_folded_back_into_its_profile() {
        let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
        let (_tmp, paths) = setup(&[("a", &stored)]);
        let rotated = format!("{JWT_HDR}.{SEAT_A}.rotated");

        let code = run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(r#"printf '{{"access_token":"{rotated}"}}' > "$CODEX_HOME/auth.json""#),
            ],
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            token_of(&paths.profiles_dir().join("a").join(AUTH_FILE)),
            rotated
        );
    }

    /// The refresh token is what buys the next access token, so a rotation of
    /// it alone still has to reach the store.
    #[cfg(unix)]
    #[test]
    fn a_rotated_refresh_token_is_captured_on_its_own() {
        let access = format!("{JWT_HDR}.{SEAT_A}.stored");
        let (_tmp, paths) = setup(&[("a", &access)]);
        let profile_auth = paths.profiles_dir().join("a").join(AUTH_FILE);
        std::fs::write(
            &profile_auth,
            format!(r#"{{"access_token":"{access}","refresh_token":"old"}}"#),
        )
        .unwrap();

        run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    r#"printf '{{"access_token":"{access}","refresh_token":"new"}}' > "$CODEX_HOME/auth.json""#
                ),
            ],
        )
        .unwrap();

        let captured = api::read_auth_json(&profile_auth).unwrap();
        assert_eq!(captured.access_token, access);
        assert_eq!(captured.refresh_token.as_deref(), Some("new"));
    }

    #[cfg(unix)]
    #[test]
    fn a_foreign_login_in_the_pinned_home_is_not_folded_into_the_store() {
        let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
        let (_tmp, paths) = setup(&[("a", &stored)]);
        // sub seatC is not saved under any alias.
        let foreign = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QyIsImV4cCI6MjAwMDAwMDAwMH0.sig");

        run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(r#"printf '{{"access_token":"{foreign}"}}' > "$CODEX_HOME/auth.json""#),
            ],
        )
        .unwrap();

        assert_eq!(
            token_of(&paths.profiles_dir().join("a").join(AUTH_FILE)),
            stored
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_earlier_expiring_pinned_token_does_not_regress_the_profile() {
        // `{"sub":"seatA","exp":1900000000}` — same seat, expires sooner.
        let earlier = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImV4cCI6MTkwMDAwMDAwMH0.old");
        let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
        let (_tmp, paths) = setup(&[("a", &stored)]);

        run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(r#"printf '{{"access_token":"{earlier}"}}' > "$CODEX_HOME/auth.json""#),
            ],
        )
        .unwrap();

        assert_eq!(
            token_of(&paths.profiles_dir().join("a").join(AUTH_FILE)),
            stored
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_token_left_by_a_killed_run_is_captured_on_the_next_launch() {
        let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
        let (_tmp, paths) = setup(&[("a", &stored)]);
        let rotated = format!("{JWT_HDR}.{SEAT_A}.rotated");
        let home = provision_exec_home(&paths, "a").unwrap();
        // Stands in for a run that was killed before it could capture.
        std::fs::write(
            home.join(AUTH_FILE),
            format!(r#"{{"access_token":"{rotated}"}}"#),
        )
        .unwrap();

        provision_exec_home(&paths, "a").unwrap();

        assert_eq!(
            token_of(&paths.profiles_dir().join("a").join(AUTH_FILE)),
            rotated
        );
        assert_eq!(token_of(&home.join(AUTH_FILE)), rotated);
    }

    #[cfg(unix)]
    #[test]
    fn child_exit_code_is_returned() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);

        let code = run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 7".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(code, 7);
    }

    #[cfg(unix)]
    #[test]
    fn child_killed_by_a_signal_reports_the_shell_convention_code() {
        let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);

        let code = run_from(
            &paths,
            "a",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "kill -TERM $$".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(code, 128 + libc::SIGTERM);
    }
}
