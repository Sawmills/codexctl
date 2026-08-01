use codexctl::config::Paths;
use codexctl::profile;

fn setup_test_env() -> (tempfile::TempDir, Paths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_home(tmp.path().to_path_buf());
    paths.ensure_dirs().unwrap();

    // Create a fake ~/.codex/auth.json
    let codex_dir = tmp.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        r#"{"access_token": "test_tok"}"#,
    )
    .unwrap();

    (tmp, paths)
}

#[test]
fn save_and_list_profile() {
    let (_tmp, paths) = setup_test_env();
    let auth_src = paths.codex_auth_json();

    profile::save_profile_to(
        &paths,
        "test@example.com",
        Some("test@example.com"),
        &auth_src,
    )
    .unwrap();

    let profiles = profile::list_profiles_from(&paths).unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].meta.alias, "test@example.com");
    assert_eq!(profiles[0].meta.email.as_deref(), Some("test@example.com"));
}

#[test]
fn get_profile_not_found() {
    let (_tmp, paths) = setup_test_env();
    let result = profile::get_profile_from(&paths, "nonexistent");
    assert!(result.is_err());
}

#[test]
fn delete_profile() {
    let (_tmp, paths) = setup_test_env();
    let auth_src = paths.codex_auth_json();

    profile::save_profile_to(&paths, "del@test.com", Some("del@test.com"), &auth_src).unwrap();
    assert_eq!(profile::list_profiles_from(&paths).unwrap().len(), 1);

    profile::delete_profile_from(&paths, "del@test.com").unwrap();
    assert_eq!(profile::list_profiles_from(&paths).unwrap().len(), 0);
}

#[test]
fn switch_copies_auth_json() {
    let (_tmp, paths) = setup_test_env();

    // Save a profile with specific content
    let profile_dir = paths.profiles_dir().join("acct@test.com");
    std::fs::create_dir_all(&profile_dir).unwrap();
    std::fs::write(
        profile_dir.join("auth.json"),
        r#"{"access_token": "switched_tok"}"#,
    )
    .unwrap();
    let meta = codexctl::profile::Meta {
        alias: "acct@test.com".to_string(),
        email: Some("acct@test.com".to_string()),
        plan: None,
        saved_at: "2026-01-01T00:00:00Z".to_string(),
    };
    std::fs::write(
        profile_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    profile::switch_to_from(&paths, "acct@test.com").unwrap();

    let auth_src = paths.codex_auth_json();
    let contents = std::fs::read_to_string(&auth_src).unwrap();
    assert!(contents.contains("switched_tok"));

    let active = profile::get_active_from(&paths).unwrap();
    assert_eq!(active.as_deref(), Some("acct@test.com"));
}

#[test]
fn switch_copies_auth_json_to_custom_codex_auth_path() {
    let (tmp, paths) = setup_test_env();

    write_profile(&paths, "acct@test.com", "custom_home_tok");
    let custom_auth = tmp.path().join("custom-codex-home").join("auth.json");

    profile::switch_to_auth_json_from(&paths, "acct@test.com", &custom_auth).unwrap();

    let custom_contents = std::fs::read_to_string(&custom_auth).unwrap();
    assert!(custom_contents.contains("custom_home_tok"));
    let default_contents = std::fs::read_to_string(paths.codex_auth_json()).unwrap();
    assert!(default_contents.contains("test_tok"));
    assert!(profile::get_active_from(&paths).unwrap().is_none());
}

#[test]
fn switch_custom_auth_path_captures_matching_profile_tokens_before_overwrite() {
    let (tmp, paths) = setup_test_env();
    let failed_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old");
    let failed_live = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live");
    let next_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.next");
    write_profile(&paths, "failed@test", &failed_old);
    write_profile(&paths, "next@test", &next_tok);
    let custom_auth = tmp.path().join("custom-codex-home").join("auth.json");
    std::fs::create_dir_all(custom_auth.parent().unwrap()).unwrap();
    std::fs::write(
        &custom_auth,
        format!(r#"{{"access_token":"{failed_live}"}}"#),
    )
    .unwrap();

    profile::switch_to_auth_json_from(&paths, "next@test", &custom_auth).unwrap();

    let failed_store =
        std::fs::read_to_string(paths.profiles_dir().join("failed@test").join("auth.json"))
            .unwrap();
    assert!(failed_store.contains(".live"));
    let custom_contents = std::fs::read_to_string(&custom_auth).unwrap();
    assert!(custom_contents.contains(&next_tok));
}

#[test]
fn alias_for_auth_json_prefers_exact_token_before_subject_fallback() {
    let (tmp, paths) = setup_test_env();
    let stale_same_seat = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old");
    let exact_token = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.exact");
    write_profile(&paths, "aaa-stale@test", &stale_same_seat);
    write_profile(&paths, "zzz-exact@test", &exact_token);
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{exact_token}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json)
            .unwrap()
            .as_deref(),
        Some("zzz-exact@test")
    );
}

