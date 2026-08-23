use codexctl::config::Paths;
use codexctl::profile;

/// A login known only by its JWT subject, as every pre-`chatgpt_user_id` token
/// is.
fn sub(subject: &str) -> codexctl::api::Logins {
    codexctl::api::Logins {
        uid: None,
        sub: Some(subject.to_string()),
    }
}

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

/// A profile that never recorded a workspace cannot confirm that an arriving
/// one is the same account — one login holds seats in several workspaces, and
/// a native login elsewhere can put another seat's credential in the live file.
/// So a claimed rotation is left unattributed rather than written over it.
///
/// The cost is that such a profile goes stale until its next `save` or `login`.
/// That is recoverable; overwriting its credentials is not.
#[test]
fn alias_for_auth_json_leaves_a_claimed_rotation_unattributed_to_a_claimless_profile() {
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
        None
    );
}

/// Displaying the active profile picks the live auth file only when it belongs
/// to that profile. Two workspace seats of one login share a subject, so the
/// subject alone is not ownership — otherwise the live seat's usage renders
/// under the other seat's row.
#[test]
fn active_profile_ignores_live_auth_from_a_different_workspace() {
    let (_tmp, paths) = setup_test_env();
    let stored = synthetic_token(
        r#"{"sub":"seatA","jti":"stored","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    write_profile(&paths, "team@test", &stored);
    // Live file: same login, other workspace.
    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"live","https://api.openai.com/auth":{"chatgpt_account_id":"acct-personal"}}"#,
    );
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{live}"}}"#),
    )
    .unwrap();

    let profile = profile::get_profile_from(&paths, "team@test").unwrap();
    let chosen = profile::auth_json_path_for_profile_from(&paths, &profile, Some("team@test"));

    assert_eq!(chosen, profile.auth_json_path());
}

/// The same login and the same workspace is genuine ownership, so the live file
/// still wins — that is what keeps the active row showing current usage.
#[test]
fn active_profile_uses_live_auth_from_the_same_workspace() {
    let (_tmp, paths) = setup_test_env();
    let seat = |jti: &str| {
        synthetic_token(&format!(
            r#"{{"sub":"seatA","jti":"{jti}","https://api.openai.com/auth":{{"chatgpt_account_id":"acct-team"}}}}"#
        ))
    };
    write_profile(&paths, "team@test", &seat("stored"));
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{}"}}"#, seat("live")),
    )
    .unwrap();

    let profile = profile::get_profile_from(&paths, "team@test").unwrap();
    let chosen = profile::auth_json_path_for_profile_from(&paths, &profile, Some("team@test"));

    assert_eq!(chosen, paths.codex_auth_json());
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

/// One seat can be saved under two aliases. Subject matching is ambiguous
/// there, so the capture would drop the rotation; the alias the launch was
/// pinned to is the fact that settles it.
#[test]
fn exec_capture_prefers_the_pinned_alias_over_an_ambiguous_subject() {
    let (_tmp, paths) = setup_test_env();
    let stored = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.stored");
    let rotated = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.rotated");
    write_profile(&paths, "primary", &stored);
    write_profile(&paths, "duplicate", &stored);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{rotated}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "primary").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("primary").join("auth.json"))
            .unwrap()
            .contains(&rotated)
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("duplicate").join("auth.json"))
            .unwrap()
            .contains(&stored),
        "an unrelated alias must not be rewritten"
    );
}

/// Recovery inside a pinned lane rotates the home to another account, so a
/// token that is not the pinned alias's still has to reach its real owner.
#[test]
fn exec_capture_falls_back_to_the_real_owner_after_a_recovery_switch() {
    let (_tmp, paths) = setup_test_env();
    let pinned_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.pinned");
    let recovered_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.old");
    let recovered_live = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.live");
    write_profile(&paths, "pinned", &pinned_tok);
    write_profile(&paths, "recovered", &recovered_old);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{recovered_live}"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "pinned").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("recovered").join("auth.json"))
            .unwrap()
            .contains(&recovered_live)
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("pinned").join("auth.json"))
            .unwrap()
            .contains(&pinned_tok),
        "the pinned alias must keep its own token"
    );
}

/// A pinned run folds its rotated token into the profile while the live Codex
/// file still holds the older one. The next switch must not copy that stale
/// file back over the rotation.
#[test]
fn switch_capture_does_not_regress_a_profile_to_an_older_live_token() {
    let (_tmp, paths) = setup_test_env();
    // sub seatA, exp 2000000000 (fresh) and exp 1900000000 (older).
    let rotated = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImV4cCI6MjAwMDAwMDAwMH0.rotated");
    let stale = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImV4cCI6MTkwMDAwMDAwMH0.stale");
    write_profile(&paths, "a", &rotated);
    write_profile(&paths, "b", &format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.b"));
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{stale}"}}"#),
    )
    .unwrap();
    profile::set_active_from(&paths, "a").unwrap();

    profile::switch_to_from(&paths, "b").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("a").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "the switch capture overwrote a newer token with an older live copy"
    );
}

