//! Independent Linux device-polling custody and durable exit evidence.
use super::{process, relogin, storage, vault};
use crate::store;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{io::Read, path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

fn intact_home(home: &Path, expected: &Value) -> Result<()> {
    let actual: Value =
        serde_json::from_slice(&vault::private_read(&home.join("operation.json"))?)?;
    if &actual != expected {
        bail!("polling operation evidence changed");
    }
    Ok(())
}

/// Internal account-server child entry point. Stdin carries lease heartbeats,
/// never credentials. It survives parent death long enough to settle polling.
#[doc(hidden)]
pub async fn supervise_login(state: &Path, key: &Path, binary: &Path) -> Result<()> {
    let home = std::env::var_os("CODEX_HOME").context("polling home missing")?;
    let home = Path::new(&home);
    let receipt: Value =
        serde_json::from_slice(&vault::private_read(&home.join("operation.json"))?)?;
    let user = receipt["user"]
        .as_str()
        .context("polling company user missing")?;
    let id = receipt["id"].as_str().context("polling request missing")?;
    let db = storage::runtime_store(state, key).await?;
    let mut op = db
        .login_get(user, id)
        .await?
        .context("polling request missing")?;
    if receipt["holder"] != op.holder || receipt["epoch"] != op.epoch {
        bail!("polling incarnation fenced");
    }
    if op.phase != storage::login::LoginPhase::Starting || op.polling_clear {
        bail!("polling launch already settled");
    }
    if db.login_heartbeat(&op).await? {
        intact_home(home, &receipt)?;
        if home.join("auth.json").try_exists()? {
            bail!("unexpected pre-launch grant");
        }
        db.login_record_polling_absence(&op).await?;
        db.login_save(&mut op, storage::login::LoginPhase::Canceled)
            .await?;
        return Ok(());
    }
    let mut command = Command::new(binary);
    command
        .args(process::LOGIN_ARGS)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process::isolate(&mut command);
    let mut child = command.spawn()?;
    let mut output = child.stdout.take().context("polling output missing")?;
    let native = process::Process::capture(child.id().context("polling child missing")?)?;
    store::atomic_write(&home.join("process.json"), &serde_json::to_vec(&native)?)?;

    let (send, mut receive) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut byte = [0];
        while matches!(input.read(&mut byte), Ok(1)) {
            if send.blocking_send(byte[0]).is_err() {
                break;
            }
        }
    });
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut authority_tick = tokio::time::interval(Duration::from_secs(1));
    let mut canceled = false;
    let mut bytes = Vec::new();
    let mut output_open = true;
    let result: Result<bool> = async {
        loop {
            tokio::select! {
                status = child.wait() => break Ok(status?.success()),
                _ = authority_tick.tick() => {
                    let (intent,live)=db.login_polling_authority(&op).await?;
                    canceled=intent;
                    if !live {bail!("polling authority lost");}
                    if canceled {
                        child.start_kill()?;
                        child.wait().await?;
                        break Ok(false);
                    }
                }
                byte = receive.recv() => {
                    if byte == Some(b'H') {
                        deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    } else {
                        child.start_kill()?;
                        child.wait().await?;
                        break Ok(false);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    child.start_kill()?;
                    child.wait().await?;
                    break Ok(false);
                }
                byte = output.read_u8(), if output_open => {
                    match byte {
                        Ok(byte) => {
                            bytes.push(byte);
                            if bytes.len() > 32768 {bail!("login output exceeds bound");}
                            if byte == b'\n' && op.phase == storage::login::LoginPhase::Starting
                                && let Some(code) = relogin::challenge(&bytes)? {
                                op.payload.code = Some(code);
                                db.login_save(&mut op,storage::login::LoginPhase::Pending).await?;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => output_open = false,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }.await;
    if result.is_err() {
        child.start_kill()?;
        child.wait().await?;
    }
    let success = matches!(result, Ok(true));
    // Only an awaited native exit permits inspection of grant absence.
    let path = home.join("auth.json");
    let captured = (|| -> Result<Option<Value>> {
        if !path.try_exists()? {
            intact_home(home, &receipt)?;
            return Ok(None);
        }
        let auth = serde_json::from_slice(&vault::private_read(&path)?)?;
        vault::validate_auth(&auth)?;
        Ok(Some(auth))
    })();
    let current = db
        .login_get(user, id)
        .await?
        .context("polling request missing")?;
    if current.holder != op.holder || current.epoch != op.epoch {
        bail!("polling settlement fenced");
    }
    op = current;
    let (intent, live) = db.login_polling_authority(&op).await?;
    canceled |= intent;
    let success = success && !canceled && live;
    match captured {
        Ok(None) => {
            db.login_record_polling_absence(&op).await?;
            if canceled && live {
                op.payload.code = None;
                db.login_save(&mut op, storage::login::LoginPhase::Canceled)
                    .await?;
                return Ok(());
            }
        }
        Ok(Some(auth)) => {
            op.payload.code = None;
            op.payload.candidate = Some(auth);
            if !success {
                op.payload.error = Some("login_stopped_account_requires_relogin".into());
            }
            let phase = if success {
                storage::login::LoginPhase::Candidate
            } else {
                storage::login::LoginPhase::Rejected
            };
            if let Err(error) = db.login_save(&mut op, phase).await {
                db.login_record_unpublished_grant(&op).await?;
                return Err(error);
            }
        }
        Err(error) => {
            db.login_record_unpublished_grant(&op).await?;
            return Err(error);
        }
    }
    result?;
    if !success {
        bail!("device polling stopped");
    }
    Ok(())
}
