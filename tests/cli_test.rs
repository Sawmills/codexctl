use assert_cmd::Command;

#[test]
fn help_shows_all_subcommands() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.arg("--help").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("status"));
    assert!(stdout.contains("login"));
    assert!(stdout.contains("save"));
    assert!(stdout.contains("use"));
    assert!(stdout.contains("switch"));
    assert!(stdout.contains("list"));
    assert!(stdout.contains("remove"));
    assert!(stdout.contains("whoami"));
    assert!(stdout.contains("codex"));
    assert!(stdout.contains("resets"));
    assert!(stdout.contains("reset"));
    assert!(stdout.contains("label"));
    assert!(stdout.contains("completions"));
}

// === Labels ===

const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

/// Build an unsigned JWT carrying `claims`. Every claim is synthetic; no real
/// token value enters a fixture.
fn synthetic_token(claims: &str) -> String {
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    format!("{JWT_HDR}.{payload}.sig")
}

fn seat_token(email: &str, account_id: &str, plan: &str) -> String {
    synthetic_token(&format!(
        r#"{{"sub":"seat-{account_id}",
             "https://api.openai.com/profile":{{"email":"{email}"}},
             "https://api.openai.com/auth":{{
                "chatgpt_account_id":"{account_id}",
                "chatgpt_user_id":"user-1",
                "chatgpt_plan_type":"{plan}"}}}}"#
    ))
}

fn write_profile(home: &std::path::Path, alias: &str, token: &str, meta: &str) {
    let dir = home.join(".codexctl/profiles").join(alias);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();
    std::fs::write(dir.join("meta.json"), meta).unwrap();
}

fn run(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn label_sets_and_clears_a_profile_label() {
    let tmp = tempfile::tempdir().unwrap();
    let token = seat_token("amir@sawmills.ai", "acct-team", "business");
    write_profile(
        tmp.path(),
        "amir-team",
        &token,
        r#"{"alias":"amir-team","email":"amir@sawmills.ai","plan":"business","saved_at":"2026-01-01T00:00:00Z"}"#,
    );

    assert!(
        run(tmp.path(), &["label", "amir-team", "team"])
            .status
            .success()
    );
    let listed = stdout_of(&run(tmp.path(), &["list"]));
    assert!(listed.contains("team"), "label missing from list: {listed}");

    // Omitting the text clears the label.
    assert!(run(tmp.path(), &["label", "amir-team"]).status.success());
    let cleared = stdout_of(&run(tmp.path(), &["list"]));
    assert!(
        !cleared.contains("Label"),
        "cleared label still shown: {cleared}"
    );
}

#[test]
fn label_fails_for_an_unknown_alias() {
    let tmp = tempfile::tempdir().unwrap();

    assert!(
        !run(tmp.path(), &["label", "missing", "team"])
            .status
            .success()
    );
}

/// Until a label exists the tables must look exactly as they did before, so an
/// operator who never labels anything sees no new empty column.
#[test]
fn list_hides_the_label_column_until_a_label_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let token = seat_token("amir@sawmills.ai", "acct-personal", "pro");
    write_profile(
        tmp.path(),
        "amir@sawmills.ai",
        &token,
        r#"{"alias":"amir@sawmills.ai","email":"amir@sawmills.ai","plan":"pro","saved_at":"2026-01-01T00:00:00Z"}"#,
    );

    let bare = stdout_of(&run(tmp.path(), &["list"]));
    assert!(bare.contains("Account"), "list is not a table: {bare}");
    assert!(!bare.contains("Label"), "empty label column shown: {bare}");

    run(tmp.path(), &["label", "amir@sawmills.ai", "personal"]);

    let labeled = stdout_of(&run(tmp.path(), &["list"]));
    assert!(labeled.contains("Label"), "label column missing: {labeled}");
    assert!(labeled.contains("personal"), "label missing: {labeled}");
}

/// Two profiles on one email are the case this whole feature exists for: the
/// email cannot tell them apart, so the label has to.
#[test]
fn list_distinguishes_two_profiles_that_share_one_email() {
    let tmp = tempfile::tempdir().unwrap();
    // Label text deliberately shares no substring with the alias, so matching
    // it proves the label column is rendered rather than the alias.
    for (alias, account, plan, label) in [
        ("amir-1", "acct-personal", "pro", "my own"),
        ("amir-2", "acct-team", "business", "sawmills seat"),
    ] {
        let token = seat_token("amir@sawmills.ai", account, plan);
        write_profile(
            tmp.path(),
            alias,
            &token,
            &format!(
                r#"{{"alias":"{alias}","email":"amir@sawmills.ai","plan":"{plan}","account_id":"{account}","saved_at":"2026-01-01T00:00:00Z"}}"#
            ),
        );
        run(tmp.path(), &["label", alias, label]);
    }

    let listed = stdout_of(&run(tmp.path(), &["list"]));

    assert!(listed.contains("Label"), "{listed}");
    assert!(listed.contains("my own"), "{listed}");
    assert!(listed.contains("sawmills seat"), "{listed}");
    // One email, two rows: the address alone cannot separate them.
    assert_eq!(listed.matches("amir@sawmills.ai").count(), 2, "{listed}");
}