/// The active marker is only a hint. It decides which alias owns a live token
/// when two aliases hold the same seat, but it never overrides the token: a
/// marker left stale by an interrupted switch cannot claim a foreign seat,
/// which `switch_skips_capture_for_foreign_codex_auth` proves from the other
/// side.
#[test]
fn switch_capture_uses_the_active_marker_only_to_break_a_tie() {
    let (_tmp, paths) = setup_test_env();
    let shared_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old");
    let shared_live = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live");
    // One seat, saved twice — ambiguous by subject alone.
    write_profile(&paths, "marked", &shared_old);
    write_profile(&paths, "duplicate", &shared_old);
    write_profile(
        &paths,
        "next",
        &format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.next"),
    );
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{shared_live}"}}"#),
    )
    .unwrap();
    profile::set_active_from(&paths, "marked").unwrap();

    profile::switch_to_from(&paths, "next").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("marked").join("auth.json"))
            .unwrap()
            .contains(&shared_live),
        "the marked alias should receive the rotation instead of it being dropped"
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("duplicate").join("auth.json"))
            .unwrap()
            .contains(&shared_old),
        "an unmarked duplicate must not be rewritten"
    );
}

/// A shortened token lifetime makes the newer token expire first, so expiry
/// alone would refuse a genuine rotation and strand the profile on a refresh
/// token the server has already replaced.
#[test]
fn exec_capture_orders_tokens_by_issued_at_not_expiry() {
    let (_tmp, paths) = setup_test_env();
    // iat 1900000000 / exp 2000000000 — issued earlier, longer lifetime.
    let older =
        format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImlhdCI6MTkwMDAwMDAwMCwiZXhwIjoyMDAwMDAwMDAwfQ.old");
    // iat 1950000000 / exp 1960000000 — issued later, shorter lifetime.
    let newer =
        format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImlhdCI6MTk1MDAwMDAwMCwiZXhwIjoxOTYwMDAwMDAwfQ.new");
    write_profile(&paths, "a", &older);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{newer}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "a").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("a").join("auth.json"))
            .unwrap()
            .contains(&newer),
        "a later-issued token was refused because it expires sooner"
    );
}

/// A token saved verbatim under one alias belongs to that alias, whatever the
/// caller's hint says. The hint only settles what the token cannot.
#[test]
fn exec_capture_prefers_an_exact_token_owner_over_the_hint() {
    let (_tmp, paths) = setup_test_env();
    let hinted_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.hinted");
    let owner_tok = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.owner");
    // One seat, two aliases holding different tokens for it.
    write_profile(&paths, "hinted", &hinted_tok);
    write_profile(&paths, "owner", &owner_tok);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{owner_tok}","refresh_token":"rotated"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "hinted").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("owner").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "the exact-token owner should receive the capture"
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("hinted").join("auth.json"))
            .unwrap()
            .contains(&hinted_tok),
        "the hinted alias must not absorb another alias's token"
    );
}

/// Two tokens issued in the same second still have to be ordered, and the one
/// that expires sooner is not the newer one.
#[test]
fn exec_capture_falls_back_to_expiry_when_issued_at_ties() {
    let (_tmp, paths) = setup_test_env();
    // Both iat 1900000000; exp 2000000000 stored, exp 1950000000 captured.
    let stored = format!(
        "{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImlhdCI6MTkwMDAwMDAwMCwiZXhwIjoyMDAwMDAwMDAwfQ.stored"
    );
    let sooner = format!(
        "{JWT_HDR}.eyJzdWIiOiJzZWF0QSIsImlhdCI6MTkwMDAwMDAwMCwiZXhwIjoxOTUwMDAwMDAwfQ.sooner"
    );
    write_profile(&paths, "a", &stored);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{sooner}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "a").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("a").join("auth.json"))
            .unwrap()
            .contains(&stored),
        "an earlier-expiring token from the same second must not win"
    );
}

/// Two aliases saved from one login are byte-identical, so a store-wide scan
/// answers by directory order. A refresh-only rotation still has the original
/// access token, so only the launch label can say which alias earned it.
#[test]
fn exec_capture_keeps_a_refresh_rotation_on_the_pinned_identical_duplicate() {
    let (_tmp, paths) = setup_test_env();
    let shared = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.shared");
    for alias in ["a-dup", "z-pin"] {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            format!(r#"{{"access_token":"{shared}","refresh_token":"old"}}"#),
        )
        .unwrap();
        let meta = profile::Meta {
            alias: alias.to_string(),
            email: None,
            plan: None,
            saved_at: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{shared}","refresh_token":"new"}}"#),
    )
    .unwrap();

    // Pinned to the alias that sorts last, so directory order cannot supply it.
    profile::capture_exec_auth_from(&paths, &exec_auth, "z-pin").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("z-pin").join("auth.json"))
            .unwrap()
            .contains("new"),
        "the pinned alias lost its own refresh rotation"
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("a-dup").join("auth.json"))
            .unwrap()
            .contains("old"),
        "an identical duplicate must not absorb the rotation"
    );
}

