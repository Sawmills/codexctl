//! Native TUI credentials through Codex's command-backed provider authentication.
use super::{
    server::{TokenRequest, TokenResponse},
    vault,
};
use crate::{api, config, store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table, value};

mod launch;
#[cfg(feature = "central-prototype")]
pub use launch::pinned_arguments;
pub(super) use launch::sweep_stale_launches;
pub use launch::{PinnedLaunch, launch_owners, prepare_included_codex, run_pinned_codex};

pub(super) const PROVIDER: &str = "codexctl-central";
const ACTIVE_POINTER: &str = ".active-account";
const ACTIVE_HISTORY: &str = "active-history.jsonl";
const ACTIVE_HISTORY_ROTATED: &str = "active-history.jsonl.1";
const ACTIVE_HISTORY_LIMIT: u64 = 1024 * 1024;
const BILLING_SWITCH_NOTICE: &str = "ALL running Codex sessions on this machine will also move to this account within 60 seconds and may bill credits.";

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerCause {
    Use,
    UseAuto,
    Recovery,
    Rollback,
    Deactivate,
}

impl PointerCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::UseAuto => "use-auto",
            Self::Recovery => "recovery",
            Self::Rollback => "rollback",
            Self::Deactivate => "deactivate",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ActiveHistoryEntry {
    at: String,
    from_alias: Option<String>,
    to_alias: Option<String>,
    cause: String,
    pid: u32,
    ppid: u32,
    parent_cmd: String,
    tty: Option<String>,
    codexctl_version: String,
}

fn billing_switch_prompt() -> String {
    format!("This remote account may bill credits. {BILLING_SWITCH_NOTICE} Switch?")
}

#[derive(Serialize, Deserialize, PartialEq)]
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    launch_pinned: bool,
    #[serde(default)]
    approved_billing_plan: Option<String>,
    #[serde(default)]
    approved_billing_class: Option<api::BillingClass>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    session_id: String,
}
#[derive(Serialize, Deserialize)]
struct Activation {
    home: PathBuf,
    original_provider: Option<String>,
}
fn root() -> Result<PathBuf> {
    Ok(config::default_paths()?.codexctl_dir().join("central"))
}
fn alias_from_pointer_bytes(bytes: Option<&[u8]>) -> Option<String> {
    let raw = std::str::from_utf8(bytes?).ok()?;
    let alias = raw.trim();
    if alias.is_empty()
        || (raw != alias && raw != format!("{alias}\n") && raw != format!("{alias}\r\n"))
    {
        return None;
    }
    store::validate_alias(alias).ok().map(str::to_owned)
}

fn render_parent_cmd(args: &[String]) -> String {
    let mut rendered = Vec::new();
    let mut redact = false;
    for arg in args.iter().flat_map(|arg| arg.split_whitespace()) {
        let lower = arg.to_ascii_lowercase();
        if redact {
            rendered.push("<redacted>".to_owned());
            redact = lower == "bearer";
        } else if lower == "bearer" {
            rendered.push("<redacted>".to_owned());
            redact = true;
        } else {
            let (name, value) = lower.split_once('=').unwrap_or((lower.as_str(), ""));
            let sensitive_name = name
                .trim_start_matches('-')
                .trim_end_matches(':')
                .replace('-', "");
            let sensitive = matches!(
                sensitive_name.as_str(),
                "token"
                    | "password"
                    | "secret"
                    | "key"
                    | "apikey"
                    | "accesstoken"
                    | "refreshtoken"
                    | "authorization"
                    | "credential"
            );
            if !value.is_empty() && sensitive {
                rendered.push(format!("{name}=<redacted>"));
            } else {
                redact = sensitive && value.is_empty();
                if lower.split('.').count() == 3 && arg.len() > 30 {
                    rendered.push("<redacted>".to_owned());
                } else {
                    rendered.push(arg.to_owned());
                }
            }
        }
    }
    // Keep only the executable token. The remaining parent argv is open-ended
    // shell input and cannot be made secret-safe with a finite denylist.
    rendered
        .into_iter()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

fn parent_cmd() -> String {
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let ppid = 0;
    #[cfg(unix)]
    let args = fs::read(format!("/proc/{ppid}/cmdline"))
        .ok()
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            std::process::Command::new("ps")
                .args(["-p", &ppid.to_string(), "-o", "command="])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect()
                })
        })
        .unwrap_or_default();
    #[cfg(not(unix))]
    let args: Vec<String> = Vec::new();
    render_parent_cmd(&args)
}

