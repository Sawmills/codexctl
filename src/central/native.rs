//! Native TUI credentials through Codex's command-backed provider authentication.
use super::{
    server::{TokenRequest, TokenResponse},
    vault,
};
use crate::{api, config, store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table, value};

pub(super) const PROVIDER: &str = "codexctl-central";
const ACTIVE_POINTER: &str = ".active-account";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    server: String,
    device_token_file: PathBuf,
    account_id: String,
    revision: String,
    #[serde(default)]
    allow_billing: bool,
    #[serde(default)]
    approved_billing_plan: Option<String>,
    #[serde(default)]
    approved_billing_class: Option<api::BillingClass>,
}
#[derive(Serialize, Deserialize)]
struct Activation {
    home: PathBuf,
    original_provider: Option<String>,
}
fn root() -> Result<PathBuf> {
    Ok(config::default_paths()?.codexctl_dir().join("central"))
}
fn connection_path(alias: &str) -> Result<PathBuf> {
    let alias = store::validate_alias(alias)?;
    Ok(root()?.join(format!("{alias}.json")))
}
fn codex_home() -> Result<PathBuf> {
    Ok(config::default_paths()?.codex_home())
}
pub(super) fn native_lock(directory: &Path) -> Result<vault::Lock> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match vault::lock(directory, "native.lock") {
            Ok(file) => return Ok(file),
            Err(error)
                if error
                    .downcast_ref::<std::fs::TryLockError>()
                    .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
}
fn config_path(home: &Path) -> Result<PathBuf> {
    let path = home.join("config.toml");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        Ok(std::fs::canonicalize(path)?)
    } else {
        Ok(path)
    }
}
fn write_config(destination: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = destination
        .parent()
        .context("configuration has no parent")?;
    let permissions = match std::fs::metadata(destination) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions)?;
    }
    file.as_file().sync_all()?;
    file.persist(destination).map_err(|e| e.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn require_local_mode() -> Result<()> {
    require_local_mode_from(&config::default_paths()?)
}
fn require_local_mode_from(paths: &config::Paths) -> Result<()> {
    if !paths
        .codexctl_dir()
        .join("central/.native-active.json")
        .try_exists()?
    {
        return Ok(());
    }
    let home = paths.codex_home();
    if document(&home)?
        .get("model_provider")
        .and_then(Item::as_str)
        == Some(PROVIDER)
    {
        bail!(
            "remote provider is active; run codexctl codex or disconnect before local account wrappers"
        );
    }
    Ok(())
}

#[must_use = "keep this guard alive for the complete local credential operation"]
pub struct LocalOperation {
    _guard: std::fs::File,
    paths: config::Paths,
}

pub fn local_operation(paths: &config::Paths) -> Result<LocalOperation> {
    require_local_mode_from(paths)?;
    let operation = local_lease(paths)?;
    operation.revalidate()?;
    Ok(operation)
}

/// Restore the local provider and retain a lease through its credential swap.
pub fn local_selection() -> Result<LocalOperation> {
    let operation = local_lease(&config::default_paths()?)?;
    deactivate()?;
    operation.revalidate()?;
    Ok(operation)
}

fn local_lease(paths: &config::Paths) -> Result<LocalOperation> {
    let guard = vault::mode_lock(
        &paths.codexctl_dir().join("central"),
        vault::LockMode::Shared,
    )?;
    Ok(LocalOperation {
        _guard: guard,
        paths: paths.clone(),
    })
}

impl LocalOperation {
    pub fn revalidate(&self) -> Result<()> {
        require_local_mode_from(&self.paths)
    }

    pub fn run_child(
        &self,
        command: &mut std::process::Command,
    ) -> Result<std::process::ExitStatus> {
        run_child_with_lease(&self._guard, command)
    }
}

fn run_child_with_lease(
    guard: &std::fs::File,
    command: &mut std::process::Command,
) -> Result<std::process::ExitStatus> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let fd = guard.as_raw_fd();
    // The child retains the mode lease if its launcher dies.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.status().context("Codex process failed to run")
}

