#![cfg(feature = "central-prototype")]

use std::{
    fs,
    fs::File,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
use tempfile::TempDir;

const HEADER: &str = "{ \"type\": \"session_meta\", \"payload\": {\"id\":\"fixture\",\"nested\":{\"model_provider\":\"openai\"}, \"model_provider\" : \"openai\",\"text\":\"openai\"} }\r\n";
const TAIL: &[u8] = b"{\"type\":\"event_msg\",\"payload\":{\"text\":\"openai\"}}\nlast byte";
fn migrated() -> String {
    HEADER.replace(
        "\"model_provider\" : \"openai\"",
        "\"model_provider\" : \"codexctl-central\"",
    )
}
fn age(path: &std::path::Path) {
    File::open(path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200)),
        )
        .unwrap();
}

struct Fixture {
    root: TempDir,
    home: PathBuf,
    rollout: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("active-home");
        let rollout = home.join("sessions/2026/10/rollout-fixture.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(root.path().join(".codexctl/central")).unwrap();
        fs::write(
            home.join("config.toml"),
            "model_provider = 'codexctl-central'\n",
        )
        .unwrap();
        codexctl::store::atomic_write(
            &root.path().join(".codexctl/central/.native-active.json"),
            &serde_json::to_vec(&serde_json::json!({"home":home,"original_provider":null}))
                .unwrap(),
        )
        .unwrap();
        fs::write(&rollout, [HEADER.as_bytes(), TAIL].concat()).unwrap();
        age(&rollout);
        let quiet_tools = root.path().join("quiet-tools");
        fs::create_dir_all(&quiet_tools).unwrap();
        install_quiet_lsof(&quiet_tools);
        Self {
            root,
            home,
            rollout,
        }
    }
    fn write(&self, bytes: impl AsRef<[u8]>) {
        fs::write(&self.rollout, bytes).unwrap();
        age(&self.rollout);
    }
    fn run(&self, action: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .env("HOME", self.root.path())
            .env("CODEX_HOME", &self.home)
            .env("PATH", self.root.path().join("quiet-tools"))
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .args(["session-provider", action])
            .output()
            .unwrap()
    }
    fn backup(&self) -> PathBuf {
        self.home
            .join(".codexctl-session-provider-backups/sessions/2026/10/rollout-fixture.jsonl")
    }
    fn run_with_lsof(&self, script: &str) -> Output {
        let tools = self.root.path().join("tools");
        fs::create_dir_all(&tools).unwrap();
        fs::write(tools.join("lsof"), format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(tools.join("lsof"), fs::Permissions::from_mode(0o700)).unwrap();
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .env("HOME", self.root.path())
            .env("CODEX_HOME", &self.home)
            .env("PATH", &tools)
            .args(["session-provider", "rewrite"])
            .output()
            .unwrap()
    }
}

fn install_quiet_lsof(directory: &std::path::Path) {
    let lsof = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("lsof"))
        .find(|path| path.is_file())
        .expect("lsof must be installed for session-provider tests");
    fs::write(
        directory.join("lsof"),
        format!("#!/bin/sh\nexec {} -w \"$@\"\n", lsof.display()),
    )
    .unwrap();
    fs::set_permissions(directory.join("lsof"), fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn dry_run_lists_changes_without_writing() {
    let f = Fixture::new();
    let result = f.run("dry-run");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("would-change=1"));
    assert!(!f.backup().exists());
}

#[test]
fn rewrite_changes_only_provider_and_preserves_mtime_and_permissions() {
    let f = Fixture::new();
    fs::set_permissions(&f.rollout, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&f.rollout).unwrap();
    let result = f.run("rewrite");
    let after = fs::metadata(&f.rollout).unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
    assert_eq!(
        (after.modified().unwrap(), after.permissions().mode()),
        (before.modified().unwrap(), before.permissions().mode())
    );
}

#[test]
fn backup_is_private_and_restore_is_byte_exact_and_repeatable() {
    let f = Fixture::new();
    let rewrite = f.run("rewrite");
    assert!(rewrite.status.success());
    assert_eq!(
        (
            fs::read(f.backup()).unwrap(),
            fs::metadata(f.backup()).unwrap().permissions().mode() & 0o777
        ),
        (HEADER.as_bytes().to_vec(), 0o600)
    );
    let first = f.run("restore");
    let second = f.run("restore");
    assert!(
        first.status.success()
            && second.status.success()
            && fs::read(&f.rollout).unwrap() == [HEADER.as_bytes(), TAIL].concat()
    );
}

#[test]
fn repeat_rewrite_keeps_backup_and_does_not_replace_rollout() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    f.run("rewrite");
    let inode = fs::metadata(&f.rollout).unwrap().ino();
    let result = f.run("rewrite");
    assert!(result.status.success());
    assert_eq!(fs::metadata(&f.rollout).unwrap().ino(), inode);
    assert_eq!(fs::read(f.backup()).unwrap(), HEADER.as_bytes());
}