/// A token declaring no workspace cannot prove it belongs to a profile that
/// declares one. Accepting it is how the other seat's rotated token gets
/// attributed to — and captured over — this profile's credentials.
#[test]
fn active_profile_ignores_a_claimless_live_token_against_a_claimed_profile() {
    let (_tmp, paths) = setup_test_env();
    let stored = synthetic_token(
        r#"{"sub":"seatA","jti":"stored","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    write_profile(&paths, "team@test", &stored);
    // Same login, rotated, and declaring no workspace at all.
    let live = synthetic_token(r#"{"sub":"seatA","jti":"live"}"#);
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{live}"}}"#),
    )
    .unwrap();

    let profile = profile::get_profile_from(&paths, "team@test").unwrap();
    let chosen = profile::auth_json_path_for_profile_from(&paths, &profile, Some("team@test"));

    assert_eq!(
        chosen,
        profile.auth_json_path(),
        "an unprovable live token was treated as this profile's own"
    );
}

/// The same rule governs which auth file the active row reads: a claimed live
/// token is not shown as a claimless profile's own, because it may be another
/// seat of that login. The stored copy is used instead.
#[test]
fn active_profile_ignores_a_claimed_live_token_for_a_claimless_profile() {
    let (_tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy@test",
        &synthetic_token(r#"{"sub":"seatA"}"#),
    );
    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"live","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1"}}"#,
    );
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{live}"}}"#),
    )
    .unwrap();

    let profile = profile::get_profile_from(&paths, "legacy@test").unwrap();
    let chosen = profile::auth_json_path_for_profile_from(&paths, &profile, Some("legacy@test"));

    assert_eq!(chosen, profile.auth_json_path());
}