/// Launch the active server account without restoring a resumed thread's old provider.
/// Local account launches remain the caller's responsibility.
pub fn run_codex(args: &[String]) -> Result<Option<i32>> {
    use std::os::unix::process::ExitStatusExt;
    let paths = config::default_paths()?;
    let directory = paths.codexctl_dir().join("central");
    let marker = directory.join(".native-active.json");
    if !marker.try_exists()? {
        return Ok(None);
    }
    let lease = vault::mode_lock(&directory, vault::LockMode::Shared)?;
    {
        let _lock = native_lock(&directory)?;
        if !marker.try_exists()? {
            return Ok(None);
        }
        let home = paths.codex_home();
        if document(&home)?
            .get("model_provider")
            .and_then(Item::as_str)
            != Some(PROVIDER)
        {
            return Ok(None);
        }
        if std::env::var_os("CODEX_HOME").is_some()
            || std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some()
        {
            bail!("server account launch refuses an inherited or pinned Codex home");
        }
        let activation: Activation = serde_json::from_slice(&vault::private_read(&marker)?)?;
        if activation.home != home {
            bail!("remote provider is active in another Codex home");
        }
    }
    // Codex 0.160 restores the saved provider unless the launch explicitly overrides it.
    // Keep this before user arguments so a prompt after `--` remains a prompt.
    let mut command = std::process::Command::new("codex");
    command
        .args(["-c", "model_provider=\"codexctl-central\""])
        .args(args);
    let status = run_child_with_lease(&lease, &mut command)?;
    Ok(Some(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    ))
}

pub(super) fn exclusive_mode(paths: &config::Paths) -> Result<std::fs::File> {
    vault::mode_lock(
        &paths.codexctl_dir().join("central"),
        vault::LockMode::Exclusive,
    )
}
fn fetch(connection: &Connection, refresh: bool) -> Result<TokenResponse> {
    super::transport::origin(&connection.server)?;
    let secret = String::from_utf8(vault::private_read(&connection.device_token_file)?)?;
    let http = super::transport::blocking()?;
    let response = http
        .post(format!(
            "{}/v1/token",
            connection.server.trim_end_matches('/')
        ))
        .bearer_auth(secret.trim())
        .json(&TokenRequest {
            previous_revision: (refresh && !connection.revision.is_empty())
                .then(|| connection.revision.clone()),
            account_id: (!connection.account_id.is_empty()).then(|| connection.account_id.clone()),
            billing: true,
            alias: connection.alias.clone(),
        })
        .send()
        .context("central token request failed")?;
    let status = response.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::CONFLICT
            && response
                .json::<serde_json::Value>()
                .ok()
                .is_some_and(|v| v["error"] == "unsupported_workspace_routing")
        {
            bail!(
                "this account requires workspace routing that the central native provider does not yet support"
            );
        }
        bail!("central token request rejected (HTTP {})", status);
    }
    let token: TokenResponse = response.json().context("invalid central token response")?;
    if !token.native_routing_supported {
        bail!("server has not verified supported workspace routing for the native provider");
    }
    if connection.user_id.is_some() && token.user_id != connection.user_id {
        bail!("server user identity changed");
    }
    if !connection.account_id.is_empty() && token.chatgpt_account_id != connection.account_id {
        bail!("central account identity changed");
    }
    if token.access_token.is_empty()
        || token.revision.is_empty()
        || token.chatgpt_account_id.is_empty()
    {
        bail!("incomplete central token response");
    }
    Ok(token)
}
fn save_connection(path: &Path, connection: &Connection) -> Result<()> {
    store::atomic_write(path, &serde_json::to_vec(connection)?)
}
fn read_connection(path: &Path) -> Result<Connection> {
    Ok(serde_json::from_slice(&vault::private_read(path)?)?)
}

pub fn connect(alias: &str, server: &str, token_file: &Path) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let path = connection_path(alias)?;
    let _lock = native_lock(&root()?)?;
    if path.try_exists()? || store::profile_dir(&config::default_paths()?, alias)?.try_exists()? {
        bail!("alias already exists");
    }
    let mut connection = Connection {
        user_id: None,
        alias: None,
        server: server.into(),
        device_token_file: std::fs::canonicalize(token_file)?,
        account_id: String::new(),
        revision: String::new(),
        allow_billing: false,
        approved_billing_plan: None,
        approved_billing_class: None,
    };
    drop(_lock);
    let token = fetch(&connection, false)?;
    let _lock = native_lock(&root()?)?;
    if path.try_exists()? {
        bail!("alias registered while connecting");
    }
    connection.account_id = token.chatgpt_account_id;
    connection.revision = token.revision;
    save_connection(&path, &connection)?;
    println!("registered remote account {alias}; run codexctl use {alias}");
    Ok(())
}

