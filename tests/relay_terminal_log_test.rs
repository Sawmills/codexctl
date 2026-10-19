#![cfg(all(unix, feature = "central-prototype"))]

#[test]
fn failed_capacity_report_goes_to_private_log_not_child_terminal() {
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/relay_terminal_log.py"))
        .arg(env!("CARGO_BIN_EXE_codexctl"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