/// The store-wide resolver is the other route credentials reach a profile, so
/// it needs the same rule as ownership matching: a token declaring no workspace
/// cannot be attributed to the one profile that declares one, even when it is
/// the only candidate on that login.
#[test]
fn alias_for_auth_json_refuses_a_claimless_token_against_a_lone_claimed_profile() {
    let (tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "team@test",
        &synthetic_token(
            r#"{"sub":"seatA","jti":"stored","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
        ),
    );
    // Same login, rotated, declaring no workspace.
    let live = synthetic_token(r#"{"sub":"seatA","jti":"live"}"#);
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// `auth.json` may carry an explicit `account_id`, which `read_auth_json`
/// prefers over the JWT claim. Two files can therefore share one access token
/// and still name different workspaces, so token equality alone is not identity.
#[test]
fn alias_for_auth_json_refuses_one_token_naming_two_workspaces() {
    let (tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    let dir = paths.profiles_dir().join("team@test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team@test","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same token, explicitly a different workspace.
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(
        &auth_json,
        format!(r#"{{"access_token":"{shared}","account_id":"acct-personal"}}"#),
    )
    .unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// A save writes `auth.json` before `meta.json`. If it stops in between, the
/// stored credential is the newer fact — trusting metadata there would reject
/// the account actually stored and leave the profile unrepairable.
#[test]
fn workspace_comes_from_the_stored_token_when_metadata_lags() {
    let (_tmp, paths) = setup_test_env();
    let dir = paths.profiles_dir().join("work@test");
    std::fs::create_dir_all(&dir).unwrap();
    let stored = synthetic_token(
        r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-new"}}"#,
    );
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{stored}"}}"#),
    )
    .unwrap();
    // Metadata still describes the workspace held before the interrupted save.
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"work@test","email":null,"plan":null,"account_id":"acct-old","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    assert_eq!(
        profile::workspace_of_profile(&paths, "work@test").as_deref(),
        Some("acct-new")
    );
    // So the same login re-saving the account actually stored can repair the
    // profile. (`seatA` is the stored token's subject, which is the login claim
    // a real incoming token always carries.)
    assert!(
        profile::conflicting_workspace(&paths, "work@test", Some("acct-new"), &sub("seatA"))
            .is_none()
    );
    // ...while the stale metadata's workspace is still refused. Both sides make
    // a claim and the claims disagree, so no answer could reconcile them: this
    // is a refusal, not a question for the operator.
    assert!(matches!(
        profile::conflicting_workspace(&paths, "work@test", Some("acct-old"), &sub("seatA")),
        Some(profile::AccountConflict::Different(stored)) if stored == "acct-new"
    ));
    // A different login in the workspace actually stored is refused too — and
    // the refusal names the login, since that is the claim that disagrees.
    // Naming the workspace here would report "stored acct-new, incoming
    // acct-new" as the reason the two differ.
    assert!(matches!(
        profile::conflicting_workspace(&paths, "work@test", Some("acct-new"), &sub("seatB")),
        Some(profile::AccountConflict::Different(stored)) if stored == "sub:seatA"
    ));
}

/// The pinned-alias capture takes an exact-token shortcut before the ownership
/// rule. One access token can name two workspaces through an explicit
/// `account_id`, so that shortcut has to honour the rule too — otherwise a
/// pinned run copies a foreign workspace's whole auth file over the profile.
#[test]
fn exec_capture_refuses_a_foreign_workspace_sharing_the_access_token() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    let dir = paths.profiles_dir().join("pinned");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-pinned"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"pinned","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same access token, different workspace, and a rotated refresh token.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(
            r#"{{"access_token":"{shared}","refresh_token":"foreign","account_id":"acct-other"}}"#
        ),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "pinned").unwrap();

    let kept = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(
        !kept.contains("foreign"),
        "a foreign workspace was captured over the pinned profile"
    );
    assert!(kept.contains("acct-pinned"));
}

/// Two profiles can hold one access token: a legacy one declaring no workspace
/// and the real owner declaring it. The one that declares it is the stronger
/// match, and directory order must not hand the credentials to the other.
#[test]
fn exact_token_resolution_prefers_the_declared_workspace_over_directory_order() {
    let (tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    // Sorts first, declares nothing.
    let legacy = paths.profiles_dir().join("aaa-legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("auth.json"),
        format!(r#"{{"access_token":"{shared}"}}"#),
    )
    .unwrap();
    std::fs::write(
        legacy.join("meta.json"),
        r#"{"alias":"aaa-legacy","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    // Sorts later, declares the workspace the target names.
    let owner = paths.profiles_dir().join("zzz-owner");
    std::fs::create_dir_all(&owner).unwrap();
    std::fs::write(
        owner.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        owner.join("meta.json"),
        r#"{"alias":"zzz-owner","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let auth_json = tmp.path().join("auth.json");
    std::fs::write(
        &auth_json,
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json)
            .unwrap()
            .as_deref(),
        Some("zzz-owner")
    );
}

/// Codex can add an explicit `account_id` without changing either token. That
/// is a real credential update: discarding it leaves the profile claimless, and
/// a claimless profile is what keeps duplicate-email ownership ambiguous.
#[test]
fn capture_records_a_workspace_that_appears_without_a_token_change() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(r#"{"sub":"seatA","jti":"stable"}"#);
    let dir = paths.profiles_dir().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"work","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same token, now naming its workspace.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{token}","account_id":"acct-team"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "work").unwrap();

    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("acct-team"),
        "the workspace claim was discarded"
    );
}

/// One token held by a claimless profile and by a profile declaring another
/// workspace is demonstrably ambiguous. The claimless one is not the safe
/// default there — it is simply the one that declares nothing.
#[test]
fn exact_token_resolution_refuses_when_a_sibling_declares_another_workspace() {
    let (tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    for (alias, account) in [("legacy", None), ("other", Some("acct-other"))] {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        let auth = match account {
            Some(account) => format!(r#"{{"access_token":"{shared}","account_id":"{account}"}}"#),
            None => format!(r#"{{"access_token":"{shared}"}}"#),
        };
        std::fs::write(dir.join("auth.json"), auth).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"alias":"{alias}","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
    }

    // The live file names a third workspace nobody declared.
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(
        &auth_json,
        format!(r#"{{"access_token":"{shared}","account_id":"acct-target"}}"#),
    )
    .unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// A save writes `auth.json` before `meta.json`, so an interrupted one leaves a
/// real seat with no metadata. Reading that as "not saved" is how a second
/// alias gets created for an account already in the store.
#[test]
fn existing_seat_sees_a_profile_left_without_metadata() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(
        r#"{"sub":"seatA","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    let dir = paths.profiles_dir().join("half-written");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}"}}"#),
    )
    .unwrap();
    // No meta.json: the save stopped between the two writes.

    let seat = profile::existing_seat(&paths, Some("acct-team"), &sub("seatA")).unwrap();

    assert!(
        matches!(seat, profile::ExistingSeat::One(alias) if alias == "half-written"),
        "an interrupted save was treated as no seat at all"
    );
}

/// A rotated token need not carry the workspace claim, so a claimless token on
/// a login that also has a declared profile could belong to either. Handing it
/// to the claimless one would capture the declared account's rotation into the
/// wrong profile.
#[test]
fn subject_fallback_refuses_a_claimless_token_when_a_sibling_declares_a_workspace() {
    let (tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy@test",
        &synthetic_token(r#"{"sub":"seatA","jti":"legacy"}"#),
    );
    write_profile(
        &paths,
        "team@test",
        &synthetic_token(
            r#"{"sub":"seatA","jti":"team","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
        ),
    );

    // Rotated, same login, declaring no workspace.
    let live = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None
    );
}

/// The pinned-alias hint settles ties between equals. It does not settle a
/// claimless file against a login that also has a declared profile: that file
/// may be the declared sibling's rotation, so the hint must not claim it.
#[test]
fn hint_does_not_claim_a_claimless_token_when_a_sibling_declares_a_workspace() {
    let (_tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy@test",
        &synthetic_token(r#"{"sub":"seatA","jti":"legacy"}"#),
    );
    write_profile(
        &paths,
        "team@test",
        &synthetic_token(
            r#"{"sub":"seatA","jti":"team","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
        ),
    );

    let rotated = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{rotated}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "legacy@test").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("legacy@test").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "an undecided token was captured into the hinted profile"
    );
}

/// Automatic seat reuse replaces a profile, so a missing claim on either side
/// is not a match. Otherwise a claimless token would look like the same seat as
/// any legacy profile on that login.
#[test]
fn existing_seat_requires_both_identity_halves() {
    let (_tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy@test",
        &synthetic_token(r#"{"sub":"seatA","jti":"legacy"}"#),
    );

    // The incoming token names a login but no workspace.
    let seat = profile::existing_seat(&paths, None, &sub("seatA")).unwrap();

    assert!(
        matches!(seat, profile::ExistingSeat::None),
        "a partial identity was accepted as an exact saved seat"
    );
}

/// A stored token with no claim does not make a profile anonymous when its
/// metadata records the account. Ignoring that fallback during attribution lets
/// another workspace's token be captured over it.
#[test]
fn attribution_honours_a_workspace_recorded_only_in_metadata() {
    let (_tmp, paths) = setup_test_env();
    // Stored token is claimless; metadata knows the workspace.
    let dir = paths.profiles_dir().join("team@test");
    std::fs::create_dir_all(&dir).unwrap();
    let stored = synthetic_token(r#"{"sub":"seatA","jti":"stored"}"#);
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{stored}"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team@test","email":null,"plan":null,"account_id":"acct-team","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same login, a different workspace, rotated.
    let foreign = synthetic_token(
        r#"{"sub":"seatA","jti":"other","https://api.openai.com/auth":{"chatgpt_account_id":"acct-other"}}"#,
    );
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{foreign}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "team@test").unwrap();

    // Assert on the token itself: the workspace name lives inside a base64
    // payload, so searching the file for it would match nothing either way.
    let kept = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(
        kept.contains(&stored),
        "the profile's own credential was replaced"
    );
    assert!(
        !kept.contains(&foreign),
        "a foreign workspace was captured over a profile identified by metadata"
    );
}

/// The exact-token shortcut runs before the fuller ownership check, so it has
/// to honour a workspace held only in metadata too. One access token can name
/// two workspaces through an explicit `account_id`.
#[test]
fn exact_token_shortcut_honours_a_metadata_only_workspace() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    let dir = paths.profiles_dir().join("team@test");
    std::fs::create_dir_all(&dir).unwrap();
    // Claimless stored token; the workspace lives in metadata only.
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team@test","email":null,"plan":null,"account_id":"acct-team","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same access token, explicitly a different workspace, rotated refresh.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(
            r#"{{"access_token":"{shared}","refresh_token":"foreign","account_id":"acct-other"}}"#
        ),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "team@test").unwrap();

    let kept = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(
        !kept.contains("foreign"),
        "a foreign workspace was captured through the exact-token shortcut"
    );
}

/// Adding a second workspace for one login creates a declared sibling. If that
/// happens before the outgoing credential is captured, the outgoing token —
/// claimless, rotated, and the only copy — looks ownerless and is lost when the
/// new profile is installed over the live file.
#[test]
fn saving_a_sibling_workspace_captures_the_outgoing_rotation_first() {
    let (_tmp, paths) = setup_test_env();
    // Active legacy profile: same login, no workspace claim.
    let stored = synthetic_token(r#"{"sub":"seatA","jti":"stored"}"#);
    write_profile(&paths, "personal", &stored);
    profile::set_active_from(&paths, "personal").unwrap();

    // Its live token rotated, still claimless — this is the only copy.
    let rotated = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{rotated}"}}"#),
    )
    .unwrap();

    // Now save a second workspace for the same login from an isolated home.
    let incoming = synthetic_token(
        r#"{"sub":"seatA","jti":"team","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    let login_home = paths.home.join("login-auth.json");
    std::fs::write(&login_home, format!(r#"{{"access_token":"{incoming}"}}"#)).unwrap();

    profile::save_profile_and_activate_to(&paths, "team", None, &login_home).unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("personal").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "the outgoing rotation was lost when the sibling workspace was added"
    );
}

/// The hint must not outrank evidence: when another profile holds the same
/// access token *and* declares the arriving workspace, it is the stronger
/// owner even though the caller named a different alias.
#[test]
fn hint_does_not_outrank_a_stronger_exact_token_owner() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    for (alias, account) in [("legacy", None), ("team", Some("acct-team"))] {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        let auth = match account {
            Some(account) => format!(r#"{{"access_token":"{shared}","account_id":"{account}"}}"#),
            None => format!(r#"{{"access_token":"{shared}"}}"#),
        };
        std::fs::write(dir.join("auth.json"), auth).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"alias":"{alias}","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
    }

    // Same token, declaring the team workspace, with a rotated refresh token.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(
            r#"{{"access_token":"{shared}","refresh_token":"rotated","account_id":"acct-team"}}"#
        ),
    )
    .unwrap();

    // The caller names the legacy profile; the declared one is stronger.
    profile::capture_exec_auth_from(&paths, &exec_auth, "legacy").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("legacy").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "the hint took a credential belonging to a stronger owner"
    );
    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("team").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "the declared owner did not receive its rotation"
    );
}

/// A lone holder of an exact access token owns it even when only the profile
/// declares a workspace — otherwise a refresh-only rotation is discarded and
/// the profile keeps stale credentials.
#[test]
fn a_sole_exact_token_owner_receives_a_claimless_rotation() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same token, refresh rotated, and this file declares no workspace.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{shared}","refresh_token":"rotated"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "team").unwrap();

    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "a sole exact-token owner lost its refresh rotation"
    );
}

/// A contradiction anywhere in the candidate set makes ownership undecidable
/// for all of them: the token demonstrably spans workspaces. Naming one holder
/// is then an override, not a tie-break.
#[test]
fn hint_does_not_revive_a_candidate_after_a_contradiction() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    for (alias, account) in [("legacy", None), ("other", Some("acct-b"))] {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        let auth = match account {
            Some(account) => format!(r#"{{"access_token":"{shared}","account_id":"{account}"}}"#),
            None => format!(r#"{{"access_token":"{shared}"}}"#),
        };
        std::fs::write(dir.join("auth.json"), auth).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"alias":"{alias}","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
    }

    // A third workspace, same token, rotated refresh.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{shared}","refresh_token":"rotated","account_id":"acct-c"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "legacy").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("legacy").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "the hint revived a candidate after ownership was undecidable"
    );
}

/// A save writes `auth.json` before `meta.json`, so an interrupted one still
/// holds the token. Omitting it can leave a claimless sibling looking like the
/// sole owner of another workspace's credential.
#[test]
fn exact_token_candidates_include_a_profile_left_without_metadata() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    // Complete, claimless.
    write_profile(&paths, "legacy", &shared);
    // Half-written, and it declares the arriving workspace.
    let dir = paths.profiles_dir().join("half-written");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();

    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(
            r#"{{"access_token":"{shared}","refresh_token":"rotated","account_id":"acct-team"}}"#
        ),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "legacy").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("legacy").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "an interrupted profile was ignored and its workspace landed on a sibling"
    );
}

