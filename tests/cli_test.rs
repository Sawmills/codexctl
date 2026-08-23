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
    assert!(stdout.contains("exec"));
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

    // Label text shares no substring with the alias, so matching it proves the
    // label was rendered rather than the Account column.
    assert!(
        run(tmp.path(), &["label", "amir-team", "sawmills seat"])
            .status
            .success()
    );
    let listed = stdout_of(&run(tmp.path(), &["list"]));
    assert!(listed.contains("Label"), "label column missing: {listed}");
    assert!(
        listed.contains("sawmills seat"),
        "label missing from list: {listed}"
    );

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

/// An invalid label must fail before the save switches the live auth file,
/// otherwise the active account changes under an error exit.
#[test]
fn save_rejects_an_invalid_label_without_touching_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let codex_dir = tmp.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        format!(
            r#"{{"access_token":"{}"}}"#,
            seat_token("amir@sawmills.ai", "acct-team", "business")
        ),
    )
    .unwrap();

    let output = run(
        tmp.path(),
        &[
            "save",
            "--label",
            "wildly over the forty byte limit for a label",
        ],
    );

    assert!(!output.status.success());
    assert!(
        !tmp.path()
            .join(".codexctl/profiles/amir@sawmills.ai")
            .exists()
    );
    assert!(!tmp.path().join(".codexctl/active").exists());
}

/// Telling an operator who already chose an alias to "pass an explicit alias"
/// describes what they just did.
#[test]
fn save_refusal_names_a_usable_remedy_for_an_explicit_alias() {
    let tmp = tempfile::tempdir().unwrap();
    write_profile(
        tmp.path(),
        "work",
        &seat_token("amir@sawmills.ai", "acct-personal", "pro"),
        r#"{"alias":"work","email":"amir@sawmills.ai","plan":"pro","account_id":"acct-personal","saved_at":"2026-01-01T00:00:00Z"}"#,
    );
    let codex_dir = tmp.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        format!(
            r#"{{"access_token":"{}"}}"#,
            seat_token("amir@sawmills.ai", "acct-team", "business")
        ),
    )
    .unwrap();

    let output = run(tmp.path(), &["save", "work"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("codexctl remove work"),
        "no usable remedy: {stderr}"
    );
    assert!(
        !stderr.contains("Pass an explicit alias"),
        "advice repeats what was already done: {stderr}"
    );
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

/// A profile the store cannot identify needs an answer, and an unattended run
/// has nobody to give one. That must fail: exiting 0 having saved nothing would
/// tell a script the account was captured when it was not.
#[test]
fn save_without_a_terminal_fails_rather_than_reporting_success() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    // A profile whose token declares a login but no workspace — what every
    // profile written before workspaces were recorded looks like.
    let legacy = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzZWF0QSJ9.sig";
    let dir = home.join(".codexctl").join("profiles").join("legacy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{legacy}"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"legacy","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // The live file: the same login, now carrying a workspace.
    let claims =
        r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#;
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    let incoming = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
    let codex = home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("auth.json"),
        format!(r#"{{"access_token":"{incoming}"}}"#),
    )
    .unwrap();

    let output = Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(["save", "legacy"])
        .write_stdin("")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "an unanswerable save reported success: {output:?}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--allow-adopt"), "{stderr}");
    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains(legacy),
        "the profile was replaced without approval"
    );

    // The same command with the answer supplied ahead of time does save.
    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(["save", "legacy", "--allow-adopt"])
        .write_stdin("")
        .assert()
        .success();
    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains(&incoming),
        "the approved save did not land"
    );
}

/// `--allow-adopt` approves replacing a profile. Without an alias, `save`
/// derives one from the token's email claim, so the flag would consent to
/// whatever that resolves to — a target the operator never named. One login can
/// hold seats in several workspaces behind one address, so that is the same
/// hazard the prompt exists to prevent, re-entered through the flag.
#[test]
fn save_requires_an_explicit_alias_to_pre_approve_adoption() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", tmp.path())
        .args(["save", "--allow-adopt"])
        .write_stdin("")
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("name the one you are approving"),
        "{stderr}"
    );
}