#[test]
fn open_rollout_is_skipped_in_rewrite_and_restore() {
    let f = Fixture::new();
    let held = File::open(&f.rollout).unwrap();
    let result = f.run("rewrite");
    assert!(
        result.status.success()
            && String::from_utf8_lossy(&result.stdout).contains("skipped-open=1")
    );
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
    drop(held);
    f.run("rewrite");
    let _held = File::open(&f.rollout).unwrap();
    let result = f.run("restore");
    assert!(
        result.status.success()
            && String::from_utf8_lossy(&result.stdout).contains("skipped-open=1")
    );
}

#[test]
fn only_active_home_session_directories_change() {
    let f = Fixture::new();
    let outside = f.home.join("other.jsonl");
    let archived = f.home.join("archived_sessions/rollout-archived.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::write(&outside, HEADER).unwrap();
    fs::write(&archived, HEADER).unwrap();
    age(&archived);
    let result = f.run("rewrite");
    assert!(result.status.success());
    assert_eq!(fs::read_to_string(archived).unwrap(), migrated());
    assert_eq!(fs::read_to_string(outside).unwrap(), HEADER);
}

#[test]
fn local_provider_refuses_rewrite() {
    let f = Fixture::new();
    fs::write(f.home.join("config.toml"), "model_provider = 'openai'\n").unwrap();
    let result = f.run("rewrite");
    assert!(!result.status.success());
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
}

#[test]
fn large_rollout_streams_and_restores_unchanged_tail() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&f.rollout)
        .unwrap();
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..64 {
        file.write_all(&block).unwrap();
    }
    drop(file);
    age(&f.rollout);
    fn digest(path: &std::path::Path) -> Vec<u8> {
        let mut file = File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        hash.finalize().to_vec()
    }
    let before = digest(&f.rollout);
    let rewritten = f.run("rewrite");
    let restored = f.run("restore");
    assert!(rewritten.status.success() && restored.status.success());
    assert_eq!(digest(&f.rollout), before);
}

#[test]
fn escaped_provider_changes_only_the_raw_value() {
    let f = Fixture::new();
    let source = HEADER.replace(" : \"openai\"", " : \"op\\u0065nai\"");
    f.write(source);
    let result = f.run("rewrite");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read_to_string(&f.rollout).unwrap(), migrated());
}

#[test]
fn session_symlinks_never_modify_their_target() {
    let f = Fixture::new();
    let target = f.root.path().join("outside.jsonl");
    fs::rename(&f.rollout, &target).unwrap();
    std::os::unix::fs::symlink(&target, &f.rollout).unwrap();
    let result = f.run("rewrite");
    assert!(result.status.success());
    assert_eq!(
        fs::read(target).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-path=1"));
}

#[test]
fn changed_metadata_refuses_restore_without_losing_backup() {
    let f = Fixture::new();
    f.run("rewrite");
    let changed = migrated().replace("fixture", "other-session");
    f.write(&changed);
    let result = f.run("restore");
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(&f.rollout).unwrap(), changed);
    assert_eq!(fs::read(f.backup()).unwrap(), HEADER.as_bytes());
}