/// A capture that simply omits the workspace is not an update. Erasing it
/// would destroy a legacy profile's only ownership evidence and make every
/// later rotation unattributable.
#[test]
fn capture_does_not_erase_a_stored_workspace() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(r#"{"sub":"seatA","jti":"stable"}"#);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Identical credential, but this copy names no workspace.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{token}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "team").unwrap();

    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("acct-team"),
        "the stored workspace was erased by a claimless copy"
    );
}

/// The active row reads the live file only when it truly belongs to the active
/// profile. A sibling holding the same token and declaring the live workspace
/// is the stronger owner, so its usage must not render under this alias.
#[test]
fn active_profile_defers_to_a_stronger_exact_token_sibling() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    write_profile(&paths, "legacy", &shared);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // The live file is that team seat.
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();

    let legacy = profile::get_profile_from(&paths, "legacy").unwrap();
    let chosen = profile::auth_json_path_for_profile_from(&paths, &legacy, Some("legacy"));

    assert_eq!(
        chosen,
        legacy.auth_json_path(),
        "another seat's live usage would render under this alias"
    );
}

/// A refresh-only rotation whose file omits the workspace still captures — but
/// the profile's proven workspace has to survive it, or a pre-0.1.22 profile
/// loses its only ownership evidence and later rotations become unattributable.
#[test]
fn capture_keeps_the_workspace_through_a_claimless_refresh_rotation() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(r#"{"sub":"seatA","jti":"stable"}"#);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}","refresh_token":"old","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same access token, rotated refresh, and no workspace claim in the file.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{token}","refresh_token":"new"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "team").unwrap();

    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("new"),
        "the refresh rotation was not captured"
    );
    assert_eq!(
        profile::workspace_of_profile(&paths, "team").as_deref(),
        Some("acct-team"),
        "the profile's proven workspace was lost with the copy"
    );
}

