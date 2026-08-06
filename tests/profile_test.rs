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
        saved_at: "2026-01-01T00:00:00Z".to_string(),
        ..codexctl::profile::Meta::default()
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

/// The case adding a team seat on an existing email creates: one login, two
/// workspaces. The subject alone is ambiguous, so the workspace has to settle
/// it — otherwise a rotated token is never captured back and the profile later
/// reports `expired` for no visible reason.
#[test]
fn alias_for_auth_json_separates_one_login_across_two_workspaces() {
    let (tmp, paths) = setup_test_env();
    let seat = |account: &str, jti: &str| {
        synthetic_token(&format!(
            r#"{{"sub":"seatA","jti":"{jti}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account}"}}}}"#
        ))
    };
    write_profile(&paths, "personal@test", &seat("acct-personal", "stored"));
    write_profile(&paths, "team@test", &seat("acct-team", "stored"));

    // Same seat and workspace as the team profile, but a rotated token value,
    // so the exact-token pass cannot resolve it.
    let live = seat("acct-team", "rotated");
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        Some("team@test".to_string())
    );
}

/// Excluding a candidate by workspace must not promote a *claimless* sibling to
/// a unique win. Otherwise a seat whose workspace was never saved would capture
/// its tokens into an unrelated profile and overwrite that profile's
/// credentials — the ambiguity guard exists to prevent exactly that guess.
#[test]
fn alias_for_auth_json_does_not_let_a_claimless_profile_absorb_a_new_workspace() {
    let (tmp, paths) = setup_test_env();
    let with_workspace = synthetic_token(
        r#"{"sub":"seatA","jti":"x","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"}}"#,
    );
    // Same seat, but its stored token declares no workspace at all.
    let claimless = synthetic_token(r#"{"sub":"seatA","jti":"y"}"#);
    write_profile(&paths, "has-workspace@test", &with_workspace);
    write_profile(&paths, "claimless@test", &claimless);

    // A third workspace on the same seat, matching neither stored profile.
    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"live","https://api.openai.com/auth":{"chatgpt_account_id":"acct-2"}}"#,
    );
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// The simplest shape of the same hazard: one saved profile, and a live token
/// for a second workspace on that login which was never saved. A candidate that
/// positively declares a *different* workspace is not the owner, so being the
/// only candidate must not make it one.
#[test]
fn alias_for_auth_json_refuses_a_lone_profile_declaring_another_workspace() {
    let (tmp, paths) = setup_test_env();
    let stored = synthetic_token(
        r#"{"sub":"seatA","jti":"stored","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"}}"#,
    );
    write_profile(&paths, "work@test", &stored);

    // A second workspace on the same login, never saved as a profile.
    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"live","https://api.openai.com/auth":{"chatgpt_account_id":"acct-2"}}"#,
    );
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// A profile saved before workspace claims existed still captures its own
/// rotated tokens: with nothing declaring a conflicting workspace, a lone
/// claimless candidate remains the owner.
#[test]
fn alias_for_auth_json_still_matches_a_claimless_profile_with_no_rival() {
    let (tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy@test",
        &synthetic_token(r#"{"sub":"seatA"}"#),
    );

    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"live","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"}}"#,
    );
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        Some("legacy@test".to_string())
    );
}

