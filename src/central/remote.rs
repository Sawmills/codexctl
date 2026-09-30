//! Device setup, account discovery, and explicit refresh-ownership migration.
use super::{
    enrollment::{Challenge, Grant, Poll, Start},
    managed::{Account, Import, RevokeDevice},
    native, transport, vault,
};
use crate::{api, config, profile, store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize, Clone)]
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
pub fn accounts() -> Result<Option<Vec<Account>>> {
    Ok(catalog()?.map(|c| c.accounts))
}
pub(super) fn catalog() -> Result<Option<Catalog>> {
    let Some(connection) = connection()? else {
        return Ok(None);
    };
    let response = request(&connection, "/v1/accounts")?
        .send()
        .context("cannot reach the account server")?;
    let accounts: Vec<Account> = check(response)?.json()?;
    require_current_connection(&connection)?;
    for account in &accounts {
        if account.user_id != connection.user_id {
            bail!("server user identity changed");
        }
    }
    Ok(Some(Catalog {
        connection,
        accounts,
    }))
}
pub fn show(status: bool, filter: Option<api::BillingClass>) -> Result<bool> {
    let Some(accounts) = accounts()? else {
        return Ok(false);
    };
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_header(if status {
        vec![
            "Account",
            "Label",
            "Plan",
            "Short used",
            "Long used",
            "Resets",
            "State",
        ]
    } else {
        vec!["Account", "Label", "Plan", "State"]
    });
    for account in accounts
        .iter()
        .filter(|a| filter.is_none_or(|f| a.billing_class == f))
    {
        let state = if !account.available {
            "unavailable"
        } else if native::active_alias()?.as_deref() == Some(&account.alias) {
            "active"
        } else {
            "server"
        };
        let mut row = vec![
            account.alias.clone(),
            account.label.clone().unwrap_or_else(|| "-".into()),
            account.plan.clone().unwrap_or_else(|| "unknown".into()),
        ];
        if status {
            row.extend([
                account
                    .primary_used
                    .map_or("-".into(), |n| format!("{n:.0}%")),
                account
                    .secondary_used
                    .map_or("-".into(), |n| format!("{n:.0}%")),
                account
                    .resets_at
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map_or("-".into(), |t| t.to_rfc3339()),
            ]);
        }
        row.push(state.into());
        table.add_row(row);
    }
    if accounts.is_empty() {
        println!("No server accounts yet. Run codexctl migrate --all on your source machine.");
    } else {
        println!("{table}");
    }
    Ok(true)
}
pub fn whoami() -> Result<bool> {
    if connection()?.is_none() {
        return Ok(false);
    }
    match native::active_alias()? {
        Some(alias) => println!("{alias} [server]"),
        None => println!("Connected to account server; run codexctl use."),
    };
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
    if crate::daemon::running_pid(&paths.codex_home()).is_some() {
        bail!("finish Codex sessions and stop the Codex daemon before migration");
    }
    require_stopped_owners(&paths)?;
    let connection = connection()?.context("run codexctl connect first")?;
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
    println!(
        "Migration complete. Run codexctl use, then regular codex. Keep other machines from refreshing their old credential copies."
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
    accounts.iter().filter(|a|a.available&&a.billing_class==api::BillingClass::RateLimited&&(a.primary_used.is_some()||a.secondary_used.is_some())).min_by(|a,b|{
        let by_score=score(a).total_cmp(&score(b));
        let exhausted_a=score(a)>=500.0;let exhausted_b=score(b)>=500.0;
        exhausted_a.cmp(&exhausted_b).then_with(||if most||exhausted_a{by_score}else{a.resets_at.unwrap_or(i64::MAX).cmp(&b.resets_at.unwrap_or(i64::MAX)).then(by_score)})
    }).map(|a|a.alias.clone()).context("no available account with verified included usage; select an alias explicitly to approve credit billing")
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
