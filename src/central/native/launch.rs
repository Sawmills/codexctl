//! One server account for one child, independent of the machine's active pointer.
use super::*;
use std::io::IsTerminal;
use std::os::unix::process::ExitStatusExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// This exclusive lock serializes directory creation and sweeping independently
// of the mode lease: other live server lanes must be able to keep running.
pub(crate) fn sweep_stale_launches() -> Result<()> {
    let lanes = root()?.join("lanes");
    if !lanes.try_exists()? {
        return Ok(());
    }
    let _sweep = vault::registry_lock(&lanes, "sweep.lock")?;
    for entry in std::fs::read_dir(&lanes)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("launch-")
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let owner = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path().join("owner.lock"))
        {
            Ok(owner) => match owner.try_lock() {
                Ok(()) => Some(owner),
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(error) => return Err(error.into()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // The owner lock is CLOEXEC and belongs only to the launcher. It avoids
        // PID reuse and releases even after SIGKILL. Legacy dirs have no owner.
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(owner);
    }
    Ok(())
}

// Hold this through helper persistence/output so cleanup cannot race a token
// refresh and recreate an approved connection after its directory was removed.
pub(super) fn lock_live_launch(connection: &Path) -> Result<vault::Lock> {
    let guard = vault::registry_lock(&root()?.join("lanes"), "sweep.lock")?;
    require_live_launch(connection)?;
    Ok(guard)
}

fn close_launch(directory: tempfile::TempDir) -> Result<()> {
    let _sweep = vault::registry_lock(&root()?.join("lanes"), "sweep.lock")?;
    directory.close().context("cannot remove launch connection")
}

pub(super) fn require_live_launch(connection: &Path) -> Result<()> {
    let owner = std::fs::File::open(connection.with_file_name("owner.lock"))
        .context("launch is no longer active")?;
    match owner.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Ok(()) => bail!("launch is no longer active"),
        Err(error) => Err(error).context("cannot check launch owner"),
    }
}

struct LaunchSignals {
    received: Arc<AtomicUsize>,
    handlers: Vec<signal_hook::SigId>,
}
impl LaunchSignals {
    fn register() -> Result<Self> {
        let mut signals = Self {
            received: Arc::new(AtomicUsize::new(0)),
            handlers: Vec::new(),
        };
        for signal in [libc::SIGHUP, libc::SIGTERM] {
            signals.handlers.push(signal_hook::flag::register_usize(
                signal,
                signals.received.clone(),
                signal as usize,
            )?);
        }
        Ok(signals)
    }
    fn received(&self) -> i32 {
        self.received.load(Ordering::Relaxed) as i32
    }
}
impl Drop for LaunchSignals {
    fn drop(&mut self) {
        for handler in &self.handlers {
            signal_hook::low_level::unregister(*handler);
        }
    }
}

pub(super) fn require_headroom(alias: &str, token: &TokenResponse) -> Result<()> {
    if token.statusline_usage.as_ref().is_some_and(|usage| {
        !matches!(
            (usage.allowed, usage.limit_reached),
            (Some(true), Some(false))
        ) && [usage.five_hour_used_percent, usage.weekly_used_percent]
            .into_iter()
            .flatten()
            .any(|used| used >= 100.0)
    }) {
        bail!(
            "server account {alias} is exhausted; choose another account (no reset was redeemed)"
        );
    }
    Ok(())
}

fn pinned_arguments(args: &[String]) -> Result<Vec<String>> {
    let mut config_args = Vec::new();
    let mut remaining = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            remaining.push(arg.clone());
            remaining.extend(args.cloned());
            break;
        }
        if arg == "--profile"
            || arg.starts_with("--profile=")
            || arg == "-p"
            || (arg.starts_with("-p") && arg.len() > 2)
        {
            bail!("--account cannot be combined with a Codex profile override");
        }
        let config = if arg == "-c" || arg == "--config" {
            Some(
                args.next()
                    .context("Codex config override requires a value")?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--config=").or_else(|| {
                arg.strip_prefix("-c")
                    .filter(|s| !s.is_empty())
                    .map(|s| s.strip_prefix('=').unwrap_or(s))
            })
        };
        if let Some(config) = config {
            let key = config.split('=').next().unwrap_or_default().trim();
            if key == "model_provider"
                || key == "profile"
                || key == "model_providers"
                || key.starts_with("model_providers.")
                || key.starts_with("profiles.")
            {
                bail!(
                    "--account cannot be combined with a provider or profile configuration override"
                );
            }
            config_args.extend(["-c".to_owned(), config.to_owned()]);
        } else {
            remaining.push(arg.clone());
        }
    }
    // Codex's global Clap Append option keeps only the deepest subcommand's
    // values when -c occurs at multiple levels. Keep user and launch overrides
    // together at the root so resume/exec cannot discard the private helper.
    config_args.extend(remaining);
    Ok(config_args)
}