/// The rotated-token pass must see interrupted profiles too: a half-written
/// sibling declaring a workspace makes a claimless rotation ambiguous rather
/// than leaving the visible profile as its sole owner.
#[test]
fn rotated_token_resolution_sees_a_profile_left_without_metadata() {
    let (tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy",
        &synthetic_token(r#"{"sub":"seatA","jti":"legacy"}"#),
    );
    // Half-written sibling on the same login, declaring a workspace.
    let dir = paths.profiles_dir().join("half-written");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(
            r#"{{"access_token":"{}"}}"#,
            synthetic_token(
                r#"{"sub":"seatA","jti":"team","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#
            )
        ),
    )
    .unwrap();

    // A claimless rotation on that login.
    let live = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None,
        "an interrupted sibling was ignored, making an ambiguous token look owned"
    );
}

/// The live-file capture that `use` performs preserves a proven workspace just
/// as the pinned one does — both go through the same step, so neither can
/// erase the evidence the other protects.
#[test]
fn switch_capture_keeps_the_workspace_through_a_claimless_rotation() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(r#"{"sub":"seatA","jti":"stable"}"#);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}","refresh_token":"old","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    write_profile(
        &paths,
        "next",
        &synthetic_token(r#"{"sub":"seatB","jti":"next"}"#),
    );
    profile::set_active_from(&paths, "team").unwrap();

    // The live file rotated its refresh token and carries no workspace claim.
    std::fs::write(
        paths.codex_auth_json(),
        format!(r#"{{"access_token":"{token}","refresh_token":"new"}}"#),
    )
    .unwrap();

    profile::switch_to_from(&paths, "next").unwrap();

    assert!(
        std::fs::read_to_string(dir.join("auth.json"))
            .unwrap()
            .contains("new"),
        "the outgoing rotation was not captured"
    );
    assert_eq!(
        profile::workspace_of_profile(&paths, "team").as_deref(),
        Some("acct-team"),
        "the outgoing profile's proven workspace was erased"
    );
}

/// The weighing rules out a claimless file against a declared sibling. The
/// hint must apply the same rule, or it revives a candidate already ruled out.
#[test]
fn hint_does_not_revive_a_claimless_candidate_against_a_declared_sibling() {
    let (_tmp, paths) = setup_test_env();
    let shared = synthetic_token(r#"{"sub":"seatA","jti":"shared"}"#);
    write_profile(&paths, "legacy", &shared);
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{shared}","account_id":"acct-team"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // Same token, rotated refresh, declaring no workspace.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{shared}","refresh_token":"rotated"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "legacy").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("legacy").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "the hint revived a candidate the weighing had ruled out"
    );
}

