#![cfg(unix)]
use assert_cmd::Command;
use serde_json::json;
use std::{path::Path, time::Duration};

fn run(home: &Path) -> std::process::Output {
    Command::cargo_bin("codexctl")
        .unwrap()
        .env("HOME", home)
        .env_remove("CODEX_HOME")
        .arg("statusline")
        .timeout(Duration::from_secs(2))
        .output()
        .unwrap()
}

#[test]
fn statusline_empty_home_is_silent_and_does_not_create_a_store() {
    let home = tempfile::tempdir().unwrap();

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
    assert!(!home.path().join(".codexctl").exists());
}

fn cached(home: &Path, age: i64, label: Option<&str>) {
    let root = home.join(".codexctl");
    std::fs::create_dir_all(root.join("profiles/amir+p2@example.test")).unwrap();
    let meta = json!({"alias":"amir+p2@example.test", "label":label,"account_id":"seat", "user_id":"login", "saved_at":"today"});
    std::fs::write(root.join("active"), "amir+p2@example.test").unwrap();
    std::fs::write(
        root.join("profiles/amir+p2@example.test/meta.json"),
        meta.to_string(),
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp();
    let cache = json!({"version":1,"sampled_at":now-age,"selection":{"alias":"amir+p2@example.test","source":{"local":{"account_id":"seat","user_id":"login","saved_at":"today"}}},"account":{"alias":"amir+p2@example.test","label":label,"plan":null,"source":"local","state":"active","primary_used_percent":null,"secondary_used_percent":38.0,"resets_at":codexctl::status_json::timestamp(Some(now+600000)),"billing_class":"rate_limited","error":null},"five_hour_resets_at":null});
    std::fs::write(root.join("statusline.json"), cache.to_string()).unwrap();
}

#[test]
fn statusline_fresh_weekly_only_cache_uses_short_alias_and_remaining_percent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "p2 62% wk · 6d22h\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn statusline_expired_cache_is_silent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 121, Some("team"));

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[test]
fn statusline_changed_active_account_is_silent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    std::fs::write(home.path().join(".codexctl/active"), "other").unwrap();

    let output = run(home.path());

    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[test]
fn statusline_blocked_cache_read_does_not_wait_for_a_writer() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".codexctl");
    std::fs::create_dir_all(&root).unwrap();
    let path = std::ffi::CString::new(root.join("statusline.json").as_os_str().as_encoded_bytes())
        .unwrap();
    unsafe { libc::mkfifo(path.as_ptr(), 0o600) };

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

fn change_cache(home: &Path, key: &str, value: serde_json::Value) {
    let path = home.join(".codexctl/statusline.json");
    let mut cache: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    *cache.pointer_mut(key).unwrap() = value;
    std::fs::write(path, cache.to_string()).unwrap();
}

#[test]
fn statusline_includes_five_hour_only_when_present() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, Some("team"));
    change_cache(home.path(), "/account/primary_used_percent", json!(12));

    let output = run(home.path());

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "team 62% wk · 6d22h · 88% 5h\n"
    );
}

#[test]
fn statusline_prompt_control_characters_cannot_escape_the_label() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, Some("%F{red}$(x)`y`#[z]\n\x1b"));

    let output = run(home.path());

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Fredxyz 62% wk · 6d22h\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn statusline_past_reset_hides_outdated_budget() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    change_cache(
        home.path(),
        "/account/resets_at",
        json!("2000-01-01T00:00:00Z"),
    );

    let output = run(home.path());

    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[test]
fn statusline_invalid_usage_is_silent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    change_cache(home.path(), "/account/secondary_used_percent", json!(101));

    let output = run(home.path());

    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[test]
fn statusline_same_alias_replaced_by_another_account_is_silent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    change_cache(
        home.path(),
        "/selection/source/local/account_id",
        json!("other-seat"),
    );

    let output = run(home.path());

    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[test]
fn statusline_corrupt_cache_is_silent() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    std::fs::write(home.path().join(".codexctl/statusline.json"), "broken").unwrap();

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}

#[cfg(feature = "central-prototype")]
#[test]
fn statusline_stalled_provider_configuration_exits_without_waiting() {
    let home = tempfile::tempdir().unwrap();
    cached(home.path(), 0, None);
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    let path = std::ffi::CString::new(
        home.path()
            .join(".codex/config.toml")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    unsafe { libc::mkfifo(path.as_ptr(), 0o600) };

    let output = run(home.path());

    assert!(output.status.success());
    assert_eq!((output.stdout, output.stderr), (vec![], vec![]));
}