fn active_pointer_path() -> Result<PathBuf> {
    Ok(root()?.join(ACTIVE_POINTER))
}
fn read_active_alias() -> Result<String> {
    let path = active_pointer_path()?;
    if !path.try_exists()? {
        bail!("active account pointer is missing; run codexctl use again");
    }
    let raw = String::from_utf8(vault::private_read(&path)?)
        .context("invalid active account pointer; run codexctl use again")?;
    let alias = raw.trim();
    if alias.is_empty()
        || (raw != alias && raw != format!("{alias}\n") && raw != format!("{alias}\r\n"))
    {
        bail!("invalid active account pointer; run codexctl use again");
    }
    store::validate_alias(alias)
        .map(|alias| alias.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid active account pointer; run codexctl use again"))
}

fn connection_alias(path: &Path, connection: &Connection) -> String {
    connection
        .alias
        .clone()
        .or_else(|| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn billing_error(alias: &str, token: &TokenResponse) -> anyhow::Error {
    if token.billing_class == Some(api::BillingClass::Unknown)
        && token.statusline_usage.as_ref().is_some_and(|usage| {
            usage
                .five_hour_used_percent
                .is_some_and(|used| used >= 100.0)
                || usage.weekly_used_percent.is_some_and(|used| used >= 100.0)
        })
    {
        anyhow::anyhow!("remote account {alias} is exhausted; run codexctl use")
    } else {
        anyhow::anyhow!("remote billing changed; select the account again with billing approval")
    }
}

fn finish_token(
    path: &Path,
    connection: &Connection,
    token: TokenResponse,
    expected_active: Option<&str>,
) -> Result<()> {
    let alias = connection_alias(path, connection);
    let mut latest = read_connection(path)?;
    if let Some(expected) = expected_active
        && read_active_alias()?.as_str() != expected
    {
        bail!("active account changed during token retrieval; retry");
    }
    if latest.account_id != connection.account_id
        || latest.server != connection.server
        || latest.device_token_file != connection.device_token_file
    {
        bail!("remote connection changed during token retrieval");
    }
    if token.billing_class != Some(api::BillingClass::RateLimited)
        && (!latest.allow_billing
            || latest.approved_billing_plan != token.chatgpt_plan_type
            || latest.approved_billing_class != token.billing_class)
    {
        return Err(billing_error(&alias, &token));
    }
    if latest.revision == connection.revision {
        latest.revision = token.revision;
        save_connection(path, &latest)?;
    }
    if let (Ok(paths), Ok(selection), Some(usage)) = (
        config::default_paths(),
        statusline_identity(path, connection),
        token.statusline_usage,
    ) {
        crate::statusline::record(&paths, selection, token.label.as_deref(), Some(usage));
    }
    println!("{}", token.access_token);
    Ok(())
}
pub fn print_token(path: &Path) -> Result<()> {
    if std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some() {
        bail!("remote credentials cannot be supplied to a pinned local launch");
    }
    let directory = path.parent().context("missing connection directory")?;
    let connection = {
        let _lock = native_lock(directory)?;
        read_connection(path)?
    };
    let token = fetch(&connection, true)?;
    let _lock = native_lock(directory)?;
    finish_token(path, &connection, token, None)
}
pub fn print_active_token() -> Result<()> {
    if std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some() {
        bail!("remote credentials cannot be supplied to a pinned local launch");
    }
    let directory = root()?;
    let (alias, path, connection) = {
        let _lock = native_lock(&directory)?;
        let alias = read_active_alias()?;
        let path = connection_path(&alias)?;
        if !path.try_exists()? {
            bail!(
                "active account pointer names missing connection {alias}; run codexctl use again"
            );
        }
        let connection = read_connection(&path)?;
        (alias, path, connection)
    };
    let token = fetch(&connection, true)?;
    let _lock = native_lock(&directory)?;
    finish_token(&path, &connection, token, Some(&alias))
}
fn document(home: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(home.join("config.toml")) {
        Ok(text) => Ok(text.parse()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

/// Resolve explicit unmigrated local names without requiring an online catalog.
pub fn known_local_alias(alias: &str) -> Result<bool> {
    let paths = config::default_paths()?;
    let local = store::profile_dir(&paths, alias)?;
    if local.try_exists()? && !local.join(".central-transfer.json").try_exists()? {
        if connection_path(alias)?.try_exists()? {
            bail!("remote alias conflicts with a local profile; rename one before selection");
        }
        store::require_local_auth(&local.join("auth.json"))?;
        return Ok(true);
    }
    Ok(false)
}

pub fn activate(
    alias: Option<&str>,
    allow_billing: bool,
    allow_resets: bool,
    restart_daemon: bool,
) -> Result<bool> {
    let explicit = alias.is_some();
    if let Some(alias) = alias
        && known_local_alias(alias)?
    {
        return Ok(false);
    }
    let catalog = super::remote::catalog()?;
    let mut redeem_reset = false;
    let selected_remote = catalog
        .as_ref()
        .map(|catalog| {
            let accounts = &catalog.accounts;
            if let Some(alias) = alias {
                accounts
                    .iter()
                    .find(|a| a.alias == alias)
                    .map(|a| a.alias.clone())
                    .context("server account alias not found")
            } else {
                let (alias, redeem) = super::remote::select_for_activation(accounts, allow_resets)?;
                redeem_reset = redeem;
                Ok(alias)
            }
        })
        .transpose()?;
    let alias = selected_remote.as_deref().or(alias);
    let selected = match alias {
        Some(alias) => alias.to_owned(),
        None => {
            let directory = root()?;
            if !directory.try_exists()? {
                return Ok(false);
            }
            let candidates = std::fs::read_dir(&directory)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "json")
                        && p.file_name()
                            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
                })
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return Ok(false);
            }
            if candidates.len() != 1 {
                bail!("multiple remote accounts registered; select an explicit alias");
            }
            candidates[0]
                .file_stem()
                .and_then(|s| s.to_str())
                .context("invalid remote alias")?
                .to_owned()
        }
    };
    let alias = selected.as_str();
    let path = connection_path(alias)?;
    if (catalog.is_some() || path.try_exists()?)
        && (std::env::var_os("CODEX_HOME").is_some()
            || std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some())
    {
        bail!("remote activation refuses an inherited or pinned Codex home");
    }
    if catalog.is_none() && !path.try_exists()? {
        return Ok(false);
    }
    let paths = config::default_paths()?;
    let directory = root()?;
    let shared = vault::mode_lock(&directory, vault::LockMode::Shared)?;
    let switching_server = {
        let _lock = native_lock(&directory)?;
        directory.join(".native-active.json").try_exists()?
            && document(&paths.codex_home())?
                .get("model_provider")
                .and_then(Item::as_str)
                == Some(PROVIDER)
    };
    // Server sessions keep their launch-time provider. Only entry from local
    // mode must exclude credential owners; native.lock serializes config writes.
    let _mode = if switching_server {
        shared
    } else {
        drop(shared);
        exclusive_mode(&paths)?
    };
    if let Some(catalog) = catalog.as_ref() {
        let _lock = native_lock(&root()?)?;
        super::remote::require_current_connection(&catalog.connection)?;
        let account = catalog
            .accounts
            .iter()
            .find(|a| a.alias == alias)
            .context("server account alias not found")?;
        sync_account(&catalog.connection, account)?;
    }
    if !path.try_exists()? {
        return Ok(false);
    }
    let local = store::profile_dir(&config::default_paths()?, alias)?;
    if local.try_exists()? && !local.join(".central-transfer.json").try_exists()? {
        bail!("remote alias conflicts with a local profile; rename one before selection");
    }
    let _lock = native_lock(&root()?)?;
    let mut connection = read_connection(&path)?;
    drop(_lock);
    let token = fetch(&connection, false)?;
    let usage_based = token.billing_class != Some(api::BillingClass::RateLimited);
    if redeem_reset
        && !token
            .chatgpt_plan_type
            .as_deref()
            .is_some_and(api::is_known_rate_limited_plan)
    {
        bail!("automatic reset selection refuses usage-based or unknown plans");
    }
    if usage_based && !explicit && !redeem_reset {
        bail!("automatic remote selection refuses usage-based or unknown billing");
    }
    if usage_based && !allow_billing && !redeem_reset {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            bail!("remote account may bill credits; use --allow-billing explicitly");
        }
        if !dialoguer::Confirm::new()
            .with_prompt("This remote account may bill credits. Switch?")
            .default(false)
            .interact()?
        {
            bail!("remote billing switch declined");
        }
    }
    let approve_billing = usage_based && !redeem_reset;
    connection.allow_billing = approve_billing;
    connection.approved_billing_plan = approve_billing
        .then(|| token.chatgpt_plan_type.clone())
        .flatten();
    connection.approved_billing_class = approve_billing.then_some(token.billing_class).flatten();
    let _lock = native_lock(&root()?)?;
    if let Some(catalog) = catalog.as_ref() {
        super::remote::require_current_connection(&catalog.connection)?;
    }
    let paths = config::default_paths()?;
    // Migration takes store then native. Never wait for the store while holding native.
    let _store =
        store::try_lock(&paths)?.context("local account store is busy; retry selection")?;
    super::remote::require_local_handoff(
        &paths,
        &serde_json::json!({"tokens":{"access_token":token.access_token,"account_id":token.chatgpt_account_id}}),
    )?;
    let home = codex_home()?;
    if !restart_daemon && crate::daemon::running_pid(&home).is_some() {
        bail!(
            "Codex daemon is running; use --restart-daemon to apply the switch and resume its sessions, or finish its sessions and run codex app-server daemon stop before remote activation"
        );
    }
    if !home.try_exists()? {
        store::ensure_private_dir(&home)?;
    }
    let mut doc = document(&home)?;
    if let Some(selected_profile) = doc.get("profile").and_then(Item::as_str)
        && doc
            .get("profiles")
            .and_then(|profiles| profiles.get(selected_profile))
            .and_then(|profile| profile.get("model_provider"))
            .is_some()
    {
        bail!(
            "selected Codex profile overrides model_provider; remove that override before remote activation"
        );
    }
    let marker = root()?.join(".native-active.json");
    let pointer = active_pointer_path()?;
    let previous_pointer = match std::fs::read(&pointer) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // Disconnect or local selection can restore the provider during token fetch.
    // A shared lease must never turn that into a fresh activation from local mode.
    if switching_server && !marker.try_exists()? {
        bail!("remote provider was disconnected during selection; retry codexctl use");
    }
    let activation = if marker.try_exists()? {
        let active: Activation = serde_json::from_slice(&vault::private_read(&marker)?)?;
        if active.home != home {
            bail!("remote provider is active in another Codex home");
        }
        if doc.get("model_provider").and_then(Item::as_str) != Some(PROVIDER) {
            bail!("Codex provider changed since remote activation; reconcile configuration first");
        }
        active
    } else {
        if doc
            .get("model_providers")
            .and_then(|t| t.get(PROVIDER))
            .is_some()
        {
            bail!("reserved central provider already exists");
        }
        Activation {
            home: home.clone(),
            original_provider: doc
                .get("model_provider")
                .map(|i| i.as_str().context("model_provider must be a string"))
                .transpose()?
                .map(str::to_owned),
        }
    };
    let helper = std::env::current_exe()?;
    let mut provider = Table::new();
    provider["name"] = value("Central Codex");
    provider["base_url"] = value("https://chatgpt.com/backend-api/codex");
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);
    provider["auth"]["command"] = value(helper.to_str().context("helper path must be UTF-8")?);
    let mut args = toml_edit::Array::new();
    args.push("central-token");
    args.push("--active");
    provider["auth"]["args"] = value(args);
    // Codex checks cached helper-token age before requests. This bounds normal
    // reuse to one minute; it does not cancel in-flight work or revoke tokens.
    provider["auth"]["refresh_interval_ms"] = value(60_000);
    provider["auth"]["timeout_ms"] = value(210_000);
    if let Some(inline) = doc.get("model_providers").and_then(Item::as_inline_table) {
        doc["model_providers"] = Item::Table(inline.clone().into_table());
    } else if doc.get("model_providers").is_none() {
        doc["model_providers"] = Item::Table(Table::new());
    } else if !doc["model_providers"].is_table() {
        bail!("model_providers must be a table");
    }
    doc["model_provider"] = value(PROVIDER);
    doc["model_providers"][PROVIDER] = Item::Table(provider);
    let latest = read_connection(&path)?;
    if latest.server != connection.server
        || latest.account_id != connection.account_id
        || latest.device_token_file != connection.device_token_file
    {
        bail!("remote connection changed during activation");
    }
    connection.revision = latest.revision;
    let destination = config_path(&home)?;
    let had_marker = marker.try_exists()?;
    // All local refusal checks have passed. Hold both mutation locks through the
    // spend and activation so another local command cannot invalidate the checks.
    if redeem_reset {
        let response = super::remote::redeem_reset(alias)?;
        if !matches!(
            response.code,
            api::ConsumeResetCode::Reset | api::ConsumeResetCode::AlreadyRedeemed
        ) {
            bail!("banked reset did not clear the account's exhausted window");
        }
        eprintln!("codexctl: redeemed a banked reset for {alias}; checking included usage");
        let mut included = false;
        for attempt in 0..4 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            let refreshed = fetch(&connection, false)
                .context("a reset was redeemed, but activation could not confirm included usage; wait and retry codexctl use without --allow-resets")?;
            if refreshed.billing_class == Some(api::BillingClass::RateLimited) {
                included = true;
                break;
            }
        }
        if !included {
            bail!(
                "a reset was redeemed, but included usage is not yet confirmed; activation was not completed; wait and retry codexctl use without --allow-resets"
            );
        }
    }
    store::atomic_write(&marker, &serde_json::to_vec(&activation)?)?;
    store::atomic_write(&pointer, format!("{alias}\n").as_bytes())?;
    if let Err(error) = write_config(&destination, doc.to_string().as_bytes()) {
        let not_installed = match std::fs::read(&destination) {
            Ok(bytes) => bytes != doc.to_string().as_bytes(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        if !had_marker && not_installed {
            std::fs::remove_file(&marker)
                .context("failed to roll back prepared remote activation")?;
        }
        match previous_pointer {
            Some(bytes) => store::atomic_write(&pointer, &bytes)?,
            None => {
                let _ = std::fs::remove_file(&pointer);
            }
        }
        return Err(error);
    }
    save_connection(&path, &connection)?;
    repair_sessions_if_active(SessionProviderAction::Rewrite).with_context(|| {
        format!("server account {alias} is active; session repair failed; resolve the reported cause and retry codexctl session-provider rewrite")
    })?;
    println!(
        "switched to remote account {alias}; start codexctl codex (resume: codexctl codex resume <session-id>)"
    );
    Ok(true)
}

pub fn deactivate() -> Result<()> {
    let _lock = native_lock(&root()?)?;
    deactivate_locked()
}

pub(super) fn deactivate_locked() -> Result<()> {
    let marker = root()?.join(".native-active.json");
    if !marker.try_exists()? {
        return Ok(());
    }
    let active: Activation = serde_json::from_slice(&vault::private_read(&marker)?)?;
    if crate::daemon::running_pid(&active.home).is_some() {
        bail!(
            "Codex daemon is running; finish its sessions and run codex app-server daemon stop before disconnecting"
        );
    }
    super::sessions::require_restored(&active.home)?;
    let mut doc = document(&active.home)?;
    if doc.get("model_provider").and_then(Item::as_str) == Some(PROVIDER) {
        match active.original_provider {
            Some(original) => doc["model_provider"] = value(original),
            None => {
                doc.remove("model_provider");
            }
        }
    }
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        providers.remove(PROVIDER);
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }
    write_config(&config_path(&active.home)?, doc.to_string().as_bytes())?;
    std::fs::remove_file(marker)?;
    let _ = std::fs::remove_file(active_pointer_path()?);
    Ok(())
}

