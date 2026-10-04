#![cfg(feature = "central-prototype")]

use std::process::Command;

#[test]
fn explicit_migration_bounds_a_stalled_postgres_handshake() {
    use std::{
        net::TcpListener,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = listener.local_addr().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
        .current_dir(root.path())
        .env("CODEXCTL_CENTRAL_STORE", "postgres")
        .env("CODEXCTL_CENTRAL_DB_TLS", "false")
        .env("DATABASE_URL", format!("postgres://test@{endpoint}/test"))
        .args(["migrate", "--state", "state", "--key-file", "key"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    let mut accepted = Vec::new();
    loop {
        if let Ok((socket, _)) = listener.accept() {
            accepted.push(socket); // Accept PostgreSQL startup bytes but never answer them.
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() > Duration::from_secs(15) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("migration exceeded its operation deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !accepted.is_empty(),
        "migration must connect, not hit the runtime guard"
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("timed out"), "{error}");
}

#[test]
fn shared_modes_refuse_startup_and_admin_before_touching_files() {
    let root = tempfile::tempdir().unwrap();
    for (mode, dual_write) in [("dual", "0"), ("postgres", "0"), ("file", "1")] {
        for args in [
            vec!["setup", "--key-file", "key"],
            vec!["serve", "--key-file", "key"],
            vec!["users", "--user", "test", "--enable"],
            vec!["users", "--user", "test", "--disable"],
            vec!["users"],
            vec!["revoke", "--device", "test"],
            vec![
                "register",
                "--device",
                "test",
                "--tenant",
                "sawmills",
                "--user",
                "test",
                "--token-file",
                "token",
            ],
            vec![
                "init",
                "--key-file",
                "key",
                "--auth",
                "auth",
                "--alias",
                "test",
                "--tenant",
                "sawmills",
                "--user",
                "test",
            ],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
                .current_dir(root.path())
                .env("CODEXCTL_CENTRAL_STORE", mode)
                .env("CODEXCTL_CENTRAL_DUAL_WRITE", dual_write)
                .env("CODEXCTL_CENTRAL_DUAL_ACK", "1")
                .env_remove("DATABASE_URL")
                .args(&args)
                .args(["--state", "state"])
                .output()
                .unwrap();
            assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("only file mode"), "{args:?}: {error}");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }
}

#[test]
fn file_admin_remains_available_without_database_configuration() {
    let root = tempfile::tempdir().unwrap();
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_codexctl-central"))
            .current_dir(root.path())
            .env_remove("CODEXCTL_CENTRAL_STORE")
            .env_remove("CODEXCTL_CENTRAL_DUAL_WRITE")
            .env_remove("DATABASE_URL")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        command(&["setup", "--state", "state", "--key-file", "key"])
            .status
            .success()
    );
    assert!(command(&["users", "--state", "state"]).status.success());
}