#[test]
fn alias_for_auth_json_rejects_ambiguous_subject_fallback() {
    let (tmp, paths) = setup_test_env();
    let stale_a = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old-a");
    let stale_b = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old-b");
    let live_same_seat = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live");
    write_profile(&paths, "seat-a-primary@test", &stale_a);
    write_profile(&paths, "seat-a-copy@test", &stale_b);
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(
        &auth_json,
        format!(r#"{{"access_token":"{live_same_seat}"}}"#),
    )
    .unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

#[test]
fn active_starts_as_none() {
    let (_tmp, paths) = setup_test_env();
    let active = profile::get_active_from(&paths).unwrap();
    assert!(active.is_none());
}

// Fake JWT header `{"alg":"none"}`; profile capture only reads the `sub` claim.
const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

fn write_profile(paths: &Paths, alias: &str, access_token: &str) {
    let dir = paths.profiles_dir().join(alias);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{access_token}"}}"#),
    )
    .unwrap();
    let meta = profile::Meta {
        alias: alias.to_string(),
        email: None,
        plan: None,
        saved_at: "2026-01-01T00:00:00Z".to_string(),
    };
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
}

#[test]
fn switch_captures_outgoing_active_tokens() {
    let (_tmp, paths) = setup_test_env();
    // sub seatA (payload eyJzdWIiOiJzZWF0QSJ9), sub seatB (eyJzdWIiOiJzZWF0QiJ9)
    let a_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old");
    let a_live = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live"); // rotated by Codex
    let b_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.sig");

    write_profile(&paths, "a@test", &a_old);
    write_profile(&paths, "b@test", &b_tok);
    profile::set_active_from(&paths, "a@test").unwrap();
    // Codex rotated the active profile's token in ~/.codex after it was saved.
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{a_live}"}}"#),
    )
    .unwrap();

    profile::switch_to_from(&paths, "b@test").unwrap();

    // Outgoing profile's rotated token was folded back into its store.
    let a_store =
        std::fs::read_to_string(paths.profiles_dir().join("a@test").join("auth.json")).unwrap();
    assert!(
        a_store.contains(".live"),
        "expected captured token, got {a_store}"
    );
    // ~/.codex now holds the switched-to profile.
    let codex = std::fs::read_to_string(paths.codex_auth_json()).unwrap();
    assert!(codex.contains(&b_tok));
    assert_eq!(
        profile::get_active_from(&paths).unwrap().as_deref(),
        Some("b@test")
    );
}

#[test]
fn switch_skips_capture_for_foreign_codex_auth() {
    let (_tmp, paths) = setup_test_env();
    let a_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old"); // active store, sub seatA
    let foreign = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.live"); // ~/.codex, sub seatB
    let c_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.c");

    write_profile(&paths, "a@test", &a_old);
    write_profile(&paths, "c@test", &c_tok);
    profile::set_active_from(&paths, "a@test").unwrap();
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{foreign}"}}"#),
    )
    .unwrap();

    profile::switch_to_from(&paths, "c@test").unwrap();

    // A different seat in ~/.codex must not clobber the active profile's store.
    let a_store =
        std::fs::read_to_string(paths.profiles_dir().join("a@test").join("auth.json")).unwrap();
    assert!(a_store.contains(".old") && !a_store.contains(".live"));
}

#[test]
fn profile_aliases_cannot_escape_the_store() {
    let (tmp, paths) = setup_test_env();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "safe").unwrap();

    assert!(profile::get_profile_from(&paths, "../outside").is_err());
    assert!(profile::delete_profile_from(&paths, "../outside").is_err());
    assert!(
        profile::save_profile_to(&paths, "/tmp/escape", None, &paths.codex_auth_json()).is_err()
    );
    assert!(outside.join("keep").exists());
}

#[test]
fn directory_alias_is_authoritative_over_stored_metadata() {
    let (_tmp, paths) = setup_test_env();
    write_profile(&paths, "safe@test", "safe-token");
    let meta_path = paths.profiles_dir().join("safe@test").join("meta.json");
    let mut meta: profile::Meta =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta.alias = "../../outside".to_string();
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();

    let profiles = profile::list_profiles_from(&paths).unwrap();

    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].meta.alias, "safe@test");
    assert_eq!(profiles[0].dir, paths.profiles_dir().join("safe@test"));
}

