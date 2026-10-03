//! Device setup, account discovery, and explicit refresh-ownership migration.
use super::{
    enrollment::{Challenge, Grant, Poll, Start},
    managed::{Account, Import, RevokeDevice},
    native, transport, vault,
};
use crate::status_format::format_window_reset as reset_time;
use crate::status_json::{self, AccountStatus, Source, State};
use crate::{api, config, profile, store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub server: String,
    pub token_file: PathBuf,
    pub user_id: String,
}
fn root() -> Result<PathBuf> {
    Ok(config::default_paths()?.codexctl_dir().join("central"))
}
fn path() -> Result<PathBuf> {
    Ok(root()?.join(".server.json"))
}
fn pending_path() -> Result<PathBuf> {
    Ok(root()?.join(".pending-server.json"))
}
fn registration(path: &Path) -> Result<Option<Connection>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&vault::private_read(path)?)?))
}
pub fn connection() -> Result<Option<Connection>> {
    registration(&path()?)
}
fn secret(connection: &Connection) -> Result<String> {
    transport::origin(&connection.server)?;
    Ok(
        String::from_utf8(vault::private_read(&connection.token_file)?)?
            .trim()
            .into(),
    )
}
fn check(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
    if !response.status().is_success() {
        bail!("server request rejected (HTTP {})", response.status());
    }
    Ok(response)
}
fn request(connection: &Connection, path: &str) -> Result<reqwest::blocking::RequestBuilder> {
    transport::origin(&connection.server)?;
    Ok(transport::blocking()?
        .get(format!("{}{path}", connection.server.trim_end_matches('/')))
        .bearer_auth(secret(connection)?))
}
fn browser(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        bail!("automatic browser launch is unsupported; run connect with --no-browser");
    };
    let status = std::process::Command::new(program).arg(url).status()?;
    if !status.success() {
        bail!("browser did not open; use the printed sign-in link");
    }
    Ok(())
}

