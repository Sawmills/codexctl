use super::*;
pub(in crate::central) fn promote(
    state: &Path,
    key: &Path,
    record: &mut Record,
    auth: &Value,
) -> Result<()> {
    let mut saved = vault::load(state, key)?;
    if saved.user != record.user || saved.alias != record.alias {
        bail!("re-login ownership changed");
    }
    let identity = Owner {
        vault: saved,
        rpc: None,
        home: state.join("runtime"),
        state: state.into(),
        key: key.into(),
        available: false,
        retryable_unavailable: false,
        retry_started: None,
        retry_failures: 0,
        routing_refused: false,
        refresh_enabled: true,
        limits: None,
        limits_observed: None,
        verification_input: None,
    };
    identity.validate_owned_auth(auth)?;
    saved = identity.vault;
    let old = vault::digest(&serde_json::to_vec(&saved.auth)?);
    let new = vault::digest(&serde_json::to_vec(auth)?);
    if record.phase == Phase::Committing && record.candidate_revision.as_ref() != Some(&new) {
        bail!("re-login candidate changed after commit intent");
    }
    if old != record.original_revision && old != new {
        bail!("credentials changed during re-login");
    }
    record.candidate = Some(auth.clone());
    record.candidate_revision = Some(new);
    record.phase = Phase::Committing;
    record.code = None;
    save(state, record)?;
    store::atomic_write(&state.join("runtime/auth.json"), &serde_json::to_vec(auth)?)?;
    saved.auth = auth.clone();
    saved.verified = false;
    saved.import_rejected = false;
    vault::save(state, key, &saved)?;
    record.phase = Phase::Promoted;
    save(state, record)
}
pub(in crate::central) fn needs_verification(state: &Path) -> Result<bool> {
    Ok(current(state)?.is_some_and(|r| matches!(r.phase, Phase::Promoted | Phase::Retiring)))
}
pub(in crate::central) async fn verify_replacement(
    owner: &mut Owner,
    binary: &Path,
    lock: &tokio::sync::MutexGuard<'_, ()>,
) -> Result<()> {
    if !owner.refresh_enabled {
        bail!("read-only owner cannot verify a replacement");
    }
    let result = verify_inner(owner, binary, lock).await;
    if let Err(error) = result {
        owner.fence(false);
        let reconciled = finish_rejection(owner).await;
        let mut record = current(&owner.state)?.context("missing login operation")?;
        record.error.get_or_insert_with(|| "relogin_failed".into());
        save(&owner.state, &record)?;
        reconciled?;
        return Err(error);
    }
    Ok(())
}
async fn verify_inner(
    owner: &mut Owner,
    binary: &Path,
    lock: &tokio::sync::MutexGuard<'_, ()>,
) -> Result<()> {
    check_claim(
        owner.state.parent().context("missing registry")?,
        &owner.key,
        &owner.state,
        &owner.vault.auth,
    )?;
    // Validate and reconcile the selected journal BEFORE a refresh process can
    // read it. A vault-only identity check does not clear the selected journal.
    owner.snapshot()?;
    // A previous attempt may have proved authentication but failed routing or
    // billing. Establish a fresh verification baseline, including permanent
    // rejection evidence, for the retained (possibly rotated) grant.
    owner.vault.verified = false;
    owner.vault.import_rejected = false;
    owner.verification_input = Some(owner.vault.auth.clone());
    vault::save(&owner.state, &owner.key, &owner.vault)?;
    if owner.rpc.is_none() {
        // Clearance must read the previous verifier's exit evidence before the
        // new intent replaces its recorded account-server incarnation.
        let proof = identity_inventory(&owner.state, &owner.key, &owner.home).clear_for_launch(
            owner,
            AdmissionKind::Renewal,
            lock,
        )?;
        let mut record = current(&owner.state)?.context("missing re-login commit")?;
        // Persist BEFORE launch_owner invalidates runtime/pid and spawn-failed.
        // Production spawns on the broker's long-lived main thread; isolate()
        // gives this verifier the same Linux parent-death contract as login.
        record.verifier_broker = Some(process::Process::capture(std::process::id())?);
        save(&owner.state, &record)?;
        launch_owner(owner, binary, proof).await?;
    }
    let revision = owner.snapshot()?.revision;
    owner
        .tokens(TokenRequest {
            previous_revision: Some(revision),
            billing: true,
            ..Default::default()
        })
        .await
        .map_err(|_| anyhow::anyhow!("replacement verification failed"))?;
    if !owner.vault.verified {
        bail!("replacement not verified");
    }
    let mut record = current(&owner.state)?.context("missing re-login commit")?;
    record.phase = Phase::Retiring;
    record.error = None;
    save(&owner.state, &record)?;
    retire_reservations(
        owner.state.parent().context("missing accounts directory")?,
        &owner.vault.auth,
        &directory(&owner.state, &record.id)?,
    )?;
    record.phase = Phase::Completed;
    record.error = None;
    save(&owner.state, &record)?;
    Ok(())
}
pub(in crate::central) fn retire_reservations(
    accounts: &Path,
    auth: &Value,
    skip: &Path,
) -> Result<()> {
    for entry in std::fs::read_dir(accounts)? {
        let state = entry?.path();
        for mut record in records(&state)? {
            if directory(&state, &record.id)? == skip {
                continue;
            }
            if record.phase == Phase::Completed || record.retired {
                continue;
            }
            if reservation(&state, &record)?.is_some_and(|a| overlaps(&a, auth)) {
                stopped(&state, &record)?;
                record.retired = true;
                if matches!(record.phase, Phase::Starting | Phase::Pending) {
                    record.phase = Phase::Failed;
                    record.code = None;
                    record.error = Some("superseded_by_owner_repair".into());
                }
                save(&state, &record)?;
            }
        }
    }
    Ok(())
}
#[derive(Default)]
pub(in crate::central) struct Recovery {
    pub blocked: bool,
    pub verify: bool,
}
pub(in crate::central) fn recover(state: &Path, key: &Path) -> Result<Recovery> {
    // An unknown login child can hold any account and needs a global fence.
    // The verifier only receives an identity-checked grant: failure to prove its
    // exit fences that account inside recover_inner, not unrelated accounts.
    let records = records(state)?;
    for record in &records {
        if !record.retired && record.phase != Phase::Completed {
            stopped(state, record)?;
        }
    }
    match recover_inner(state, key) {
        Ok(result) => Ok(result),
        Err(_) => Ok(Recovery {
            blocked: true,
            verify: false,
        }),
    }
}
fn recover_inner(state: &Path, key: &Path) -> Result<Recovery> {
    if local_corruption(state)? {
        return Ok(Recovery {
            blocked: true,
            ..Default::default()
        });
    }
    let latest = current(state)?;
    let mut result = Recovery {
        blocked: latest.as_ref().is_some_and(|r| {
            !matches!(
                r.phase,
                Phase::Completed | Phase::Promoted | Phase::Retiring
            )
        }),
        ..Default::default()
    };
    for record in records(state)? {
        let mut record = load(state, &record.id)?;
        if record.retired || record.phase == Phase::Completed {
            continue;
        }
        stopped(state, &record)?;
        let auth = candidate(state, &record);
        let is_latest = latest.as_ref().is_some_and(|r| r.id == record.id);
        if is_latest && record.phase == Phase::Retiring {
            previous_owner_exited(&state.join("runtime"))?;
            if finish_verified(state, key, &mut record)? {
                continue;
            }
        }
        if is_latest
            && matches!(
                record.phase,
                Phase::Starting | Phase::Pending | Phase::Committing
            )
        {
            previous_owner_exited(&state.join("runtime"))?;
            if let Ok(Some(auth)) = &auth {
                if check_claim(
                    state.parent().context("missing registry")?,
                    key,
                    state,
                    auth,
                )
                .is_ok()
                    && promote(state, key, &mut record, auth).is_ok()
                {
                    result.verify = true;
                    result.blocked = false;
                    continue;
                }
                // A failed effect may have changed the in-memory phase before
                // its atomic write failed. Only durable state permits verification.
                record = load(state, &record.id)?;
            }
        }
        if is_latest && matches!(record.phase, Phase::Promoted | Phase::Retiring) {
            previous_owner_exited(&state.join("runtime"))?;
            let saved = vault::load(state, key)?;
            if rejected(&saved) {
                record.phase = Phase::Failed;
                record.error = Some("login_rejected_retry".into());
                save(state, &record)?;
            } else {
                check_claim(
                    state.parent().context("missing registry")?,
                    key,
                    state,
                    &saved.auth,
                )?;
                result.verify = true;
                continue;
            }
        }
        if matches!(record.phase, Phase::Starting | Phase::Pending) {
            record.phase = Phase::Failed;
            record.code = None;
            record.error = Some("login_interrupted_retry".into());
            save(state, &record)?;
        }
        if is_latest {
            result.blocked = true;
        }
        if let Ok(Some(auth)) = auth {
            record.candidate = Some(auth.clone());
            save(state, &record)?;
        }
    }
    Ok(result)
}
fn finish_verified(state: &Path, key: &Path, record: &mut Record) -> Result<bool> {
    let mut saved = vault::load(state, key)?;
    if !saved.verified {
        return Ok(false);
    }
    if saved.user != record.user || saved.alias != record.alias {
        bail!("re-login ownership changed");
    }
    let grant = candidate(state, record)?.context("missing committed candidate")?;
    if record.candidate_revision.as_ref() != Some(&vault::digest(&serde_json::to_vec(&grant)?)) {
        bail!("committed candidate changed");
    }
    let verified = saved.auth.clone();
    saved.auth = grant;
    let baseline = Owner {
        vault: saved,
        rpc: None,
        home: state.join("runtime"),
        state: state.into(),
        key: key.into(),
        available: false,
        retryable_unavailable: false,
        retry_started: None,
        retry_failures: 0,
        routing_refused: false,
        refresh_enabled: false,
        limits: None,
        limits_observed: None,
        verification_input: None,
    };
    baseline.validate_owned_auth(&verified)?;
    retire_reservations(
        state.parent().context("missing account registry")?,
        &verified,
        &directory(state, &record.id)?,
    )?;
    record.phase = Phase::Completed;
    record.error = None;
    save(state, record)?;
    Ok(true)
}