/// A real Codex auth.json declares `tokens.account_id` directly. The saved
/// workspace must come from the same resolution the rest of the code uses, or
/// the save guard silently degrades to the plain overwrite prompt.
#[test]
fn save_profile_records_the_account_id_declared_by_the_auth_file() {
    let (_tmp, paths) = setup_test_env();
    // Token payload carries a subject but no workspace claim.
    let token = synthetic_token(r#"{"sub":"seatA"}"#);
    let auth_src = paths.codex_auth_json();
    std::fs::write(
        &auth_src,
        format!(r#"{{"tokens":{{"access_token":"{token}","account_id":"acct-from-file"}}}}"#),
    )
    .unwrap();

    profile::save_profile_to(&paths, "team", None, &auth_src).unwrap();

    assert_eq!(
        profile::get_profile_from(&paths, "team")
            .unwrap()
            .meta
            .account_id
            .as_deref(),
        Some("acct-from-file")
    );
}

/// Two profiles holding the same seat *and* the same workspace stay ambiguous.
/// Guessing between them could overwrite the wrong profile's tokens.
#[test]
fn alias_for_auth_json_rejects_two_profiles_on_one_workspace() {
    let (tmp, paths) = setup_test_env();
    let seat = |jti: &str| {
        synthetic_token(&format!(
            r#"{{"sub":"seatA","jti":"{jti}","https://api.openai.com/auth":{{"chatgpt_account_id":"acct-one"}}}}"#
        ))
    };
    write_profile(&paths, "copy-a@test", &seat("a"));
    write_profile(&paths, "copy-b@test", &seat("b"));
    let auth_json = tmp.path().join("auth.json");
    let live = seat("live");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

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

// Fake JWT header `{"alg":"none"}`; profile capture only reads the claims payload.
const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

/// Build an unsigned JWT carrying `claims`. Every claim here is synthetic; no
/// real token value enters a fixture.
fn synthetic_token(claims: &str) -> String {
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    format!("{JWT_HDR}.{payload}.sig")
}

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
        saved_at: "2026-01-01T00:00:00Z".to_string(),
        ..profile::Meta::default()
    };
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
}

/// A `meta.json` written before labels existed must keep loading unchanged.
/// No migration runs, so this is the format most stores are still in.
#[test]
fn meta_json_without_label_fields_still_parses() {
    let (_tmp, paths) = setup_test_env();
    let dir = paths.profiles_dir().join("legacy@test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("auth.json"), r#"{"access_token":"tok"}"#).unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"legacy@test","email":"legacy@test","plan":"pro","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let profiles = profile::list_profiles_from(&paths).unwrap();

    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].meta.plan.as_deref(), Some("pro"));
    assert!(profiles[0].meta.label.is_none());
    assert!(profiles[0].meta.account_id.is_none());
    assert!(profiles[0].meta.user_id.is_none());
}

#[test]
fn save_profile_records_identity_from_token_claims() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(
        r#"{
            "https://api.openai.com/profile": {"email": "claim@example.com"},
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-team",
                "chatgpt_user_id": "user-1",
                "chatgpt_plan_type": "business"
            }
        }"#,
    );
    let auth_src = paths.codex_auth_json();
    std::fs::write(&auth_src, format!(r#"{{"access_token":"{token}"}}"#)).unwrap();

    profile::save_profile_to(&paths, "team", None, &auth_src).unwrap();

    let profile = profile::get_profile_from(&paths, "team").unwrap();
    assert_eq!(profile.meta.account_id.as_deref(), Some("acct-team"));
    assert_eq!(profile.meta.user_id.as_deref(), Some("user-1"));
    assert_eq!(profile.meta.plan.as_deref(), Some("business"));
    // The claim is authoritative for the address, so no alias guess is needed.
    assert_eq!(profile.meta.email.as_deref(), Some("claim@example.com"));
}

#[test]
fn set_label_trims_clears_and_rejects_invalid_text() {
    let (_tmp, paths) = setup_test_env();
    let auth_src = paths.codex_auth_json();
    profile::save_profile_to(&paths, "team", None, &auth_src).unwrap();
    let label_of = |paths: &Paths| profile::get_profile_from(paths, "team").unwrap().meta.label;

    profile::set_label_from(&paths, "team", Some("  team  ")).unwrap();
    assert_eq!(label_of(&paths).as_deref(), Some("team"));

    profile::set_label_from(&paths, "team", Some("   ")).unwrap();
    assert_eq!(label_of(&paths), None);

    assert!(profile::set_label_from(&paths, "team", Some("two\nlines")).is_err());
}

#[test]
fn set_label_fails_for_an_unknown_alias() {
    let (_tmp, paths) = setup_test_env();

    assert!(profile::set_label_from(&paths, "missing", Some("team")).is_err());
}

/// Re-saving an account must not silently erase the name the operator gave it.
#[test]
fn save_profile_preserves_an_existing_label() {
    let (_tmp, paths) = setup_test_env();
    let auth_src = paths.codex_auth_json();
    profile::save_profile_to(&paths, "team", None, &auth_src).unwrap();
    profile::set_label_from(&paths, "team", Some("team")).unwrap();

    profile::save_profile_to(&paths, "team", None, &auth_src).unwrap();

    let profile = profile::get_profile_from(&paths, "team").unwrap();
    assert_eq!(profile.meta.label.as_deref(), Some("team"));
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