fn tty_path() -> Option<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 256];
        let result = unsafe { libc::ttyname_r(0, bytes.as_mut_ptr().cast(), bytes.len()) };
        if result == 0 {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            return std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned);
        }
    }
    None
}

fn append_active_history(
    history_root: &Path,
    from_alias: Option<String>,
    to_alias: Option<String>,
    cause: PointerCause,
) -> Result<()> {
    store::ensure_private_dir(history_root)?;
    let path = history_root.join(ACTIVE_HISTORY);
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= ACTIVE_HISTORY_LIMIT)
    {
        let rotated = history_root.join(ACTIVE_HISTORY_ROTATED);
        match fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::rename(&path, &rotated)?;
        #[cfg(unix)]
        fs::set_permissions(&rotated, fs::Permissions::from_mode(0o600))?;
    }
    let entry = ActiveHistoryEntry {
        at: chrono::Utc::now().to_rfc3339(),
        from_alias,
        to_alias,
        cause: cause.as_str().to_owned(),
        pid: std::process::id(),
        ppid: {
            #[cfg(unix)]
            {
                unsafe { libc::getppid() as u32 }
            }
            #[cfg(not(unix))]
            {
                0
            }
        },
        parent_cmd: parent_cmd(),
        tty: tty_path(),
        codexctl_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let bytes = serde_json::to_vec(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    sync_history_directory(history_root)?;
    Ok(())
}

fn sync_history_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn remove_active_pointer_audited(pointer: &Path, cause: PointerCause) -> Result<()> {
    let previous = read_optional_file(pointer)?;
    remove_active_pointer_with(pointer, |path| fs::remove_file(path))?;
    if previous.is_some() {
        let append = append_active_history(
            pointer.parent().context("active pointer has no parent")?,
            alias_from_pointer_bytes(previous.as_deref()),
            None,
            cause,
        );
        if let Err(error) = append {
            let rollback = previous
                .as_deref()
                .map(|bytes| store::atomic_write(pointer, bytes))
                .transpose();
            return match rollback {
                Ok(_) => Err(error),
                Err(rollback_error) => Err(error.context(format!(
                    "active account pointer rollback failed: {rollback_error:#}"
                ))),
            };
        }
    }
    Ok(())
}

pub fn print_history(limit: usize, json: bool) -> Result<()> {
    let history_root = root()?;
    let _lock = native_lock(&history_root)?;
    let mut entries = Vec::new();
    for path in [
        history_root.join(ACTIVE_HISTORY_ROTATED),
        history_root.join(ACTIVE_HISTORY),
    ] {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        for line in text.lines() {
            if let Ok(entry) = serde_json::from_str::<ActiveHistoryEntry>(line) {
                entries.push(entry);
            }
        }
    }
    let start = entries.len().saturating_sub(limit);
    let entries = &entries[start..];
    if json {
        println!("{}", serde_json::to_string_pretty(entries)?);
    } else {
        for entry in entries {
            println!("{}", serde_json::to_string(entry)?);
        }
    }
    Ok(())
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
    write_config_with(destination, bytes, |parent| {
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })
}
fn write_config_with(
    destination: &Path,
    bytes: &[u8],
    sync_parent: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
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
    sync_parent(parent)?;
    Ok(())
}
fn restore_active_pointer(pointer: &Path, previous_pointer: Option<&[u8]>) -> Result<()> {
    let current = read_optional_file(pointer)?;
    if current.as_deref() == previous_pointer {
        return Ok(());
    }
    match previous_pointer {
        Some(bytes) => {
            store::atomic_write(pointer, bytes)?;
            append_active_history(
                pointer.parent().context("active pointer has no parent")?,
                alias_from_pointer_bytes(current.as_deref()),
                alias_from_pointer_bytes(Some(bytes)),
                PointerCause::Rollback,
            )
        }
        None => {
            remove_active_pointer_with(pointer, |path| std::fs::remove_file(path))?;
            append_active_history(
                pointer.parent().context("active pointer has no parent")?,
                alias_from_pointer_bytes(current.as_deref()),
                None,
                PointerCause::Rollback,
            )
        }
    }
}
fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn restore_optional_file(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    match previous {
        Some(bytes) => store::atomic_write(path, bytes),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
    }
}
fn restore_config(path: &Path, previous: Option<&[u8]>, attempted: &[u8]) -> Result<()> {
    let current = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if current.as_deref() == previous {
        return Ok(());
    }
    if current.as_deref() != Some(attempted) {
        bail!("Codex configuration changed during activation; preserving the newer file");
    }
    match previous {
        Some(bytes) => write_config(path, bytes),
        None => restore_optional_file(path, None),
    }
}
struct ActivationRollback<'a> {
    connection_path: &'a Path,
    previous_connection: &'a [u8],
    config: &'a Path,
    previous_config: Option<&'a [u8]>,
    attempted_config: &'a [u8],
    marker: &'a Path,
    previous_marker: Option<&'a [u8]>,
    pointer: &'a Path,
    previous_pointer: Option<&'a [u8]>,
}
fn rollback_activation(state: ActivationRollback<'_>) -> Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = restore_active_pointer(state.pointer, state.previous_pointer) {
        failures.push(format!("active account pointer: {error:#}"));
    }
    if let Err(error) = restore_config(state.config, state.previous_config, state.attempted_config)
    {
        failures.push(format!("Codex configuration: {error:#}"));
    }
    if let Err(error) = restore_optional_file(state.marker, state.previous_marker) {
        failures.push(format!("remote activation marker: {error:#}"));
    }
    if let Err(error) = store::atomic_write(state.connection_path, state.previous_connection) {
        failures.push(format!("saved remote connection: {error:#}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("activation rollback failed: {}", failures.join("; "))
    }
}
fn write_pointer_after_connection(
    connection_path: &Path,
    expected_connection: &Connection,
    pointer: &Path,
    pointer_bytes: &[u8],
    write_pointer: impl FnOnce(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let saved = read_connection(connection_path)?;
    if saved != *expected_connection {
        bail!("activation approval was not saved before moving the active account pointer");
    }
    write_pointer(pointer, pointer_bytes)
}
fn write_pointer_with_rollback(
    connection_path: &Path,
    expected_connection: &Connection,
    pointer: &Path,
    pointer_bytes: &[u8],
    previous_pointer: Option<&[u8]>,
    cause: PointerCause,
    write_pointer: impl FnOnce(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    if previous_pointer == Some(pointer_bytes) {
        let saved = read_connection(connection_path)?;
        if saved != *expected_connection {
            bail!("activation approval was not saved before moving the active account pointer");
        }
        return Ok(());
    }
    if let Err(error) = write_pointer_after_connection(
        connection_path,
        expected_connection,
        pointer,
        pointer_bytes,
        write_pointer,
    ) {
        return match restore_active_pointer(pointer, previous_pointer) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "active account pointer rollback failed: {rollback_error:#}"
            ))),
        };
    }
    if let Err(error) = append_active_history(
        pointer.parent().context("active pointer has no parent")?,
        alias_from_pointer_bytes(previous_pointer),
        alias_from_pointer_bytes(Some(pointer_bytes)),
        cause,
    ) {
        let rollback = restore_active_pointer(pointer, previous_pointer);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "active account pointer rollback failed: {rollback_error:#}"
            ))),
        };
    }
    Ok(())
}
fn rollback_or_context(error: anyhow::Error, state: ActivationRollback<'_>) -> anyhow::Error {
    match rollback_activation(state) {
        Ok(()) => error,
        Err(rollback_error) => {
            error.context(format!("activation rollback failed: {rollback_error:#}"))
        }
    }
}
fn remove_active_pointer_with(
    pointer: &Path,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    match remove(pointer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove active account pointer {}",
                pointer.display()
            )
        }),
    }
}
#[cfg(test)]
fn remove_pointer_then_marker_with(
    pointer: &Path,
    marker: &Path,
    remove_pointer: impl FnOnce(&Path) -> std::io::Result<()>,
    remove_marker: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    remove_active_pointer_with(pointer, remove_pointer)?;
    remove_marker(marker).context("failed to remove prepared remote activation")?;
    Ok(())
}
fn remove_pointer_then_marker_audited(
    pointer: &Path,
    marker: &Path,
    cause: PointerCause,
) -> Result<()> {
    let previous = read_optional_file(pointer)?;
    remove_active_pointer_audited(pointer, cause)?;
    if let Err(error) =
        std::fs::remove_file(marker).context("failed to remove prepared remote activation")
    {
        let rollback = restore_active_pointer(pointer, previous.as_deref());
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "deactivation pointer rollback failed: {rollback_error:#}"
            ))),
        };
    }
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
    spawn_child_with_lease(guard, command)?
        .wait()
        .context("Codex process failed to wait")
}