#[test]
fn whoami_shows_the_label_of_the_active_profile() {
    let tmp = tempfile::tempdir().unwrap();
    write_profile(
        tmp.path(),
        "amir-2",
        &seat_token("amir@sawmills.ai", "acct-team", "business"),
        r#"{"alias":"amir-2","label":"sawmills seat","email":"amir@sawmills.ai","plan":"business","saved_at":"2026-01-01T00:00:00Z"}"#,
    );
    std::fs::write(tmp.path().join(".codexctl/active"), "amir-2").unwrap();

    let out = stdout_of(&run(tmp.path(), &["whoami"]));

    assert!(out.contains("sawmills seat"), "label missing: {out}");
    assert!(out.contains("amir@sawmills.ai"), "email missing: {out}");
}

#[test]
fn save_and_login_accept_a_label() {
    for command in ["save", "login"] {
        let output = Command::cargo_bin("codexctl")
            .unwrap()
            .args([command, "--help"])
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("--label"), "{command} lacks --label");
    }
}

/// The hazard that makes a duplicate-email account dangerous: `save` with no
/// alias targets the existing profile, and one keystroke at the overwrite
/// prompt would destroy the other account's tokens.
#[test]
fn save_refuses_to_overwrite_a_profile_holding_a_different_account() {
    let tmp = tempfile::tempdir().unwrap();
    write_profile(
        tmp.path(),
        "amir@sawmills.ai",
        &seat_token("amir@sawmills.ai", "acct-personal", "pro"),
        r#"{"alias":"amir@sawmills.ai","email":"amir@sawmills.ai","plan":"pro","account_id":"acct-personal","saved_at":"2026-01-01T00:00:00Z"}"#,
    );
    let codex_dir = tmp.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let incoming = seat_token("amir@sawmills.ai", "acct-team", "business");
    std::fs::write(
        codex_dir.join("auth.json"),
        format!(r#"{{"access_token":"{incoming}"}}"#),
    )
    .unwrap();

    let output = run(tmp.path(), &["save"]);

    assert!(!output.status.success(), "save did not refuse");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("different account"),
        "unhelpful refusal: {stderr}"
    );
    // The stored tokens must be untouched.
    let stored = std::fs::read_to_string(
        tmp.path()
            .join(".codexctl/profiles/amir@sawmills.ai/auth.json"),
    )
    .unwrap();
    assert!(
        !stored.contains(&incoming),
        "personal profile was clobbered"
    );
}

#[test]
fn reset_accepts_an_alias_and_unattended_flags() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["reset", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[ALIAS]"), "alias is optional: {stdout}");
    assert!(stdout.contains("--yes"));
    assert!(stdout.contains("--credit"));
}

/// Spending banked resets and spending credits are separate approvals, so the
/// wrapper must expose a separate flag for each.
#[test]
fn codex_has_independent_reset_and_billing_flags() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["codex", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--allow-billing"));
    assert!(stdout.contains("--allow-resets"));
}

#[test]
fn use_keeps_reset_approval_but_hides_obsolete_billing_approval() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["use", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--allow-resets"));
    assert!(!stdout.contains("--allow-billing"));

    let mut compatibility_cmd = Command::cargo_bin("codexctl").unwrap();
    let output = compatibility_cmd
        .args(["use", "--allow-billing", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

/// Installed builds need to be able to report their own version, so an upgrade
/// can be confirmed from the binary rather than from the package manager.
#[test]
fn version_flag_reports_the_package_version() {
    for flag in ["--version", "-V"] {
        let mut cmd = Command::cargo_bin("codexctl").unwrap();
        let output = cmd.arg(flag).output().unwrap();
        assert!(output.status.success(), "{flag} should exit zero");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.trim(),
            format!("codexctl {}", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn unknown_subcommand_fails() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    cmd.arg("nonexistent").assert().failure();
}

#[test]
fn codex_help_uses_safe_recovery_prompt_default() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["codex", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Continue the previous request."));
    assert!(!stdout.contains("[default: resume]"));
}

#[test]
fn status_accepts_rate_limited_flag() {
    let mut cmd = Command::cargo_bin("codexctl").unwrap();
    let output = cmd.args(["status", "--help"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--rate-limited"));
    assert!(stdout.contains("--usage-based"));
}

#[test]
fn informational_and_invalid_commands_do_not_create_profile_state() {
    let commands: &[&[&str]] = &[
        &["--help"],
        &["--version"],
        &["-V"],
        &["completions", "bash"],
        &["nonexistent"],
    ];

    for args in commands {
        let tmp = tempfile::tempdir().unwrap();
        let mut command = Command::cargo_bin("codexctl").unwrap();
        let output = command
            .env("HOME", tmp.path())
            .args(*args)
            .output()
            .unwrap();
        let expect_failure = *args == ["nonexistent"];
        assert_eq!(
            output.status.success(),
            !expect_failure,
            "unexpected status for {args:?}: {:?}",
            output.status
        );
        assert!(
            !tmp.path().join(".codexctl").exists(),
            "created state for {args:?}"
        );
    }
}

#[test]
fn stateful_command_initializes_private_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("codexctl").unwrap();

    command
        .env("HOME", tmp.path())
        .arg("list")
        .assert()
        .success();

    assert!(tmp.path().join(".codexctl/profiles").is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [".codexctl", ".codexctl/profiles"] {
            let mode = std::fs::metadata(tmp.path().join(dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{dir} is not private");
        }
    }
}
