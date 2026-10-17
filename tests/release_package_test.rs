#![cfg(unix)]

#[test]
fn release_packager_preserves_matching_symbols_and_debug_links() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/release_package_test.py"
        ))
        .arg("-v")
        .output()
        .expect("run release packaging protocol tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