/// Existing remote aliases renew on the server. Local login retains its old contract.
pub fn login(
    alias: &str,
    label: Option<&str>,
    allow_adopt: bool,
    no_browser: bool,
    cancel: bool,
) -> Result<bool> {
    let alias = store::validate_alias(alias)?;
    let Some(connection) = connection()? else {
        return Ok(false);
    };
    if !known_server_alias(&connection, alias)? {
        return Ok(false);
    }
    let catalog = catalog()
        .with_context(|| format!("cannot renew server account {alias}; local login is disabled"))?
        .context("machine registration removed; local login is disabled for this server account")?;
    let Some(account) = catalog
        .accounts
        .iter()
        .find(|a| a.alias.eq_ignore_ascii_case(alias))
    else {
        bail!(
            "known server account {alias} is absent from the catalog; local login is disabled; reconcile its migration or connection records"
        );
    };
    if label.is_some() || allow_adopt {
        bail!(
            "server login preserves the account identity and label; omit --label and --allow-adopt"
        );
    }
    let connection = catalog.connection;
    let alias = &account.alias;
    let directory = root()?;
    store::ensure_private_dir(&directory)?;
    let lock_name = format!(
        "login-{}.lock",
        vault::digest(alias.to_ascii_lowercase().as_bytes())
    );
    let receipt_lock = vault::registry_lock(&directory, &lock_name)?;
    let path = directory.join(format!(
        ".login-{}.json",
        vault::digest(alias.to_ascii_lowercase().as_bytes())
    ));
    let mut id = if path.try_exists()? {
        let saved: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
        if saved["server"] != connection.server
            || saved["userId"] != connection.user_id
            || saved["alias"] != *alias
        {
            bail!(
                "saved server login belongs to another registration; retain it until ownership is reconciled"
            );
        }
        saved["id"]
            .as_str()
            .context("invalid saved login operation")?
            .to_owned()
    } else {
        vault::digest(&super::enrollment::random_bytes())
    };
    let persist = |id: &str| {
        store::atomic_write(
            &path,
            &serde_json::to_vec(
                &json!({"server":connection.server,"userId":connection.user_id,"alias":alias,"id":id}),
            )?,
        )
    };
    let call = |endpoint: &str, id: &str| -> Result<Value> {
        require_current_connection(&connection)?;
        let http = transport::blocking()?;
        let response = http
            .post(format!(
                "{}{endpoint}",
                connection.server.trim_end_matches('/')
            ))
            .bearer_auth(secret(&connection)?)
            .json(&json!({"alias":alias,"id":id}))
            .send()?;
        if !response.status().is_success() {
            bail!(
                "server login request rejected (HTTP {}); the operation is retained, rerun codexctl login {alias}",
                response.status()
            );
        }
        let value: Value = response.json()?;
        require_current_connection(&connection)?;
        if value["userId"] != connection.user_id || value["alias"] != *alias {
            bail!("server login identity changed");
        }
        let response_id = value["id"]
            .as_str()
            .context("missing server login operation")?;
        if response_id.len() != 64
            || !response_id
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            bail!("invalid server login operation");
        }
        Ok(value)
    };
    let had_receipt = path.try_exists()?;
    if !cancel {
        persist(&id)?;
    }
    drop(receipt_lock);
    let requested_id = id.clone();
    let mut status = if cancel {
        call("/v1/relogin/status", if had_receipt { &id } else { "" })?
    } else {
        println!(
            "Renewing OpenAI login for server account {alias}. Sign in to that same OpenAI account and workspace."
        );
        println!("Approving a different account can invalidate its previous OpenAI login.");
        call("/v1/relogin/start", &id)?
    };
    id = status["id"]
        .as_str()
        .context("missing login operation")?
        .into();
    {
        let _lock = vault::registry_lock(&directory, &lock_name)?;
        let saved = if path.try_exists()? {
            Some(serde_json::from_slice::<Value>(&vault::private_read(
                &path,
            )?)?)
        } else {
            None
        };
        if saved
            .as_ref()
            .is_none_or(|value| value["id"] == requested_id)
        {
            persist(&id)?;
        }
    }
    if cancel {
        status = call("/v1/relogin/cancel", &id)?;
    }
    let clear_receipt = || -> Result<()> {
        let _lock = vault::registry_lock(&directory, &lock_name)?;
        if path.try_exists()? {
            let saved: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
            if saved["id"] == id {
                std::fs::remove_file(&path)?;
                store::sync_directory(&directory)?;
            }
        }
        Ok(())
    };
    let started = std::time::Instant::now();
    let mut displayed = false;
    loop {
        match status["status"].as_str() {
            Some("verifying") if status["error"].is_string() => {
                bail!(
                    "server could not finish credential verification; the operation is retained. Retry codexctl login {alias}"
                );
            }
            Some("completed") => {
                clear_receipt()?;
                println!(
                    "Server login renewed for {alias}. Connected machines can keep using this account."
                );
                return Ok(true);
            }
            Some("failed" | "canceled") => {
                clear_receipt()?;
                let reason = match status["error"].as_str() {
                    Some("wrong_account") => {
                        "OpenAI returned a different account; its grant is retained on the server"
                    }
                    Some("login_stopped_account_requires_relogin") => {
                        "login stopped; the account requires a new login"
                    }
                    _ => "server login failed; the account remains unavailable",
                };
                bail!("{reason}. Retry with codexctl login {alias}");
            }
            Some("starting" | "verifying" | "pending") => {}
            _ => bail!("unsupported server login state; operation retained"),
        }
        if !cancel && !displayed && status["status"] == "pending" {
            if status["verificationUrl"] != "https://auth.openai.com/codex/device" {
                bail!("unsupported OpenAI login URL");
            }
            let code = status["userCode"]
                .as_str()
                .context("missing OpenAI login code")?;
            if code.is_empty() || code.len() > 128 || !code.bytes().all(|c| c.is_ascii_graphic()) {
                bail!("invalid OpenAI login code");
            }
            println!("Open https://auth.openai.com/codex/device and enter: {code}");
            if !no_browser {
                browser("https://auth.openai.com/codex/device")?;
            }
            displayed = true;
        }
        if started.elapsed() > Duration::from_secs(960) {
            bail!(
                "login status timed out; rerun codexctl login {alias} to resume, or add --cancel"
            );
        }
        std::thread::sleep(Duration::from_secs(1));
        status = call("/v1/relogin/status", &id)?;
        if status["id"] != id {
            bail!("server login operation changed");
        }
    }
}
pub fn connect(server: &str, name: Option<&str>, no_browser: bool) -> Result<()> {
    transport::origin(server)?;
    if connection()?.is_some() || registration(&pending_path()?)?.is_some() {
        bail!("this machine is already connected; run codexctl disconnect --forget first");
    }
    native::require_local_mode()?;
    let name = name
        .map(str::to_owned)
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "My machine".into());
    let http = transport::blocking()?;
    let challenge: Challenge = check(
        http.post(format!(
            "{}/v1/enrollment/start",
            server.trim_end_matches('/')
        ))
        .json(&Start { name })
        .send()?,
    )?
    .json()?;
    let expected = transport::origin(server)?;
    let verification = reqwest::Url::parse(&challenge.verification_url)?;
    if verification.origin() != expected.origin()
        || verification.path() != "/enroll"
        || challenge.expires_in > 600
        || challenge.interval < 1
        || challenge.interval > 10
    {
        bail!("invalid enrollment response");
    }
    println!(
        "Sign in with company SSO: {}\nConfirm code: {}",
        challenge.verification_url, challenge.user_code
    );
    if !no_browser {
        browser(&challenge.verification_url)?;
    }
    let deadline = Instant::now() + Duration::from_secs(challenge.expires_in);
    let token = loop {
        if Instant::now() >= deadline {
            bail!("device sign-in expired; run connect again");
        }
        let response = http
            .post(format!(
                "{}/v1/enrollment/poll",
                server.trim_end_matches('/')
            ))
            .json(&Poll {
                device_code: challenge.device_code.clone(),
            })
            .send()?;
        if response.status() == reqwest::StatusCode::ACCEPTED
            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            std::thread::sleep(Duration::from_secs(challenge.interval));
            continue;
        }
        let grant: Grant = check(response)?.json()?;
        if grant.device_token.is_empty() {
            bail!("invalid enrollment grant");
        }
        break grant.device_token;
    };
    install(server, &token)?;
    println!(
        "Machine connected. Run codexctl migrate --all on your source machine, or codexctl use."
    );
    Ok(())
}
pub(crate) fn install(server: &str, token: &str) -> Result<()> {
    transport::origin(server)?;
    let directory = root()?;
    let _lock = vault::lock(&directory, "native.lock")?;
    if path()?.try_exists()? || pending_path()?.try_exists()? {
        bail!("machine already connected");
    }
    let me: Value = check(
        transport::blocking()?
            .get(format!("{}/v1/me", server.trim_end_matches('/')))
            .bearer_auth(token)
            .send()?,
    )?
    .json()?;
    let user_id = me["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("invalid server identity")?
        .to_owned();
    let token_file = directory.join(format!(
        ".device-{}.token",
        &vault::digest(token.as_bytes())[..16]
    ));
    let connection = Connection {
        server: server.trim_end_matches('/').into(),
        token_file,
        user_id,
    };
    let registration = serde_json::to_vec(&connection)?;
    // Allocate cleanup evidence before any credential can appear on disk.
    store::atomic_write(&pending_path()?, &registration)?;
    vault::create_secret(&connection.token_file, token.as_bytes())?;
    store::sync_directory(&directory)?;
    store::atomic_write(&path()?, &registration)?;
    std::fs::remove_file(pending_path()?)?;
    store::sync_directory(&directory)?;
    Ok(())
}
// This cache only routes login. Selection and credential delivery still require
// a current catalog from the account server.
#[derive(Serialize, Deserialize)]
struct KnownAliases {
    connection: Connection,
    aliases: Vec<String>,
}

fn known_server_alias(connection: &Connection, alias: &str) -> Result<bool> {
    let directory = root()?;
    let cache = directory.join(".catalog.json");
    if cache.try_exists()? {
        let known: KnownAliases = serde_json::from_slice(&vault::private_read(&cache)?)
            .context("cannot read known server aliases; local login is disabled")?;
        if known.connection == *connection
            && known
                .aliases
                .iter()
                .any(|name| name.eq_ignore_ascii_case(alias))
        {
            return Ok(true);
        }
    }
    // Retained migration and native connection records fence names even if the
    // cached catalog is absent or no longer lists them. Match aliases on Linux too.
    let profiles = config::default_paths()?.profiles_dir();
    for (parent, name, marker) in [
        (directory, format!("{alias}.json"), None),
        (profiles, alias.to_owned(), Some(".central-transfer.json")),
    ] {
        if !parent.try_exists()? {
            continue;
        }
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(&name))
                && match marker {
                    Some(marker) => entry.path().join(marker).try_exists()?,
                    None => true,
                }
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) struct Catalog {
    pub connection: Connection,
    pub accounts: Vec<Account>,
}
pub(super) fn require_current_connection(expected: &Connection) -> Result<()> {
    let latest =
        connection()?.context("server connection removed during discovery or selection")?;
    if latest.server != expected.server
        || latest.user_id != expected.user_id
        || latest.token_file != expected.token_file
    {
        bail!("server connection changed during discovery or selection");
    }
    Ok(())
}
pub use super::resets::{Inventory as ResetInventory, Outcome as ResetOutcome};

pub fn resets() -> Result<Option<ResetInventory>> {
    let Some(connection) = connection()? else {
        return Ok(None);
    };
    let result: super::resets::Inventory = check(
        request(&connection, "/v1/resets")?
            .send()
            .context("cannot reach the account server for reset credits")?,
    )?
    .json()?;
    require_current_connection(&connection)?;
    if result.user_id != connection.user_id {
        bail!("server user identity changed");
    }
    Ok(Some(result))
}

/// Retain the operation ID until its result is known, including across CLI restarts.
pub fn redeem_reset(alias: &str) -> Result<api::ConsumeResetResponse> {
    let alias = super::managed::normalize_alias(alias)?;
    let connection = connection()?.context("run codexctl connect first")?;
    let identity = vault::digest(&serde_json::to_vec(&(
        &connection.server,
        &connection.user_id,
        alias,
    ))?);
    let directory = root()?.join("reset-requests");
    let _lock = vault::lock(&directory, &format!("{identity}.lock"))?;
    require_current_connection(&connection)?;
    let pending = directory.join(format!("{identity}.json"));
    let request_id: String = if pending.try_exists()? {
        serde_json::from_slice(&vault::private_read(&pending)?)?
    } else {
        let id = api::new_redeem_request_id("server");
        store::atomic_write(&pending, &serde_json::to_vec(&id)?)?;
        id
    };
    let response = transport::blocking()?
        .post(format!(
            "{}/v1/resets/redeem",
            connection.server.trim_end_matches('/')
        ))
        .bearer_auth(secret(&connection)?)
        .json(&super::resets::Redemption {
            alias: alias.into(),
            redeem_request_id: request_id,
        })
        .send()
        .context("reset outcome unknown; rerun the same command on this machine to retry safely")?;
    if !response.status().is_success() {
        let status = response.status();
        let error: Value = response.json().unwrap_or(Value::Null);
        let reason = match error["error"].as_str() {
            Some("nothing_to_reset") => "no exhausted window; no reset was spent",
            Some("no_reset_credit") => "no qualifying banked reset is available",
            Some("reset_rejected") => {
                "provider rejected this operation; retry to start a new redemption"
            }
            _ => "reset outcome unknown; rerun the same command on this machine to retry safely",
        };
        if matches!(
            error["error"].as_str(),
            Some("nothing_to_reset" | "no_reset_credit" | "reset_rejected")
        ) {
            std::fs::remove_file(&pending)?;
            store::sync_directory(&directory)?;
        }
        bail!("{alias}: {reason} (HTTP {status})");
    }
    let result: api::ConsumeResetResponse = response
        .json()
        .context("reset outcome unknown; rerun the same command on this machine to retry safely")?;
    if result.code == api::ConsumeResetCode::Unknown {
        bail!("reset outcome unknown; rerun the same command on this machine to retry safely");
    }
    require_current_connection(&connection)?;
    std::fs::remove_file(&pending)?;
    store::sync_directory(&directory)?;
    Ok(result)
}

pub fn accounts() -> Result<Option<Vec<Account>>> {
    Ok(catalog()?.map(|c| c.accounts))
}
pub(super) fn catalog() -> Result<Option<Catalog>> {
    let Some(connection) = connection()? else {
        return Ok(None);
    };
    let directory = root()?;
    // Serialize discovery with its cache write so an older response cannot
    // erase a server alias that another discovery already recorded.
    let _discovery = vault::registry_lock(&directory, "catalog.lock")?;
    require_current_connection(&connection)?;
    let response = request(&connection, "/v1/accounts")?
        .send()
        .context("cannot reach the account server")?;
    let accounts: Vec<Account> = check(response)?.json()?;
    let _lock = native::native_lock(&directory)?;
    require_current_connection(&connection)?;
    for account in &accounts {
        if account.user_id != connection.user_id {
            bail!("server user identity changed");
        }
    }
    store::atomic_write(
        &directory.join(".catalog.json"),
        &serde_json::to_vec(&KnownAliases {
            connection: connection.clone(),
            aliases: accounts
                .iter()
                .map(|account| account.alias.clone())
                .collect(),
        })?,
    )?;
    Ok(Some(Catalog {
        connection,
        accounts,
    }))
}
// Display rows never enter the server catalog used by selection or recovery.
struct DisplayRow {
    cells: Vec<String>,
    billing: api::BillingClass,
    account: AccountStatus,
}

fn usage_cells(usage: &api::RateLimitResponse) -> Vec<String> {
    let limits = usage.rate_limit.as_ref();
    vec![
        percentage(
            limits
                .and_then(api::RateLimit::short_window)
                .map(|w| w.used_percent),
        ),
        percentage(
            limits
                .and_then(api::RateLimit::long_window)
                .map(|w| w.used_percent),
        ),
        reset_time(
            limits
                .and_then(api::RateLimit::long_window)
                .and_then(api::RateLimitWindow::reset_timestamp),
        ),
        usage
            .credits
            .as_ref()
            .and_then(|c| c.balance.as_deref())
            .map(|balance| match balance.parse::<f64>() {
                Ok(value) if value.is_finite() => format!("{value:.2}"),
                _ => balance.to_owned(),
            })
            .unwrap_or_else(|| "-".into()),
    ]
}

fn percentage(value: Option<f64>) -> String {
    value.map_or("-".into(), |n| format!("{n:.0}%"))
}

async fn local_display_rows(
    profiles: &[profile::Profile],
    paths: &config::Paths,
    active: Option<&str>,
) -> Result<Vec<DisplayRow>> {
    let client = api::http_client()?;
    futures::future::try_join_all(profiles.iter().map(|p| {
        let client = &client;
        async move {
            // A pending migration also fences local credentials. Never query a
            // retired copy or infer migration from an alias shared with the server.
            if p.dir.join(".central-transfer.json").try_exists()? {
                return Ok(None);
            }
            let mut row = DisplayRow {
                cells: vec![
                    p.meta.alias.clone(),
                    p.meta.label.clone().unwrap_or_else(|| "-".into()),
                    p.meta.plan.clone().unwrap_or_else(|| "-".into()),
                ],
                billing: api::BillingClass::Unknown,
                account: AccountStatus::local(&p.meta, false),
            };
            let auth_path = profile::auth_json_path_for_profile_from(paths, p, active);
            let usage = match api::read_auth_json(&auth_path) {
                Ok(auth) => {
                    api::fetch_usage_async(client, &auth.access_token, auth.account_id.as_deref())
                        .await
                        .map_err(|_| "usage unavailable")
                }
                Err(_) => Err("credentials unavailable"),
            };
            crate::statusline::record_local(paths, &p.meta, usage.as_ref().ok());
            match usage {
                Ok(usage) => {
                    row.account.set_usage(&usage);
                    // Local status groups unknown billing with rate-limited
                    // results for display; selection still uses the catalog.
                    row.billing = match usage.billing_class() {
                        api::BillingClass::UsageBased => api::BillingClass::UsageBased,
                        _ => api::BillingClass::RateLimited,
                    };
                    if let Some(plan) = &usage.plan_type {
                        row.cells[2] = plan.clone();
                    }
                    row.cells.extend(usage_cells(&usage));
                    row.cells.extend(["local".into(), "-".into()]);
                }
                Err(reason) => {
                    row.account.error = Some(reason.into());
                    // Match the local status command's error grouping. This is
                    // display metadata, never permission to select or bill.
                    row.billing = if p
                        .meta
                        .plan
                        .as_deref()
                        .is_some_and(|plan| plan.contains("usage_based"))
                    {
                        api::BillingClass::UsageBased
                    } else {
                        api::BillingClass::RateLimited
                    };
                    row.cells.extend([
                        "-".into(),
                        "-".into(),
                        "-".into(),
                        "-".into(),
                        "local".into(),
                        reason.into(),
                    ]);
                }
            }
            Ok(Some(row))
        }
    }))
    .await
    .map(|rows| rows.into_iter().flatten().collect())
}

pub fn show(status: bool, filter: Option<api::BillingClass>, json: bool) -> Result<bool> {
    let Some(catalog) = catalog()? else {
        return Ok(false);
    };
    let accounts = catalog.accounts;
    let paths = config::default_paths()?;
    let profiles = profile::list_profiles_from(&paths)?;
    let local_active = profile::get_active_from(&paths)?;
    let mut rows = tokio::runtime::Runtime::new()?.block_on(local_display_rows(
        &profiles,
        &paths,
        local_active.as_deref(),
    ))?;
    let show_usage = status || !rows.is_empty();
    let active = native::active_alias()?;
    for account in &accounts {
        // Summary columns do not prove window durations on an older server.
        let usage = (account.available && !account.usage_stale && account.usage_error.is_none())
            .then(|| account.statusline_usage.clone())
            .flatten();
        crate::statusline::record(
            &paths,
            crate::statusline::Selection {
                alias: account.alias.clone(),
                source: crate::statusline::Source::Server {
                    server: catalog.connection.server.clone(),
                    user_id: Some(account.user_id.clone()),
                    account_id: account.account_id.clone(),
                },
            },
            account.label.as_deref(),
            usage,
        );
    }
    let mut server_rows: Vec<_> = accounts
        .iter()
        .map(|account| {
            let state = if !account.available {
                "unavailable"
            } else if active.as_deref() == Some(&account.alias) {
                "active"
            } else {
                "server"
            };
            DisplayRow {
                cells: vec![
                    account.alias.clone(),
                    account.label.clone().unwrap_or_else(|| "-".into()),
                    account.plan.clone().unwrap_or_else(|| "-".into()),
                    percentage(account.primary_used),
                    percentage(account.secondary_used),
                    reset_time(account.resets_at),
                    "-".into(),
                    state.into(),
                    if account.usage_stale {
                        match account.usage_age_seconds {
                            Some(age) => format!(
                                "usage stale ({age}s): {}",
                                account.usage_error.as_deref().unwrap_or("refresh pending")
                            ),
                            None => format!(
                                "usage unknown: {}",
                                account.usage_error.as_deref().unwrap_or("not fetched")
                            ),
                        }
                    } else {
                        "-".into()
                    },
                ],
                billing: account.billing_class,
                account: AccountStatus {
                    alias: account.alias.clone(),
                    label: account.label.clone(),
                    plan: account.plan.clone(),
                    source: Source::Server,
                    state: if !account.available {
                        State::Unavailable
                    } else if active.as_deref() == Some(&account.alias) {
                        State::Active
                    } else {
                        State::Server
                    },
                    primary_used_percent: account.primary_used,
                    secondary_used_percent: account.secondary_used,
                    resets_at: status_json::timestamp(account.resets_at),
                    billing_class: account.billing_class,
                    error: if !account.available {
                        Some("account unavailable".into())
                    } else if account.usage_stale {
                        Some(
                            account
                                .usage_error
                                .clone()
                                .unwrap_or_else(|| "usage stale".into()),
                        )
                    } else {
                        None
                    },
                    usage_age_seconds: account.usage_age_seconds,
                    usage_stale: Some(account.usage_stale),
                },
            }
        })
        .collect();
    server_rows.append(&mut rows);
    server_rows.retain(|row| filter.is_none_or(|f| row.billing == f));
    if json {
        let accounts: Vec<_> = server_rows.into_iter().map(|row| row.account).collect();
        status_json::print(&accounts)?;
        return Ok(true);
    }
    if !server_rows.is_empty() {
        let headers = [
            "Account",
            "Label",
            "Plan",
            "Short used",
            "Long used",
            "Resets",
            "Balance",
            "State",
            "Error",
        ];
        let columns: Vec<_> = (0..headers.len())
            .filter(|&i| {
                (show_usage || !(3..=6).contains(&i))
                    && (i == 0
                        || i == 7
                        || server_rows
                            .iter()
                            .any(|row| !row.cells[i].trim().is_empty() && row.cells[i] != "-"))
            })
            .collect();
        let mut table = comfy_table::Table::new();
        table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
        table.set_header(columns.iter().map(|&i| headers[i]));
        for row in server_rows {
            table.add_row(columns.iter().map(|&i| {
                row.cells[i]
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(160)
                    .collect::<String>()
            }));
        }
        println!("{table}");
    } else if !accounts.is_empty() {
        println!("no matching accounts found.");
    }
    if accounts.is_empty() {
        println!("No server accounts yet. Run codexctl migrate --all on your source machine.");
    }
    Ok(true)
}
pub fn whoami() -> Result<bool> {
    let Some(alias) = native::active_alias()? else {
        return Ok(false);
    };
    println!("{alias} [server]");
    Ok(true)
}
pub fn devices(revoke: Option<&str>) -> Result<()> {
    let connection = connection()?.context("not connected to an account server")?;
    if let Some(id) = revoke {
        check(
            transport::blocking()?
                .post(format!("{}/v1/devices/revoke", connection.server))
                .bearer_auth(secret(&connection)?)
                .json(&RevokeDevice { id: id.into() })
                .send()?,
        )?;
        println!("Device revoked: {id}");
    } else {
        let devices: Vec<Value> = check(request(&connection, "/v1/devices")?.send()?)?.json()?;
        for device in devices {
            println!(
                "{} {}",
                device["id"].as_str().unwrap_or("?"),
                if device["revoked"] == true {
                    "revoked"
                } else {
                    "active"
                }
            );
        }
    }
    Ok(())
}
pub fn disconnect(forget: bool) -> Result<()> {
    // Cleanup must remain available while migration waits on the server.
    // The native lock serializes provider restoration with activation.
    let _lock = native::native_lock(&root()?)?;
    native::deactivate_locked()?;
    if forget {
        let registrations: Vec<_> = [connection()?, registration(&pending_path()?)?]
            .into_iter()
            .flatten()
            .collect();
        for connection in &registrations {
            // Remove only this connection's files after the provider is restored.
            for entry in std::fs::read_dir(root()?)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json")
                    && !path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                {
                    native::remove_managed_connection(&path, connection)?;
                }
            }
            // Keep the connection as a durable cleanup reference until its credential
            // is gone. A retry after either removal must tolerate a missing token.
            match std::fs::remove_file(&connection.token_file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            store::sync_directory(
                connection
                    .token_file
                    .parent()
                    .context("missing token directory")?,
            )?;
        }
        if !registrations.is_empty() {
            for reference in [path()?, pending_path()?] {
                match std::fs::remove_file(reference) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            store::sync_directory(&root()?)?;
        }
    }
    println!("Disconnected from the remote provider.");
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Transfer {
    server: String,
    user_id: String,
    alias: String,
    account_id: String,
    confirmed: bool,
}

pub fn migrate(all: bool, exclusive_owner: bool) -> Result<()> {
    if !all || !exclusive_owner {
        bail!(
            "migration requires --all --exclusive-owner; first stop every Codex session and login owner using these accounts on all machines"
        );
    }
    if std::env::var_os("CODEX_HOME").is_some()
        || std::env::var_os("CODEXCTL_PINNED_ALIAS").is_some()
    {
        bail!("migration requires the normal unpinned home");
    }
    let paths = config::default_paths()?;
    let _mode = native::exclusive_mode(&paths)?;
    if crate::daemon::running_pid(&paths.codex_home()).is_some() {
        bail!("finish Codex sessions and stop the Codex daemon before migration");
    }
    require_stopped_owners(&paths)?;
    let connection = connection()?.context("run codexctl connect first")?;
    // Reject an unsafe persisted origin before any credential is retired.
    transport::origin(&connection.server)?;
    let _store = store::lock(&paths)?;
    let profiles = profile::list_profiles_from(&paths)?;
    if profiles.is_empty() {
        bail!("no local profiles to migrate");
    }
    // Validate every source before changing ownership for the first account.
    let prepared = profiles
        .iter()
        .map(|p| prepare_auth(&paths, p))
        .collect::<Result<Vec<_>>>()?;
    let mut seats = std::collections::HashSet::new();
    for auth in &prepared {
        if !seats.insert((
            vault::account(auth)?,
            api::token_subject(vault::token(auth)?),
        )) {
            bail!(
                "local aliases share an account; select one alias for that account before migration"
            );
        }
    }
    for (p, auth) in profiles.into_iter().zip(prepared) {
        vault::validate_auth(&auth)?;
        let marker = p.dir.join(".central-transfer.json");
        let mut transfer = Transfer {
            server: connection.server.clone(),
            user_id: connection.user_id.clone(),
            alias: p.meta.alias.clone(),
            account_id: vault::account(&auth)?,
            confirmed: false,
        };
        if marker.try_exists()? {
            let prior: Transfer = serde_json::from_slice(&vault::private_read(&marker)?)?;
            if prior.server != transfer.server
                || prior.user_id != transfer.user_id
                || prior.alias != transfer.alias
                || prior.account_id != transfer.account_id
            {
                bail!("profile already transferred to another server or user");
            }
            transfer.confirmed = prior.confirmed;
        }
        // Fence local refresh before sending credentials. Uncertain HTTP completion stays fenced.
        store::atomic_write(
            &p.dir.join(".central-source.json"),
            &serde_json::to_vec(&auth)?,
        )?;
        store::atomic_write(&marker, &serde_json::to_vec(&transfer)?)?;
        retire_local_holders(&paths, &p, &auth)?;
        let input = Import {
            alias: p.meta.alias.clone(),
            label: p.meta.label.clone(),
            auth,
        };
        let result: Account = check(
            transport::blocking()?
                .post(format!("{}/v1/accounts", connection.server))
                .bearer_auth(secret(&connection)?)
                .json(&input)
                .send()
                .context("migration completion unknown; rerun the same migration to reconcile")?,
        )?
        .json()?;
        if result.user_id != transfer.user_id
            || result.account_id != transfer.account_id
            || !result.available
        {
            bail!("server has not verified this account; local refresh remains fenced");
        }
        transfer.confirmed = true;
        store::atomic_write(&marker, &serde_json::to_vec(&transfer)?)?;
        let _lock = vault::lock(&root()?, "native.lock")?;
        require_current_connection(&connection)?;
        native::sync_account(&connection, &result)?;
        println!("Transferred {}", p.meta.alias);
    }
    {
        let _lock = native::native_lock(&root()?)?;
        if !native::repair_sessions_if_active(native::SessionProviderAction::Rewrite)
            .context("accounts transferred; session repair failed; resolve the reported cause and retry codexctl session-provider rewrite")? {
            println!("Session providers: deferred until codexctl use activates a server account.");
        }
    }
    println!(
        "Migration complete. Run codexctl use, then codexctl codex. Resume old sessions with codexctl codex resume <session-id>. Keep other machines from refreshing their old credential copies."
    );
    Ok(())
}

pub fn select(accounts: &[Account]) -> Result<String> {
    let most = std::env::var("CODEXCTL_SELECT").ok().is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "most-available" | "most_available" | "headroom" | "legacy" | "off" | "false"
        )
    });
    let score = |a: &Account| a.usage_score.unwrap_or(f64::MAX);
    accounts.iter().filter(|a|a.available&&!a.usage_stale&&a.billing_class==api::BillingClass::RateLimited&&(a.primary_used.is_some()||a.secondary_used.is_some())).min_by(|a,b|{
        let by_score=score(a).total_cmp(&score(b));
        let exhausted_a=score(a)>=500.0;let exhausted_b=score(b)>=500.0;
        exhausted_a.cmp(&exhausted_b).then_with(||if most||exhausted_a{by_score}else{a.resets_at.unwrap_or(i64::MAX).cmp(&b.resets_at.unwrap_or(i64::MAX)).then(by_score)})
    }).map(|a|a.alias.clone()).context("no available account with verified included usage; select an alias explicitly to approve credit billing")
}

