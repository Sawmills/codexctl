//! Server-owned login candidates. Native device login never receives old credentials.
use super::*;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

const URL: &str = "https://auth.openai.com/codex/device";
const DEADLINE: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Starting,
    Pending,
    Committing,
    Promoted,
    Verified,
    Completed,
    Failed,
    Canceled,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    user: String,
    alias: String,
    original_revision: String,
    candidate_revision: Option<String>,
    phase: Phase,
    code: Option<String>,
    error: Option<String>,
    retired: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    alias: String,
    id: String,
}
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("invalid re-login operation");
    }
    Ok(())
}
fn directory(state: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(state.join("relogin").join(id))
}
fn save(state: &Path, record: &Record) -> Result<()> {
    store::atomic_write(
        &directory(state, &record.id)?.join("record.json"),
        &serde_json::to_vec(record)?,
    )
}
fn publish(state: &Path, record: &Record) -> Result<()> {
    let root = state.join("relogin");
    store::ensure_private_dir(&root)?;
    // An unpublished directory cannot have spawned a process and is never inventoried.
    let prepared = tempfile::Builder::new()
        .prefix(".pending-login-")
        .tempdir_in(state)?;
    store::ensure_private_dir(&prepared.path().join("home"))?;
    store::atomic_write(&prepared.path().join("home/spawn-failed"), b"not-started")?;
    store::atomic_write(
        &prepared.path().join("record.json"),
        &serde_json::to_vec(record)?,
    )?;
    store::sync_directory(prepared.path())?;
    std::fs::rename(prepared.path(), directory(state, &record.id)?)?;
    store::sync_directory(&root)
}
fn load(state: &Path, id: &str) -> Result<Record> {
    let record: Record = serde_json::from_slice(&vault::private_read(
        &directory(state, id)?.join("record.json"),
    )?)?;
    if record.id != id {
        bail!("re-login operation identity changed");
    }
    Ok(record)
}
fn current(state: &Path) -> Result<Option<Record>> {
    let path = state.join("relogin-current");
    if !path.try_exists()? {
        return Ok(None);
    }
    load(state, std::str::from_utf8(&vault::private_read(&path)?)?).map(Some)
}
fn records(state: &Path) -> Result<Vec<Record>> {
    let root = state.join("relogin");
    if !root.try_exists()? {
        return Ok(Vec::new());
    }
    std::fs::read_dir(root)?
        .map(|entry| {
            let entry = entry?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid re-login directory"))?;
            load(state, &id)
        })
        .collect()
}
fn candidate(state: &Path, record: &Record) -> Result<Option<Value>> {
    let dir = directory(state, &record.id)?;
    let durable = dir.join("candidate.json");
    let native = dir.join("home/auth.json");
    let path = if durable.try_exists()? {
        durable
    } else {
        native
    };
    if !path.try_exists()? {
        return Ok(None);
    }
    let auth = serde_json::from_slice(&vault::private_read(&path)?)?;
    vault::validate_auth(&auth)?;
    Ok(Some(auth))
}
fn stopped(state: &Path, record: &Record) -> Result<()> {
    let home = directory(state, &record.id)?.join("home");
    if vault::private_read(&home.join("exited")).is_ok_and(|v| v == b"confirmed") {
        return Ok(());
    }
    previous_owner_exited(&home)
}
fn job_key(state: &Path, id: &str) -> String {
    format!("{}\0{id}", state.display())
}
fn reply(record: &Record) -> Value {
    let status = match record.phase {
        Phase::Starting => "starting",
        Phase::Pending => "pending",
        Phase::Committing | Phase::Promoted | Phase::Verified => "verifying",
        Phase::Completed => "completed",
        Phase::Failed => "failed",
        Phase::Canceled => "canceled",
    };
    json!({"id":record.id,"alias":record.alias,"userId":record.user,"status":status,
        "verificationUrl":if record.phase == Phase::Pending { Some(URL) } else { None },
        "userCode":if record.phase == Phase::Pending { record.code.as_deref() } else { None },
        "error":record.error})
}
fn response(record: &Record) -> Response {
    ([("cache-control", "no-store")], Json(reply(record))).into_response()
}

