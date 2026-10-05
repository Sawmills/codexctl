use super::*;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
// Native CLI output is not a transport API. Accept only the pinned, bounded prompt.
pub(super) fn challenge(bytes: &[u8]) -> Result<Option<String>> {
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
pub(super) async fn run(
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
    let home = std::fs::canonicalize(directory(&state, &record.id)?.join("home"))?;
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
    let mut child = spawn_login(&broker.imports, &state, &home, record, &mut command).await?;
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
                record.phase = Phase::Canceled;
                record.code = None;
                record.error = Some("login_stopped_account_requires_relogin".into());
                broker.record_failure("relogin_stopped", "relogin", StatusCode::CONFLICT);
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
                            if b == b'\n' && let Some(code) = challenge(&bytes)? {
                                let _import = broker.imports.lock().await;
                                record.phase = Phase::Pending;
                                record.code = Some(code);
                                save(&state, record)?;
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
    let _import = broker.imports.lock().await;
    let durable = load(&state, &record.id)?;
    // A different account's repair may have retired this stopped candidate while
    // this worker waited for the import lock. Never overwrite that decision.
    if durable.retired {
        *record = durable;
        return Ok(());
    }
    record.child = Child::Exited;
    record.candidate = candidate(&state, &durable).ok().flatten();
    save(&state, record)?;
    if let Some(auth) = &record.candidate
        && owner.lock().await.validate_owned_auth(auth).is_err()
    {
        record.error = Some("wrong_account".into());
        record.phase = Phase::Failed;
        record.code = None;
        save(&state, record)?;
        fence(broker, auth).await?;
        bail!("wrong account retained");
    }
    let result = outcome?;
    if result.is_none() {
        return Ok(());
    }
    if !result.is_some_and(|s| s.success()) {
        bail!("native login failed");
    }
    let auth = record
        .candidate
        .clone()
        .context("native login did not save credentials")?;
    broker
        .authorize(headers)
        .await
        .map_err(|_| anyhow::anyhow!("device revoked"))?;
    check_claim(
        state.parent().context("missing registry")?,
        &broker.key,
        &state,
        &auth,
    )?;
    let mut old = owner.lock().await;
    promote(&state, &broker.key, record, &auth)?;
    old.vault = vault::load(&state, &broker.key)?;
    old.verification_input = Some(old.vault.auth.clone());
    old.refresh_enabled = true;
    old.available = true;
    if let Err(error) = verify_replacement(&mut old, &broker.binary, &_import).await {
        finish_rejection(&mut old).await?;
        return Err(error);
    }
    Ok(())
}

async fn fence(broker: &Broker, auth: &Value) -> Result<()> {
    let owners = broker.owners.read().await.clone();
    let mut failure = None;
    for (_, owner) in owners.values() {
        let mut owner = owner.lock().await;
        let inventory = identity_inventory(&owner.state, &broker.key, &owner.home);
        if !inventory.needs_fence(auth) {
            continue;
        }
        owner.fence(false);
        let stopped = if let Some(rpc) = owner.rpc.as_mut() {
            rpc.settle_and_stop().await.map(|_| ())
        } else {
            previous_owner_exited(&owner.home)
        };
        if let Err(error) = stopped {
            failure.get_or_insert(error);
            continue;
        }
        owner.rpc = None;
        // A conflicting journal remains reserved. Report its validation failure
        // after stopping every matching refresh process, rather than returning early.
        if let Err(error) = owner.snapshot() {
            failure.get_or_insert(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) async fn spawn_login(
    imports: &Mutex<()>,
    state: &Path,
    home: &Path,
    record: &mut Record,
    command: &mut Command,
) -> Result<tokio::process::Child> {
    // Keep intent, spawn, and process evidence in one migration critical section.
    // A queued inventory must not turn a transient spawn window into a global fence.
    let _import = imports.lock().await;
    record.child = Child::Spawning;
    save(state, record)?;
    std::fs::remove_file(home.join("spawn-failed"))?;
    store::sync_directory(home)?;
    // Direct spawn on the account server's long-lived main thread. A blocking-pool
    // thread must never be the parent of a PDEATHSIG-protected login child.
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            record.child = Child::NotStarted;
            save(state, record)?;
            return Err(error.into());
        }
    };
    let recorded = (|| {
        let process = process::Process::capture(child.id().context("login exited")?)?;
        record.child = Child::Running(process);
        save(state, record)
    })();
    if let Err(error) = recorded {
        child.start_kill()?;
        child.wait().await?;
        record.child = Child::Exited;
        save(state, record)?;
        return Err(error);
    }
    Ok(child)
}