/// Plan a reset only after included headroom is unavailable. Spending is deferred
/// until native activation has checked the home and account migration fences.
pub(super) fn select_for_activation(
    accounts: &[Account],
    allow_resets: bool,
) -> Result<(String, bool)> {
    let ready: Vec<_> = accounts
        .iter()
        .filter(|a| {
            [a.primary_used, a.secondary_used]
                .into_iter()
                .flatten()
                .all(|used| used.is_finite() && (0.0..100.0).contains(&used))
        })
        .cloned()
        .collect();
    if let Ok(alias) = select(&ready) {
        return Ok((alias, false));
    }
    if !allow_resets {
        return select(accounts).map(|alias| (alias, false));
    }
    let inventory = resets()?.context("account server disconnected")?;
    let mut candidates = Vec::new();
    for account in accounts {
        if !account.available
            || account.usage_stale
            || account.billing_class == api::BillingClass::UsageBased
            || !account
                .plan
                .as_deref()
                .is_some_and(api::is_known_rate_limited_plan)
            || ![account.primary_used, account.secondary_used]
                .into_iter()
                .flatten()
                .any(|used| used.is_finite() && used >= 100.0)
        {
            continue;
        }
        let Some(reset) = inventory.accounts.iter().find(|r| r.alias == account.alias) else {
            continue;
        };
        if let ResetOutcome::Read {
            applicable,
            credits,
            ..
        } = &reset.outcome
            && *applicable > 0
            && let Some(expiry) = credits
                .iter()
                .filter(|c| c.is_available())
                .filter(|c| {
                    c.expires_at_timestamp()
                        .is_none_or(|t| t > chrono::Utc::now().timestamp())
                })
                .map(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
                .min()
        {
            candidates.push((expiry, account.alias.clone()));
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|(_, alias)| (alias, true))
        .context("no included headroom or qualifying banked reset is available")
}

fn same_seat(left: &Value, right: &Value) -> Result<bool> {
    if right.get("tokens").is_none_or(Value::is_null)
        && right.get("access_token").is_none_or(Value::is_null)
        && right
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|k| !k.is_empty())
    {
        return Ok(false);
    }
    let account = vault::account(left)?;
    let candidate_account = vault::account(right).ok();
    let login = api::token_logins(vault::token(left)?);
    let candidate_login = api::token_logins(vault::token(right)?);
    if candidate_account
        .as_ref()
        .is_some_and(|other| other != &account)
        || login.differs(&candidate_login)
    {
        return Ok(false);
    }
    if candidate_account.is_none() || !login.same(&candidate_login) {
        bail!(
            "cannot prove workspace or login of a local credential copy; reconcile local homes before migration"
        );
    }
    Ok(true)
}
fn holders(paths: &config::Paths) -> Result<Vec<PathBuf>> {
    let mut holders = vec![paths.codex_auth_json()];
    // Recovery can install another account under an exec home's original alias.
    // Match credentials by identity across every known home, not by directory name.
    for root in [
        paths.profiles_dir(),
        paths.login_homes_dir(),
        paths.exec_homes_dir(),
    ] {
        if !root
            .try_exists()
            .context("cannot inspect local credential home")?
        {
            continue;
        }
        if std::fs::symlink_metadata(&root)?.file_type().is_symlink() {
            bail!("managed home root must be a real directory");
        }
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                bail!("managed home must be a real directory");
            }
            if !entry.file_type()?.is_dir() {
                continue;
            }
            holders.push(entry.path().join("auth.json"));
            for session in std::fs::read_dir(entry.path())? {
                let session = session?;
                if session
                    .file_name()
                    .to_string_lossy()
                    .starts_with("session-")
                {
                    if !session.file_type()?.is_dir() {
                        bail!("isolated login session must be a real directory");
                    }
                    holders.push(session.path().join("auth.json"));
                }
            }
        }
    }
    holders.sort();
    holders.dedup();
    Ok(holders)
}

