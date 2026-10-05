//! One server account for one child, independent of the machine's active pointer.
use super::*;
use std::io::IsTerminal;
use std::os::unix::process::ExitStatusExt;
use std::sync::{
    Arc, OnceLock,
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

const LAUNCH_SIGNALS: [i32; 3] = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT];
static SIGNAL_USERS: AtomicUsize = AtomicUsize::new(0);
static SIGNAL_ACTIONS: OnceLock<std::io::Result<Vec<i32>>> = OnceLock::new();

pub(crate) struct LaunchSignals {
    received: Arc<AtomicUsize>,
    handlers: Vec<signal_hook::SigId>,
    active: bool,
}
impl LaunchSignals {
    pub(crate) fn register() -> Result<Self> {
        let watched = SIGNAL_ACTIONS
            .get_or_init(|| {
                let mut watched = Vec::new();
                for signal in LAUNCH_SIGNALS {
                    // signal-hook keeps its dispatcher after unregister. Keep one
                    // default action when no guard exists, but preserve inherited
                    // SIG_IGN (for example nohup) and handlers owned by other code.
                    unsafe {
                        let mut action: libc::sigaction = std::mem::zeroed();
                        if libc::sigaction(signal, std::ptr::null(), &mut action) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        // Installing even a flag handler replaces SIG_IGN with a
                        // caught signal, which exec resets to SIG_DFL in children.
                        if action.sa_sigaction == libc::SIG_IGN {
                            continue;
                        }
                        if action.sa_sigaction == libc::SIG_DFL {
                            signal_hook::low_level::register(signal, move || {
                                if SIGNAL_USERS.load(Ordering::SeqCst) == 0 {
                                    let _ = signal_hook::low_level::emulate_default_handler(signal);
                                }
                            })?;
                        }
                    }
                    watched.push(signal);
                }
                Ok(watched)
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("cannot register default signal actions: {error}"))?;
        let mut signals = Self {
            received: Arc::new(AtomicUsize::new(0)),
            handlers: Vec::new(),
            active: false,
        };
        for &signal in watched {
            signals.handlers.push(signal_hook::flag::register_usize(
                signal,
                signals.received.clone(),
                signal as usize,
            )?);
        }
        SIGNAL_USERS.fetch_add(1, Ordering::SeqCst);
        signals.active = true;
        Ok(signals)
    }
    pub(crate) fn received(&self) -> i32 {
        self.received.load(Ordering::Relaxed) as i32
    }

    pub(crate) fn reraise(self) -> Result<()> {
        let received = Arc::clone(&self.received);
        drop(self);
        // Read after unregister so signals received during the final wait or
        // teardown are not discarded. Remaining guards retain recovery signals.
        let signal = received.load(Ordering::Relaxed) as i32;
        if signal != 0 {
            signal_hook::low_level::raise(signal)?;
        }
        Ok(())
    }
}
impl Drop for LaunchSignals {
    fn drop(&mut self) {
        if self.active {
            SIGNAL_USERS.fetch_sub(1, Ordering::SeqCst);
        }
        for handler in &self.handlers {
            signal_hook::low_level::unregister(*handler);
        }
    }
}

fn is_exhausted(token: &TokenResponse) -> bool {
    token.statusline_usage.as_ref().is_some_and(|usage| {
        !matches!(
            (usage.allowed, usage.limit_reached),
            (Some(true), Some(false))
        ) && [usage.five_hour_used_percent, usage.weekly_used_percent]
            .into_iter()
            .flatten()
            .any(|used| used >= 100.0)
    })
}

pub(super) fn require_headroom(alias: &str, token: &TokenResponse) -> Result<()> {
    if is_exhausted(token) {
        bail!(
            "server account {alias} is exhausted; choose another account (no reset was redeemed)"
        );
    }
    Ok(())
}

pub fn pinned_arguments(args: &[String]) -> Result<Vec<String>> {
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
            bail!("server account launches cannot be combined with a Codex profile override");
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
                    "server account launches cannot be combined with a provider or profile configuration override"
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

/// A private central connection and its launch lifetime guards.
pub struct PinnedLaunch {
    prepared: Option<tempfile::TempDir>,
    _owner: vault::Lock,
    lease: std::fs::File,
    alias: String,
    codex_args: Vec<String>,
    signals: Arc<LaunchSignals>,
}

impl Drop for PinnedLaunch {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        if let Err(error) = close_launch(prepared) {
            eprintln!("warning: failed to close private central launch: {error:#}");
        }
    }
}

impl PinnedLaunch {
    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn codex_args(&self) -> &[String] {
        &self.codex_args
    }

