use std::path::Path;

use assert_cmd::Command;
use codexctl::config::Paths;
use codexctl::profile;

// Fake JWT header `{"alg":"none"}`; only the claims payload is read.
const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";
// `{"sub":"seatA","exp":2000000000}`
const SEAT_A: &str = "eyJzdWIiOiJzZWF0QSIsImV4cCI6MjAwMDAwMDAwMH0";
// `{"sub":"seatB","exp":2000000000}`
const SEAT_B: &str = "eyJzdWIiOiJzZWF0QiIsImV4cCI6MjAwMDAwMDAwMH0";
// `{"sub":"seatC","exp":2000000000}`
const SEAT_C: &str = "eyJzdWIiOiJzZWF0QyIsImV4cCI6MjAwMDAwMDAwMH0";

fn setup(aliases: &[(&str, &str)]) -> (tempfile::TempDir, Paths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_home(tmp.path().to_path_buf());
    paths.ensure_dirs().unwrap();
    std::fs::create_dir_all(paths.codex_home()).unwrap();
    for (alias, token) in aliases {
        write_profile(&paths, alias, token);
    }
    (tmp, paths)
}

fn write_profile(paths: &Paths, alias: &str, access_token: &str) {
    let dir = paths.profiles_dir().join(alias);
    std::fs::create_dir_all(&dir).unwrap();
    write_auth(&dir.join("auth.json"), access_token);
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

fn write_auth(path: &Path, access_token: &str) {
    std::fs::write(path, format!(r#"{{"access_token":"{access_token}"}}"#)).unwrap();
}

fn codexctl(paths: &Paths) -> Command {
    let mut command = Command::cargo_bin("codexctl").unwrap();
    command.env("HOME", &paths.home).env_remove("CODEX_HOME");
    command
}

fn exec_home(paths: &Paths, alias: &str) -> std::path::PathBuf {
    paths.exec_homes_dir().join(alias)
}

#[test]
fn exec_help_documents_the_pinned_launch_shape() {
    let output = Command::cargo_bin("codexctl")
        .unwrap()
        .args(["exec", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--account"), "{stdout}");
    assert!(stdout.contains("<ARGS>"), "{stdout}");
}

/// The command to run is the whole point, so `exec` must not silently do
/// nothing when it is missing.
#[test]
fn exec_requires_an_account_and_a_command() {
    for args in [vec!["exec"], vec!["exec", "--account", "a"]] {
        Command::cargo_bin("codexctl")
            .unwrap()
            .args(&args)
            .assert()
            .failure();
    }
}

#[test]
fn unknown_alias_fails_without_provisioning_anything() {
    let (_tmp, paths) = setup(&[]);

    let output = codexctl(&paths)
        .args(["exec", "--account", "missing", "--", "true"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("missing"), "{stderr}");
    assert!(!exec_home(&paths, "missing").exists());
}

#[cfg(unix)]
#[test]
fn pinned_launch_leaves_the_active_account_alone() {
    let pinned = format!("{JWT_HDR}.{SEAT_A}.sig");
    let live = format!("{JWT_HDR}.{SEAT_B}.live");
    let (_tmp, paths) = setup(&[("a", &pinned), ("b", &live)]);
    write_auth(&paths.codex_auth_json(), &live);
    profile::set_active_from(&paths, "b").unwrap();

    codexctl(&paths)
        .args(["exec", "--account", "a", "--", "true"])
        .assert()
        .success();

    let exec_auth = std::fs::read_to_string(exec_home(&paths, "a").join("auth.json")).unwrap();
    assert!(exec_auth.contains(&pinned), "pinned auth not seeded");
    assert!(
        std::fs::read_to_string(paths.codex_auth_json())
            .unwrap()
            .contains(&live),
        "live Codex auth was rewritten"
    );
    assert_eq!(
        profile::get_active_from(&paths).unwrap().as_deref(),
        Some("b"),
        "active profile marker was rewritten"
    );
}

/// A pinned launch owns CODEX_HOME, so it must not quietly discard one the
/// caller already set.
#[test]
fn an_inherited_codex_home_is_refused() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
    let foreign = paths.home.join("work-codex");
    std::fs::create_dir_all(&foreign).unwrap();

    let output = codexctl(&paths)
        .env("CODEX_HOME", &foreign)
        .args(["exec", "--account", "a", "--", "true"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("CODEX_HOME"), "{stderr}");
    assert!(
        !exec_home(&paths, "a").exists(),
        "refused launches must not provision anything"
    );
}

/// The warning has to describe the token the child will use, not the one the
/// store held before seeding folded a killed run's fresher token back in.
#[cfg(unix)]
#[test]
fn a_healed_token_does_not_raise_a_stale_expiry_warning() {
    // `{"sub":"seatA","exp":1000000000}` — expired in 2001.
    let expired = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImV4cCI6MTAwMDAwMDAwMH0.old");
    let (_tmp, paths) = setup(&[("a", &expired)]);
    let fresh = format!("{JWT_HDR}.{SEAT_A}.rotated");
    std::fs::create_dir_all(exec_home(&paths, "a")).unwrap();
    write_auth(&exec_home(&paths, "a").join("auth.json"), &fresh);

    let output = codexctl(&paths)
        .args(["exec", "--account", "a", "--", "true"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("expired"),
        "false stale-token alarm: {stderr}"
    );
}

/// One seat saved under two aliases is ambiguous by token subject alone. The
/// alias the launch was pinned to is the fact that resolves it.
#[cfg(unix)]
#[test]
fn a_rotation_lands_on_the_pinned_alias_when_two_aliases_share_a_seat() {
    let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
    let (_tmp, paths) = setup(&[("primary", &stored), ("duplicate", &stored)]);
    let rotated = format!("{JWT_HDR}.{SEAT_A}.rotated");

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "primary",
            "--",
            "/bin/sh",
            "-c",
            &format!(r#"printf '{{"access_token":"{rotated}"}}' > "$CODEX_HOME/auth.json""#),
        ])
        .assert()
        .success();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("primary").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "the pinned alias did not receive its own rotation"
    );
}

#[cfg(unix)]
#[test]
fn child_runs_with_codex_home_pointed_at_the_pinned_account() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
    let out = paths.home.join("seen-home");

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            &format!(r#"printf %s "$CODEX_HOME" > "{}""#, out.display()),
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        exec_home(&paths, "a").display().to_string()
    );
}

/// A nested `codexctl codex` must be told which account it is running as. It
/// cannot always infer that from the token, and an account it fails to identify
/// is one its recovery will switch back to right after that account failed.
#[cfg(unix)]
#[test]
fn the_pinned_alias_is_named_for_nested_codexctl_processes() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
    let out = paths.home.join("seen-alias");

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            &format!(
                r#"printf %s "$CODEXCTL_PINNED_ALIAS" > "{}""#,
                out.display()
            ),
        ])
        .assert()
        .success();

    assert_eq!(std::fs::read_to_string(&out).unwrap(), "a");
}

#[cfg(unix)]
#[test]
fn child_keeps_the_current_working_directory() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
    let workdir = paths.home.join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    let output = codexctl(&paths)
        .current_dir(&workdir)
        .args(["exec", "--account", "a", "--", "/bin/sh", "-c", "pwd"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        std::fs::canonicalize(stdout.trim()).unwrap(),
        std::fs::canonicalize(&workdir).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn child_exit_code_becomes_the_process_exit_code() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);

    let output = codexctl(&paths)
        .args(["exec", "--account", "a", "--", "/bin/sh", "-c", "exit 7"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
}

/// Forwarded arguments belong to the child, not to codexctl.
#[cfg(unix)]
#[test]
fn child_arguments_that_look_like_flags_are_forwarded() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);

    let output = codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            r#"printf %s "$1""#,
            "sh",
            "--allow-billing",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "--allow-billing");
}