// The import lock protects this disk inventory in live mutation paths. Startup
// holds the exclusive broker lock and completes inventory before any launch.
pub(super) fn check_claim(
    accounts: &Path,
    key: &Path,
    selected: &Path,
    auth: &Value,
) -> Result<()> {
    inventory::clear_registry(accounts, key, selected, auth, AdmissionKind::Renewal).map(|_| ())
}

fn rejected(saved: &Vault) -> bool {
    // Owner::snapshot only records this proof for the unchanged, validated
    // verification input. That input can be a rotation of the original candidate.
    !saved.verified && saved.import_rejected
}
pub(super) async fn finish_rejection(owner: &mut Owner) -> Result<bool> {
    let mut record = current(&owner.state)?.context("missing operation")?;
    if !record.phase.commit_started() || !rejected(&owner.vault) {
        return Ok(false);
    }
    if let Some(rpc) = owner.rpc.as_mut() {
        rpc.settle_and_stop().await?;
    } else {
        previous_owner_exited(&owner.home)?;
    }
    owner.snapshot()?;
    owner.rpc = None;
    if !rejected(&owner.vault) {
        return Ok(false);
    }
    record.phase = Phase::Failed;
    record.code = None;
    record.error = Some("login_rejected_retry".into());
    save(&owner.state, &record)?;
    Ok(true)
}

/// Only a current verifier intent can extend the ordinary runtime PID evidence.
/// Never infer this from the older device-login broker incarnation.
pub(in crate::central) fn verifier_parent_exited(state: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    if let Some(record) = current(state)?
        && record.phase.commit_started()
        && let Some(broker) = record.verifier_broker
    {
        return Ok(!broker.alive()?);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = state;
    Ok(false)
}
