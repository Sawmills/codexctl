use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
fn auth(uid: Option<&str>, generation: u64) -> Value {
    let claims = json!({"sub":"login","iat":2000000000+generation,"exp":4102444800_u64,"generation":generation,"https://api.openai.com/auth":{"chatgpt_account_id":"seat","chatgpt_user_id":uid,"chatgpt_plan_type":"pro"}});
    json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":format!("synthetic-{generation}"),"account_id":"seat"}})
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Record) {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    vault::create_secret(&key, &[7; 32]).unwrap();
    let state = root.path().join("accounts/account");
    let original = auth(Some("known-login"), 0);
    vault::save(
        &state,
        &key,
        &Vault {
            auth: original.clone(),
            alias: "personal".into(),
            user: "amir".into(),
            tenant: "sawmills".into(),
            label: None,
            verified: true,
            import_rejected: false,
            revision: 0,
        },
    )
    .unwrap();
    store::ensure_private_dir(&state.join("runtime")).unwrap();
    store::atomic_write(
        &state.join("runtime/auth.json"),
        &serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    store::atomic_write(&state.join("runtime/spawn-failed"), b"not-started").unwrap();
    let record = Record {
        sequence: 1,
        device: "laptop".into(),
        broker: process::Process::capture(std::process::id()).unwrap(),
        child: Child::NotStarted,
        verifier_broker: None,
        candidate: None,
        id: "f".repeat(64),
        user: "amir".into(),
        alias: "personal".into(),
        original_revision: vault::digest(&serde_json::to_vec(&original).unwrap()),
        candidate_revision: None,
        phase: Phase::Pending,
        code: None,
        error: None,
        retired: false,
    };
    let home = directory(&state, &record.id).unwrap().join("home");
    store::ensure_private_dir(&home).unwrap();
    store::atomic_write(&home.join("spawn-failed"), b"not-started").unwrap();
    save(&state, &record).unwrap();
    (root, state, key, record)
}
#[test]
fn recovery_finishes_a_journal_write_even_when_explicit_login_has_the_same_timestamp() {
    let (_root, state, key, mut record) = fixture();
    let mut fresh = auth(Some("known-login"), 0);
    fresh["tokens"]["refresh_token"] = json!("new-explicit-grant");
    let before = std::fs::read(state.join("vault.enc")).unwrap();
    record.candidate = Some(fresh.clone());
    record.phase = Phase::Committing;
    record.candidate_revision = Some(vault::digest(&serde_json::to_vec(&fresh).unwrap()));
    save(&state, &record).unwrap();
    store::atomic_write(
        &state.join("runtime/auth.json"),
        &serde_json::to_vec(&fresh).unwrap(),
    )
    .unwrap();
    assert_eq!(std::fs::read(state.join("vault.enc")).unwrap(), before);
    let recovery = recover(&state, &key).unwrap();
    assert!(recovery.verify && !recovery.blocked);
    assert_eq!(vault::load(&state, &key).unwrap().auth, fresh);
    assert_eq!(current(&state).unwrap().unwrap().phase, Phase::Promoted);
}
#[test]
fn recovery_retains_uid_loss_and_uid_conflict_without_promoting_either() {
    for uid in [None, Some("different-login")] {
        let (_root, state, key, record) = fixture();
        let before = std::fs::read(state.join("vault.enc")).unwrap();
        let unexpected = auth(uid, 2);
        store::atomic_write(
            &directory(&state, &record.id)
                .unwrap()
                .join("home/auth.json"),
            &serde_json::to_vec(&unexpected).unwrap(),
        )
        .unwrap();
        let recovery = recover(&state, &key).unwrap();
        assert!(recovery.blocked && !recovery.verify);
        assert_eq!(
            identity_inventory(&state, &key, &state.join("runtime"))
                .candidates
                .unwrap()
                .into_iter()
                .map(|c| c.auth)
                .collect::<Vec<_>>(),
            vec![unexpected]
        );
        assert_eq!(std::fs::read(state.join("vault.enc")).unwrap(), before);
    }
}
#[test]
fn recovery_refuses_a_live_login_child_before_promotion() {
    let (_root, state, key, mut record) = fixture();
    record.child = Child::Running(process::Process::capture(std::process::id()).unwrap());
    save(&state, &record).unwrap();
    assert!(recover(&state, &key).is_err());
}
#[test]
fn incomplete_native_credentials_keep_only_the_target_unavailable_after_proven_exit() {
    let (_root, state, key, record) = fixture();
    store::atomic_write(
        &directory(&state, &record.id)
            .unwrap()
            .join("home/auth.json"),
        b"{partial",
    )
    .unwrap();
    let recovery = recover(&state, &key).unwrap();
    assert!(
        recovery.blocked
            && !recovery.verify
            && identity_inventory(&state, &key, &state.join("runtime"))
                .candidates
                .unwrap()
                .is_empty()
    );
}
#[test]
fn unpublished_preparation_is_ignored_and_a_stopped_interruption_becomes_terminal() {
    let (_root, state, key, record) = fixture();
    let prepared = tempfile::Builder::new()
        .prefix(".pending-login-")
        .tempdir_in(&state)
        .unwrap();
    store::ensure_private_dir(&prepared.path().join("home")).unwrap();
    let recovery = recover(&state, &key).unwrap();
    assert!(recovery.blocked);
    assert_eq!(load(&state, &record.id).unwrap().phase, Phase::Failed);
    assert_eq!(
        load(&state, &record.id).unwrap().error.as_deref(),
        Some("login_interrupted_retry")
    );
}
#[test]
fn pinned_prompt_parsing_refuses_control_sequences_and_other_origins() {
    let prompt = format!(
        "1. Open this link in your browser and sign in to your account\n   {URL}\n\n2. Enter this one-time code (expires in 15 minutes)\n   \u{1b}[32mTEST-123\u{1b}[0m\n"
    );
    assert_eq!(
        challenge(prompt.as_bytes()).unwrap().as_deref(),
        Some("TEST-123")
    );
    assert!(
        challenge(
            prompt
                .replace(URL, "https://example.com/codex/device")
                .as_bytes()
        )
        .unwrap()
        .is_none()
    );
    assert!(challenge(prompt.replace("TEST-123", "\u{1b}]52;copy").as_bytes()).is_err());
}

#[derive(Clone, Copy)]
enum CrashPoint {
    Unpublished,
    Prepared,
    Spawning,
    Live,
    Partial,
    Saved,
    Wrong,
    BeforeJournal,
    AfterJournal,
    AfterVault,
    Promoted,
    Rejected,
    Retiring,
    Completed,
    Failed,
    Corrupt,
    ReadOnly,
}
fn recovery_row(point: CrashPoint) {
    let (_root, state, key, mut record) = fixture();
    let dir = directory(&state, &record.id).unwrap();
    let fresh = auth(Some("known-login"), 1);
    let mut expected = Phase::Failed;
    let mut blocked = true;
    let mut verify = false;
    match point {
        CrashPoint::Unpublished => {
            std::fs::rename(&dir, state.join(".pending-login-unpublished")).unwrap();
            assert!(!recover(&state, &key).unwrap().blocked);
            assert!(current(&state).unwrap().is_none());
            return;
        }
        CrashPoint::Prepared => record.phase = Phase::Starting,
        CrashPoint::Spawning => {
            // The real parent process has exited; no guessed PID evidence.
            let mut parent = std::process::Command::new("sleep")
                .arg("10")
                .spawn()
                .unwrap();
            record.broker = process::Process::capture(parent.id()).unwrap();
            parent.kill().unwrap();
            parent.wait().unwrap();
            record.child = Child::Spawning;
            std::fs::remove_file(dir.join("home/spawn-failed")).unwrap();
            save(&state, &record).unwrap();
            if !cfg!(target_os = "linux") {
                assert!(recover(&state, &key).is_err());
                return;
            }
        }
        CrashPoint::Live => {
            record.child = Child::Running(process::Process::capture(std::process::id()).unwrap());
            save(&state, &record).unwrap();
            assert!(recover(&state, &key).is_err());
            return;
        }
        CrashPoint::Partial => {
            store::atomic_write(&dir.join("home/auth.json"), b"{partial").unwrap();
        }
        CrashPoint::Saved | CrashPoint::Wrong => {
            let grant = if matches!(point, CrashPoint::Wrong) {
                auth(Some("wrong-login"), 1)
            } else {
                fresh.clone()
            };
            store::atomic_write(
                &dir.join("home/auth.json"),
                &serde_json::to_vec(&grant).unwrap(),
            )
            .unwrap();
            if matches!(point, CrashPoint::Saved) {
                expected = Phase::Promoted;
                blocked = false;
                verify = true;
            }
        }
        CrashPoint::BeforeJournal | CrashPoint::AfterJournal | CrashPoint::AfterVault => {
            record.phase = Phase::Committing;
            record.candidate = Some(fresh.clone());
            record.candidate_revision = Some(vault::digest(&serde_json::to_vec(&fresh).unwrap()));
            if !matches!(point, CrashPoint::BeforeJournal) {
                store::atomic_write(
                    &state.join("runtime/auth.json"),
                    &serde_json::to_vec(&fresh).unwrap(),
                )
                .unwrap();
            }
            if matches!(point, CrashPoint::AfterVault) {
                let mut saved = vault::load(&state, &key).unwrap();
                saved.auth = fresh.clone();
                saved.verified = false;
                vault::save(&state, &key, &saved).unwrap();
            }
            expected = Phase::Promoted;
            blocked = false;
            verify = true;
        }
        CrashPoint::Promoted
        | CrashPoint::Rejected
        | CrashPoint::Retiring
        | CrashPoint::Completed
        | CrashPoint::ReadOnly => {
            promote(&state, &key, &mut record, &fresh).unwrap();
            let mut saved = vault::load(&state, &key).unwrap();
            if matches!(point, CrashPoint::Rejected) {
                saved.import_rejected = true;
            }
            if matches!(point, CrashPoint::Retiring | CrashPoint::Completed) {
                saved.verified = true;
                record.phase = if matches!(point, CrashPoint::Retiring) {
                    Phase::Retiring
                } else {
                    Phase::Completed
                };
            }
            vault::save(&state, &key, &saved).unwrap();
            if matches!(point, CrashPoint::Rejected) {
                expected = Phase::Failed;
            } else if matches!(point, CrashPoint::Retiring | CrashPoint::Completed) {
                expected = Phase::Completed;
                blocked = false;
            } else {
                expected = Phase::Promoted;
                blocked = false;
                verify = true;
            }
        }
        CrashPoint::Failed => {
            record.phase = Phase::Failed;
        }
        CrashPoint::Corrupt => {
            store::atomic_write(&dir.join("record.json"), b"{partial").unwrap();
            let recovery = recover(&state, &key).unwrap();
            assert!(recovery.blocked && !recovery.verify);
            return;
        }
    }
    save(&state, &record).unwrap();
    let result = recover(&state, &key).unwrap();
    assert_eq!(result.blocked, blocked);
    assert_eq!(result.verify, verify);
    assert_eq!(current(&state).unwrap().unwrap().phase, expected);
}
macro_rules! recovery_table {
    ($($name:ident => $point:ident),* $(,)?) => { $(
        #[test] fn $name() { recovery_row(CrashPoint::$point); }
    )* };
}
recovery_table! {
    hq_table_unpublished => Unpublished,
    hq_table_prepared => Prepared,
    hq_table_spawning => Spawning,
    hq_table_live => Live,
    hq_table_partial => Partial,
    hq_table_saved => Saved,
    hq_table_wrong => Wrong,
    hq_table_before_journal => BeforeJournal,
    hq_table_after_journal => AfterJournal,
    hq_table_after_vault => AfterVault,
    hq_table_promoted => Promoted,
    hq_table_rejected => Rejected,
    hq_table_retiring => Retiring,
    hq_table_completed => Completed,
    hq_table_failed => Failed,
    hq_table_corrupt => Corrupt,
    hq_table_read_only => ReadOnly,
}

#[test]
fn promoted_verifier_without_exit_proof_fences_only_its_account() {
    for verifier_intent in [false, true] {
        let (_root, state, key, mut record) = fixture();
        promote(&state, &key, &mut record, &auth(Some("known-login"), 1)).unwrap();
        if verifier_intent {
            // A still-live verifier broker never supplies Linux exit proof.
            record.verifier_broker = Some(process::Process::capture(std::process::id()).unwrap());
            save(&state, &record).unwrap();
        }
        std::fs::remove_file(state.join("runtime/spawn-failed")).unwrap();
        let recovery = recover(&state, &key).expect("verifier uncertainty must be account scoped");
        assert!(recovery.blocked && !recovery.verify);
        assert!(previous_owner_exited(&state.join("runtime")).is_err());
    }
}

#[tokio::test]
async fn hq6_queued_inventory_never_observes_a_transient_spawning_child() {
    let (_root, state, _key, mut record) = fixture();
    let home = directory(&state, &record.id).unwrap().join("home");
    let imports = Mutex::new(());
    let held = imports.lock().await;
    let mut command = tokio::process::Command::new("/bin/sleep");
    command.arg("30").kill_on_drop(true);
    process::isolate(&mut command);
    let mut spawn = Box::pin(spawn_login(
        &imports,
        &state,
        &home,
        &mut record,
        &mut command,
    ));
    assert!(futures::poll!(&mut spawn).is_pending());
    let mut observe = Box::pin(async {
        let _lock = imports.lock().await;
        current(&state).unwrap().unwrap().child
    });
    assert!(futures::poll!(&mut observe).is_pending());
    drop(held);
    let (child, observed) = tokio::join!(spawn, observe);
    let mut child = child.unwrap();
    child.start_kill().unwrap();
    child.wait().await.unwrap();
    assert!(
        matches!(observed, Child::Running(_)),
        "migration saw transient spawn intent without PID evidence"
    );
}
