#![cfg(all(unix, feature = "central-prototype"))]

use std::process::Command;

#[test]
fn native_usage_limit_recovers_same_session_on_another_included_account() {
    check_recovery("json");
}

#[test]
fn marked_usage_limit_recovers_same_session_on_another_included_account() {
    check_recovery("marked");
}

#[test]
fn native_usage_limit_recovers_when_utf8_is_split_between_reads() {
    check_recovery("split_utf8");
}

#[test]
fn wrapped_marked_usage_limit_recovers_same_session() {
    check_recovery("marked_wrapped");
}

#[test]
fn quoted_tool_json_does_not_trigger_recovery() {
    check_recovery("quoted_json");
}

#[test]
fn long_quoted_tool_json_does_not_trigger_recovery_after_window_truncates() {
    check_recovery("long_quoted_json");
}

#[test]
fn quoted_terminal_text_does_not_trigger_recovery() {
    check_recovery("quoted_text");
}

#[test]
fn nonterminal_json_error_does_not_trigger_recovery() {
    check_recovery("nonterminal_error");
}

#[test]
fn explicit_account_usage_limit_does_not_switch_accounts() {
    check_recovery("explicit");
}

#[test]
fn usage_limit_without_an_included_alternate_never_uses_paid_accounts() {
    check_recovery("no_alternate");
}

fn check_recovery(case: &str) {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/usage_limit_recovery.py"
        ))
        .args(["--binary", env!("CARGO_BIN_EXE_codexctl"), "--case", case])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
