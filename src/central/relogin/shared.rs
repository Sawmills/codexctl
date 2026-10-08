//! PostgreSQL renewal uses shared receipts; only execution homes are replica-local.
use super::*;
use crate::central::storage::{
    CentralStore,
    login::{AddAdmission, LoginKind, LoginOperation, LoginPayload, LoginPhase},
};
use std::{process::Stdio, time::Duration};
#[cfg(target_os = "linux")]
use tokio::io::AsyncWriteExt;
use tokio::{io::AsyncReadExt, process::Command};

type Body = Result<Json<Request>, axum::extract::rejection::JsonRejection>;
fn failure(broker: &Broker) -> HttpError {
    broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_failed")
}
fn database(broker: &Broker) -> Result<&CentralStore, HttpError> {
    broker.central.as_ref().ok_or_else(|| failure(broker))
}
fn view(op: &LoginOperation) -> Response {
    let status = match op.phase {
        LoginPhase::Candidate | LoginPhase::Verifying => "verifying",
        LoginPhase::Unresolved | LoginPhase::Rejected => "failed",
        LoginPhase::ReplicaLost => "expired",
        phase => phase.as_str(),
    };
    (
        [("cache-control", "no-store")],
        Json(json!({
            "id":op.id,"alias":op.alias,"userId":op.user,"status":status,
            "verificationUrl":if op.phase == LoginPhase::Pending {Some(URL)} else {None},
            "userCode":if op.phase == LoginPhase::Pending {op.payload.code.as_deref()} else {None},
            "error":if op.phase == LoginPhase::ReplicaLost && !op.polling_clear {
                Some("login_expired_requires_recovery")
            } else {op.payload.error.as_deref()},
            "landedAlias":op.payload.landed,
        })),
    )
        .into_response()
}
fn owned(
    broker: &Broker,
    device: &vault::Device,
    request: &Request,
    op: &LoginOperation,
) -> Result<(), HttpError> {
    if op.user != device.user || !op.alias.eq_ignore_ascii_case(request.alias.trim()) {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    if op.device != device.id {
        return Err(broker.error(StatusCode::CONFLICT, "login_belongs_to_another_device"));
    }
    Ok(())
}
/// The reporter is the holder that observed the failure: a worker reports as
/// the holder it ran under, a recovery read as the current login holder.
/// A `grantless` claim counts only when the durable receipt proves no grant.
async fn record_login_failure(
    broker: &Broker,
    op: &mut LoginOperation,
    reporter: &str,
    grantless: bool,
) -> Result<(), HttpError> {
    if op.failure_reported {
        return Ok(());
    }
    let db = database(broker)?;
    let claimed = if grantless {
        db.login_take_grantless_failure(op, reporter).await
    } else {
        db.login_take_failure(op, reporter).await
    };
    if claimed.map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))? {
        let reason = if op.payload.error.as_deref() == Some("account_identity_unresolved")
            || ((op.phase == LoginPhase::Unresolved
                || (op.phase == LoginPhase::ReplicaLost && !op.polling_clear))
                && op.payload.candidate.is_none())
        {
            "relogin_identity_unresolved"
        } else {
            "relogin_failed"
        };
        eprintln!(
            "{}",
            json!({"operation":"login_recovery","stage":"polling_settlement","reason":reason})
        );
        broker.record_failure(
            reason,
            "polling_settlement",
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    Ok(())
}
async fn recover(
    broker: &Broker,
    headers: &HeaderMap,
    op: &mut LoginOperation,
) -> Result<(), HttpError> {
    let holder_id = broker.login_holder();
    if matches!(op.phase, LoginPhase::Rejected | LoginPhase::Unresolved)
        || (op.phase == LoginPhase::ReplicaLost && !op.polling_clear)
    {
        record_login_failure(broker, op, &holder_id, false).await?;
    }
    // The same database read that returns the receipt observes lease expiry.
    // Live receipts must not queue behind admission or settlement work.
    if !op.lease_expired && !(op.phase == LoginPhase::Candidate && op.holder == holder_id) {
        return Ok(());
    }
    if database(broker)?
        .login_recover_polling(op, &holder_id)
        .await
        .map_err(|_| failure(broker))?
    {
        eprintln!(
            "{}",
            json!({"operation":"login_recovery","stage":"device_polling","reason":"replica_lost"})
        );
        record_login_failure(broker, op, &holder_id, false).await?;
    }
    // A live holder settles its own receipt only after its local worker exits.
    let own = op.holder == holder_id;
    let guard = if own {
        WorkerGuard::claim(broker, op)?
    } else {
        None
    };
    let settled = (!own || guard.is_some())
        && database(broker)?
            .login_recover_unresolved(op, &holder_id)
            .await
            .map_err(|_| failure(broker))?;
    drop(guard);
    if settled {
        eprintln!(
            "{}",
            json!({"operation":"login_recovery","stage":"verification","reason":"replica_lost"})
        );
        record_login_failure(broker, op, &holder_id, false).await?;
    }
    if op.phase == LoginPhase::Candidate
        && !broker.read_only
        && !broker.stopping.load(Ordering::Acquire)
        && !broker.ownership_unresolved.load(Ordering::Acquire)
        && broker.login_holder_live.load(Ordering::Acquire)
        && let Ok(permit) = broker.work.clone().try_acquire_owned()
        && let Some(guard) = WorkerGuard::claim(broker, op)?
        && database(broker)?
            .login_takeover_candidate(op, &holder_id)
            .await
            .map_err(|_| failure(broker))?
    {
        spawn_claimed_worker(broker.clone(), headers.clone(), op.clone(), permit, guard);
    }
    Ok(())
}
async fn lookup(
    broker: &Broker,
    headers: &HeaderMap,
    body: Body,
    kind: LoginKind,
) -> Result<LoginOperation, HttpError> {
    let device = broker.authorize(headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let db = database(broker)?;
    let op = if request.id.is_empty() {
        let alias = managed::normalize_alias(&request.alias)
            .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
        db.login_active_alias(&device.user, alias).await
    } else {
        db.login_get(&device.user, &request.id).await
    }
    .map_err(|_| failure(broker))?
    .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if op.kind != kind {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    owned(broker, &device, &request, &op)?;
    Ok(op)
}
pub(super) async fn status(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let mut op = lookup(&broker, &headers, body, LoginKind::Renewal).await?;
    recover(&broker, &headers, &mut op).await?;
    Ok(view(&op))
}
pub(super) async fn cancel(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let op = lookup(&broker, &headers, body, LoginKind::Renewal).await?;
    database(&broker)?
        .login_cancel(&op)
        .await
        .map_err(|_| failure(&broker))?;
    Ok(view(&op))
}
pub(super) async fn start(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    start_kind(broker, headers, body, LoginKind::Renewal, None).await
}
pub(super) async fn add_status(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let mut op = lookup(&broker, &headers, body, LoginKind::Add).await?;
    recover(&broker, &headers, &mut op).await?;
    Ok(view(&op))
}
pub(super) async fn add_cancel(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let op = lookup(&broker, &headers, body, LoginKind::Add).await?;
    database(&broker)?
        .login_cancel(&op)
        .await
        .map_err(|_| failure(&broker))?;
    Ok(view(&op))
}
pub(super) async fn start_kind(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
    kind: LoginKind,
    label: Option<String>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    validate_id(&request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let db = database(&broker)?;
    // Receipts precede availability and alias checks, including completed retries.
    if let Some(mut op) = db
        .login_get(&device.user, &request.id)
        .await
        .map_err(|_| failure(&broker))?
    {
        if op.kind != kind {
            return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
        }
        owned(&broker, &device, &request, &op)?;
        recover(&broker, &headers, &mut op).await?;
        return Ok(view(&op));
    }
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
        || !broker.login_holder_live.load(Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_unavailable"));
    }
    let alias = managed::normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    if let Some(mut active) = db
        .login_active_alias(&device.user, alias)
        .await
        .map_err(|_| failure(&broker))?
    {
        if active.kind != kind {
            return Err(broker.error(StatusCode::CONFLICT, "relogin_reserved"));
        }
        if active.phase == LoginPhase::ReplicaLost {
            // A fresh request belongs to its caller. Cleanup uses company-user
            // authorization and does not expose another machine's old receipt.
            recover(&broker, &headers, &mut active).await?;
        } else {
            owned(&broker, &device, &request, &active)?;
            recover(&broker, &headers, &mut active).await?;
            return Ok(view(&active));
        }
    }
    if kind == LoginKind::Add
        && db
            .renamed_alias(&device.user, alias)
            .await
            .map_err(|_| failure(&broker))?
            .is_some()
    {
        return Err(broker.error(StatusCode::CONFLICT, "alias_renamed"));
    }
    let resolved = broker.resolve_alias(&device.user, alias).await?;
    let account = if kind == LoginKind::Add {
        if resolved.is_some() {
            return Err(broker.error(StatusCode::CONFLICT, "alias_exists"));
        }
        None
    } else {
        Some(
            resolved
                .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "account_not_found"))?
                .0,
        )
    };
    if let Some(account) = account.as_deref()
        && db
            .login_active_account(account)
            .await
            .map_err(|_| failure(&broker))?
            .is_some()
    {
        return Err(broker.error(StatusCode::CONFLICT, "relogin_reserved"));
    }
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let op = LoginOperation {
        kind,
        user: device.user.clone(),
        id: request.id.clone(),
        account_id: account.clone(),
        alias: alias.into(),
        device: device.id.clone(),
        phase: LoginPhase::Starting,
        sequence: 0,
        holder: broker.login_holder(),
        epoch: 1,
        payload: LoginPayload {
            label,
            ..Default::default()
        },
        polling_clear: false,
        lease_expired: false,
        failure_reported: false,
    };
    if !db.login_create(&op).await.map_err(|_| failure(&broker))? {
        let existing = db
            .login_get(&device.user, &request.id)
            .await
            .map_err(|_| failure(&broker))?;
        let existing = match existing {
            Some(op) => Some(op),
            None => db
                .login_active_alias(&device.user, alias)
                .await
                .map_err(|_| failure(&broker))?,
        };
        let existing = match existing {
            Some(existing) => Some(existing),
            None => match account.as_deref() {
                Some(account) => db
                    .login_active_account(account)
                    .await
                    .map_err(|_| failure(&broker))?,
                None => None,
            },
        };
        let existing = existing.ok_or_else(|| failure(&broker))?;
        if existing.kind != kind || !existing.alias.eq_ignore_ascii_case(alias) {
            return Err(broker.error(StatusCode::CONFLICT, "relogin_reserved"));
        }
        owned(&broker, &device, &request, &existing)?;
        return Ok(view(&existing));
    }
    spawn_worker(broker.clone(), headers.clone(), op.clone(), permit)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current = db
            .login_get(&op.user, &op.id)
            .await
            .map_err(|_| failure(&broker))?
            .ok_or_else(|| failure(&broker))?;
        if current.phase != LoginPhase::Starting || tokio::time::Instant::now() >= deadline {
            broker.authorize(&headers).await?;
            return Ok(view(&current));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
struct WorkerGuard {
    broker: Broker,
    key: (String, String),
}
impl WorkerGuard {
    fn claim(broker: &Broker, op: &LoginOperation) -> Result<Option<Self>, HttpError> {
        let key = (op.user.clone(), op.id.clone());
        let mut workers = broker
            .shared_login_workers
            .lock()
            .map_err(|_| failure(broker))?;
        if !workers.insert(key.clone()) {
            return Ok(None);
        }
        Ok(Some(Self {
            broker: broker.clone(),
            key,
        }))
    }
}
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Ok(mut workers) = self.broker.shared_login_workers.lock() {
            workers.remove(&self.key);
        }
    }
}
fn spawn_worker(
    worker: Broker,
    worker_headers: HeaderMap,
    worker_op: LoginOperation,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), HttpError> {
    if let Some(guard) = WorkerGuard::claim(&worker, &worker_op)? {
        spawn_claimed_worker(worker, worker_headers, worker_op, permit, guard);
    }
    Ok(())
}
async fn report_worker_failure(broker: &Broker, op: &mut LoginOperation, grantless: bool) {
    let reporter = op.holder.clone();
    if let Err(error) = record_login_failure(broker, op, &reporter, grantless).await {
        eprintln!(
            "{}",
            json!({"operation":"login_failure_report","stage":"settlement","reason":"report_unavailable","status":error.status.as_u16()})
        );
    }
}
fn spawn_claimed_worker(
    worker: Broker,
    worker_headers: HeaderMap,
    mut worker_op: LoginOperation,
    permit: tokio::sync::OwnedSemaphorePermit,
    guard: WorkerGuard,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let _guard = guard;
        if let Err(error) = run(&worker, &worker_headers, &mut worker_op).await {
            let mut detail = format!("{error:#}");
            if let Some(auth) = worker_op.payload.candidate.as_ref() {
                crate::central::resets::redact_auth_strings(auth, &mut detail);
            }
            let detail: String = detail.chars().take(4096).collect();
            eprintln!(
                "{}",
                json!({"operation":if worker_op.kind == LoginKind::Add {"login_add"} else {"login_renewal"},"stage":worker_op.phase.as_str(),"error":detail})
            );
            if matches!(
                worker_op.phase,
                LoginPhase::Completed
                    | LoginPhase::Canceled
                    | LoginPhase::Rejected
                    | LoginPhase::ReplicaLost
            ) {
                report_worker_failure(&worker, &mut worker_op, false).await;
                return;
            }
            worker_op
                .payload
                .error
                .get_or_insert_with(|| "relogin_interrupted_retry".into());
            let phase = if matches!(
                worker_op.phase,
                LoginPhase::Verifying | LoginPhase::Unresolved
            ) {
                LoginPhase::Unresolved
            } else if worker_op.payload.candidate.is_some() {
                // Candidate capture proves device-login exit. No verifier started.
                LoginPhase::Rejected
            } else {
                LoginPhase::Failed
            };
            if let Ok(db) = database(&worker) {
                // A transient database error must not leave a stale receipt.
                // Report a durable outcome. After a fenced save, recovery decides
                // between resume and an unresolved failure for a grant.
                for attempt in 1..=3 {
                    let Err(error) = db.login_save(&mut worker_op, phase).await else {
                        report_worker_failure(&worker, &mut worker_op, false).await;
                        break;
                    };
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    let mut detail = format!("{error:#}");
                    if let Some(auth) = worker_op.payload.candidate.as_ref() {
                        crate::central::resets::redact_auth_strings(auth, &mut detail);
                    }
                    eprintln!(
                        "{}",
                        json!({"operation":"login_settlement","stage":"terminal_save","reason":"save_unavailable","error":detail})
                    );
                    // Without a grant the attempt cannot resume as a success. The
                    // worker may have missed a committed grant, so the database
                    // decides from the durable receipt.
                    if worker_op.payload.candidate.is_none() {
                        report_worker_failure(&worker, &mut worker_op, true).await;
                    }
                }
            }
        }
    });
}
async fn run(broker: &Broker, headers: &HeaderMap, op: &mut LoginOperation) -> Result<()> {
    let db = database(broker).map_err(|_| anyhow::anyhow!("shared store unavailable"))?;
    if op.phase == LoginPhase::Candidate {
        return continue_candidate(broker, headers, op).await;
    }
    let root = std::fs::canonicalize(&broker.state)?
        .join("shared-logins")
        .join(&op.id);
    store::ensure_private_dir(&root)?;
    let home = tempfile::Builder::new()
        .prefix("login-")
        .tempdir_in(&root)?
        .keep();
    // Keep local evidence on every uncertain exit. Shared publication, not task
    // lifetime, determines when this isolated credential home can be removed.
    store::ensure_private_dir(&home)?;
    store::atomic_write(
        &home.join("operation.json"),
        &serde_json::to_vec(&json!({
            "user":op.user,"id":op.id,"accountId":op.account_id,
            "holder":op.holder,"epoch":op.epoch,
        }))?,
    )?;
    let binary = process::owner_binary(&broker.binary)?;
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("supervise-login")
            .arg("--state")
            .arg(std::fs::canonicalize(&broker.state)?)
            .arg("--key-file")
            .arg(std::fs::canonicalize(&broker.key)?)
            .arg("--codex-bin")
            .arg(binary);
        command.process_group(0);
        command
    };
    #[cfg(not(target_os = "linux"))]
    let mut command = {
        let mut command = Command::new(binary);
        command.args(process::LOGIN_ARGS);
        process::isolate(&mut command);
        command
    };
    command
        .env("CODEX_HOME", &home)
        .env_remove("CODEXCTL_PINNED_ALIAS")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_API_KEY")
        // The supervisor opens relative connection configuration from the
        // server cwd. Its native child still runs inside the private home.
        .current_dir(if cfg!(target_os = "linux") {
            std::env::current_dir()?
        } else {
            home.clone()
        })
        .stdin(if cfg!(target_os = "linux") {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(if cfg!(target_os = "linux") {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    if db.login_heartbeat(op).await? {
        db.login_save(op, LoginPhase::Canceled).await?;
        std::fs::remove_dir_all(&home)?;
        return Ok(());
    }
    let mut child = command.spawn()?;
    #[cfg(target_os = "linux")]
    let mut heartbeat = child.stdin.take();
    let mut output = child.stdout.take().context("missing login output")?;
    let mut bytes = Vec::new();
    let mut output_open = true;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    let result: Result<bool> = async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let canceled = db.login_heartbeat(op).await? || broker.stopping.load(Ordering::Acquire) || broker.login_machine_revoked(headers).await?;
                    if canceled { return Ok(false); }
                    #[cfg(target_os = "linux")]
                    if let Err(error) = heartbeat.as_mut().context("polling heartbeat pipe missing")?.write_all(b"H").await {
                        if error.kind() != std::io::ErrorKind::BrokenPipe {return Err(error.into());}
                        if child.wait().await?.success() {return Ok(true);}
                        bail!("native login failed");
                    }
                    #[cfg(target_os = "linux")]
                    if let Some(current) = db.login_get(&op.user, &op.id).await? {
                        if current.holder != op.holder || current.epoch != op.epoch {bail!("polling incarnation fenced");}
                        *op = current;
                    }
                    if started.elapsed() > DEADLINE || (op.phase == LoginPhase::Starting && started.elapsed() > Duration::from_secs(30)) { bail!("login deadline expired"); }
                }
                status = child.wait() => { if !status?.success() { bail!("native login failed"); } return Ok(true); }
                byte = output.read_u8(), if output_open => {
                    match byte {
                        Ok(byte) => {
                            bytes.push(byte);
                            if bytes.len() > 32768 { bail!("login output exceeds bound"); }
                            #[cfg(not(target_os = "linux"))]
                            if byte == b'\n' && op.phase == LoginPhase::Starting && let Some(code) = challenge(&bytes)? {
                                op.payload.code = Some(code);
                                db.login_save(op,LoginPhase::Pending).await?;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => output_open=false,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }.await;
    // Await exit even on DB loss. A dropped RPC future alone is not settlement.
    if !matches!(result, Ok(true))
        && let Err(error) = async {
            #[cfg(target_os = "linux")]
            {
                drop(heartbeat.take());
                tokio::time::timeout(Duration::from_secs(12), child.wait())
                    .await
                    .map_err(|_| std::io::Error::other("polling supervisor did not settle"))??;
            }
            #[cfg(not(target_os = "linux"))]
            {
                child.start_kill()?;
                child.wait().await?;
            }
            Ok::<_, std::io::Error>(())
        }
        .await
    {
        op.phase = LoginPhase::Unresolved;
        return Err(error.into());
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(current) = db
            .login_get(&op.user, &op.id)
            .await
            .inspect_err(|_| op.phase = LoginPhase::Unresolved)?
            && current.holder == op.holder
            && current.epoch == op.epoch
        {
            *op = current;
        }
        if op.phase == LoginPhase::Canceled && op.polling_clear && op.payload.candidate.is_none() {
            std::fs::remove_dir_all(&home)?;
            return Ok(());
        }
        // Once published, PostgreSQL owns the grant. Loss or damage of the
        // local copy must never erase its identity or remove its refresh fence.
        if op.payload.candidate.is_some() {
            // The publication commit may have completed even when the
            // supervisor response was interrupted. The durable phase is the
            // authority once the candidate is visible in PostgreSQL.
            if op.phase != LoginPhase::Candidate {
                bail!("polling grant retained after interruption");
            }
            if let Err(error) = std::fs::remove_dir_all(&home)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
            return continue_candidate(broker, headers, op).await;
        }
        let settled: Result<()> = async {
            // A durable absence receipt also covers spawn failure and exit
            // before a live process identity could be recorded.
            if op.polling_clear {
                return Ok(());
            }
            let native: process::Process =
                serde_json::from_slice(&vault::private_read(&home.join("process.json"))?)?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while native.alive()? {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("native polling exit unproven")??;
            Ok(())
        }
        .await;
        if let Err(error) = settled {
            op.phase = LoginPhase::Unresolved;
            return Err(error);
        }
    }
    // A child can save an issued grant and then fail or be canceled. Capture it
    // after confirmed exit before classifying the process outcome.
    let path = home.join("auth.json");
    let captured = (|| -> Result<Option<Value>> {
        if !path.try_exists()? {
            return Ok(None);
        }
        let auth: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
        vault::validate_auth(&auth)?;
        Ok(Some(auth))
    })();
    op.payload.candidate = captured.inspect_err(|_| {
        // An unreadable grant is not evidence that no grant was issued.
        op.phase = LoginPhase::Unresolved;
    })?;
    if op.payload.candidate.is_none() {
        db.login_polling_clear(op).await?;
    }
    if !result? {
        op.payload.code = None;
        let phase = if op.payload.candidate.is_some() {
            op.payload.error = Some("login_stopped_account_requires_relogin".into());
            LoginPhase::Rejected
        } else {
            LoginPhase::Canceled
        };
        db.login_save(op, phase).await?;
        std::fs::remove_dir_all(&home)?;
        return Ok(());
    }
    if op.payload.candidate.is_none() {
        bail!("native login did not save credentials");
    }
    op.payload.code = None;
    if op.phase == LoginPhase::Unresolved {
        bail!("polling grant publication unresolved");
    }
    if op.phase != LoginPhase::Candidate {
        db.login_save(op, LoginPhase::Candidate).await?;
    }
    std::fs::remove_dir_all(&home)?;
    continue_candidate(broker, headers, op).await
}
async fn continue_candidate(
    broker: &Broker,
    headers: &HeaderMap,
    op: &mut LoginOperation,
) -> Result<()> {
    let db = database(broker).map_err(|_| anyhow::anyhow!("shared store unavailable"))?;
    if op.kind == LoginKind::Add
        && op.account_id.is_none()
        && let AddAdmission::Refused(reason) = db.login_admit_add(op).await?
    {
        op.payload.error = Some(reason.into());
        if matches!(reason, "account_identity_unresolved" | "login_canceled") {
            db.login_save(op, LoginPhase::Rejected).await?;
            if reason == "account_identity_unresolved" {
                let reporter = op.holder.clone();
                record_login_failure(broker, op, &reporter, false)
                    .await
                    .map_err(|_| anyhow::anyhow!("login failure report unavailable"))?;
            }
        } else {
            op.payload.candidate = None;
            db.login_save(op, LoginPhase::Failed).await?;
            eprintln!(
                "{}",
                json!({"operation":"login_add","stage":"admission","reason":reason})
            );
            broker.record_failure(
                "relogin_failed",
                "relogin_add_admission",
                StatusCode::CONFLICT,
            );
        }
        return Ok(());
    }
    broker.verify_shared_renewal(headers, op).await
}
