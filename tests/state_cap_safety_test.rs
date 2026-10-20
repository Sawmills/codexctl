#![cfg(unix)]

#[test]
fn numbered_state_databases_and_orphan_sidecars_count_toward_the_cap() {
    check("accounting");
}

#[test]
fn every_numbered_state_database_is_checkpointed_when_unheld() {
    check("checkpoint");
}

#[test]
fn symlinked_state_databases_are_rejected_without_touching_external_wal() {
    check("symlink");
}

#[cfg(target_os = "linux")]
#[test]
fn archive_and_index_are_durable_before_source_removal() {
    for stage in ["success", "archive", "index", "source"] {
        check(stage);
    }
}

fn check(case: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/state_cap_safety.py"
        ))
        .args([env!("CARGO_BIN_EXE_codexctl"), case])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
