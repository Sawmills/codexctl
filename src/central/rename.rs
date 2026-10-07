//! Server-side rename of one company user's account alias.
//!
//! The alias names the account directory through `account_key`, so a rename
//! moves the account under an intent journal and keeps its credentials, label
//! and refresh owner. A tombstone answers the old alias with the new one, so a
//! stale client fails clearly instead of reaching another account.
use super::{
    managed::{
        Broker, HttpError, account_key, launch_owner, normalize_alias, previous_owner_exited,
    },
    relogin, vault,
};
use crate::store;
use anyhow::{Context, Result, bail};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

const INTENT: &str = "rename.json";
const TOMBSTONES: &str = "renames.json";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RenameRequest {
    alias: String,
    new_alias: String,
    /// The workspace the client expects under `alias`; binds a retry to the
    /// account it meant, never to a later account that took the old name.
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Rename {
    user: String,
    from: String,
    to: String,
}

fn tombstones(state: &Path) -> Result<Vec<Rename>> {
    let path = state.join(TOMBSTONES);
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&vault::private_read(&path)?)?)
}

/// The current alias for a renamed one, following any chain of renames.
pub(super) fn renamed_to(state: &Path, user: &str, alias: &str) -> Result<Option<String>> {
    let renames = tombstones(state)?;
    let mut current = alias.to_ascii_lowercase();
    let mut found = None;
    for _ in 0..=renames.len() {
        let Some(next) = renames
            .iter()
            .find(|r| r.user == user && r.from.eq_ignore_ascii_case(&current))
        else {
            break;
        };
        current = next.to.to_ascii_lowercase();
        found = Some(next.to.clone());
    }
    Ok(found)
}

fn record_tombstone(state: &Path, rename: &Rename) -> Result<()> {
    let mut renames = tombstones(state)?;
    // A tombstoned alias is never reused (add and rename refuse it), so only a
    // repeat of this same rename replaces an entry.
    renames.retain(|r| !(r.user == rename.user && r.from.eq_ignore_ascii_case(&rename.from)));
    if !rename.from.eq_ignore_ascii_case(&rename.to) {
        renames.push(rename.clone());
    }
    store::atomic_write(&state.join(TOMBSTONES), &serde_json::to_vec(&renames)?)
}

/// Finish a rename whose intent is recorded in `directory`. Idempotent: the
/// vault alias, the directory move and the tombstone each tolerate a repeat.
pub(super) fn finish(state: &Path, key: &Path, directory: &Path) -> Result<PathBuf> {
    let intent: Rename = serde_json::from_slice(&vault::private_read(&directory.join(INTENT))?)?;
    let mut saved = vault::load(directory, key)?;
    if saved.user != intent.user
        || !(saved.alias.eq_ignore_ascii_case(&intent.from) || saved.alias == intent.to)
    {
        bail!("rename intent does not match its account");
    }
    if saved.alias != intent.to {
        saved.alias = intent.to.clone();
        vault::save(directory, key, &saved)?;
    }
    let accounts = state.join("accounts");
    let target = accounts.join(account_key(&intent.user, &intent.to));
    if target != directory {
        if target.try_exists()? {
            bail!("rename target already exists");
        }
        std::fs::rename(directory, &target)?;
        store::sync_directory(&accounts)?;
    }
    record_tombstone(state, &intent)?;
    std::fs::remove_file(target.join(INTENT))?;
    store::sync_directory(&target)?;
    Ok(target)
}

