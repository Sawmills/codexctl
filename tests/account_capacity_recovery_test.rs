#![cfg(all(unix, feature = "central-prototype"))]

#[test]
fn terminal_capacity_resumes_same_session_on_another_included_account() {
    check("json");
}
#[test]
fn marked_capacity_resumes_with_matching_upstream_evidence() {
    check("marked");
}
#[test]
fn compact_model_config_uses_the_same_policy_key_as_the_child() {
    check("compact_config");
}
#[test]
fn explicit_capacity_launch_stays_pinned() {
    check("explicit");
}
#[test]
fn quoted_capacity_does_not_move_a_lane() {
    check("quoted");
}
#[test]
fn capacity_without_upstream_evidence_does_not_move_a_lane() {
    check("no_evidence");
}
#[test]
fn two_overloads_do_not_authorize_recovery() {
    check("below_threshold");
}
#[test]
fn capacity_with_no_resolved_model_keeps_the_running_session() {
    check("no_model");
}
#[test]
fn marked_http_capacity_requires_matching_signed_terminal_receipt() {
    check("http_marked");
}
fn check(case: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/account_capacity_recovery.py"
        ))
        .args(["--binary", env!("CARGO_BIN_EXE_codexctl"), "--case", case])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