#[cfg(unix)]
#[test]
fn shared_codex_entries_are_linked_and_sessions_land_in_the_real_home() {
    let (_tmp, paths) = setup(&[("a", &format!("{JWT_HDR}.{SEAT_A}.sig"))]);
    std::fs::create_dir_all(paths.codex_home().join("sessions")).unwrap();
    std::fs::write(paths.codex_home().join("config.toml"), "model = \"gpt-5\"").unwrap();
    write_auth(
        &paths.codex_auth_json(),
        &format!("{JWT_HDR}.{SEAT_B}.live"),
    );

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            r#"printf rollout > "$CODEX_HOME/sessions/x""#,
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(paths.codex_home().join("sessions").join("x")).unwrap(),
        "rollout",
        "session rollouts must reach the real Codex home"
    );
    assert!(
        !exec_home(&paths, "a")
            .join("auth.json")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "credentials must not be shared"
    );
    assert!(
        exec_home(&paths, "a")
            .join("config.toml")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn a_refreshed_token_is_folded_back_and_a_foreign_one_is_not() {
    let stored = format!("{JWT_HDR}.{SEAT_A}.stored");
    let (_tmp, paths) = setup(&[("a", &stored), ("b", &format!("{JWT_HDR}.{SEAT_B}.b"))]);
    let rotated = format!("{JWT_HDR}.{SEAT_A}.rotated");
    let foreign = format!("{JWT_HDR}.{SEAT_C}.foreign");

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            &format!(r#"printf '{{"access_token":"{rotated}"}}' > "$CODEX_HOME/auth.json""#),
        ])
        .assert()
        .success();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("a").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "refreshed token was not captured"
    );

    codexctl(&paths)
        .args([
            "exec",
            "--account",
            "a",
            "--",
            "/bin/sh",
            "-c",
            &format!(r#"printf '{{"access_token":"{foreign}"}}' > "$CODEX_HOME/auth.json""#),
        ])
        .assert()
        .success();

    for alias in ["a", "b"] {
        assert!(
            !std::fs::read_to_string(paths.profiles_dir().join(alias).join("auth.json"))
                .unwrap()
                .contains(&foreign),
            "a foreign login was folded into profile '{alias}'"
        );
    }
}