pub(super) async fn status(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.owner(&device, &request.alias).await?;
    let state = broker
        .state
        .join("accounts")
        .join(account_key(&device.user, &request.alias));
    let record = if request.id.is_empty() {
        current(&state).and_then(|r| r.context("no login operation"))
    } else {
        load(&state, &request.id)
    }
    .map_err(|_| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if record.user != device.user || !record.alias.eq_ignore_ascii_case(&request.alias) {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    broker.authorize(&headers)?;
    Ok(response(&record))
}
pub(super) async fn cancel(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.owner(&device, &request.alias).await?;
    let state = broker
        .state
        .join("accounts")
        .join(account_key(&device.user, &request.alias));
    let record = load(&state, &request.id)
        .map_err(|_| broker.error(StatusCode::NOT_FOUND, "relogin_not_found"))?;
    if record.user != device.user || !record.alias.eq_ignore_ascii_case(&request.alias) {
        return Err(broker.error(StatusCode::NOT_FOUND, "relogin_not_found"));
    }
    if let Some(flag) = broker
        .relogins
        .lock()
        .expect("re-login lock")
        .get(&job_key(&state, &record.id))
    {
        flag.store(true, Ordering::Release);
    }
    // This is an acknowledgment, not proof of process exit. Poll for the terminal result.
    Ok(response(&record))
}
pub(super) async fn start(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
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
    broker.authorize(&headers)?;
    Ok(response)
}
async fn start_owned(
    broker: Broker,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers)?;
    validate_id(&request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let owner = broker.owner(&device, &request.alias).await?;
    let _import = broker.imports.lock().await;
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::CONFLICT, "relogin_unavailable"));
    }
    let state = owner.lock().await.state.clone();
    if let Some(record) = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
        && matches!(
            record.phase,
            Phase::Committing | Phase::Promoted | Phase::Verified
        )
        && !broker
            .relogins
            .lock()
            .expect("re-login lock")
            .contains_key(&job_key(&state, &record.id))
    {
        let mut original = owner.lock().await;
        if let Some(rpc) = original.rpc.as_mut() {
            rpc.settle_and_stop()
                .await
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        } else {
            previous_owner_exited(&original.home)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        }
        original.rpc = None;
        let repair = recover(&state, &broker.key)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
        if repair.blocked {
            return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_commit_pending"));
        }
        original.vault = vault::load(&state, &broker.key)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        original.refresh_enabled = true;
        original.available = true;
        if verify_replacement(&mut original, &broker.binary)
            .await
            .is_err()
        {
            original.available = false;
            return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "relogin_failed"));
        }
        return Ok(response(
            &current(&state)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?,
        ));
    }
    if directory(&state, &request.id)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?
        .join("record.json")
        .try_exists()
        .unwrap_or(true)
    {
        let record = load(&state, &request.id)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?;
        return Ok(response(&record));
    }
    if let Some(record) = current(&state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
    {
        if broker
            .relogins
            .lock()
            .expect("re-login lock")
            .contains_key(&job_key(&state, &record.id))
        {
            return Ok(response(&record));
        }
        stopped(&state, &record)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
    }
    let permit = broker
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_busy"))?;
    let mut original = owner.lock().await;
    let record = Record {
        id: request.id,
        user: device.user,
        alias: original.vault.alias.clone(),
        original_revision: vault::digest(
            &serde_json::to_vec(&original.vault.auth)
                .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
        ),
        candidate_revision: None,
        phase: Phase::Starting,
        code: None,
        error: None,
        retired: false,
    };
    publish(&state, &record)
        .and_then(|_| store::atomic_write(&state.join("relogin-current"), record.id.as_bytes()))
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    original.available = false;
    drop(original);
    let flag = Arc::new(AtomicBool::new(false));
    broker
        .relogins
        .lock()
        .expect("re-login lock")
        .insert(job_key(&state, &record.id), flag.clone());
    let worker = broker.clone();
    let initial_id = record.id.clone();
    let initial_state = state.clone();
    let worker_headers = headers.clone();
    let (ready, initial) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _permit = permit;
        let mut record = record;
        let result = run(&worker, &worker_headers, &owner, &mut record, &flag, ready).await;
        if let Err(error) = result {
            let _ = error; // Never log native output or credential data.
            owner.lock().await.available = false;
            match load(&state, &record.id) {
                Ok(durable) => {
                    record = durable;
                    if !matches!(
                        record.phase,
                        Phase::Committing | Phase::Promoted | Phase::Verified
                    ) {
                        record.phase = Phase::Failed;
                    }
                    record.code = None;
                    record.error.get_or_insert_with(|| "relogin_failed".into());
                    if save(&state, &record).is_err() {
                        worker.ownership_unresolved.store(true, Ordering::Release);
                    }
                }
                Err(_) => {
                    worker.ownership_unresolved.store(true, Ordering::Release);
                }
            }
            worker.record_failure("relogin_failed", "relogin", StatusCode::SERVICE_UNAVAILABLE);
        }
        worker
            .relogins
            .lock()
            .expect("re-login lock")
            .remove(&job_key(&state, &record.id));
    });
    drop(_import);
    let record = match initial.await {
        Ok(record) => record,
        Err(_) => load(&initial_state, &initial_id)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
    };
    broker.authorize(&headers)?;
    Ok(response(&record))
}