fn existing_holders(paths: &config::Paths) -> Result<Vec<PathBuf>> {
    let mut present = Vec::new();
    for path in holders(paths)? {
        // Only NotFound means absence. Inaccessible copies may still own refresh.
        if path
            .try_exists()
            .context("cannot inspect local credential path")?
        {
            present.push(path);
        }
    }
    Ok(present)
}

fn prepare_auth(paths: &config::Paths, p: &profile::Profile) -> Result<Value> {
    let source = if p.dir.join(".central-transfer.json").try_exists()?
        && p.dir.join(".central-source.json").try_exists()?
    {
        p.dir.join(".central-source.json")
    } else {
        p.auth_json_path()
    };
    let mut auth: Value = serde_json::from_slice(&vault::private_read(&source)?)?;
    vault::validate_auth(&auth)?;
    for path in existing_holders(paths)? {
        let candidate: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
        if !same_seat(&auth, &candidate)? {
            continue;
        }
        vault::validate_auth(&candidate)?;
        let a = vault::token(&auth)?;
        let b = vault::token(&candidate)?;
        let order = api::token_issued_at(a)
            .zip(api::token_issued_at(b))
            .or_else(|| api::token_expiry(a).zip(api::token_expiry(b)));
        match order {
            Some((old, new)) if new > old => auth = candidate,
            Some((old, new)) if new < old => {}
            _ if auth != candidate => bail!(
                "cannot determine the latest credentials for {}; reconcile local homes first",
                p.meta.alias
            ),
            _ => {}
        }
    }
    Ok(auth)
}
fn retire_local_holders(paths: &config::Paths, p: &profile::Profile, auth: &Value) -> Result<()> {
    for path in existing_holders(paths)? {
        let contents = vault::private_read(&path)?;
        let candidate: Value = serde_json::from_slice(&contents)?;
        if same_seat(auth, &candidate)? {
            let backup = p
                .dir
                .join(format!(".retired-{}.json", vault::digest(&contents)));
            if !backup.try_exists()? {
                vault::create_secret(&backup, &contents)?;
                store::sync_directory(&p.dir)?;
            }
            if path == paths.codex_auth_json() && paths.active_file().try_exists()? {
                // Clear the marker first: interruption may leave credentials
                // with no marker, but must not leave a marker without credentials.
                std::fs::remove_file(paths.active_file())?;
                store::sync_directory(&paths.codexctl_dir())?;
            }
            std::fs::remove_file(&path)?;
            store::sync_directory(path.parent().context("credential file has no parent")?)?;
        }
    }
    Ok(())
}