fn spawn_child_with_lease(
    guard: &std::fs::File,
    command: &mut std::process::Command,
) -> Result<std::process::Child> {
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
    command.spawn().context("Codex process failed to run")
}

/// Whether this home's active provider is the account server.
pub fn central_active() -> Result<bool> {
    let paths = config::default_paths()?;
    let marker = paths.codexctl_dir().join("central/.native-active.json");
    if !marker.try_exists()? {
        return Ok(false);
    }
    Ok(document(&paths.codex_home())?
        .get("model_provider")
        .and_then(Item::as_str)
        == Some(PROVIDER))
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
        .args(pinned_arguments(args)?);
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
    let mut request = http
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
        });
    if !connection.session_id.is_empty() {
        request = request.header("x-codexctl-session", &connection.session_id);
    }
    let response = request.send().context("central token request failed")?;
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
        launch_pinned: false,
        approved_billing_plan: None,
        approved_billing_class: None,
        session_id: String::new(),
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

fn validate_token_account(token: &str, expected_account: &str) -> Result<()> {
    let actual = api::token_identity(token)
        .and_then(|identity| identity.account_id)
        .context("central access token has no workspace claim")?;
    if actual != expected_account {
        bail!("central access token workspace does not match the selected account");
    }
    Ok(())
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
    validate_token_account(&token.access_token, &connection.account_id)?;
    let _launch = connection
        .launch_pinned
        .then(|| launch::lock_live_launch(path))
        .transpose()?;
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
    let billing_approved = latest.allow_billing
        && latest.approved_billing_plan == token.chatgpt_plan_type
        && latest.approved_billing_class == token.billing_class;
    if connection.launch_pinned && !billing_approved {
        launch::require_headroom(&alias, &token)?;
    }
    if token.billing_class != Some(api::BillingClass::RateLimited) && !billing_approved {
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
    let directory = path.parent().context("missing connection directory")?;
    let connection = {
        let _lock = native_lock(directory)?;
        read_connection(path)?
    };
    if let Some(alias) = std::env::var_os("CODEXCTL_PINNED_ALIAS")
        && (!connection.launch_pinned
            || connection.alias.as_deref() != alias.to_str()
            || std::env::var_os("CODEX_HOME").is_some())
    {
        bail!("remote credentials cannot be supplied to this pinned launch");
    }
    if connection.launch_pinned {
        launch::require_live_launch(path)?;
    }
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
    // Server sessions share the active provider and resolve the selected account
    // on each helper refresh. Only entry from local mode must exclude credential
    // owners; native.lock serializes configuration writes.
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
            bail!(
                "remote account may bill credits; {BILLING_SWITCH_NOTICE} Use --allow-billing explicitly"
            );
        }
        if !dialoguer::Confirm::new()
            .with_prompt(billing_switch_prompt())
            .default(false)
            .interact()?
        {
            bail!("remote billing switch declined");
        }
    }
    let approve_billing = usage_based && !redeem_reset;
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
    let existing_provider = doc
        .get("model_providers")
        .and_then(|providers| providers.get(PROVIDER))
        .cloned();
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
    // OpenAI's short edge-rate-limit bursts are retried by default. Preserve
    // an operator's explicit values when an active provider is rewritten.
    provider["request_max_retries"] = existing_provider
        .as_ref()
        .and_then(|item| item.get("request_max_retries"))
        .cloned()
        .unwrap_or_else(|| value(12));
    provider["stream_max_retries"] = existing_provider
        .as_ref()
        .and_then(|item| item.get("stream_max_retries"))
        .cloned()
        .unwrap_or_else(|| value(12));
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
    let previous_connection = serde_json::to_vec(&latest)?;
    connection = latest;
    connection.allow_billing = approve_billing;
    connection.approved_billing_plan = approve_billing
        .then(|| token.chatgpt_plan_type.clone())
        .flatten();
    connection.approved_billing_class = approve_billing.then_some(token.billing_class).flatten();
    let destination = config_path(&home)?;
    let activation_bytes = serde_json::to_vec(&activation)?;
    let previous_config = read_optional_file(&destination)?;
    let desired_config = doc.to_string().into_bytes();
    let previous_marker = read_optional_file(&marker)?;
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
    if let Err(error) = save_connection(&path, &connection) {
        return Err(rollback_or_context(
            error,
            ActivationRollback {
                connection_path: &path,
                previous_connection: &previous_connection,
                config: &destination,
                previous_config: previous_config.as_deref(),
                attempted_config: &desired_config,
                marker: &marker,
                previous_marker: previous_marker.as_deref(),
                pointer: &pointer,
                previous_pointer: previous_pointer.as_deref(),
            },
        ));
    }
    if let Err(error) = store::atomic_write(&marker, &activation_bytes) {
        return Err(rollback_or_context(
            error,
            ActivationRollback {
                connection_path: &path,
                previous_connection: &previous_connection,
                config: &destination,
                previous_config: previous_config.as_deref(),
                attempted_config: &desired_config,
                marker: &marker,
                previous_marker: previous_marker.as_deref(),
                pointer: &pointer,
                previous_pointer: previous_pointer.as_deref(),
            },
        ));
    }
    if let Err(error) = write_config(&destination, &desired_config) {
        return Err(rollback_or_context(
            error,
            ActivationRollback {
                connection_path: &path,
                previous_connection: &previous_connection,
                config: &destination,
                previous_config: previous_config.as_deref(),
                attempted_config: &desired_config,
                marker: &marker,
                previous_marker: previous_marker.as_deref(),
                pointer: &pointer,
                previous_pointer: previous_pointer.as_deref(),
            },
        ));
    }
    if let Err(error) = write_pointer_with_rollback(
        &path,
        &connection,
        &pointer,
        format!("{alias}\n").as_bytes(),
        previous_pointer.as_deref(),
        if explicit {
            PointerCause::Use
        } else {
            PointerCause::UseAuto
        },
        store::atomic_write,
    ) {
        return Err(rollback_or_context(
            error,
            ActivationRollback {
                connection_path: &path,
                previous_connection: &previous_connection,
                config: &destination,
                previous_config: previous_config.as_deref(),
                attempted_config: &desired_config,
                marker: &marker,
                previous_marker: previous_marker.as_deref(),
                pointer: &pointer,
                previous_pointer: previous_pointer.as_deref(),
            },
        ));
    }
    // Configuration, marker, connection, and pointer are committed together before
    // session repair. Keep that activation available so the operator can retry the
    // explicit repair command without exposing a half-installed provider.
    repair_sessions_if_active(SessionProviderAction::Rewrite).with_context(|| {
        format!("server account {alias} is active; session repair failed; resolve the reported cause and retry codexctl session-provider rewrite")
    })?;
    if usage_based {
        println!(
            "switched to remote account {alias}; {BILLING_SWITCH_NOTICE} Start codexctl codex (resume: codexctl codex resume <session-id>)"
        );
    } else {
        println!(
            "switched to remote account {alias}; running Codex sessions on this machine will move to it within 60 seconds. Start codexctl codex (resume: codexctl codex resume <session-id>)"
        );
    }
    Ok(true)
}