#[test]
fn duplicate_provider_is_rejected_without_rewriting() {
    let f = Fixture::new();
    let original = b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\",\"model_provider\":\"openai\"}}\n";
    f.write(original);
    let result = f.run("rewrite");
    assert!(!result.status.success());
    assert_eq!(fs::read(&f.rollout).unwrap(), original);
    assert!(!f.backup().exists());
}

impl Fixture {
    fn spawn_rewrite(&self) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_codexctl"))
            .env("HOME", self.root.path())
            .env("CODEX_HOME", &self.home)
            .env("PATH", self.root.path().join("quiet-tools"))
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .args(["session-provider", "rewrite"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }
    fn make_large_and_wait_for_copy(&self) -> std::process::Child {
        File::options()
            .write(true)
            .open(&self.rollout)
            .unwrap()
            .set_len(2 * 1024 * 1024 * 1024)
            .unwrap();
        age(&self.rollout);
        let mut child = self.spawn_rewrite();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if fs::read_dir(self.rollout.parent().unwrap())
                .unwrap()
                .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".tmp"))
            {
                return child;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("rewrite exited before streaming: {status}");
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("copy did not start");
            }
            std::thread::yield_now();
        }
    }
}

#[test]
fn interruption_during_large_copy_keeps_original_rollout() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let inode = fs::metadata(&f.rollout).unwrap().ino();
    let mut child = f.make_large_and_wait_for_copy();
    child.kill().unwrap();
    child.wait().unwrap();
    let mut header = vec![0; HEADER.len()];
    File::open(&f.rollout)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    assert_eq!(header, HEADER.as_bytes());
    assert_eq!(fs::metadata(&f.rollout).unwrap().ino(), inode);
}

#[test]
fn a_file_opened_during_streaming_is_skipped_before_replace() {
    let f = Fixture::new();
    let child = f.make_large_and_wait_for_copy();
    let _held = File::open(&f.rollout).unwrap();
    let result = child.wait_with_output().unwrap();
    let mut header = vec![0; HEADER.len()];
    File::open(&f.rollout)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-open=1"));
    assert_eq!(header, HEADER.as_bytes());
}

#[test]
fn empty_os_inventory_allows_rewrite() {
    let f = Fixture::new();
    let result = f.run_with_lsof("exit 1");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
}

#[test]
fn lsof_warnings_are_suppressed_with_w_flag() {
    let f = Fixture::new();
    let result = f.run_with_lsof("for arg do\nif [ \"$arg\" = -w ]; then echo unexpected >&2; exit 2; fi\nif [ \"$arg\" = pDi ]; then echo \"lsof: WARNING: can't stat() nsfs file system /run/docker/netns/test\" >&2; echo \"      Output information may be incomplete.\" >&2; exit 0; fi\ndone\necho \"lsof: WARNING: can't stat() nsfs file system /run/docker/netns/test\" >&2\necho \"      Output information may be incomplete.\" >&2\nexit 1");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[cfg(target_os = "linux")]
fn docker_lsof_filesystem_warnings_are_ignored() {
    let f = Fixture::new();
    let result = f.run_with_lsof(
        "echo \"lsof: WARNING: can't stat() overlay file system /var/lib/docker/rootfs/overlayfs/test\" >&2\necho \"      Output information may be incomplete.\" >&2\necho \"lsof: WARNING: can't stat() nsfs file system /run/docker/netns/test\" >&2\necho \"      Output information may be incomplete.\" >&2\nexit 1",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
}

#[test]
fn unknown_lsof_filesystem_warning_refuses_to_modify_rollouts() {
    let f = Fixture::new();
    let result = f.run_with_lsof(
        "echo \"lsof: WARNING: can't stat() overlay file system /var/lib/other/test\" >&2\necho \"      Output information may be incomplete.\" >&2\nexit 1",
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("OS open-file check failed"));
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
}

#[test]
fn uncertain_os_inventory_refuses_to_modify_rollouts() {
    for failure in [
        "echo 'cannot inspect file system' >&2\nexit 0",
        "echo 'cannot inspect file system' >&2\nexit 1",
        "echo p123\nexit 1",
        "exit 2",
        "kill -TERM $$",
    ] {
        for inventory in [true, false] {
            let f = Fixture::new();
            let script = if inventory {
                failure.to_owned()
            } else {
                format!("for arg do\nif [ \"$arg\" = pDi ]; then exit 0; fi\ndone\n{failure}")
            };
            let result = f.run_with_lsof(&script);
            assert!(!result.status.success(), "{script}");
            assert!(String::from_utf8_lossy(&result.stderr).contains("OS open-file check failed"));
            assert_eq!(
                fs::read(&f.rollout).unwrap(),
                [HEADER.as_bytes(), TAIL].concat()
            );
            if inventory {
                assert!(!f.backup().exists());
            }
        }
    }
}

#[test]
fn valid_legacy_metadata_without_a_provider_stays_unchanged() {
    let f = Fixture::new();
    let original =
        b"{\"id\":\"legacy-session\",\"timestamp\":\"2025-01-01\",\"instructions\":\"keep me\"}\n";
    f.write(original);
    let result = f.run("rewrite");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&f.rollout).unwrap(), original);
}