pub(super) fn require_local_handoff(paths: &config::Paths, remote: &Value) -> Result<()> {
    for path in existing_holders(paths)? {
        let local: Value = serde_json::from_slice(&vault::private_read(&path)?)?;
        if !same_seat(remote, &local)? {
            continue;
        }
        bail!(
            "local credentials may own this remote account; migrate them with --all --exclusive-owner before selection"
        );
    }
    Ok(())
}

pub(super) fn local_alias_matches(
    device: &Connection,
    account: &Account,
    dir: &Path,
) -> Result<bool> {
    if !dir.try_exists()? {
        return Ok(true);
    }
    let marker = dir.join(".central-transfer.json");
    if !marker.try_exists()? {
        return Ok(false);
    }
    let transfer: Transfer = serde_json::from_slice(&vault::private_read(&marker)?)?;
    Ok(transfer.server == device.server
        && transfer.user_id == device.user_id
        && transfer.alias == account.alias
        && transfer.account_id == account.account_id
        && transfer.confirmed)
}

fn require_stopped_owners(paths: &config::Paths) -> Result<()> {
    let processes = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,comm="])
        .output()
        .context("cannot check running Codex owners")?;
    if !processes.status.success() {
        bail!("cannot check running Codex owners");
    }
    for line in String::from_utf8_lossy(&processes.stdout).lines() {
        let mut fields = line.trim().splitn(2, char::is_whitespace);
        let pid = fields.next().context("invalid process list")?;
        let command = fields.next().unwrap_or("").trim();
        if !std::path::Path::new(command)
            .file_name()
            .is_some_and(|n| n == "codex")
        {
            continue;
        }
        #[cfg(target_os = "linux")]
        let environment = match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(v) => String::from_utf8_lossy(&v).replace('\0', " "),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => bail!("cannot identify a running Codex home"),
        };
        #[cfg(not(target_os = "linux"))]
        let environment = {
            let result = std::process::Command::new("ps")
                .args(["eww", "-p", pid, "-o", "command="])
                .output()?;
            if result.stdout.is_empty() {
                continue;
            }
            if !result.status.success() {
                bail!("cannot identify a running Codex home");
            }
            String::from_utf8_lossy(&result.stdout).into_owned()
        };
        let home = paths.home.to_str().context("home path must be UTF-8")?;
        // Inspect only ownership. Never print process environments or command arguments.
        if environment.contains(&format!("HOME={home}"))
            || environment.contains(&format!("CODEX_HOME={home}/"))
        {
            bail!("stop every Codex process using this home before transferring refresh ownership");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    #[test]
    fn local_usage_keeps_a_weekly_only_window_in_the_long_column() {
        let usage = serde_json::from_value(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {"primary_window": {"used_percent": 37, "limit_window_seconds": 604800}},
            "credits": {"has_credits": true, "balance": "12.50"}
        }))
        .unwrap();

        assert_eq!(usage_cells(&usage), ["-", "37%", "-", "12.50"]);
    }
    #[test]
    fn connected_local_balance_is_short() {
        let usage = serde_json::from_value(serde_json::json!({
            "credits": {"has_credits": true, "balance": "55835.5394250000"}
        }))
        .unwrap();

        assert_eq!(usage_cells(&usage)[3], "55835.54");
    }

    #[test]
    fn connected_local_reset_uses_relative_time_and_local_date() {
        let reset = chrono::Utc::now().timestamp() + 6 * 86400 + 3 * 3600 + 1800;
        let date = chrono::DateTime::from_timestamp(reset, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%a %b %d %H:%M")
            .to_string();
        let usage = serde_json::from_value(serde_json::json!({
            "rate_limit": {"primary_window": {
                "used_percent": 37, "limit_window_seconds": 604800, "reset_at": reset
            }}
        }))
        .unwrap();

        assert_eq!(usage_cells(&usage)[2], format!("in 6d 3h ({date})"));
    }

    fn profiles_with_refresh_only_rotation() -> (tempfile::TempDir, config::Paths, profile::Profile)
    {
        let root = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_home(root.path().into());
        paths.ensure_dirs().unwrap();
        store::ensure_private_dir(&paths.codex_home()).unwrap();
        let payload=URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({"sub":"amir-login","exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"amir-seat"}})).unwrap());
        let mut auth = serde_json::json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"older-refresh","account_id":"amir-seat"}});
        store::atomic_write(
            &paths.codex_auth_json(),
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
        auth["tokens"]["refresh_token"] = serde_json::json!("newer-refresh");
        store::atomic_write(
            &paths.codex_auth_json(),
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        let profile = profile::get_profile_from(&paths, "personal").unwrap();
        (root, paths, profile)
    }
    #[test]
    fn when_only_refresh_tokens_differ_then_migration_preserves_both_copies_and_refuses() {
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        let result = prepare_auth(&paths, &profile);
        assert!(result.is_err());
        assert!(paths.codex_auth_json().exists());
        assert!(!profile.dir.join(".central-transfer.json").exists());
    }

    #[test]
    fn retiring_live_credentials_clears_the_active_marker() {
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        profile::set_active_from(&paths, "personal").unwrap();
        let auth: Value =
            serde_json::from_slice(&std::fs::read(paths.codex_auth_json()).unwrap()).unwrap();
        assert_eq!(
            profile::get_active_from(&paths).unwrap().as_deref(),
            Some("personal")
        );

        retire_local_holders(&paths, &profile, &auth).unwrap();

        assert!(!paths.codex_auth_json().exists());
        assert_eq!(profile::get_active_from(&paths).unwrap(), None);
    }

    #[test]
    fn retiring_another_seat_preserves_live_credentials_and_the_active_marker() {
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        profile::set_active_from(&paths, "personal").unwrap();
        let mut auth: Value =
            serde_json::from_slice(&std::fs::read(profile.auth_json_path()).unwrap()).unwrap();
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({"sub":"another-login","exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"another-seat"}})).unwrap());
        auth["tokens"]["access_token"] = serde_json::json!(format!("header.{claims}."));
        auth["tokens"]["account_id"] = serde_json::json!("another-seat");
        let original = std::fs::read(paths.codex_auth_json()).unwrap();

        retire_local_holders(&paths, &profile, &auth).unwrap();

        assert_eq!(std::fs::read(paths.codex_auth_json()).unwrap(), original);
        assert_eq!(
            profile::get_active_from(&paths).unwrap().as_deref(),
            Some("personal")
        );
    }
    #[test]
    fn interrupted_login_sessions_are_selected_and_retired_during_migration() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        std::fs::remove_file(paths.codex_auth_json()).unwrap();
        let mut newer: Value =
            serde_json::from_slice(&std::fs::read(profile.auth_json_path()).unwrap()).unwrap();
        let old = vault::token(&newer).unwrap();
        let mut claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(old.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        claims["exp"] = serde_json::json!(4102444801_u64);
        newer["tokens"]["access_token"] = serde_json::json!(format!(
            "header.{}.",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        ));
        newer["tokens"]["refresh_token"] = serde_json::json!("newest-refresh");
        let session = store::login_home(&paths, "personal")
            .unwrap()
            .join("session-interrupted");
        store::ensure_private_dir(&session).unwrap();
        store::atomic_write(
            &session.join("auth.json"),
            &serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        assert_eq!(prepare_auth(&paths, &profile).unwrap(), newer);
        retire_local_holders(&paths, &profile, &newer).unwrap();
        assert!(!session.join("auth.json").exists());
        assert!(std::fs::read_dir(&profile.dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".retired-")
        }));
    }
    #[test]
    fn recovery_credentials_under_another_alias_are_selected_and_retired() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        std::fs::remove_file(paths.codex_auth_json()).unwrap();
        let mut newer: Value =
            serde_json::from_slice(&std::fs::read(profile.auth_json_path()).unwrap()).unwrap();
        let old = vault::token(&newer).unwrap();
        let mut claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(old.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        claims["exp"] = serde_json::json!(4102444801_u64);
        newer["tokens"]["access_token"] = serde_json::json!(format!(
            "header.{}.",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        ));
        newer["tokens"]["refresh_token"] = serde_json::json!("newest-refresh");
        let exec = store::exec_home(&paths, "another-alias").unwrap();
        let session = store::login_home(&paths, "another-alias")
            .unwrap()
            .join("session-interrupted");
        for home in [&exec, &session] {
            store::ensure_private_dir(home).unwrap();
            store::atomic_write(
                &home.join("auth.json"),
                &serde_json::to_vec(&newer).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(prepare_auth(&paths, &profile).unwrap(), newer);
        retire_local_holders(&paths, &profile, &newer).unwrap();
        assert!(!exec.join("auth.json").exists());
        assert!(!session.join("auth.json").exists());
        assert!(!profile.auth_json_path().exists());
    }
    #[test]
    fn metadata_less_profile_credentials_are_selected_and_retired() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let (_root, paths, profile) = profiles_with_refresh_only_rotation();
        std::fs::remove_file(paths.codex_auth_json()).unwrap();
        let mut newer: Value =
            serde_json::from_slice(&std::fs::read(profile.auth_json_path()).unwrap()).unwrap();
        let mut claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(vault::token(&newer).unwrap().split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        claims["exp"] = serde_json::json!(4102444801_u64);
        newer["tokens"]["access_token"] = serde_json::json!(format!(
            "header.{}.",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        ));
        newer["tokens"]["refresh_token"] = serde_json::json!("latest-refresh");
        let orphan = paths.profiles_dir().join("interrupted-save/auth.json");
        store::atomic_write(&orphan, &serde_json::to_vec(&newer).unwrap()).unwrap();
        assert_eq!(prepare_auth(&paths, &profile).unwrap(), newer);
        retire_local_holders(&paths, &profile, &newer).unwrap();
        assert!(!orphan.exists());
    }
    #[test]
    fn a_holder_missing_workspace_evidence_blocks_migration_before_handoff() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let root = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_home(root.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({"sub":"amir-login","exp":4102444800_u64}))
                .unwrap(),
        );
        let auth = serde_json::json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"same-refresh","account_id":"workspace"}});
        store::atomic_write(
            &paths.codex_auth_json(),
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        profile::save_profile_to(&paths, "personal", None, &paths.codex_auth_json()).unwrap();
        let mut claimless = auth.clone();
        claimless["tokens"]
            .as_object_mut()
            .unwrap()
            .remove("account_id");
        store::atomic_write(
            &paths.codex_auth_json(),
            &serde_json::to_vec(&claimless).unwrap(),
        )
        .unwrap();
        let profile = profile::get_profile_from(&paths, "personal").unwrap();
        assert!(
            prepare_auth(&paths, &profile)
                .unwrap_err()
                .to_string()
                .contains("workspace")
        );
        assert!(profile.auth_json_path().exists());
        assert!(paths.codex_auth_json().exists());
        assert!(!profile.dir.join(".central-transfer.json").exists());
    }
}
