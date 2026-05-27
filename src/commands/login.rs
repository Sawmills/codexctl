use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::commands::{alias, status};
use crate::config::{self, Paths};
use crate::profile;

trait CodexLoginRunner {
    fn run_codex_login(&mut self, codex_home: &Path) -> Result<()>;
}

struct CodexCliLoginRunner;

impl CodexLoginRunner for CodexCliLoginRunner {
    fn run_codex_login(&mut self, codex_home: &Path) -> Result<()> {
        std::fs::create_dir_all(codex_home)
            .with_context(|| format!("failed to create {}", codex_home.display()))?;

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

pub fn run(alias: &str) -> Result<()> {
    let alias = alias::required(alias)?;
    let paths = config::default_paths()?;
    let mut runner = CodexCliLoginRunner;
    run_from(&paths, alias, &mut runner)?;

    println!("logged in and saved profile '{alias}'");
    println!();
    status::run_focused(alias)?;
    Ok(())
}

fn run_from(paths: &Paths, alias: &str, runner: &mut impl CodexLoginRunner) -> Result<()> {
    let alias = alias::required(alias)?;
    let codex_home = isolated_login_home(paths, alias);
    runner.run_codex_login(&codex_home)?;

    let auth_path = codex_home.join("auth.json");
    if !auth_path.exists() {
        bail!("codex login did not create {}", auth_path.display());
    }

    let email = email_from_alias(alias);
    profile::save_profile_to(paths, alias, email.as_deref(), &auth_path)?;
    activate_imported_profile(paths, alias)?;

    Ok(())
}

fn isolated_login_home(paths: &Paths, alias: &str) -> PathBuf {
    paths.login_homes_dir().join(alias)
}

fn email_from_alias(alias: &str) -> Option<String> {
    if alias.contains('@') {
        Some(alias.to_string())
    } else {
        None
    }
}

fn activate_imported_profile(paths: &Paths, alias: &str) -> Result<()> {
    let codex_auth = paths.codex_auth_json();
    if let Some(parent) = codex_auth.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if profile::get_active_from(paths)?.as_deref() == Some(alias) {
        let profile = profile::get_profile_from(paths, alias)?;
        std::fs::copy(profile.auth_json_path(), &codex_auth)
            .with_context(|| "failed to copy auth.json to ~/.codex/")?;
        profile::set_active_from(paths, alias)?;
        return Ok(());
    }

    profile::switch_to_from(paths, alias)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLoginRunner {
        auth_json: String,
        seen_home: Option<PathBuf>,
    }

    impl FakeLoginRunner {
        fn new(auth_json: &str) -> Self {
            Self {
                auth_json: auth_json.to_string(),
                seen_home: None,
            }
        }
    }

    impl CodexLoginRunner for FakeLoginRunner {
        fn run_codex_login(&mut self, codex_home: &Path) -> Result<()> {
            self.seen_home = Some(codex_home.to_path_buf());
            std::fs::create_dir_all(codex_home)?;
            std::fs::write(codex_home.join("auth.json"), &self.auth_json)?;
            Ok(())
        }
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
    fn isolated_login_home_is_profile_scoped() {
        let (_tmp, paths) = setup_test_env();

        assert_eq!(
            isolated_login_home(&paths, "amir+8@sawmills.ai"),
            paths
                .codexctl_dir()
                .join("login-homes")
                .join("amir+8@sawmills.ai")
        );
    }

    #[test]
    fn run_from_uses_isolated_home_and_imports_auth() {
        let (_tmp, paths) = setup_test_env();
        let mut runner = FakeLoginRunner::new(r#"{"access_token":"new_tok"}"#);

        run_from(&paths, "  amir+8@sawmills.ai  ", &mut runner).unwrap();

        assert_eq!(
            runner.seen_home.as_deref(),
            Some(isolated_login_home(&paths, "amir+8@sawmills.ai").as_path())
        );
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

        run_from(&paths, "amir+8@sawmills.ai", &mut runner).unwrap();

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
}
