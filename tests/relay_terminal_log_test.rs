#![cfg(all(unix, feature = "central-prototype"))]

#[test]
fn failed_capacity_report_goes_to_private_log_not_child_terminal() {
    check("absent");
}

#[test]
fn existing_diagnostic_log_is_made_private() {
    check("permissive");
}

#[test]
fn blocked_diagnostic_log_never_falls_back_to_child_terminal() {
    check("blocked");
}

fn check(log_state: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/relay_terminal_log.py"
        ))
        .arg(env!("CARGO_BIN_EXE_codexctl"))
        .args(["--log-state", log_state])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
