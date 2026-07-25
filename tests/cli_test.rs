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
    assert!(stdout.contains("completions"));
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
