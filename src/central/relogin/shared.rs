//! PostgreSQL renewal uses shared receipts; only execution homes are replica-local.
use super::*;
use crate::central::storage::{
    CentralStore,
    login::{LoginOperation, LoginPayload, LoginPhase},
};
use std::{process::Stdio, time::Duration};
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
        phase => phase.as_str(),
    };
    (
        [("cache-control", "no-store")],
        Json(json!({
            "id":op.id,"alias":op.alias,"userId":op.user,"status":status,
            "verificationUrl":if op.phase == LoginPhase::Pending {Some(URL)} else {None},
            "userCode":if op.phase == LoginPhase::Pending {op.payload.code.as_deref()} else {None},
            "error":op.payload.error,
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
async fn lookup(
    broker: &Broker,
    headers: &HeaderMap,
    body: Body,
) -> Result<LoginOperation, HttpError> {
    let device = broker.authorize(headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let db = database(broker)?;
    let op = if request.id.is_empty() {
        let alias = managed::normalize_alias(&request.alias)
            .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
        db.login_active(&managed::account_key(&device.user, alias))
            .await
    } else {
        db.login_get(&device.user, &request.id).await
    }
    .map_err(|_| failure(broker))?
    .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    owned(broker, &device, &request, &op)?;
    Ok(op)
}
pub(super) async fn status(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let op = lookup(&broker, &headers, body).await?;
    Ok(view(&op))
}
pub(super) async fn cancel(
    broker: Broker,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let op = lookup(&broker, &headers, body).await?;
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
    let device = broker.authorize(&headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    validate_id(&request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let db = database(&broker)?;
    // Receipts precede availability and alias checks, including completed retries.
    if let Some(op) = db
        .login_get(&device.user, &request.id)
        .await
        .map_err(|_| failure(&broker))?
    {
        owned(&broker, &device, &request, &op)?;
        return Ok(view(&op));
    }
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_unavailable"));
    }
    let alias = managed::normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    let account = managed::account_key(&device.user, alias);
    if let Some(active) = db
        .login_active(&account)
        .await
        .map_err(|_| failure(&broker))?
    {
        owned(&broker, &device, &request, &active)?;
        return Ok(view(&active));
    }
    broker.owner(&device, alias).await?;
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let op = LoginOperation {
        user: device.user.clone(),
        id: request.id.clone(),
        account_id: account.clone(),
        alias: alias.into(),
        device: device.id.clone(),
        phase: LoginPhase::Starting,
        sequence: 0,
        holder: broker.holder_id.clone(),
        epoch: 1,
        payload: LoginPayload::default(),
    };
    if !db.login_create(&op).await.map_err(|_| failure(&broker))? {
        let existing = db
            .login_get(&device.user, &request.id)
            .await
            .map_err(|_| failure(&broker))?;
        let existing = match existing {
            Some(op) => Some(op),
            None => db
                .login_active(&account)
                .await
                .map_err(|_| failure(&broker))?,
        };
        let existing = existing.ok_or_else(|| failure(&broker))?;
        owned(&broker, &device, &request, &existing)?;
        return Ok(view(&existing));
    }
    let worker = broker.clone();
    let worker_headers = headers.clone();
    let mut worker_op = op.clone();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = run(&worker, &worker_headers, &mut worker_op).await {
            let mut detail = format!("{error:#}");
            if let Some(auth) = worker_op.payload.candidate.as_ref() {
                crate::central::resets::redact_auth_strings(auth, &mut detail);
            }
            let detail: String = detail.chars().take(4096).collect();
            eprintln!(
                "{}",
                json!({"operation":"login_renewal","stage":worker_op.phase.as_str(),"error":detail})
            );
            worker.record_failure("relogin_failed", "relogin", StatusCode::SERVICE_UNAVAILABLE);
            if matches!(
                worker_op.phase,
                LoginPhase::Completed | LoginPhase::Canceled | LoginPhase::Rejected
            ) {
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
                let _ = db.login_save(&mut worker_op, phase).await;
            }
        }
    });
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
async fn run(broker: &Broker, headers: &HeaderMap, op: &mut LoginOperation) -> Result<()> {
    let db = database(broker).map_err(|_| anyhow::anyhow!("shared store unavailable"))?;
    let root = broker.state.join("shared-logins").join(&op.id);
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
    if db.login_heartbeat(op).await? {
        db.login_save(op, LoginPhase::Canceled).await?;
        std::fs::remove_dir_all(&home)?;
        return Ok(());
    }
    let mut child = command.spawn()?;
    let mut output = child.stdout.take().context("missing login output")?;
    let mut bytes = Vec::new();
    let mut output_open = true;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    let result: Result<bool> = async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let canceled = db.login_heartbeat(op).await? || broker.stopping.load(Ordering::Acquire) || broker.authorize(headers).await.is_err();
                    if canceled { return Ok(false); }
                    if started.elapsed() > DEADLINE || (op.phase == LoginPhase::Starting && started.elapsed() > Duration::from_secs(30)) { bail!("login deadline expired"); }
                }
                status = child.wait() => { if !status?.success() { bail!("native login failed"); } return Ok(true); }
                byte = output.read_u8(), if output_open => {
                    match byte {
                        Ok(byte) => {
                            bytes.push(byte);
                            if bytes.len() > 32768 { bail!("login output exceeds bound"); }
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
    if !matches!(result, Ok(true)) {
        child.start_kill()?;
        child.wait().await?;
    }
    // A child can save an issued grant and then fail or be canceled. Capture it
    // after confirmed exit before classifying the process outcome.
    let path = home.join("auth.json");
    if path.try_exists()? {
        let auth: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
        vault::validate_auth(&auth)?;
        op.payload.candidate = Some(auth);
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
    db.login_save(op, LoginPhase::Candidate).await?;
    std::fs::remove_dir_all(&home)?;
    broker.verify_shared_renewal(headers, op).await
}