    pub fn close(mut self) -> Result<()> {
        self.revoke()
    }

    pub fn revoke(&mut self) -> Result<()> {
        if let Some(prepared) = self.prepared.take() {
            close_launch(prepared)?;
        }
        Ok(())
    }

    /// Replace a launch without losing signals received while choosing its successor.
    pub fn replace(mut self, previous: Self) -> Result<Self> {
        self.signals = Arc::clone(&previous.signals);
        previous.close()?;
        Ok(self)
    }

    pub fn received_signal(&self) -> i32 {
        self.signals.received()
    }

    /// Spawn on a PTY while retaining the mode lease in the child after exec.
    pub fn spawn_in_pty(
        &self,
        args: &[String],
        cwd: &Path,
        slave: &Path,
    ) -> Result<std::process::Child> {
        use std::os::{
            fd::AsRawFd,
            unix::{fs::OpenOptionsExt, process::CommandExt},
        };
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(slave)?;
        let mut command = std::process::Command::new("codex");
        command
            .args(self.codex_args())
            .args(pinned_arguments(args)?)
            .env("CODEXCTL_PINNED_ALIAS", self.alias())
            .current_dir(cwd)
            .stdin(tty.try_clone()?)
            .stdout(tty.try_clone()?)
            .stderr(tty);
        if std::env::var_os("TERM").is_none() {
            command.env("TERM", "xterm-256color");
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1
                    || libc::ioctl(std::io::stdin().as_raw_fd(), libc::TIOCSCTTY as _, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        spawn_child_with_lease(&self.lease, &mut command)
    }
}

/// Prepare an included-only private launch without changing the host pointer.
pub fn prepare_included_codex(alias: &str) -> Result<PinnedLaunch> {
    prepare_pinned_codex(alias, false, true)
}

fn prepare_pinned_codex(
    alias: &str,
    allow_billing: bool,
    included_only: bool,
) -> Result<PinnedLaunch> {
    store::validate_alias(alias)?;
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
            "selected Codex profile overrides model_provider; remove that override before a server account launch"
        );
    }
    let paths = config::default_paths()?;
    let directory = root()?;
    sweep_stale_launches()?;
    let lease = vault::mode_lock(&directory, vault::LockMode::Shared)?;
    let catalog = super::super::remote::catalog()?
        .context("server account launch requires a connected account server")?;
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
        session_id: vault::digest(&super::super::enrollment::random_bytes()),
    };
    let token = fetch(&connection, false)?;
    validate_token_account(&token.access_token, &connection.account_id)?;
    let exhausted = is_exhausted(&token);
    if included_only && token.billing_class != Some(api::BillingClass::RateLimited) {
        bail!(
            "server account {} no longer has verified included billing",
            account.alias
        );
    }
    if !allow_billing {
        require_headroom(&account.alias, &token)?;
    }
    let bills = exhausted || token.billing_class != Some(api::BillingClass::RateLimited);
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
    if exhausted {
        eprintln!(
            "codexctl: {} is exhausted; running on ChatGPT credits (--allow-billing)",
            account.alias
        );
    }
    connection.allow_billing = bills;
    connection.approved_billing_plan = bills.then(|| token.chatgpt_plan_type.clone()).flatten();
    connection.approved_billing_class = bills.then_some(token.billing_class).flatten();
    connection.revision = token.revision.clone();
    let (prepared, owner) = {
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
        store::atomic_write(
            &prepared.path().join("owner.json"),
            &serde_json::to_vec(&super::super::process::Process::capture(std::process::id())?)?,
        )?;
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
    let codex_args = [
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
    ]
    .into_iter()
    .flat_map(|value| ["-c".to_owned(), value])
    .collect();
    // Unregistering signal-hook handlers does not restore the default action.
    // Leave fallback launches untouched when any preparation step fails.
    let signals = Arc::new(LaunchSignals::register()?);
    Ok(PinnedLaunch {
        prepared: Some(prepared),
        _owner: owner,
        lease,
        alias: account.alias.clone(),
        codex_args,
        signals,
    })
}

/// Prepare consent and a private helper connection, then preserve the child exit status.
/// Does not activate a provider, change the host pointer, or redeem resets.
pub fn run_pinned_codex(alias: &str, args: &[String], allow_billing: bool) -> Result<i32> {
    let args = pinned_arguments(args)?;
    let launch = prepare_pinned_codex(alias, allow_billing, false)?;
    let mut command = std::process::Command::new("codex");
    command
        .args(launch.codex_args())
        .args(args)
        .env("CODEXCTL_PINNED_ALIAS", launch.alias());
    let signal = launch.received_signal();
    if signal != 0 {
        launch.close()?;
        return Ok(128 + signal);
    }
    let mut child = spawn_child_with_lease(&launch.lease, &mut command)?;
    loop {
        let signal = launch.received_signal();
        if signal != 0 {
            // Revoke approval before forwarding the signal, even if Codex ignores it.
            let cleanup = launch.close();
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
            launch.close()?;
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Live launcher PIDs and aliases, for read-only process ownership reporting.
/// The lock and process incarnation jointly reject stale directories and PID reuse.
pub fn launch_owners() -> Result<std::collections::BTreeMap<u32, String>> {
    let mut owners = std::collections::BTreeMap::new();
    let lanes = root()?.join("lanes");
    if !lanes.try_exists()? {
        return Ok(owners);
    }
    // Serialize this read-only scan with launch directory creation. In
    // particular, never probe a freshly-created owner.lock before its launcher
    // has acquired it.
    let _sweep = vault::registry_lock(&lanes, "sweep.lock")?;
    for entry in std::fs::read_dir(lanes)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("launch-")
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let connection_path = entry.path().join("connection.json");
        if require_live_launch(&connection_path).is_err() {
            continue;
        }
        // Older launches have no owner metadata. A sweep can also remove a
        // directory between these reads; neither proves ownership.
        let owner = std::fs::read(entry.path().join("owner.json"))
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<super::super::process::Process>(&bytes).ok()
            });
        let Some(owner) = owner else {
            continue;
        };
        // A held owner lock proves the launcher has not been swept. On hosts
        // where the process-time probe is unavailable, retain that durable
        // launch record and let rate report its inventory warning.
        if matches!(owner.alive(), Ok(false)) {
            continue;
        }
        let Ok(connection) = read_connection(&connection_path) else {
            continue;
        };
        if connection.launch_pinned
            && let Some(alias) = connection.alias
        {
            owners.insert(owner.pid(), alias);
        }
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(allowed: Option<bool>, limit_reached: Option<bool>) -> TokenResponse {
        TokenResponse {
            user_id: None,
            access_token: "token".into(),
            chatgpt_account_id: "account".into(),
            chatgpt_plan_type: Some("promax".into()),
            revision: "revision".into(),
            billing_class: Some(api::BillingClass::RateLimited),
            native_routing_supported: true,
            statusline_usage: Some(crate::statusline::Usage {
                age_seconds: 0,
                weekly_used_percent: Some(100.0),
                weekly_resets_at: None,
                five_hour_used_percent: None,
                five_hour_resets_at: None,
                allowed,
                limit_reached,
            }),
            label: None,
        }
    }

    #[test]
    fn headroom_requires_positive_admission_flags_at_one_hundred_percent() {
        assert!(require_headroom("premium", &token(Some(true), Some(false))).is_ok());
        assert!(require_headroom("premium", &token(None, None)).is_err());
        assert!(require_headroom("premium", &token(Some(true), Some(true))).is_err());
    }

    #[test]
    fn pinned_arguments_preserve_user_args_without_dropping_provider_overrides() {
        let args = vec![
            "resume".to_owned(),
            "session".to_owned(),
            "-c".to_owned(),
            "model=astra".to_owned(),
        ];
        let pinned = pinned_arguments(&args).unwrap();
        assert_eq!(pinned, vec!["-c", "model=astra", "resume", "session"]);
        assert!(pinned_arguments(&["--profile".to_owned(), "work".to_owned()]).is_err());
    }
}
