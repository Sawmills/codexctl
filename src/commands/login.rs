use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::commands::{alias, status};
use crate::config::{self, Paths};
use crate::profile;
use crate::store;

trait CodexLoginRunner {
    fn run_codex_login(&mut self, codex_home: &Path) -> Result<()>;
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
    let codex_home = {
        let _lock = store::lock(paths)?;
        let home = isolated_login_home(paths, alias)?;
        store::ensure_private_dir(&home)?;
        home
    };
    runner.run_codex_login(&codex_home)?;

    let auth_path = codex_home.join("auth.json");
    if !auth_path.exists() {
        bail!("codex login did not create {}", auth_path.display());
    }

    let email = email_from_alias(alias);
    profile::save_profile_and_activate_to(paths, alias, email.as_deref(), &auth_path)?;

    Ok(())
}

fn isolated_login_home(paths: &Paths, alias: &str) -> Result<PathBuf> {
    store::login_home(paths, alias)
}

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
            isolated_login_home(&paths, "amir+8@sawmills.ai").unwrap(),
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
            Some(
                isolated_login_home(&paths, "amir+8@sawmills.ai")
                    .unwrap()
                    .as_path()
            )
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