/// True when an interrupted rename waits for file-mode startup to finish it.
pub(super) fn pending(state: &Path) -> Result<bool> {
    let accounts = state.join("accounts");
    if !accounts.try_exists()? {
        return Ok(false);
    }
    for entry in std::fs::read_dir(&accounts)? {
        if entry?.path().join(INTENT).try_exists()? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Startup: complete every interrupted rename before the account inventory.
pub(super) fn recover(state: &Path, key: &Path) -> Result<()> {
    let accounts = state.join("accounts");
    if !accounts.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(&accounts)? {
        let directory = entry?.path();
        if directory.join(INTENT).try_exists()? {
            finish(state, key, &directory)?;
        }
    }
    Ok(())
}

pub(super) async fn rename(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<RenameRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    if let Some(error) = broker.reject_unshared_workflow("account_rename_unavailable") {
        return Err(error);
    }
    let device = broker.authorize(&headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let alias = normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
        .to_owned();
    let new_alias = normalize_alias(&request.new_alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
        .to_owned();
    let guard = broker.imports.lock().await;
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::CONFLICT, "account_rename_unavailable"));
    }
    let expected = request.account_id.as_deref().filter(|id| !id.is_empty());
    let Some((old_key, owner)) = broker.resolve_alias(&device.user, &alias).await? else {
        // A retry after a lost response: the rename already committed.
        let committed = renamed_to(&broker.state, &device.user, &alias)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        if let Some(current) = committed.filter(|to| to.eq_ignore_ascii_case(&new_alias))
            && let Some((_, renamed)) = broker.resolve_alias(&device.user, &current).await?
        {
            let renamed = renamed.lock().await;
            let summary = super::managed::account_summary(&renamed);
            if expected.is_none_or(|id| id == summary.account_id) {
                return Ok(Json(summary).into_response());
            }
        }
        return Err(renamed_error(&broker, &device.user, &alias)
            .unwrap_or_else(|| broker.error(StatusCode::NOT_FOUND, "account_not_found")));
    };
    // An active loan names this alias (ADR 0004); the lender ends it first.
    // `lend` creates grants under the same import lock, so none can start
    // between this check and the rename.
    let lent = broker
        .loan_store()
        .has_active_loan(&old_key, chrono::Utc::now().timestamp())
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    if lent {
        return Err(broker.error(StatusCode::CONFLICT, "rename_blocked_by_loan"));
    }
    if !new_alias.eq_ignore_ascii_case(&alias) {
        if broker
            .resolve_alias(&device.user, &new_alias)
            .await?
            .is_some()
        {
            return Err(broker.error(StatusCode::CONFLICT, "alias_exists"));
        }
        // A stale client still names a renamed alias; reusing it would let
        // that client reach a different account.
        if renamed_to(&broker.state, &device.user, &new_alias)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
            .is_some()
        {
            return Err(broker.error(StatusCode::CONFLICT, "alias_renamed"));
        }
    }
    let failed =
        |_: anyhow::Error| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed");
    let mut held = owner.lock().await;
    if expected.is_some_and(|id| vault::account(&held.vault.auth).ok().as_deref() != Some(id)) {
        return Err(broker.error(StatusCode::CONFLICT, "account_changed"));
    }
    let pending_add = [&alias, &new_alias]
        .into_iter()
        .try_fold(false, |pending, name| {
            relogin::add::pending_for(&broker.state, &device.user, name).map(|p| pending || p)
        });
    if held.import_settling
        || relogin::renewal_pending(&held.state).map_err(failed)?
        || pending_add.map_err(failed)?
    {
        return Err(broker.error(StatusCode::CONFLICT, "login_pending"));
    }
    let new_key = account_key(&device.user, &new_alias);
    if new_key != old_key
        && broker
            .state
            .join("accounts")
            .join(&new_key)
            .try_exists()
            .map_err(|e| failed(e.into()))?
    {
        return Err(broker.error(StatusCode::CONFLICT, "alias_exists"));
    }
    // Stop the refresh owner before its directory moves. Fence only after the
    // stop is proven, so a failed stop leaves the account serving as before.
    if let Some(rpc) = held.rpc.as_mut() {
        rpc.settle_and_stop()
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
    } else {
        previous_owner_exited(&held.home)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
    }
    held.rpc = None;
    held.fence(false);
    held.snapshot().map_err(failed)?;
    let intent = Rename {
        user: device.user.clone(),
        from: held.vault.alias.clone(),
        to: new_alias.clone(),
    };
    store::atomic_write(
        &held.state.join(INTENT),
        &serde_json::to_vec(&intent).map_err(|_| failed(anyhow::anyhow!("encode")))?,
    )
    .map_err(failed)?;
    let target = finish(&broker.state, &broker.key, &held.state).map_err(|_| {
        broker.ownership_unresolved.store(true, Ordering::Release);
        broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
    })?;
    held.vault = vault::load(&target, &broker.key).map_err(failed)?;
    held.state = target.clone();
    held.home = target.join("runtime");
    {
        let mut owners = broker.owners.write().await;
        let entry = owners
            .remove(&old_key)
            .context("renamed owner missing")
            .map_err(failed)?;
        let mut identity = entry.0;
        identity.alias = new_alias.trim().into();
        owners.insert(new_key, (identity, entry.1));
    }
    // Restart refresh through the restore clearance; any evidence against it
    // keeps the owner fenced rather than serving it unverified.
    let relaunched = async {
        let proof = relogin::identity_inventory(&held.state, &broker.key, &held.home)
            .clear_for_launch(&held, relogin::AdmissionKind::Restore, &guard)?;
        launch_owner(&mut held, &broker.binary, proof).await
    }
    .await;
    eprintln!(
        "ACCOUNT_RENAMED company_user={} device={} from={old_key} to={}",
        device.user,
        device.id,
        account_key(&device.user, &new_alias)
    );
    match relaunched {
        Ok(()) => held.available = true,
        Err(_) => {
            // The rename committed; report the owner state instead of hiding it.
            eprintln!(
                "ACCOUNT_RENAME_RELAUNCH_FAILED company_user={} to={}",
                device.user,
                account_key(&device.user, &new_alias)
            );
            broker.record_failure(
                "rename_relaunch_failed",
                "rename",
                StatusCode::SERVICE_UNAVAILABLE,
            );
        }
    }
    Ok(Json(super::managed::account_summary(&held)).into_response())
}

/// This user's renamed aliases, so a machine can drop its stale records.
pub(super) async fn renamed_aliases(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let renames = tombstones(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    let aliases: Vec<String> = renames
        .into_iter()
        .filter(|r| r.user == device.user)
        .map(|r| r.from)
        .collect();
    Ok(Json(aliases).into_response())
}

/// 404 for an alias that was renamed, naming the alias that replaced it.
pub(super) fn renamed_error(broker: &Broker, user: &str, alias: &str) -> Option<HttpError> {
    let current = renamed_to(&broker.state, user, alias).ok().flatten()?;
    let mut error = broker.error(StatusCode::NOT_FOUND, "account_renamed");
    error.alias = Some(current);
    Some(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tombstones_follow_a_chain_and_drop_a_reused_alias() {
        let root = tempfile::tempdir().unwrap();
        let rename = |from: &str, to: &str| Rename {
            user: "amir".into(),
            from: from.into(),
            to: to.into(),
        };
        record_tombstone(root.path(), &rename("a", "b")).unwrap();
        record_tombstone(root.path(), &rename("b", "c")).unwrap();
        assert_eq!(
            renamed_to(root.path(), "amir", "A").unwrap().as_deref(),
            Some("c")
        );
        assert_eq!(renamed_to(root.path(), "alex", "a").unwrap(), None);
        // Repeating a rename replaces its entry instead of adding a second one.
        record_tombstone(root.path(), &rename("a", "b")).unwrap();
        assert_eq!(tombstones(root.path()).unwrap().len(), 2);
    }
}