/// Preservation has to correct stale metadata, not merely fill an empty field:
/// an interrupted save can leave metadata naming the previous workspace, and
/// after a claimless capture that stale name would answer for the profile.
#[test]
fn capture_replaces_stale_metadata_with_the_proven_workspace() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(
        r#"{"sub":"seatA","jti":"stable","https://api.openai.com/auth":{"chatgpt_account_id":"acct-new"}}"#,
    );
    let dir = paths.profiles_dir().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}","refresh_token":"old"}}"#),
    )
    .unwrap();
    // Metadata still names the workspace held before the interrupted save.
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"work","email":null,"plan":null,"account_id":"acct-old","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // A refresh rotation whose file carries no claim at all.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{token}","refresh_token":"new"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "work").unwrap();

    assert_eq!(
        profile::workspace_of_profile(&paths, "work").as_deref(),
        Some("acct-new"),
        "stale metadata answered for the profile after capture"
    );
}

/// An interrupted save leaves credentials with no metadata. That profile's
/// workspace lives only in its stored token, so a claimless capture must write
/// the evidence out before replacing the file that carries it.
#[test]
fn capture_preserves_a_workspace_for_a_profile_with_no_metadata() {
    let (_tmp, paths) = setup_test_env();
    let token = synthetic_token(
        r#"{"sub":"seatA","jti":"stable","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    let dir = paths.profiles_dir().join("half-written");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("auth.json"),
        format!(r#"{{"access_token":"{token}","refresh_token":"old"}}"#),
    )
    .unwrap();
    // No meta.json at all.

    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(r#"{{"access_token":"{token}","refresh_token":"new"}}"#),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "half-written").unwrap();

    assert_eq!(
        profile::workspace_of_profile(&paths, "half-written").as_deref(),
        Some("acct-team"),
        "the workspace was lost with the file that carried it"
    );
}

/// A profile whose credentials are unreadable can still be identified by its
/// metadata. Dropping it from the vote would let a readable sibling look like
/// the sole owner of a credential this one may hold.
#[test]
fn resolution_stops_when_an_unreadable_profile_still_identifies_the_seat() {
    let (tmp, paths) = setup_test_env();
    write_profile(
        &paths,
        "legacy",
        &synthetic_token(r#"{"sub":"seatA","jti":"legacy"}"#),
    );
    // Damaged credentials, but metadata names this login.
    let dir = paths.profiles_dir().join("team");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("auth.json"), "{ truncated").unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"alias":"team","email":null,"plan":null,"account_id":"acct-team","user_id":"user-a","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    // The arriving token names the same `chatgpt_user_id` the metadata records.
    let live = synthetic_token(
        r#"{"sub":"seatA","jti":"rotated","https://api.openai.com/auth":{"chatgpt_user_id":"user-a"}}"#,
    );
    let auth_json = tmp.path().join("auth.json");
    std::fs::write(&auth_json, format!(r#"{{"access_token":"{live}"}}"#)).unwrap();

    assert_eq!(
        profile::alias_for_auth_json_from(&paths, &auth_json).unwrap(),
        None,
        "a damaged but identified profile was dropped from the decision"
    );
}

/// Capture leaves no snapshot behind, and the snapshot it works from is the
/// file it decided on: the source is read once, so ownership and the bytes
/// written can never come from different accounts.
#[test]
fn capture_cleans_up_its_snapshot() {
    let (_tmp, paths) = setup_test_env();
    let stored = synthetic_token(r#"{"sub":"seatA","jti":"stored"}"#);
    write_profile(&paths, "work", &stored);
    let rotated = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{rotated}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "work").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("work").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "the rotation was not captured"
    );
    assert!(
        !paths.codexctl_dir().join(".capture-snapshot.json").exists(),
        "the capture snapshot was left in the store"
    );
}

/// A damaged profile belonging to somebody else must not suppress a healthy
/// profile's capture. Failing closed against every unreadable sibling would
/// let a valid rotation be discarded and the healthy profile expire.
#[test]
fn capture_proceeds_despite_an_unrelated_damaged_profile() {
    let (_tmp, paths) = setup_test_env();
    let stored = synthetic_token(r#"{"sub":"seatA","jti":"stored"}"#);
    write_profile(&paths, "mine", &stored);
    // Damaged credentials, and its metadata names a different login.
    let other = paths.profiles_dir().join("someone-else");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("auth.json"), "{ truncated").unwrap();
    std::fs::write(
        other.join("meta.json"),
        r#"{"alias":"someone-else","email":null,"plan":null,"user_id":"seatZ","saved_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let rotated = synthetic_token(r#"{"sub":"seatA","jti":"rotated"}"#);
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{rotated}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "mine").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("mine").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "an unrelated damaged profile suppressed a valid rotation"
    );
}

/// `chatgpt_user_id` and `sub` are different namespaces. A profile identified
/// by one must not match a token carrying the other, or a foreign credential
/// could be attributed to it by a coincidental string match.
#[test]
fn login_identity_does_not_match_across_claim_namespaces() {
    let (_tmp, paths) = setup_test_env();
    // Stored: identified by its `chatgpt_user_id`.
    write_profile(
        &paths,
        "mine",
        &synthetic_token(
            r#"{"sub":"seat-1","jti":"stored","https://api.openai.com/auth":{"chatgpt_user_id":"shared-value"}}"#,
        ),
    );

    // Arriving: no `chatgpt_user_id`, and a subject that happens to read the same.
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(
        &exec_auth,
        format!(
            r#"{{"access_token":"{}","refresh_token":"rotated"}}"#,
            synthetic_token(r#"{"sub":"shared-value","jti":"live"}"#)
        ),
    )
    .unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "mine").unwrap();

    assert!(
        !std::fs::read_to_string(paths.profiles_dir().join("mine").join("auth.json"))
            .unwrap()
            .contains("rotated"),
        "a subject matched a user id and carried a foreign credential in"
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

/// A conflict may be proven by either half of the identity, so the value in an
/// operator message is sometimes a workspace and sometimes a login. Announcing
/// both as "workspace" mislabels the evidence exactly where it is used to
/// decide.
#[test]
fn a_claim_is_described_as_what_it_is() {
    assert_eq!(profile::describe_claim("uid:user-a"), "login user-a");
    assert_eq!(profile::describe_claim("sub:seatA"), "login seatA");
    let workspace = profile::describe_claim("acct-team");
    assert!(workspace.starts_with("workspace "), "{workspace}");
    assert!(!workspace.contains("login"), "{workspace}");
}

/// The ordinary legacy-to-current token transition: one seat keeps its subject
/// and gains a `chatgpt_user_id`. Identifying a token by a single preferred
/// claim makes the before and after look like two different people, so the
/// profile stops absorbing its own rotations and its saved token ages out.
#[test]
fn capture_absorbs_a_rotation_that_adds_a_user_id_claim() {
    let (_tmp, paths) = setup_test_env();
    // Saved before `chatgpt_user_id` existed: a subject and a workspace.
    let stored = synthetic_token(
        r#"{"sub":"seatA","jti":"stored","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team"}}"#,
    );
    write_profile(&paths, "mine", &stored);

    // The same seat, refreshed: same subject and workspace, now also a user id.
    let rotated = synthetic_token(
        r#"{"sub":"seatA","jti":"rotated","https://api.openai.com/auth":{"chatgpt_account_id":"acct-team","chatgpt_user_id":"user-a"}}"#,
    );
    let exec_auth = paths.home.join("exec-auth.json");
    std::fs::write(&exec_auth, format!(r#"{{"access_token":"{rotated}"}}"#)).unwrap();

    profile::capture_exec_auth_from(&paths, &exec_auth, "mine").unwrap();

    assert!(
        std::fs::read_to_string(paths.profiles_dir().join("mine").join("auth.json"))
            .unwrap()
            .contains(&rotated),
        "the profile refused its own refreshed token, so its saved copy will expire"
    );
}

/// The guard the rule above must not weaken: two claims in different namespaces
/// are unrelated facts, so a `uid` must never match a `sub` that happens to
/// carry the same string.
#[test]
fn a_user_id_never_matches_a_subject_by_coincidence() {
    let shared = "collision";
    let uid_only = codexctl::api::Logins {
        uid: Some(shared.to_string()),
        sub: None,
    };
    let sub_only = codexctl::api::Logins {
        uid: None,
        sub: Some(shared.to_string()),
    };
    assert!(!uid_only.same(&sub_only), "a uid matched a sub");
    assert!(
        !uid_only.differs(&sub_only),
        "unrelated claims proved a difference"
    );
    assert!(!uid_only.comparable(&sub_only));

    // Within one namespace both verdicts are available as usual.
    let other_uid = codexctl::api::Logins {
        uid: Some("someone-else".to_string()),
        sub: None,
    };
    assert!(uid_only.differs(&other_uid));
    assert!(uid_only.same(&uid_only.clone()));
}