pub fn deactivate() -> Result<()> {
    let _lock = native_lock(&root()?)?;
    deactivate_locked()
}

pub(super) fn deactivate_locked() -> Result<()> {
    let marker = root()?.join(".native-active.json");
    if !marker.try_exists()? {
        let pointer = active_pointer_path()?;
        if pointer.try_exists()? {
            remove_active_pointer_audited(&pointer, PointerCause::Deactivate)?;
        }
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
    let destination = config_path(&active.home)?;
    let previous_config = read_optional_file(&destination)?;
    let desired_config = doc.to_string().into_bytes();
    write_config(&destination, &desired_config)?;
    let pointer = active_pointer_path()?;
    if let Err(error) =
        remove_pointer_then_marker_audited(&pointer, &marker, PointerCause::Deactivate)
    {
        let rollback = restore_config(&destination, previous_config.as_deref(), &desired_config);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "deactivation configuration rollback failed: {rollback_error:#}"
            ))),
        };
    }
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
            launch_pinned: false,
            approved_billing_plan: None,
            approved_billing_class: None,
            session_id: String::new(),
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

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_HISTORY, ACTIVE_HISTORY_LIMIT, ACTIVE_HISTORY_ROTATED, ActivationRollback,
        ActiveHistoryEntry, BILLING_SWITCH_NOTICE, Connection, PointerCause, append_active_history,
        billing_switch_prompt, remove_active_pointer_audited, remove_active_pointer_with,
        remove_pointer_then_marker_audited, remove_pointer_then_marker_with, render_parent_cmd,
        restore_active_pointer, rollback_or_context, save_connection, validate_token_account,
        write_pointer_after_connection, write_pointer_with_rollback,
    };
    use crate::api;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn billing_confirmation_warns_about_all_running_sessions() {
        let prompt = billing_switch_prompt();
        assert!(prompt.contains("ALL running Codex sessions on this machine"));
        assert!(prompt.contains("move to this account within 60 seconds"));
        assert!(prompt.contains("may bill credits"));
        assert!(prompt.ends_with("Switch?"));
        assert!(prompt.contains(BILLING_SWITCH_NOTICE));
    }

    #[test]
    fn token_workspace_mismatch_is_refused_before_printing() {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "workspace-b"}
            }))
            .unwrap(),
        );
        let token = format!("header.{payload}.signature");
        let error = validate_token_account(&token, "workspace-a").unwrap_err();
        assert!(error.to_string().contains("workspace does not match"));
    }

    #[test]
    fn pointer_fsync_failure_rolls_back_after_persist() {
        let root = tempfile::tempdir().unwrap();
        let connection_path = root.path().join("remote.json");
        let pointer = root.path().join(".active-account");
        let connection = Connection {
            user_id: Some("user".into()),
            alias: Some("remote".into()),
            server: "https://server.invalid".into(),
            device_token_file: root.path().join("device.token"),
            account_id: "account".into(),
            revision: "revision".into(),
            allow_billing: true,
            launch_pinned: false,
            approved_billing_plan: Some("usage_based".into()),
            approved_billing_class: Some(api::BillingClass::Unknown),
            session_id: String::new(),
        };
        save_connection(&connection_path, &connection).unwrap();
        fs::write(&pointer, b"old\n").unwrap();

        let error = write_pointer_with_rollback(
            &connection_path,
            &connection,
            &pointer,
            b"new\n",
            Some(b"old\n"),
            PointerCause::Use,
            |path, bytes| {
                fs::write(path, bytes)?;
                anyhow::bail!("synthetic late pointer fsync failure")
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("late pointer fsync"));
        assert_eq!(fs::read(pointer).unwrap(), b"old\n");
    }

    #[test]
    fn unchanged_pointer_does_not_append_history() {
        let root = tempfile::tempdir().unwrap();
        let connection_path = root.path().join("remote.json");
        let pointer = root.path().join(".active-account");
        let connection = test_connection(root.path());
        save_connection(&connection_path, &connection).unwrap();
        fs::write(&pointer, b"remote\n").unwrap();

        write_pointer_with_rollback(
            &connection_path,
            &connection,
            &pointer,
            b"remote\n",
            Some(b"remote\n"),
            PointerCause::Use,
            |path, bytes| {
                fs::write(path, bytes)?;
                Ok(())
            },
        )
        .unwrap();

        assert!(!root.path().join(ACTIVE_HISTORY).exists());
    }

    #[test]
    fn pointer_transitions_record_use_auto_and_rollback() {
        let root = tempfile::tempdir().unwrap();
        let connection_path = root.path().join("remote.json");
        let pointer = root.path().join(".active-account");
        let connection = test_connection(root.path());
        save_connection(&connection_path, &connection).unwrap();
        fs::write(&pointer, b"old\n").unwrap();

        write_pointer_with_rollback(
            &connection_path,
            &connection,
            &pointer,
            b"new\n",
            Some(b"old\n"),
            PointerCause::UseAuto,
            |path, bytes| {
                fs::write(path, bytes)?;
                Ok(())
            },
        )
        .unwrap();
        restore_active_pointer(&pointer, Some(b"old\n")).unwrap();

        let causes = fs::read_to_string(root.path().join(ACTIVE_HISTORY))
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<ActiveHistoryEntry>(line)
                    .unwrap()
                    .cause
            })
            .collect::<Vec<_>>();
        assert_eq!(causes, ["use-auto", "rollback"]);
    }

    #[test]
    fn history_records_all_pointer_causes_and_private_permissions() {
        let root = tempfile::tempdir().unwrap();
        let causes = [
            PointerCause::Use,
            PointerCause::UseAuto,
            PointerCause::Recovery,
            PointerCause::Rollback,
            PointerCause::Deactivate,
        ];
        for cause in causes {
            append_active_history(root.path(), Some("from".into()), None, cause).unwrap();
        }
        let path = root.path().join(ACTIVE_HISTORY);
        let entries = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<ActiveHistoryEntry>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), causes.len());
        assert_eq!(entries[0].cause, "use");
        assert_eq!(entries[1].cause, "use-auto");
        assert_eq!(entries[2].cause, "recovery");
        assert_eq!(entries[3].cause, "rollback");
        assert_eq!(entries[4].cause, "deactivate");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn history_rotates_at_one_mib_and_keeps_one_backup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(ACTIVE_HISTORY);
        fs::write(&path, vec![b'x'; ACTIVE_HISTORY_LIMIT as usize]).unwrap();
        append_active_history(root.path(), None, Some("new".into()), PointerCause::Use).unwrap();
        assert!(root.path().join(ACTIVE_HISTORY_ROTATED).exists());
        assert!(fs::metadata(&path).unwrap().len() < ACTIVE_HISTORY_LIMIT);
        append_active_history(root.path(), None, Some("newer".into()), PointerCause::Use).unwrap();
        assert!(root.path().join(ACTIVE_HISTORY_ROTATED).exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.path().join(ACTIVE_HISTORY_ROTATED))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn deactivation_audit_records_pointer_removal() {
        let root = tempfile::tempdir().unwrap();
        let pointer = root.path().join(".active-account");
        fs::write(&pointer, b"remote\n").unwrap();
        remove_active_pointer_audited(&pointer, PointerCause::Deactivate).unwrap();
        let line = fs::read_to_string(root.path().join(ACTIVE_HISTORY)).unwrap();
        let entry: ActiveHistoryEntry = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry.from_alias.as_deref(), Some("remote"));
        assert_eq!(entry.to_alias, None);
        assert_eq!(entry.cause, "deactivate");
    }

    #[test]
    fn deactivation_restores_pointer_when_history_append_fails() {
        let root = tempfile::tempdir().unwrap();
        let pointer = root.path().join(".active-account");
        fs::write(&pointer, b"remote\n").unwrap();
        fs::create_dir(root.path().join(ACTIVE_HISTORY)).unwrap();

        let error = remove_active_pointer_audited(&pointer, PointerCause::Deactivate).unwrap_err();

        assert!(
            error.to_string().contains("Is a directory") || error.to_string().contains("directory")
        );
        assert_eq!(fs::read(pointer).unwrap(), b"remote\n");
    }

    #[test]
    fn parent_command_redacts_secrets_and_caps_length() {
        let args = vec![
            "codexctl".to_owned(),
            "use".to_owned(),
            "--token".to_owned(),
            "access-secret".to_owned(),
            "--authorization".to_owned(),
            "Bearer".to_owned(),
            "authorization-secret".to_owned(),
            "--api-key=another-secret".to_owned(),
            "--token=attached-secret".to_owned(),
            "x".repeat(300),
        ];
        let rendered = render_parent_cmd(&args);
        assert_eq!(rendered, "codexctl");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("authorization-secret"));
        assert!(!rendered.contains("another-secret"));
        assert!(!rendered.contains("attached-secret"));
        assert!(rendered.chars().count() <= 200);
    }

    #[test]
    fn parent_command_splits_shell_scripts_before_redacting() {
        let args = vec![
            "bash".to_owned(),
            "-lc".to_owned(),
            "curl -H 'Authorization: Bearer script-secret' --token script-token && codexctl use"
                .to_owned(),
        ];
        let rendered = render_parent_cmd(&args);
        assert_eq!(rendered, "bash");
        assert!(!rendered.contains("script-secret"));
        assert!(!rendered.contains("script-token"));
    }

    #[test]
    fn deactivation_restores_pointer_when_marker_removal_fails() {
        let root = tempfile::tempdir().unwrap();
        let pointer = root.path().join(".active-account");
        let marker = root.path().join(".native-active.json");
        fs::write(&pointer, b"remote\n").unwrap();
        fs::create_dir(&marker).unwrap();

        let error = remove_pointer_then_marker_audited(&pointer, &marker, PointerCause::Deactivate)
            .unwrap_err();

        assert!(error.to_string().contains("prepared remote activation"));
        assert_eq!(fs::read(pointer).unwrap(), b"remote\n");
        assert!(marker.is_dir());
    }

    fn test_connection(root: &std::path::Path) -> Connection {
        Connection {
            user_id: Some("user".into()),
            alias: Some("remote".into()),
            server: "https://server.invalid".into(),
            device_token_file: root.join("device.token"),
            account_id: "account".into(),
            revision: "revision".into(),
            allow_billing: true,
            launch_pinned: false,
            approved_billing_plan: Some("usage_based".into()),
            approved_billing_class: Some(api::BillingClass::Unknown),
            session_id: String::new(),
        }
    }

    #[test]
    fn rollback_attempts_marker_and_connection_when_config_restore_fails() {
        let root = tempfile::tempdir().unwrap();
        let connection = root.path().join("remote.json");
        let config = root.path().join("config.toml");
        let marker = root.path().join(".native-active.json");
        let pointer = root.path().join(".active-account");
        fs::create_dir(&config).unwrap();
        fs::write(&connection, b"new-connection").unwrap();
        fs::write(&marker, b"new-marker").unwrap();
        fs::write(&pointer, b"new-pointer\n").unwrap();

        let error = rollback_or_context(
            anyhow::anyhow!("original write error"),
            ActivationRollback {
                connection_path: &connection,
                previous_connection: b"old-connection",
                config: &config,
                previous_config: Some(b"old-config"),
                attempted_config: b"new-config",
                marker: &marker,
                previous_marker: Some(b"old-marker"),
                pointer: &pointer,
                previous_pointer: Some(b"old-pointer\n"),
            },
        );

        let rendered = format!("{error:#}");
        assert!(rendered.contains("original write error"));
        assert!(rendered.contains("Codex configuration"));
        assert_eq!(fs::read(connection).unwrap(), b"old-connection");
        assert_eq!(fs::read(marker).unwrap(), b"old-marker");
        assert_eq!(fs::read(pointer).unwrap(), b"old-pointer\n");
    }

    #[test]
    fn rollback_preserves_a_config_edit_outside_this_activation() {
        let root = tempfile::tempdir().unwrap();
        let connection = root.path().join("remote.json");
        let config = root.path().join("config.toml");
        let marker = root.path().join(".native-active.json");
        let pointer = root.path().join(".active-account");
        fs::write(&connection, b"new-connection").unwrap();
        fs::write(&config, b"operator-edit").unwrap();
        fs::write(&marker, b"new-marker").unwrap();
        fs::write(&pointer, b"new-pointer\n").unwrap();

        let error = rollback_or_context(
            anyhow::anyhow!("original write error"),
            ActivationRollback {
                connection_path: &connection,
                previous_connection: b"old-connection",
                config: &config,
                previous_config: Some(b"old-config"),
                attempted_config: b"new-config",
                marker: &marker,
                previous_marker: Some(b"old-marker"),
                pointer: &pointer,
                previous_pointer: Some(b"old-pointer\n"),
            },
        );

        let rendered = format!("{error:#}");
        assert!(rendered.contains("original write error"));
        assert!(rendered.contains("configuration changed"));
        assert_eq!(fs::read(config).unwrap(), b"operator-edit");
        assert_eq!(fs::read(connection).unwrap(), b"old-connection");
        assert_eq!(fs::read(marker).unwrap(), b"old-marker");
        assert_eq!(fs::read(pointer).unwrap(), b"old-pointer\n");
    }

    #[test]
    fn pointer_moves_only_after_saved_billing_approval() {
        let root = tempfile::tempdir().unwrap();
        let connection_path = root.path().join("remote.json");
        let pointer = root.path().join(".active-account");
        let connection = Connection {
            user_id: Some("user".into()),
            alias: Some("remote".into()),
            server: "https://server.invalid".into(),
            device_token_file: root.path().join("device.token"),
            account_id: "account".into(),
            revision: "revision".into(),
            allow_billing: true,
            launch_pinned: false,
            approved_billing_plan: Some("usage_based".into()),
            approved_billing_class: Some(api::BillingClass::Unknown),
            session_id: String::new(),
        };
        save_connection(&connection_path, &connection).unwrap();
        fs::write(&pointer, b"old\n").unwrap();

        write_pointer_after_connection(
            &connection_path,
            &connection,
            &pointer,
            b"new\n",
            |path, bytes| {
                let saved = super::read_connection(&connection_path)?;
                assert!(saved.allow_billing);
                assert_eq!(fs::read(path)?, b"old\n");
                fs::write(path, bytes)?;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(fs::read(pointer).unwrap(), b"new\n");
    }

    #[test]
    fn active_pointer_unlink_failure_is_reported() {
        let pointer = std::path::Path::new("/tmp/.active-account");
        let error = remove_active_pointer_with(pointer, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "synthetic unlink failure",
            ))
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to remove active account pointer")
        );
        assert!(format!("{error:#}").contains("synthetic unlink failure"));
    }

    #[test]
    fn active_pointer_unlink_failure_leaves_marker_for_retry() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join(".native-active.json");
        let pointer = root.path().join(".active-account");
        fs::write(&marker, b"marker").unwrap();
        fs::write(&pointer, b"account\n").unwrap();

        let error = remove_pointer_then_marker_with(
            &pointer,
            &marker,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "synthetic unlink failure",
                ))
            },
            |path| fs::remove_file(path),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to remove active account pointer")
        );
        assert!(marker.exists());
        assert!(pointer.exists());

        remove_pointer_then_marker_with(
            &pointer,
            &marker,
            |path| fs::remove_file(path),
            |path| fs::remove_file(path),
        )
        .unwrap();
        assert!(!marker.exists());
        assert!(!pointer.exists());
    }
}