#[test]
fn restore_finds_the_backup_after_codex_archives_a_session() {
    let f = Fixture::new();
    f.run("rewrite");
    let archived = f.home.join("archived_sessions/rollout-fixture.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(&f.rollout, &archived).unwrap();
    let result = f.run("restore");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(archived).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
}

#[test]
fn restore_reports_missing_backups() {
    let f = Fixture::new();
    f.run("rewrite");
    fs::remove_file(f.backup()).unwrap();
    let result = f.run("restore");
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-no-backup=1"));
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
}

#[test]
fn archived_rollouts_refuse_ambiguous_backups() {
    let f = Fixture::new();
    f.run("rewrite");
    codexctl::store::atomic_write(
        &f.home
            .join(".codexctl-session-provider-backups/sessions/another/rollout-fixture.jsonl"),
        HEADER.as_bytes(),
    )
    .unwrap();
    let archived = f.home.join("archived_sessions/rollout-fixture.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(&f.rollout, &archived).unwrap();
    let result = f.run("restore");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("multiple metadata backups match"));
    assert_eq!(
        fs::read(archived).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
}

#[test]
fn recent_rollouts_are_skipped_even_when_no_process_has_them_open() {
    let f = Fixture::new();
    File::open(&f.rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(59 * 60),
            ),
        )
        .unwrap();
    let result = f.run("rewrite");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-recent=1"));
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [HEADER.as_bytes(), TAIL].concat()
    );
}

#[test]
fn an_append_during_copy_is_preserved_and_counted_as_recent() {
    let f = Fixture::new();
    let child = f.make_large_and_wait_for_copy();
    File::options()
        .append(true)
        .open(&f.rollout)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-recent=1 errors=0"));
    assert_eq!(
        fs::metadata(&f.rollout).unwrap().len(),
        2 * 1024 * 1024 * 1024 + 1
    );
}

#[test]
fn restore_also_skips_recent_rollouts() {
    let f = Fixture::new();
    f.run("rewrite");
    File::open(&f.rollout)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .unwrap();
    let result = f.run("restore");
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-recent=1"));
    assert_eq!(
        fs::read(&f.rollout).unwrap(),
        [migrated().as_bytes(), TAIL].concat()
    );
}

#[test]
fn dry_run_skips_future_timestamps() {
    let f = Fixture::new();
    File::open(&f.rollout)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
        )
        .unwrap();
    let result = f.run("dry-run");
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("skipped-recent=1"));
    assert!(!f.backup().exists());
}
