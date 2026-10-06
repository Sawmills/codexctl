//! Server-managed login for a new server account.
//!
//! The device login runs in a private server home, as renewal does, so the
//! refresh token never reaches the client. Records live under
//! `account-logins/<account key>/relogin/<id>`, outside the account registry,
//! and a saved grant stays reserved there until admission retires it.
use super::inventory::Reservation;
use super::*;
use managed::{Import, account_key, normalize_alias};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::central) struct AddRequest {
    pub alias: String,
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// Refusals that prove this grant cannot become the requested account.
const REFUSALS: [&str; 3] = [
    "account_already_owned",
    "alias_identity_conflict",
    "relogin_reserved",
];

pub(in crate::central) fn root(broker_state: &Path) -> PathBuf {
    broker_state.join("account-logins")
}
fn add_state(broker_state: &Path, user: &str, alias: &str) -> PathBuf {
    root(broker_state).join(account_key(user, alias))
}
fn terminal(phase: &Phase) -> bool {
    matches!(phase, Phase::Completed | Phase::Failed | Phase::Canceled)
}
fn live(broker: &Broker, state: &Path, id: &str) -> bool {
    broker
        .relogins
        .lock()
        .expect("re-login lock")
        .contains_key(&job_key(state, id))
}
/// A saved grant whose child has exited and whose admission is not running.
fn resumable(broker: &Broker, state: &Path, record: &Record) -> bool {
    !terminal(&record.phase)
        && record.landed.is_none()
        && !live(broker, state, &record.id)
        && stopped(state, record).is_ok()
        && candidate(state, record).ok().flatten().is_some()
}

/// Retire the grant before deleting it, so a crash between the two steps can
/// never leave an unreserved credential that a later read treats as live.
/// The terminal phase is written last, so a terminal record has no grant left.
fn discard(state: &Path, record: &mut Record, phase: Phase, error: &str) -> Result<()> {
    record.code = None;
    record.candidate = None;
    record.retired = true;
    save(state, record)?;
    let operation = directory(state, &record.id)?;
    let home = operation.join("home");
    if home.try_exists()? {
        std::fs::remove_dir_all(&home)?;
        store::sync_directory(&operation)?;
    }
    record.phase = phase;
    record.error = Some(error.into());
    save(state, record)
}

/// A revoked or unknown device can never resume, cancel, or delete its record.
/// An unreadable registry counts as active, so the record stays protected.
fn device_active(broker: &Broker, id: &str) -> bool {
    vault::devices(&broker.state).map_or(true, |devices| {
        devices
            .iter()
            .any(|device| device.id == id && !device.revoked)
    })
}

/// Startup fences every owner a saved grant overlaps. Once the grant is gone,
/// relaunch each verified owner it fenced through the restore clearance; any
/// other evidence keeps that owner fenced.
async fn release(broker: &Broker, guard: &tokio::sync::MutexGuard<'_, ()>, grant: &Value) {
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return;
    }
    let owners = broker.owners.read().await.clone();
    for (_, owner) in owners.values() {
        let mut owner = owner.lock().await;
        // An owner with its own unfinished or failed renewal stays fenced
        // for that reason, whatever happened to this grant.
        let own_login = current(&owner.state).map_or(true, |r| {
            r.is_some_and(|r| r.phase != Phase::Completed && !r.retired)
        });
        if owner.available
            || owner.rpc.is_some()
            || !owner.refresh_enabled
            || own_login
            || !owner.vault.verified
            || owner.routing_refused
            || owner.retryable_unavailable
            || !overlaps(&owner.vault.auth, grant)
        {
            continue;
        }
        if relaunch(broker, guard, &mut owner).await.is_ok() {
            owner.available = true;
        }
    }
}
async fn relaunch(
    broker: &Broker,
    guard: &tokio::sync::MutexGuard<'_, ()>,
    owner: &mut Owner,
) -> Result<()> {
    owner.snapshot()?;
    let proof = identity_inventory(&owner.state, &broker.key, &owner.home).clear_for_launch(
        owner,
        AdmissionKind::Restore,
        guard,
    )?;
    launch_owner(owner, &broker.binary, proof).await
}