// Native CLI output is not a transport API. Accept only the pinned, bounded prompt.
fn challenge(bytes: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(bytes)?;
    let mut clean = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() != Some('[') {
                bail!("unsupported login output");
            }
            let mut count = 0;
            loop {
                let c = chars.next().context("incomplete login output")?;
                if c == 'm' {
                    break;
                }
                if !c.is_ascii_digit() && c != ';' {
                    bail!("unsupported login output");
                }
                count += 1;
                if count > 24 {
                    bail!("unsupported login output");
                }
            }
        } else {
            clean.push(c);
        }
    }
    let lines: Vec<_> = clean.lines().map(str::trim).collect();
    for index in 0..lines.len() {
        if lines[index] == "1. Open this link in your browser and sign in to your account"
            && lines.get(index + 1) == Some(&URL)
        {
            for pair in lines[index + 2..].windows(2) {
                if pair[0] == "2. Enter this one-time code (expires in 15 minutes)" {
                    let code = pair[1];
                    if code.is_empty()
                        || code.len() > 128
                        || !code.bytes().all(|c| c.is_ascii_graphic())
                    {
                        bail!("invalid login code");
                    }
                    return Ok(Some(code.into()));
                }
            }
        }
    }
    Ok(None)
}
async fn run(
    broker: &Broker,
    headers: &HeaderMap,
    owner: &Arc<Mutex<Owner>>,
    record: &mut Record,
    flag: &AtomicBool,
    ready: tokio::sync::oneshot::Sender<Record>,
) -> Result<()> {
    let state;
    {
        let _import = broker.imports.lock().await;
        let mut old = owner.lock().await;
        state = old.state.clone();
        if let Some(rpc) = old.rpc.as_mut() {
            rpc.settle_and_stop().await?;
        } else {
            previous_owner_exited(&old.home)?;
        }
        old.rpc = None;
        old.snapshot()?;
        record.original_revision = vault::digest(&serde_json::to_vec(&old.vault.auth)?);
        save(&state, record)?;
    }
    let home = directory(&state, &record.id)?.join("home");
    let binary = super::super::process::owner_binary(&broker.binary)?;
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
    super::super::process::isolate(&mut command);
    std::fs::remove_file(home.join("spawn-failed"))?;
    store::sync_directory(&home)?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            store::atomic_write(&home.join("spawn-failed"), b"not-started")?;
            return Err(e.into());
        }
    };
    let recorded = (|| {
        let process = super::super::process::Process::capture(child.id().context("login exited")?)?;
        store::atomic_write(&home.join("pid"), &serde_json::to_vec(&process)?)
    })();
    if let Err(error) = recorded {
        child.start_kill()?;
        child.wait().await?;
        store::atomic_write(&home.join("exited"), b"confirmed")?;
        return Err(error);
    }
    let mut output = child.stdout.take().context("missing login output")?;
    let started = tokio::time::Instant::now();
    let mut bytes = Vec::new();
    let mut ready = Some(ready);
    let mut output_open = true;
    let outcome = async { Ok::<_,anyhow::Error>(loop {
        if ready.is_some() && started.elapsed() > Duration::from_secs(30) {
            record.error=Some("unsupported_login_output".into());
            bail!("native login challenge timed out");
        }
        if flag.load(Ordering::Acquire)
            || broker.stopping.load(Ordering::Acquire)
            || broker.authorize(headers).is_err()
            || started.elapsed() >= DEADLINE
        {
            child.start_kill()?;
            child.wait().await?;
            record.phase = Phase::Canceled;
            record.code = None;
            record.error = Some("login_stopped_account_requires_relogin".into());
            save(&state, record)?;
            broker.record_failure("relogin_stopped", "relogin", StatusCode::CONFLICT);
            break None;
        }
        tokio::select! {
            result = child.wait() => { break Some(result?); }
            read = output.read_u8(), if ready.is_some() && output_open => {
                match read {
                    Ok(b) => {
                        bytes.push(b);
                        if bytes.len() > 32768 { record.error = Some("unsupported_login_output".into()); bail!("login output too large"); }
                        if b == b'\n' && let Some(code) = challenge(&bytes)? {
                                record.phase = Phase::Pending; record.code = Some(code); save(&state, record)?;
                                if let Some(ready) = ready.take() { let _ = ready.send(record.clone()); }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => { output_open = false; }
                    Err(e) => return Err(e.into()),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }) }.await;
    if outcome.is_err() {
        child.start_kill()?;
        child.wait().await?;
    }
    store::atomic_write(&home.join("exited"), b"confirmed")?;
    // Cancellation can race with native file persistence. Inventory even on failure.
    let auth = candidate(&state, record)?;
    if let Some(auth) = &auth {
        store::atomic_write(
            &directory(&state, &record.id)?.join("candidate.json"),
            &serde_json::to_vec(auth)?,
        )?;
        let _import = broker.imports.lock().await;
        let matching = owner.lock().await.validate_owned_auth(auth).is_ok();
        if !matching {
            record.error = Some("wrong_account".into());
            record.phase = Phase::Failed;
            record.code = None;
            save(&state, record)?;
            fence(broker, auth).await?;
            bail!("wrong account retained");
        }
    }
    let result = outcome?;
    if result.is_none() {
        return Ok(());
    }
    if !result.is_some_and(|s| s.success()) {
        bail!("native login failed");
    }
    let auth = auth.context("native login did not save credentials")?;
    let _import = broker.imports.lock().await;
    broker
        .authorize(headers)
        .map_err(|_| anyhow::anyhow!("device revoked"))?;
    let mut old = owner.lock().await;
    promote(&state, &broker.key, record, &auth)?;
    old.vault = vault::load(&state, &broker.key)?;
    old.verification_input = Some(old.vault.auth.clone());
    old.refresh_enabled = true;
    old.available = true;
    verify_replacement(&mut old, &broker.binary).await?;
    Ok(())
}

async fn fence(broker: &Broker, auth: &Value) -> Result<()> {
    let owners = broker.owners.read().await.clone();
    for (_, owner) in owners.values() {
        let mut owner = owner.lock().await;
        if !overlaps(&owner.vault.auth, auth) {
            continue;
        }
        owner.available = false;
        if let Some(rpc) = owner.rpc.as_mut() {
            rpc.settle_and_stop().await?;
        } else {
            previous_owner_exited(&owner.home)?;
        }
        owner.rpc = None;
        owner.snapshot()?;
    }
    Ok(())
}
fn promote(state: &Path, key: &Path, record: &mut Record, auth: &Value) -> Result<()> {
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
        refresh_enabled: true,
        limits: None,
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
pub(super) fn needs_verification(state: &Path) -> Result<bool> {
    Ok(current(state)?.is_some_and(|r| matches!(r.phase, Phase::Promoted | Phase::Verified)))
}
pub(super) async fn verify_replacement(owner: &mut Owner, binary: &Path) -> Result<()> {
    if owner.rpc.is_none() {
        launch_owner(owner, binary).await?;
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
    record.phase = Phase::Verified;
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
fn retire_reservations(accounts: &Path, auth: &Value, skip: &Path) -> Result<()> {
    for entry in std::fs::read_dir(accounts)? {
        let state = entry?.path();
        for mut record in records(&state)? {
            if directory(&state, &record.id)? == skip {
                continue;
            }
            if record.phase == Phase::Completed || record.retired {
                continue;
            }
            if candidate(&state, &record)?.is_some_and(|a| overlaps(&a, auth)) {
                stopped(&state, &record)?;
                record.retired = true;
                save(&state, &record)?;
            }
        }
    }
    Ok(())
}
pub(super) fn check_import(state: &Path, user: &str, input: &Import) -> Result<()> {
    let selected = state.join("accounts").join(account_key(user, &input.alias));
    if current(&selected)?.is_some_and(|r| r.phase != Phase::Completed) {
        bail!("selected account is reserved by re-login");
    }
    let auth = &input.auth;
    for entry in std::fs::read_dir(state.join("accounts"))? {
        let state = entry?.path();
        for record in records(&state)? {
            if record.phase == Phase::Completed || record.retired {
                continue;
            }
            if candidate(&state, &record)?.is_some_and(|a| overlaps(&a, auth)) {
                bail!("re-login candidate reserves this account");
            }
        }
    }
    Ok(())
}
#[derive(Default)]
pub(super) struct Recovery {
    pub blocked: bool,
    pub verify: bool,
    pub quarantined: Vec<Value>,
}
pub(super) fn recover(state: &Path, key: &Path) -> Result<Recovery> {
    let latest = current(state)?;
    let mut result = Recovery {
        blocked: latest.as_ref().is_some_and(|r| {
            !matches!(
                r.phase,
                Phase::Completed | Phase::Promoted | Phase::Verified
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
        if is_latest && record.phase == Phase::Verified {
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
                store::atomic_write(
                    &directory(state, &record.id)?.join("candidate.json"),
                    &serde_json::to_vec(auth)?,
                )?;
                if promote(state, key, &mut record, auth).is_ok() {
                    result.verify = true;
                    result.blocked = false;
                    continue;
                }
            }
        }
        if is_latest && matches!(record.phase, Phase::Promoted | Phase::Verified) {
            result.verify = true;
            continue;
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
            result.quarantined.push(auth);
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
        refresh_enabled: false,
        limits: None,
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

#[cfg(test)]
mod tests {
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
        store::atomic_write(&state.join("relogin-current"), record.id.as_bytes()).unwrap();
        (root, state, key, record)
    }
    #[test]
    fn recovery_finishes_a_journal_write_even_when_explicit_login_has_the_same_timestamp() {
        let (_root, state, key, mut record) = fixture();
        let mut fresh = auth(Some("known-login"), 0);
        fresh["tokens"]["refresh_token"] = json!("new-explicit-grant");
        let before = std::fs::read(state.join("vault.enc")).unwrap();
        let candidate = directory(&state, &record.id)
            .unwrap()
            .join("candidate.json");
        store::atomic_write(&candidate, &serde_json::to_vec(&fresh).unwrap()).unwrap();
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
            assert_eq!(recovery.quarantined, vec![unexpected]);
            assert_eq!(std::fs::read(state.join("vault.enc")).unwrap(), before);
        }
    }
    #[test]
    fn recovery_refuses_a_live_login_child_before_promotion() {
        let (_root, state, key, record) = fixture();
        let home = directory(&state, &record.id).unwrap().join("home");
        std::fs::remove_file(home.join("spawn-failed")).unwrap();
        store::atomic_write(
            &home.join("pid"),
            &serde_json::to_vec(
                &super::super::super::process::Process::capture(std::process::id()).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
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
        assert!(recovery.blocked && !recovery.verify && recovery.quarantined.is_empty());
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
}
