#![cfg(feature = "central-prototype")]

#[test]
fn wrapper_confirms_only_terminal_http_capacity_stops() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/capacity_receipt_test.py"
        ))
        .arg("-v")
        .env("CODEXCTL_TEST_BINARY", env!("CARGO_BIN_EXE_codexctl"))
        .output()
        .expect("run wrapper/relay capacity receipt regression");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
