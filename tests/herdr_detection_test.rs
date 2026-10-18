#![cfg(unix)]

#[test]
fn private_pty_resume_exposes_codex_to_herdr() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/herdr_detection_test.py"
        ))
        .arg("-v")
        .env("CODEXCTL_TEST_BINARY", env!("CARGO_BIN_EXE_codexctl"))
        .output()
        .expect("run the private-PTY launch regression");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