pub(super) fn sync_account(
    device: &super::remote::Connection,
    account: &super::managed::Account,
) -> Result<()> {
    if account.user_id != device.user_id {
        bail!("server user identity changed");
    }
    let path = connection_path(&account.alias)?;
    let local = store::profile_dir(&config::default_paths()?, &account.alias)?;
    if !super::remote::local_alias_matches(device, account, &local)? {
        bail!(
            "server alias {} conflicts with a local profile; migrate or rename it",
            account.alias
        );
    }
    if path.try_exists()? {
        let existing = read_connection(&path)?;
        if existing.server != device.server
            || existing.account_id != account.account_id
            || existing.alias.as_deref() != Some(&account.alias)
            || existing.user_id.as_deref() != Some(&device.user_id)
            || existing.device_token_file != device.token_file
        {
            bail!("remote account identity changed");
        }
        return Ok(());
    }
    save_connection(
        &path,
        &Connection {
            user_id: Some(device.user_id.clone()),
            alias: Some(account.alias.clone()),
            server: device.server.clone(),
            device_token_file: device.token_file.clone(),
            account_id: account.account_id.clone(),
            revision: String::new(),
            allow_billing: false,
            approved_billing_plan: None,
            approved_billing_class: None,
        },
    )
}
pub(super) fn remove_managed_connection(
    path: &Path,
    device: &super::remote::Connection,
) -> Result<()> {
    let connection = read_connection(path)?;
    if connection.server == device.server && connection.device_token_file == device.token_file {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
pub fn active_alias() -> Result<Option<String>> {
    let home = codex_home()?;
    let doc = document(&home)?;
    if doc.get("model_provider").and_then(Item::as_str) != Some(PROVIDER) {
        return Ok(None);
    }
    let args = doc
        .get("model_providers")
        .and_then(|p| p.get(PROVIDER))
        .and_then(|p| p.get("auth"))
        .and_then(|a| a.get("args"))
        .and_then(Item::as_array)
        .context("invalid central provider command")?;
    if args.iter().any(|arg| arg.as_str() == Some("--active")) {
        return Ok(Some(read_active_alias()?));
    }
    let path = args
        .iter()
        .position(|arg| arg.as_str() == Some("--connection"))
        .and_then(|i| args.get(i + 1))
        .and_then(toml_edit::Value::as_str)
        .context("invalid central provider command")?;
    Ok(std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned))
}

/// Session repair has explicit actions, so a dry run can never imply a write.
#[derive(Clone, Copy, clap::Subcommand)]
pub enum SessionProviderAction {
    /// List old sessions and open-file skips without changing rollouts or backups.
    DryRun,
    /// Rewrite old providers after saving private original metadata lines.
    Rewrite,
    /// Restore original metadata lines from private backups.
    Restore,
}

pub fn session_provider(action: SessionProviderAction) -> Result<()> {
    let paths = config::default_paths()?;
    let _mode = vault::mode_lock(
        &paths.codexctl_dir().join("central"),
        vault::LockMode::Shared,
    )?;
    let _lock = native_lock(&root()?)?;
    if !repair_sessions_if_active(action)? {
        bail!("session provider repair requires an active server account in this CODEX_HOME");
    }
    Ok(())
}

// Caller holds native.lock and a mode lease. Migration already owns both locks.
pub(super) fn repair_sessions_if_active(action: SessionProviderAction) -> Result<bool> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(codex_home()?);
    let marker = root()?.join(".native-active.json");
    if !marker.try_exists()?
        || document(&home)?
            .get("model_provider")
            .and_then(Item::as_str)
            != Some(PROVIDER)
    {
        return Ok(false);
    }
    let active: Activation = serde_json::from_slice(&vault::private_read(&marker)?)?;
    if std::fs::canonicalize(&active.home)? != std::fs::canonicalize(&home)? {
        bail!("remote provider is active in another Codex home");
    }
    super::sessions::run(&home, action)?;
    Ok(true)
}