/// Prepare consent and a private helper connection, then preserve the child exit status.
/// Does not activate a provider, change the host pointer, or redeem resets.
pub fn run_pinned_codex(alias: &str, args: &[String], allow_billing: bool) -> Result<i32> {
    store::validate_alias(alias)?;
    let args = pinned_arguments(args)?;
    if std::env::var_os("CODEX_HOME").is_some()
        || std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some()
    {
        bail!("server account launch refuses an inherited or pinned Codex home");
    }
    let doc = document(&codex_home()?)?;
    if let Some(selected) = doc.get("profile").and_then(Item::as_str)
        && doc
            .get("profiles")
            .and_then(|p| p.get(selected))
            .and_then(|p| p.get("model_provider"))
            .is_some()
    {
        bail!(
            "selected Codex profile overrides model_provider; remove that override before using --account"
        );
    }
    let paths = config::default_paths()?;
    let directory = root()?;
    sweep_stale_launches()?;
    let signals = LaunchSignals::register()?;
    let lease = vault::mode_lock(&directory, vault::LockMode::Shared)?;
    let catalog = super::super::remote::catalog()?
        .context("--account requires a connected account server")?;
    let account = catalog
        .accounts
        .iter()
        .find(|a| a.alias.eq_ignore_ascii_case(alias))
        .context("server account alias not found")?;
    if !account.available {
        bail!("server account {} is unavailable", account.alias);
    }
    if account.user_id != catalog.connection.user_id {
        bail!("server user identity changed");
    }
    let mut connection = Connection {
        user_id: Some(account.user_id.clone()),
        alias: Some(account.alias.clone()),
        server: catalog.connection.server.clone(),
        device_token_file: catalog.connection.token_file.clone(),
        account_id: account.account_id.clone(),
        revision: String::new(),
        allow_billing: false,
        launch_pinned: true,
        approved_billing_plan: None,
        approved_billing_class: None,
    };
    let token = fetch(&connection, false)?;
    validate_token_account(&token.access_token, &connection.account_id)?;
    require_headroom(&account.alias, &token)?;
    let bills = token.billing_class != Some(api::BillingClass::RateLimited);
    if bills && !allow_billing {
        if !std::io::stdin().is_terminal() {
            bail!(
                "server account {} may bill credits; use --allow-billing explicitly for this launch",
                account.alias
            );
        }
        if !dialoguer::Confirm::new()
            .with_prompt(format!(
                "This launch on {} may bill credits. Continue?",
                account.alias
            ))
            .default(false)
            .interact()?
        {
            bail!("server account launch billing declined");
        }
    }
    connection.allow_billing = bills;
    connection.approved_billing_plan = bills.then(|| token.chatgpt_plan_type.clone()).flatten();
    connection.approved_billing_class = bills.then_some(token.billing_class).flatten();
    connection.revision = token.revision.clone();
    let (prepared, _owner) = {
        // Follow migration's lock order and recheck registration after network I/O.
        let _store = store::lock(&paths)?;
        let _native = native_lock(&directory)?;
        super::super::remote::require_current_connection(&catalog.connection)?;
        super::super::remote::require_local_handoff(
            &paths,
            &serde_json::json!({"tokens":{"access_token":token.access_token,"account_id":token.chatgpt_account_id}}),
        )?;
        let launches = directory.join("lanes");
        store::ensure_private_dir(&launches)?;
        let _sweep = vault::registry_lock(&launches, "sweep.lock")?;
        let prepared = tempfile::Builder::new()
            .prefix("launch-")
            .tempdir_in(launches)?;
        let owner = vault::lock(prepared.path(), "owner.lock")?;
        save_connection(&prepared.path().join("connection.json"), &connection)?;
        (prepared, owner)
    };
    let path = prepared.path().join("connection.json");
    let helper_args = serde_json::to_string(&[
        "central-token",
        "--connection",
        path.to_str().context("connection path must be UTF-8")?,
    ])?;
    let helper = serde_json::to_string(
        std::env::current_exe()?
            .to_str()
            .context("helper path must be UTF-8")?,
    )?;
    let retries = |key| {
        doc.get("model_providers")
            .and_then(|providers| providers.get(PROVIDER))
            .and_then(|provider| provider.get(key))
            .and_then(Item::as_integer)
            .unwrap_or(12)
    };
    let mut command = std::process::Command::new("codex");
    for override_value in [
        "model_provider=\"codexctl-central\"".to_owned(),
        // Replace the entire provider first, so stale API-key or header settings
        // cannot compete with this launch's explicit token helper.
        "model_providers.codexctl-central={}".into(),
        "model_providers.codexctl-central.name=\"Central Codex\"".into(),
        "model_providers.codexctl-central.base_url=\"https://chatgpt.com/backend-api/codex\""
            .into(),
        "model_providers.codexctl-central.wire_api=\"responses\"".into(),
        "model_providers.codexctl-central.requires_openai_auth=false".into(),
        "model_providers.codexctl-central.http_headers={}".into(),
        "model_providers.codexctl-central.env_http_headers={}".into(),
        format!(
            "model_providers.codexctl-central.request_max_retries={}",
            retries("request_max_retries")
        ),
        format!(
            "model_providers.codexctl-central.stream_max_retries={}",
            retries("stream_max_retries")
        ),
        format!("model_providers.codexctl-central.auth.command={helper}"),
        format!("model_providers.codexctl-central.auth.args={helper_args}"),
        "model_providers.codexctl-central.auth.refresh_interval_ms=60000".into(),
        "model_providers.codexctl-central.auth.timeout_ms=210000".into(),
    ] {
        command.args(["-c", &override_value]);
    }
    command
        .args(args)
        .env("CODEXCTL_PINNED_ALIAS", &account.alias);
    if signals.received() != 0 {
        close_launch(prepared)?;
        return Ok(128 + signals.received());
    }
    let mut child = spawn_child_with_lease(&lease, &mut command)?;
    loop {
        let signal = signals.received();
        if signal != 0 {
            // Revoke approval before forwarding the signal, even if Codex ignores it.
            let cleanup = close_launch(prepared);
            // A filesystem or lock error must not strand the running child.
            // Keep that error until termination and reaping have been attempted.
            unsafe {
                libc::kill(child.id() as i32, signal);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while child.try_wait()?.is_none() {
                if std::time::Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            cleanup?;
            return Ok(128 + signal);
        }
        if let Some(status) = child.try_wait()? {
            close_launch(prepared)?;
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