/// One account, one profile. A mistyped alias is free by definition, so nothing
/// about the name stops a second copy being written — and the fork it leaves is
/// what every later lookup reports as ambiguous.
#[test]
fn save_reuses_the_profile_that_already_holds_this_account() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    use base64::Engine;
    let claims = r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team","chatgpt_user_id":"user-a"}}"#;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
    std::fs::write(
        home.join(".codex").join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();

    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(["save", "amir@sawmills.ai"])
        .write_stdin("")
        .assert()
        .success();

    // The same account again, under a mistyped alias.
    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(["save", "amir@sawmils.ai"])
        .write_stdin("y\n")
        .assert()
        .success();

    let mut aliases: Vec<String> = std::fs::read_dir(home.join(".codexctl").join("profiles"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    aliases.sort();
    assert_eq!(
        aliases,
        vec!["amir@sawmills.ai"],
        "one account was saved twice under different aliases"
    );
}

/// Review read a redirect as able to carry `--allow-adopt` onto a profile the
/// operator never named. The two are mutually exclusive and this pins why: a
/// redirect needs `existing_seat` to positively match both the workspace and
/// the login, and that is exactly the state in which nothing needs adopting.
/// If the preconditions ever stop excluding each other, this fails.
#[test]
fn a_redirect_never_carries_pre_approved_adoption() {
    use base64::Engine;
    let claims = r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team","chatgpt_user_id":"user-a"}}"#;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");

    // `real` is damaged but fully identified, so a redirect is possible.
    let case = |meta: &str| {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("auth.json"),
            format!(r#"{{"access_token":"{token}"}}"#),
        )
        .unwrap();
        let real = home.join(".codexctl").join("profiles").join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "{ not json").unwrap();
        std::fs::write(real.join("meta.json"), meta).unwrap();

        let output = Command::cargo_bin("codexctl")
            .unwrap()
            .env("HOME", &home)
            .args(["save", "typo", "--allow-adopt"])
            .write_stdin("")
            .output()
            .unwrap();
        let damaged = std::fs::read_to_string(real.join("auth.json")).unwrap();
        (String::from_utf8_lossy(&output.stdout).to_string(), damaged)
    };

    // Identified on both halves: the save redirects to `real`, and because the
    // account is settled it takes the ordinary overwrite prompt — the adoption
    // flag never applies, so an unanswered prompt aborts.
    let (out, damaged) = case(
        r#"{"alias":"real","email":null,"plan":null,"account_id":"acct-team","user_id":"user-a","saved_at":"2026-01-01T00:00:00Z"}"#,
    );
    assert!(out.contains("aborted"), "{out}");
    assert_eq!(damaged, "{ not json", "a redirect adopted without approval");

    // Workspace only: adoption would be required, and precisely because the
    // login cannot be matched there is no redirect to carry it.
    let (_out, damaged) = case(
        r#"{"alias":"real","email":null,"plan":null,"account_id":"acct-team","saved_at":"2026-01-01T00:00:00Z"}"#,
    );
    assert_eq!(damaged, "{ not json", "an unidentified profile was adopted");
}

/// A redirected save lands on a profile with its own established address. The
/// write rebuilds metadata, so without carrying that address forward it is
/// erased from everything `list` and `whoami` show.
#[test]
fn a_redirected_save_keeps_the_profile_email() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    // A token with no email claim, so nothing can re-derive the address.
    use base64::Engine;
    let claims = r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team","chatgpt_user_id":"user-a"}}"#;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
    std::fs::write(
        home.join(".codex").join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();

    let real = home.join(".codexctl").join("profiles").join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(
        real.join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();
    std::fs::write(
        real.join("meta.json"),
        r#"{"alias":"real","email":"amir@sawmills.ai","plan":"team","account_id":"acct-team","user_id":"user-a","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .args(["save", "typo"])
        .write_stdin("y\n")
        .assert()
        .success();

    let meta = std::fs::read_to_string(real.join("meta.json")).unwrap();
    assert!(
        meta.contains("amir@sawmills.ai"),
        "the redirected save erased the profile's email: {meta}"
    );
}
