#![cfg(all(unix, feature = "central-prototype"))]

use base64::Engine;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

#[path = "fixtures/relay.rs"]
mod relay_fixture;
use relay_fixture::TestRelay as Relay;

fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexctl"));
    command
        .env("HOME", home)
        .env_remove("CODEX_HOME")
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEXCTL_PINNED_CONNECTION");
    command
}

#[test]
fn a_service_creates_private_identity_and_status_verifies_its_proof() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let directory = home.path().join(".codexctl/relay/identity");
    let entries: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(entries.len(), 1);
    let key = entries[0].path();
    assert_eq!(std::fs::read(&key).unwrap().len(), 32);
    assert_eq!(
        std::fs::metadata(key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        cli(home.path())
            .args(["relay", "status", "--listen", &relay.address])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn service_refuses_an_unsafe_known_relay_directory_mode() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let relay_dir = home.path().join(".codexctl/relay/evidence");
    std::fs::create_dir_all(&relay_dir).unwrap();
    std::fs::set_permissions(
        home.path().join(".codexctl"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        home.path().join(".codexctl/relay"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(&relay_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    let status = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .env("HOME", home.path())
        .args([
            "relay",
            "serve",
            "--listen",
            &address,
            "--upstream",
            "http://127.0.0.1:9",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "service repaired an unsafe identity directory"
    );
    assert_eq!(
        std::fs::metadata(home.path().join(".codexctl/relay"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&relay_dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn enable_refuses_a_replacement_listener_before_writing_route_intent() {
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let address = relay.address.clone();
    drop(relay);
    let listener = TcpListener::bind(&address).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let output = cli(home.path())
        .args(["relay", "enable", "--listen", &address])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        !output.status.success(),
        "enable accepted a squatter with a stale identity file"
    );
    assert!(!home.path().join(".codexctl/relay.json").exists());
    assert!(!home.path().join(".codexctl/relay.pending.json").exists());
    assert!(!home.path().join(".codex/config.toml").exists());
}

#[test]
fn route_lock_contention_refuses_then_leaves_a_coherent_route() {
    use std::{fs::OpenOptions, os::fd::AsRawFd};
    let home = tempfile::tempdir().unwrap();
    let _relay = Relay::start(home.path());
    let lock_path = home.path().join(".codexctl/relay.lock");
    let held = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let blocked = cli(home.path())
        .args(["relay", "disable"])
        .output()
        .unwrap();
    assert!(
        !blocked.status.success(),
        "route mutation ignored a held lock"
    );
    assert!(!home.path().join(".codexctl/relay.pending.json").exists());
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
    assert!(
        cli(home.path())
            .args(["relay", "disable"])
            .status()
            .unwrap()
            .success()
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".codexctl/relay.json")).unwrap())
            .unwrap();
    assert!(saved["listen"].is_null());
    assert!(!home.path().join(".codexctl/relay.pending.json").exists());
}

#[test]
fn status_refuses_a_plain_200_squatter_without_creating_identity() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let output = cli(home.path())
        .args(["relay", "status", "--listen", &address.to_string()])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        !output.status.success(),
        "plain HTTP 200 authorized a foreign listener"
    );
    assert!(
        !home.path().join(".codexctl").exists(),
        "client created relay identity"
    );
}

fn key_files(home: &Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(home.join(".codexctl/relay/identity"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
}

#[test]
fn identity_survives_restart_and_separates_concurrent_endpoints() {
    let home = tempfile::tempdir().unwrap();
    let first = Relay::start(home.path());
    let second = Relay::start(home.path());
    let address = first.address.clone();
    let before: Vec<_> = key_files(home.path())
        .into_iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(before.len(), 2);
    assert_ne!(before[0], before[1]);
    drop(first);
    let restarted = Relay::start_at(home.path(), &address);
    let after: Vec<_> = key_files(home.path())
        .into_iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(before, after, "restart replaced a persistent identity");
    for relay in [&restarted, &second] {
        assert!(
            cli(home.path())
                .args(["relay", "status", "--listen", &relay.address])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn unsafe_key_and_parent_modes_are_refused_without_repair() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let key = key_files(home.path()).pop().unwrap();
    let bytes = std::fs::read(&key).unwrap();
    for (path, mode, restore) in [
        (key.clone(), 0o644, 0o600),
        (home.path().join(".codexctl/relay/identity"), 0o755, 0o700),
        (home.path().join(".codexctl/relay"), 0o755, 0o700),
        (home.path().join(".codexctl"), 0o755, 0o700),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let result = cli(home.path())
            .args(["relay", "status", "--listen", &relay.address])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            mode
        );
        assert_eq!(std::fs::read(&key).unwrap(), bytes);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(restore)).unwrap();
    }
}

#[test]
fn symlinked_identity_files_and_directories_are_not_followed() {
    use std::os::unix::fs::symlink;
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let key = key_files(home.path()).pop().unwrap();
    let original = std::fs::read(&key).unwrap();
    let target = home.path().join("saved-key");
    std::fs::rename(&key, &target).unwrap();
    symlink(&target, &key).unwrap();
    assert!(
        !cli(home.path())
            .args(["relay", "status", "--listen", &relay.address])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(std::fs::read(&target).unwrap(), original);
    std::fs::remove_file(&key).unwrap();
    std::fs::rename(&target, &key).unwrap();
    let directory = home.path().join(".codexctl/relay/identity");
    let moved = home.path().join("saved-identity");
    std::fs::rename(&directory, &moved).unwrap();
    symlink(&moved, &directory).unwrap();
    assert!(
        !cli(home.path())
            .args(["relay", "status", "--listen", &relay.address])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(moved.join(key.file_name().unwrap())).unwrap(),
        original
    );
}

#[test]
fn wrong_keys_and_truncated_keys_fail_closed() {
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let key = key_files(home.path()).pop().unwrap();
    let original = std::fs::read(&key).unwrap();
    for replacement in [vec![0; 32], vec![0; 31], vec![0; 33]] {
        std::fs::write(&key, replacement).unwrap();
        assert!(
            !cli(home.path())
                .args(["relay", "status", "--listen", &relay.address])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    std::fs::write(key, original).unwrap();
    assert!(
        cli(home.path())
            .args(["relay", "status", "--listen", &relay.address])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn challenge_proof_is_bound_to_a_fresh_nonce_and_wrong_proofs_fail() {
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    let first = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "http://{}/healthz?nonce={}",
            relay.address,
            "!".repeat(43)
        ))
        .send()
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::BAD_REQUEST);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/healthz?nonce={nonce}", relay.address))
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let proof: serde_json::Value = response.json().unwrap();
    assert_eq!(proof["version"], 1);
    assert_eq!(proof["nonce"], nonce);
    assert!(!proof["proof"].as_str().unwrap().is_empty());
    let next_nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([8u8; 32]);
    let next: serde_json::Value = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "http://{}/healthz?nonce={next_nonce}",
            relay.address
        ))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(next["nonce"], next_nonce);
    assert_ne!(proof["proof"], next["proof"]);
    let malformed = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "http://{}/healthz?nonce={}",
            relay.address,
            "B".repeat(42)
        ))
        .send()
        .unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[test]
fn direct_disable_supersedes_a_stale_pending_enable() {
    let home = tempfile::tempdir().unwrap();
    let relay = Relay::start(home.path());
    assert!(
        cli(home.path())
            .args(["relay", "enable", "--listen", &relay.address])
            .output()
            .unwrap()
            .status
            .success()
    );
    let pending = home.path().join(".codexctl/relay.pending.json");
    std::fs::write(
        &pending,
        serde_json::to_vec(&serde_json::json!({"routing":{"listen":relay.address}})).unwrap(),
    )
    .unwrap();
    drop(relay);
    let disabled = cli(home.path())
        .args(["relay", "disable"])
        .output()
        .unwrap();
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert!(!pending.exists());
    let route: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".codexctl/relay.json")).unwrap())
            .unwrap();
    assert!(route["listen"].is_null());
}

#[test]
fn a_new_proven_enable_replaces_a_stale_pending_listener() {
    let home = tempfile::tempdir().unwrap();
    let old = Relay::start(home.path());
    std::fs::write(
        home.path().join(".codexctl/relay.pending.json"),
        serde_json::to_vec(&serde_json::json!({"routing":{"listen":old.address}})).unwrap(),
    )
    .unwrap();
    let new = Relay::start(home.path());
    let output = cli(home.path())
        .args(["relay", "enable", "--listen", &new.address])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let route: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".codexctl/relay.json")).unwrap())
            .unwrap();
    assert_eq!(route["listen"], new.address);
}