/// Report a grant that landed on an existing alias with that renewal's phase.
async fn view(broker: &Broker, state: &Path, mut record: Record) -> Result<Record, HttpError> {
    let Some(landed) = record.landed.clone() else {
        return Ok(record);
    };
    if terminal(&record.phase) {
        return Ok(record);
    }
    let Some((_, owner)) = broker.resolve_alias(&record.user, &landed).await? else {
        return Ok(record);
    };
    let owner_state = owner.lock().await.state.clone();
    let renewal = load(&owner_state, &record.id)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
    record.phase = renewal.phase;
    record.error = renewal.error;
    // A landed renewal that stopped with an error is retried through the
    // alias it landed on, never through this add operation.
    let stalled = record.error.is_some() && !live(broker, state, &record.id);
    if record.phase == Phase::Failed || (stalled && !terminal(&record.phase)) {
        record.phase = Phase::Failed;
        record.error = Some("landed_renewal_failed".into());
    }
    if terminal(&record.phase) {
        save(state, &record)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    }
    Ok(record)
}

async fn request_record(
    broker: &Broker,
    headers: &HeaderMap,
    body: Result<Json<AddRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(vault::Device, PathBuf, Record), HttpError> {
    let device = broker.authorize(headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let alias = normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    let state = add_state(&broker.state, &device.user, alias);
    let record = if request.id.is_empty() {
        current(&state).and_then(|r| r.context("no login operation"))
    } else {
        load(&state, &request.id)
    }
    .map_err(|_| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if record.user != device.user {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    if record.device != device.id {
        return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
    }
    Ok((device, state, record))
}

pub(in crate::central) async fn status(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<AddRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    if let Some(error) = broker.reject_unshared_workflow("account_login_unavailable") {
        return Err(error);
    }
    let _import = broker.imports.lock().await;
    let (_, state, record) = request_record(&broker, &headers, body).await?;
    let record = view(&broker, &state, record).await?;
    broker.authorize(&headers).await?;
    Ok(response(&record))
}

pub(in crate::central) async fn cancel(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<AddRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    if let Some(error) = broker.reject_unshared_workflow("account_login_unavailable") {
        return Err(error);
    }
    let _import = broker.imports.lock().await;
    let (_, state, mut record) = request_record(&broker, &headers, body).await?;
    let flag = broker
        .relogins
        .lock()
        .expect("re-login lock")
        .get(&job_key(&state, &record.id))
        .cloned();
    if let Some(flag) = flag {
        // An acknowledgment, not proof of exit. Poll for the terminal result.
        flag.store(true, Ordering::Release);
    } else if resumable(&broker, &state, &record) {
        if record.retired {
            // Import already holds this grant as an unverified server account.
            // End this operation; the account stays and renews through its alias.
            discard(
                &state,
                &mut record,
                Phase::Canceled,
                "account_import_retained",
            )
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            return Ok(response(&record));
        }
        let grant = candidate(&state, &record).ok().flatten();
        discard(&state, &mut record, Phase::Canceled, "login_canceled")
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        if let Some(grant) = grant {
            release(&broker, &_import, &grant).await;
        }
    }
    Ok(response(&record))
}

pub(in crate::central) async fn start(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<AddRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    if let Some(error) = broker.reject_unshared_workflow("account_login_unavailable") {
        return Err(error);
    }
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.authorize(&headers).await?;
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let worker = broker.clone();
    let worker_headers = headers.clone();
    let response = tokio::spawn(async move {
        let _permit = permit;
        start_owned(worker, worker_headers, request).await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_failed"))??;
    broker.authorize(&headers).await?;
    Ok(response)
}

async fn start_owned(
    broker: Broker,
    headers: HeaderMap,
    request: AddRequest,
) -> Result<Response, HttpError> {
    validate_id(&request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let label = store::validate_label(request.label.as_deref().unwrap_or(""))
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_label"))?
        .map(str::to_owned);
    let import = broker.imports.lock().await;
    let device = broker.authorize(&headers).await?;
    let alias = normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
        .to_owned();
    let available = !(broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire));
    let state = add_state(&broker.state, &device.user, &alias);
    let persistence = |_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed");

    // A known operation id answers before any availability or alias check, so
    // a lost completion or a crash after the grant never starts a second login.
    if directory(&state, &request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?
        .join("record.json")
        .try_exists()
        .unwrap_or(true)
    {
        let mut record = load(&state, &request.id)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
        if record.device != device.id {
            return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
        }
        if resumable(&broker, &state, &record) {
            if !available {
                return Err(broker.error(StatusCode::CONFLICT, "account_login_unavailable"));
            }
            resume(&broker, &headers, &state, &mut record)?;
        }
        return Ok(response(&view(&broker, &state, record).await?));
    }
    if !available {
        return Err(broker.error(StatusCode::CONFLICT, "account_login_unavailable"));
    }
    store::ensure_private_dir(&root(&broker.state)).map_err(persistence)?;
    store::ensure_private_dir(&state).map_err(persistence)?;
    if broker.resolve_alias(&device.user, &alias).await?.is_some() {
        return Err(broker.error(StatusCode::CONFLICT, "alias_exists"));
    }
    let mut latest = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
    if let Some(record) = latest.take() {
        // A landed record ends with its renewal, whichever device asks.
        latest = Some(view(&broker, &state, record).await?);
    }
    if let Some(mut record) = latest.clone()
        && !terminal(&record.phase)
    {
        let stale = record.device != device.id
            && !device_active(&broker, &record.device)
            && !live(&broker, &state, &record.id)
            && record.landed.is_none()
            && stopped(&state, &record).is_ok();
        if record.device != device.id && !stale {
            return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
        }
        if !stale && (live(&broker, &state, &record.id) || record.landed.is_some()) {
            return Ok(response(&view(&broker, &state, record).await?));
        }
        if !stale && resumable(&broker, &state, &record) {
            resume(&broker, &headers, &state, &mut record)?;
            return Ok(response(&record));
        }
        stopped(&state, &record)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        if record.retired && record.candidate.is_some() {
            // Import holds this grant as an unverified account under the alias.
            return Err(broker.error(StatusCode::CONFLICT, "account_import_retained"));
        }
        let grant = candidate(&state, &record).ok().flatten();
        discard(
            &state,
            &mut record,
            Phase::Failed,
            if stale {
                "device_revoked"
            } else {
                "login_interrupted_retry"
            },
        )
        .map_err(persistence)?;
        if let Some(grant) = grant {
            release(&broker, &import, &grant).await;
        }
    }
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let record = Record {
        sequence: latest.map_or(Ok(1), |r| {
            r.sequence
                .checked_add(1)
                .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))
        })?,
        id: request.id,
        user: device.user,
        device: device.id,
        broker: process::Process::capture(std::process::id())
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?,
        child: Child::NotStarted,
        verifier_broker: None,
        candidate: None,
        alias,
        original_revision: String::new(),
        candidate_revision: None,
        phase: Phase::Starting,
        code: None,
        error: None,
        retired: false,
        label,
        landed: None,
    };
    publish(&state, &record).map_err(persistence)?;
    let flag = Arc::new(AtomicBool::new(false));
    broker
        .relogins
        .lock()
        .expect("re-login lock")
        .insert(job_key(&state, &record.id), flag.clone());
    let worker = broker.clone();
    let initial = (state.clone(), record.id.clone());
    let worker_headers = headers.clone();
    let (ready, first) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut record = record;
        if run(
            &worker,
            &worker_headers,
            &state,
            &mut record,
            &flag,
            ready,
            permit,
        )
        .await
        .is_err()
        {
            settle_failure(&worker, &state, &record).await;
        }
        worker
            .relogins
            .lock()
            .expect("re-login lock")
            .remove(&job_key(&state, &record.id));
    });
    drop(import);
    let record = match first.await {
        Ok(record) => record,
        Err(_) => load(&initial.0, &initial.1)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
    };
    broker.authorize(&headers).await?;
    Ok(response(&record))
}

/// Never log native output or credential data. A saved grant stays reserved
/// for a retry; anything before the grant fails closed.
async fn settle_failure(broker: &Broker, state: &Path, record: &Record) {
    let _import = broker.imports.lock().await;
    match load(state, &record.id) {
        Ok(mut durable) if !terminal(&durable.phase) && durable.landed.is_none() => {
            let saved = candidate(state, &durable).ok().flatten().is_some();
            let result = if saved && stopped(state, &durable).is_ok() {
                durable.phase = Phase::Committing;
                durable.error = Some("relogin_failed".into());
                save(state, &durable)
            } else {
                durable.phase = Phase::Failed;
                durable.code = None;
                durable.error.get_or_insert_with(|| "relogin_failed".into());
                save(state, &durable)
            };
            if result.is_err() {
                broker.ownership_unresolved.store(true, Ordering::Release);
            }
        }
        Ok(_) => {}
        Err(_) => {
            if stopped(state, record).is_err() {
                broker.ownership_unresolved.store(true, Ordering::Release);
            }
        }
    }
    broker.record_failure("relogin_failed", "relogin", StatusCode::SERVICE_UNAVAILABLE);
}

/// Restart admission of a saved grant. Call under the imports lock.
fn resume(
    broker: &Broker,
    headers: &HeaderMap,
    state: &Path,
    record: &mut Record,
) -> Result<(), HttpError> {
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    // The retry starts now; an earlier attempt's error no longer describes it.
    record.error = None;
    save(state, record)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    let flag = Arc::new(AtomicBool::new(false));
    broker
        .relogins
        .lock()
        .expect("re-login lock")
        .insert(job_key(state, &record.id), flag);
    let worker = broker.clone();
    let worker_headers = headers.clone();
    let state = state.to_owned();
    let id = record.id.clone();
    tokio::spawn(async move {
        if admit(&worker, &worker_headers, &state, &id, permit)
            .await
            .is_err()
            && let Ok(record) = load(&state, &id)
        {
            settle_failure(&worker, &state, &record).await;
        }
        worker
            .relogins
            .lock()
            .expect("re-login lock")
            .remove(&job_key(&state, &id));
    });
    Ok(())
}

async fn run(
    broker: &Broker,
    headers: &HeaderMap,
    state: &Path,
    record: &mut Record,
    flag: &AtomicBool,
    ready: tokio::sync::oneshot::Sender<Record>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<()> {
    let home = std::fs::canonicalize(directory(state, &record.id)?.join("home"))?;
    let binary = process::owner_binary(&broker.binary)?;
    let mut command = Command::new(binary);
    command
        .args([
            "login",
            "--device-auth",
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            "forced_login_method=\"chatgpt\"",
            "-c",
            "features.daemon_auto_start=false",
        ])
        .env("CODEX_HOME", &home)
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process::isolate(&mut command);
    let mut child = spawn_login(&broker.imports, state, &home, record, &mut command).await?;
    let mut output = child.stdout.take().context("missing login output")?;
    let started = tokio::time::Instant::now();
    let mut bytes = Vec::new();
    let mut ready = Some(ready);
    let mut output_open = true;
    let outcome: Result<Option<std::process::ExitStatus>> = async {
        loop {
            if ready.is_some() && started.elapsed() > Duration::from_secs(30) {
                record.error = Some("unsupported_login_output".into());
                bail!("native login challenge timed out");
            }
            if flag.load(Ordering::Acquire)
                || broker.stopping.load(Ordering::Acquire)
                || broker.authorize(headers).await.is_err()
                || started.elapsed() >= DEADLINE
            {
                child.start_kill()?;
                child.wait().await?;
                return Ok(None);
            }
            tokio::select! {
                result = child.wait() => return Ok(Some(result?)),
                read = output.read_u8(), if ready.is_some() && output_open => {
                    match read {
                        Ok(b) => {
                            bytes.push(b);
                            if bytes.len() > 32768 {
                                record.error = Some("unsupported_login_output".into());
                                bail!("login output too large");
                            }
                            if b == b'\n' && let Some(code) = worker::challenge(&bytes)? {
                                let _import = broker.imports.lock().await;
                                record.phase = Phase::Pending;
                                record.code = Some(code);
                                save(state, record)?;
                                if let Some(ready) = ready.take() {
                                    let _ = ready.send(record.clone());
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            output_open = false;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }
    .await;
    if outcome.is_err() {
        child.start_kill()?;
        child.wait().await?;
    }
    {
        let _import = broker.imports.lock().await;
        let durable = load(state, &record.id)?;
        if durable.retired {
            *record = durable;
            return Ok(());
        }
        record.child = Child::Exited;
        save(state, record)?;
        match &outcome {
            Ok(None) => {
                // Cancellation, revocation, shutdown, or deadline. A stopped
                // login leaves no server candidate.
                discard(state, record, Phase::Canceled, "login_canceled")?;
                broker.record_failure("relogin_stopped", "relogin", StatusCode::CONFLICT);
                return Ok(());
            }
            Ok(Some(status)) if status.success() => {}
            _ => {
                let error = record
                    .error
                    .clone()
                    .unwrap_or_else(|| "relogin_failed".into());
                discard(state, record, Phase::Failed, &error)?;
                bail!("native login failed");
            }
        }
        let auth = candidate(state, record)?.context("native login did not save credentials")?;
        record.candidate = Some(auth);
        record.phase = Phase::Committing;
        record.code = None;
        save(state, record)?;
    }
    admit(broker, headers, state, &record.id, permit).await
}

/// Admit a saved grant: land it on the alias that already holds the identity,
/// or import it as the requested new account.
async fn admit(
    broker: &Broker,
    headers: &HeaderMap,
    state: &Path,
    id: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<()> {
    let guard = broker.imports.lock().await;
    let mut record = load(state, id)?;
    if terminal(&record.phase) || record.landed.is_some() {
        return Ok(());
    }
    let auth = candidate(state, &record)?.context("missing saved grant")?;
    if broker.authorize(headers).await.is_err() {
        discard(state, &mut record, Phase::Canceled, "login_canceled")?;
        release(broker, &guard, &auth).await;
        return Ok(());
    }
    let owners = broker.owners.read().await.clone();
    let mut held = None;
    for (identity, owner) in owners.values() {
        // An earlier attempt may have imported this alias unverified; its
        // retry is import verification, not a renewal of the same alias.
        if identity.user != record.user || identity.alias.eq_ignore_ascii_case(&record.alias) {
            continue;
        }
        let guarded = owner.lock().await;
        if overlaps(&guarded.vault.auth, &auth) && guarded.validate_owned_auth(&auth).is_ok() {
            held = Some(owner.clone());
            break;
        }
    }
    if let Some(owner) = held {
        return land(broker, &guard, state, &mut record, &auth, &owner).await;
    }
    // Hand the reservation to import admission, which records its own journal
    // before verification. The imports guard stays held across the handoff.
    record.retired = true;
    save(state, &record)?;
    let input = Import {
        alias: record.alias.clone(),
        label: record.label.clone(),
        auth,
    };
    let result = broker
        .import_account_locked(&guard, permit, &record.user, input)
        .await;
    let mut record = load(state, id)?;
    match result {
        Ok(_) => {
            discard(state, &mut record, Phase::Completed, "")?;
            record.error = None;
            save(state, &record)
        }
        Err(error) if REFUSALS.contains(&error.reason) => {
            let grant = candidate(state, &record).ok().flatten();
            discard(state, &mut record, Phase::Failed, error.reason)?;
            if let Some(grant) = grant {
                release(broker, &guard, &grant).await;
            }
            Ok(())
        }
        Err(error) => {
            // Import keeps a retained account for this alias reserved itself.
            // Otherwise the grant returns to this record for a retry.
            let imported = broker
                .state
                .join("accounts")
                .join(account_key(&record.user, &record.alias))
                .join("vault.enc")
                .try_exists()?;
            record.retired = imported;
            record.error = Some(error.reason.into());
            save(state, &record)
        }
    }
}

/// Renew the alias that already holds this identity with the new grant,
/// through the same promote and verification steps as `codexctl login <alias>`.
async fn land(
    broker: &Broker,
    guard: &tokio::sync::MutexGuard<'_, ()>,
    state: &Path,
    record: &mut Record,
    auth: &Value,
    owner: &Arc<Mutex<Owner>>,
) -> Result<()> {
    let mut held = owner.lock().await;
    let owner_state = held.state.clone();
    let latest = current(&owner_state)?;
    if latest.as_ref().is_some_and(|r| !terminal(&r.phase)) {
        return discard(state, record, Phase::Failed, "relogin_reserved");
    }
    // Fence before stopping, as renewal does, so a later failure never leaves
    // an available owner without its refresh process.
    held.fence(false);
    if let Some(rpc) = held.rpc.as_mut() {
        rpc.settle_and_stop().await?;
    } else {
        previous_owner_exited(&held.home)?;
    }
    held.rpc = None;
    held.snapshot()?;
    let mut renewal = Record {
        sequence: latest.map_or(Ok(1), |r| {
            r.sequence.checked_add(1).context("login sequence overflow")
        })?,
        id: record.id.clone(),
        user: record.user.clone(),
        device: record.device.clone(),
        broker: process::Process::capture(std::process::id())?,
        child: Child::Exited,
        verifier_broker: None,
        candidate: Some(auth.clone()),
        alias: held.vault.alias.clone(),
        original_revision: vault::digest(&serde_json::to_vec(&held.vault.auth)?),
        candidate_revision: None,
        phase: Phase::Starting,
        code: None,
        error: None,
        retired: false,
        label: None,
        landed: None,
    };
    // The renewal record reserves the grant before the add record releases it.
    publish(&owner_state, &renewal)?;
    record.landed = Some(held.vault.alias.trim().into());
    record.retired = true;
    record.candidate = None;
    save(state, record)?;
    let operation = directory(state, &record.id)?;
    if operation.join("home").try_exists()? {
        std::fs::remove_dir_all(operation.join("home"))?;
        store::sync_directory(&operation)?;
    }
    held.fence(false);
    let promoted = check_claim(
        owner_state.parent().context("missing registry")?,
        &broker.key,
        &owner_state,
        auth,
    )
    .and_then(|()| promote(&owner_state, &broker.key, &mut renewal, auth));
    if let Err(error) = promoted {
        let mut durable = load(&owner_state, &renewal.id)?;
        if !durable.phase.commit_started() {
            durable.phase = Phase::Failed;
            durable.error = Some("relogin_failed".into());
            save(&owner_state, &durable)?;
        }
        return Err(error);
    }
    held.vault = vault::load(&owner_state, &broker.key)?;
    held.verification_input = Some(held.vault.auth.clone());
    held.refresh_enabled = true;
    held.available = true;
    let verified = verify_replacement(&mut held, &broker.binary, guard).await;
    let renewal = load(&owner_state, &renewal.id)?;
    record.phase = renewal.phase;
    record.error = renewal.error;
    save(state, record)?;
    verified
}

/// Startup audit of new-account logins. Returns every reserved grant so the
/// caller fences overlapping owners. An unidentified login child is an error.
pub(in crate::central) fn recover(broker_state: &Path, key: &Path) -> Result<Vec<Value>> {
    let root = root(broker_state);
    let mut reserved = Vec::new();
    if !root.try_exists()? {
        return Ok(reserved);
    }
    for entry in std::fs::read_dir(&root)? {
        let state = entry?.path();
        for mut record in records(&state)? {
            if terminal(&record.phase) && record.retired {
                continue;
            }
            stopped(&state, &record)?;
            if record.landed.is_none()
                && !terminal(&record.phase)
                && record.retired
                && record.candidate.is_none()
            {
                // A discard stopped between retiring the grant and its terminal
                // write. A verified account under this alias means admission won.
                let account = broker_state
                    .join("accounts")
                    .join(account_key(&record.user, &record.alias));
                let admitted =
                    account.join("vault.enc").try_exists()? && vault::load(&account, key)?.verified;
                if admitted {
                    discard(&state, &mut record, Phase::Completed, "")?;
                    record.error = None;
                    save(&state, &record)?;
                } else {
                    discard(
                        &state,
                        &mut record,
                        Phase::Failed,
                        "login_interrupted_retry",
                    )?;
                }
            } else if record.landed.is_none() && !terminal(&record.phase) {
                // Unusable native output after proven exit fails this record
                // alone; its bytes stay for diagnosis but reserve nothing.
                let grant = candidate(&state, &record);
                if grant.is_err() {
                    record.phase = Phase::Failed;
                    record.code = None;
                    record.retired = true;
                    record.error = Some("unsupported_login_output".into());
                    save(&state, &record)?;
                    continue;
                }
                match grant? {
                    Some(auth) => {
                        record.candidate = Some(auth);
                        record.child = Child::Exited;
                        record.phase = Phase::Committing;
                        record.code = None;
                        if record.retired {
                            // Import retains its own reservation once it wrote a vault.
                            record.retired = broker_state
                                .join("accounts")
                                .join(account_key(&record.user, &record.alias))
                                .join("vault.enc")
                                .try_exists()?;
                        }
                        save(&state, &record)?;
                    }
                    None => {
                        discard(
                            &state,
                            &mut record,
                            Phase::Failed,
                            "login_interrupted_retry",
                        )?;
                    }
                }
            }
            if let Some(auth) = reservation(&state, &record)? {
                reserved.push(auth);
            }
        }
    }
    Ok(reserved)
}

/// Reservations held by new-account logins, for the shared registry admission.
pub(in crate::central) fn reservations(accounts: &Path) -> Result<Vec<Reservation>> {
    let mut result = Vec::new();
    let Some(root) = accounts.parent().map(root) else {
        return Ok(result);
    };
    if !root.try_exists()? {
        return Ok(result);
    }
    for entry in std::fs::read_dir(root)? {
        let state = entry?.path();
        for record in records(&state)? {
            if record.retired || terminal(&record.phase) {
                continue;
            }
            let process = if stopped(&state, &record).is_ok() {
                ProcessState::Stopped
            } else if matches!(&record.child, Child::Running(p) if p.alive().is_ok_and(|alive| alive))
            {
                ProcessState::Live
            } else {
                bail!("unidentified login process");
            };
            if let Some(auth) = reservation(&state, &record)? {
                result.push(Reservation { auth, process });
            }
        }
    }
    Ok(result)
}

/// Release new-account reservations superseded by a verified owner.
pub(in crate::central) fn retire(accounts: &Path, auth: &Value, skip: &Path) -> Result<()> {
    let Some(root) = accounts.parent().map(root) else {
        return Ok(());
    };
    if !root.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let state = entry?.path();
        for mut record in records(&state)? {
            if record.retired || terminal(&record.phase) {
                continue;
            }
            if directory(&state, &record.id)? == skip {
                continue;
            }
            if reservation(&state, &record)?.is_some_and(|a| overlaps(&a, auth)) {
                stopped(&state, &record)?;
                discard(
                    &state,
                    &mut record,
                    Phase::Failed,
                    "superseded_by_owner_repair",
                )?;
            }
        }
    }
    Ok(())
}
