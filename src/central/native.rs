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

const PROVIDER: &str = "codexctl-central";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
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
fn native_lock(directory: &Path) -> Result<std::fs::File> {
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
    if !root()?.join(".native-active.json").exists() {
        return Ok(());
    }
    let home = codex_home()?;
    if document(&home)?
        .get("model_provider")
        .and_then(Item::as_str)
        == Some(PROVIDER)
    {
        bail!(
            "remote provider is active; run regular codex or disconnect before local account wrappers"
        );
    }
    Ok(())
}
fn fetch(connection: &Connection, refresh: bool) -> Result<TokenResponse> {
    let url = reqwest::Url::parse(&connection.server)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("remote account requires a loopback HTTP origin; use an SSH tunnel");
    }
    let secret = String::from_utf8(vault::private_read(&connection.device_token_file)?)?;
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(95))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    let response = http
        .post(format!(
            "{}/v1/token",
            connection.server.trim_end_matches('/')
        ))
        .bearer_auth(secret.trim())
        .json(&TokenRequest {
            previous_revision: refresh.then(|| connection.revision.clone()),
            account_id: (!connection.account_id.is_empty()).then(|| connection.account_id.clone()),
            billing: true,
        })
        .send()
        .context("central token request failed")?;
    if !response.status().is_success() {
        bail!(
            "central token request rejected (HTTP {})",
            response.status()
        );
    }
    let token: TokenResponse = response.json().context("invalid central token response")?;
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
    if path.exists() || store::profile_dir(&config::default_paths()?, alias)?.exists() {
        bail!("alias already exists");
    }
    let mut connection = Connection {
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
    if path.exists() {
        bail!("alias registered while connecting");
    }
    connection.account_id = token.chatgpt_account_id;
    connection.revision = token.revision;
    save_connection(&path, &connection)?;
    println!("registered remote account {alias}; run codexctl use {alias}");
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
    if token.billing_class != Some(api::BillingClass::RateLimited)
        && (!connection.allow_billing
            || connection.approved_billing_plan != token.chatgpt_plan_type
            || connection.approved_billing_class != token.billing_class)
    {
        bail!("remote billing changed; select the account again with billing approval");
    }
    {
        let _lock = native_lock(directory)?;
        let mut latest = read_connection(path)?;
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
            bail!("remote billing approval changed during token retrieval");
        }
        if latest.revision == connection.revision {
            latest.revision = token.revision;
            save_connection(path, &latest)?;
        }
    }
    println!("{}", token.access_token);
    Ok(())
}
fn document(home: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(home.join("config.toml")) {
        Ok(text) => Ok(text.parse()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn activate(alias: Option<&str>, allow_billing: bool) -> Result<bool> {
    let explicit = alias.is_some();
    let selected = match alias {
        Some(alias) => alias.to_owned(),
        None => {
            let directory = root()?;
            if !directory.exists() {
                return Ok(false);
            }
            let candidates = std::fs::read_dir(&directory)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "json")
                        && p.file_name().is_some_and(|n| n != ".native-active.json")
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
    if !path.exists() {
        return Ok(false);
    }
    if store::profile_dir(&config::default_paths()?, alias)?.exists() {
        bail!("remote alias conflicts with a local profile; rename one before selection");
    }
    if std::env::var_os("CODEX_HOME").is_some()
        || std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some()
    {
        bail!("remote activation refuses an inherited or pinned Codex home");
    }
    let _lock = native_lock(&root()?)?;
    let mut connection = read_connection(&path)?;
    drop(_lock);
    let token = fetch(&connection, false)?;
    let usage_based = token.billing_class != Some(api::BillingClass::RateLimited);
    if usage_based && !explicit {
        bail!("automatic remote selection refuses usage-based or unknown billing");
    }
    if usage_based && !allow_billing {
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
    connection.allow_billing = usage_based;
    connection.approved_billing_plan = usage_based
        .then(|| token.chatgpt_plan_type.clone())
        .flatten();
    connection.approved_billing_class = usage_based.then_some(token.billing_class).flatten();
    let _lock = native_lock(&root()?)?;
    let home = codex_home()?;
    if crate::daemon::running_pid(&home).is_some() {
        bail!(
            "Codex daemon is running; finish its sessions and run codex app-server daemon stop before remote activation"
        );
    }
    if !home.exists() {
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
    let activation = if marker.exists() {
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
    provider["http_headers"]["ChatGPT-Account-ID"] = value(&connection.account_id);
    provider["auth"]["command"] = value(helper.to_str().context("helper path must be UTF-8")?);
    let mut args = toml_edit::Array::new();
    args.push("central-token");
    args.push("--connection");
    args.push(path.to_str().context("connection path must be UTF-8")?);
    provider["auth"]["args"] = value(args);
    provider["auth"]["refresh_interval_ms"] = value(0);
    provider["auth"]["timeout_ms"] = value(110_000);
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
    let had_marker = marker.exists();
    store::atomic_write(&marker, &serde_json::to_vec(&activation)?)?;
    if let Err(error) = write_config(&destination, doc.to_string().as_bytes()) {
        let not_installed = match std::fs::read(&destination) {
            Ok(bytes) => bytes != doc.to_string().as_bytes(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        if !had_marker && not_installed {
            std::fs::remove_file(&marker)
                .context("failed to roll back prepared remote activation")?;
        }
        return Err(error);
    }
    save_connection(&path, &connection)?;
    println!("switched to remote account {alias}; start regular codex");
    Ok(true)
}

pub fn deactivate() -> Result<()> {
    let marker = root()?.join(".native-active.json");
    if !marker.exists() {
        return Ok(());
    }
    let _lock = native_lock(&root()?)?;
    let active: Activation = serde_json::from_slice(&vault::private_read(&marker)?)?;
    if crate::daemon::running_pid(&active.home).is_some() {
        bail!(
            "Codex daemon is running; finish its sessions and run codex app-server daemon stop before disconnecting"
        );
    }
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
    Ok(())
}
