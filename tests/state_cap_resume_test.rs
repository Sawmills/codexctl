#![cfg(unix)]

#[test]
fn resume_last_restores_all_archives_before_native_selection() {
    check_resume("codex", "restores");
    check_resume("exec", "restores");
}

#[test]
fn resume_last_in_a_pinned_home_uses_the_canonical_archive_index() {
    check_resume("codex", "inherited");
}

#[test]
fn resume_last_refuses_low_space_before_starting_the_child() {
    check_resume("codex", "low-space");
    check_resume("exec", "low-space");
}

#[test]
fn resume_last_refuses_during_archive_publication_even_without_an_index() {
    check_resume("codex", "locked");
    check_resume("exec", "locked");
}

#[test]
fn malformed_archive_index_prevents_explicit_and_last_child_launches() {
    for command in ["codex", "exec"] {
        check_resume(command, "malformed-explicit");
        check_resume(command, "malformed-last");
    }
}

fn check_resume(command: &str, case: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/state_cap_resume.py"
        ))
        .args([env!("CARGO_BIN_EXE_codexctl"), command, case])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{command}/{case}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