#[test]
fn invalid_active_marker_is_ignored() {
    let (_tmp, paths) = setup_test_env();
    std::fs::write(paths.active_file(), "../../outside").unwrap();

    assert_eq!(profile::get_active_from(&paths).unwrap(), None);
}

#[test]
fn active_profile_uses_live_auth_only_for_the_same_token_subject() {
    let (_tmp, paths) = setup_test_env();
    let stored = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.stored");
    let rotated = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live");
    let foreign = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.foreign");
    write_profile(&paths, "a@test", &stored);
    let profile = profile::get_profile_from(&paths, "a@test").unwrap();

    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{rotated}"}}"#),
    )
    .unwrap();
    assert_eq!(
        profile::auth_json_path_for_profile_from(&paths, &profile, Some("a@test")),
        paths.codex_auth_json()
    );

    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{foreign}"}}"#),
    )
    .unwrap();
    assert_eq!(
        profile::auth_json_path_for_profile_from(&paths, &profile, Some("a@test")),
        profile.auth_json_path()
    );
    assert_eq!(
        profile::auth_json_path_for_profile_from(&paths, &profile, Some("other@test")),
        profile.auth_json_path()
    );
}

#[cfg(unix)]
#[test]
fn profile_and_live_auth_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let (_tmp, paths) = setup_test_env();
    profile::save_profile_and_activate_to(
        &paths,
        "private@test",
        Some("private@test"),
        &paths.codex_auth_json(),
    )
    .unwrap();

    let profile_dir = paths.profiles_dir().join("private@test");
    assert_eq!(
        std::fs::metadata(&profile_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        profile_dir.join("auth.json"),
        profile_dir.join("meta.json"),
        paths.active_file(),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    write_profile(&paths, "other@test", "other-token");
    profile::switch_to_from(&paths, "other@test").unwrap();
    assert_eq!(
        std::fs::metadata(paths.codex_auth_json())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(target_os = "linux")]
#[test]
fn case_colliding_profile_directories_do_not_break_listing() {
    let (_tmp, paths) = setup_test_env();
    write_profile(&paths, "Work", "upper-token");
    write_profile(&paths, "work", "lower-token");

    let profiles = profile::list_profiles_from(&paths).unwrap();

    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].meta.alias, "Work");
}

#[cfg(unix)]
#[test]
fn symbolic_link_profile_directory_is_rejected() {
    use std::os::unix::fs::symlink;

    let (tmp, paths) = setup_test_env();
    let outside = tmp.path().join("outside-profile");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, paths.profiles_dir().join("linked@test")).unwrap();

    assert!(profile::get_profile_from(&paths, "linked@test").is_err());
    assert!(
        profile::save_profile_to(&paths, "linked@test", None, &paths.codex_auth_json()).is_err()
    );
}

#[test]
fn concurrent_switches_keep_active_marker_and_live_auth_aligned() {
    use std::sync::{Arc, Barrier};

    let (_tmp, paths) = setup_test_env();
    write_profile(&paths, "a@test", "token-a");
    write_profile(&paths, "b@test", "token-b");
    let paths = Arc::new(paths);
    let barrier = Arc::new(Barrier::new(3));

    let handles: Vec<_> = ["a@test", "b@test"]
        .into_iter()
        .map(|alias| {
            let paths = Arc::clone(&paths);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                profile::switch_to_from(&paths, alias).unwrap();
            })
        })
        .collect();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }

    let active = profile::get_active_from(&paths).unwrap().unwrap();
    let live = codexctl::api::read_auth_json(&paths.codex_auth_json()).unwrap();
    let saved = codexctl::api::read_auth_json(
        &profile::get_profile_from(&paths, &active)
            .unwrap()
            .auth_json_path(),
    )
    .unwrap();
    assert_eq!(live.access_token, saved.access_token);
}

#[test]
fn case_fold_alias_collision_cannot_overwrite_credentials() {
    let (_tmp, paths) = setup_test_env();
    write_profile(&paths, "Work", "original-token");
    std::fs::write(paths.codex_auth_json(), r#"{"access_token":"new-token"}"#).unwrap();

    let result = profile::save_profile_to(&paths, "work", None, &paths.codex_auth_json());

    assert!(result.is_err());
    let original = std::fs::read_to_string(
        profile::get_profile_from(&paths, "Work")
            .unwrap()
            .auth_json_path(),
    )
    .unwrap();
    assert!(original.contains("original-token"));
    assert!(!original.contains("new-token"));
}