/// The SAW-9631 regression: two lanes launching at the same time must each keep
/// their own account, which a global `use` cannot guarantee.
#[cfg(unix)]
#[test]
fn simultaneous_pinned_launches_never_cross_credentials() {
    let a_token = format!("{JWT_HDR}.{SEAT_A}.a");
    let b_token = format!("{JWT_HDR}.{SEAT_B}.b");
    let live = format!("{JWT_HDR}.{SEAT_C}.live");
    let (_tmp, paths) = setup(&[("a", &a_token), ("b", &b_token)]);
    write_auth(&paths.codex_auth_json(), &live);
    profile::set_active_from(&paths, "a").unwrap();

    let children: Vec<_> = ["a", "b"]
        .iter()
        .map(|alias| {
            let out = paths.home.join(format!("out-{alias}"));
            let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("codexctl"));
            command
                .env("HOME", &paths.home)
                // `exec` refuses an inherited Codex home, so the developer's
                // own CODEX_HOME must not decide whether this test runs.
                .env_remove("CODEX_HOME")
                .args([
                    "exec",
                    "--account",
                    alias,
                    "--",
                    "/bin/sh",
                    "-c",
                    &format!(
                        r#"sleep 1; cat "$CODEX_HOME/auth.json" > "{}""#,
                        out.display()
                    ),
                ])
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let seen_a = std::fs::read_to_string(paths.home.join("out-a")).unwrap();
    let seen_b = std::fs::read_to_string(paths.home.join("out-b")).unwrap();
    assert!(
        seen_a.contains(&a_token) && !seen_a.contains(&b_token),
        "{seen_a}"
    );
    assert!(
        seen_b.contains(&b_token) && !seen_b.contains(&a_token),
        "{seen_b}"
    );
    assert!(
        std::fs::read_to_string(paths.codex_auth_json())
            .unwrap()
            .contains(&live),
        "live Codex auth was rewritten"
    );
    assert_eq!(
        profile::get_active_from(&paths).unwrap().as_deref(),
        Some("a")
    );
}