fn statusline_identity(
    path: &Path,
    connection: &Connection,
) -> Result<crate::statusline::Selection> {
    Ok(crate::statusline::Selection {
        alias: path
            .file_stem()
            .and_then(|v| v.to_str())
            .context("invalid connection path")?
            .to_owned(),
        source: crate::statusline::Source::Server {
            server: connection.server.clone(),
            user_id: connection.user_id.clone(),
            account_id: connection.account_id.clone(),
        },
    })
}

pub(crate) fn statusline_selection(
    paths: &config::Paths,
) -> Result<Option<crate::statusline::Selection>> {
    let doc = document(&paths.codex_home())?;
    if doc.get("model_provider").and_then(Item::as_str) != Some(PROVIDER) {
        return Ok(None);
    }
    let args = doc
        .get("model_providers")
        .and_then(|p| p.get(PROVIDER))
        .and_then(|p| p.get("auth"))
        .and_then(|p| p.get("args"))
        .and_then(Item::as_array)
        .context("invalid central provider command")?;
    let path = if args.iter().any(|arg| arg.as_str() == Some("--active")) {
        connection_path(&read_active_alias()?)?
    } else {
        let path = args
            .iter()
            .position(|arg| arg.as_str() == Some("--connection"))
            .and_then(|i| args.get(i + 1))
            .and_then(toml_edit::Value::as_str)
            .context("invalid central provider command")?;
        PathBuf::from(path)
    };
    statusline_identity(&path, &read_connection(&path)?).map(Some)
}
