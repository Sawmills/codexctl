//! Desktop app auth (CODEX-APP-A): `~/.codex/auth.json` in Codex's
//! `chatgptAuthTokens` mode, written from server tokens with no refresh
//! token. Codex never refreshes this mode; on a 401 it reloads the file, so
//! the account server stays the only refresh owner and a launchd agent keeps
//! the file current.
use super::*;

/// Codex versions whose file-backed `chatgptAuthTokens` mode passed LIVE-A.
const SUPPORTED_CODEX: &[&str] = &["0.161.0", "0.162.0-alpha.2"];
/// The Codex binary the desktop app runs.
const APP_CODEX: &str = "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex";
const AGENT_LABEL: &str = "ai.sawmills.codexctl.app-auth";
const AGENT_INTERVAL_SECONDS: i64 = 300;
/// A written token must outlive the next agent run with room for a retry.
const MIN_LIFETIME_SECONDS: i64 = AGENT_INTERVAL_SECONDS + 900;
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const MODE: &str = "chatgptAuthTokens";

#[derive(clap::Subcommand)]
pub enum AppAuthAction {
    /// Pin the desktop app's ~/.codex to one server account.
    Enable {
        #[arg(long)]
        account: String,
        /// Accept an account the server may bill.
        #[arg(long)]
        allow_billing: bool,
        /// Write the file without installing the refresh agent.
        #[arg(long)]
        no_agent: bool,
        /// Accept a Codex version app-auth has not been tested with.
        #[arg(long)]
        allow_codex_version: bool,
    },
    /// Rewrite the file from the server when its token changed (the agent runs this).
    Refresh,
    /// Show the pinned account and the last refresh result.
    Status,
    /// Remove the agent and the app-auth file.
    Disable {
        /// Restore the login that enable backed up, even a server account's.
        #[arg(long)]
        restore_login: bool,
    },
}

pub fn run(action: AppAuthAction) -> Result<()> {
    match action {
        AppAuthAction::Enable {
            account,
            allow_billing,
            no_agent,
            allow_codex_version,
        } => enable(&account, allow_billing, no_agent, allow_codex_version),
        AppAuthAction::Refresh => refresh(),
        AppAuthAction::Status => status(),
        AppAuthAction::Disable { restore_login } => disable(restore_login),
    }
}

#[derive(Serialize, Deserialize)]
struct State {
    alias: String,
    connection: Connection,
    allow_billing: bool,
    /// The plan and billing class `--allow-billing` approved; a billing
    /// token that no longer matches needs a new approval.
    #[serde(default)]
    approved_plan: Option<String>,
    #[serde(default)]
    approved_class: Option<api::BillingClass>,
    allow_codex_version: bool,
    #[serde(default)]
    backup_account: Option<String>,
    /// Digest of the exact bytes app-auth last wrote; any other content in
    /// auth.json is another login, even in the same mode and workspace.
    #[serde(default)]
    written: Option<String>,
    /// Digest of a write journaled but not yet confirmed. A crash between the
    /// journal and the write leaves either file recognizable.
    #[serde(default)]
    pending: Option<String>,
    #[serde(default)]
    last: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
struct Outcome {
    at: String,
    ok: bool,
    detail: String,
}

struct Paths {
    state_dir: PathBuf,
    codex_home: PathBuf,
}

impl Paths {
    fn new() -> Result<Self> {
        let paths = config::default_paths()?;
        Ok(Self {
            state_dir: paths.codexctl_dir().join("app-auth"),
            codex_home: paths.codex_home(),
        })
    }
    fn state(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    fn backup(&self) -> PathBuf {
        self.state_dir.join("backup/auth.json")
    }
    fn auth(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }
}

/// Refuse a command that would replace `~/.codex/auth.json` while the
/// desktop app is pinned to a server account through it.
pub fn refuse_while_enabled(action: &str) -> Result<()> {
    refuse_app_auth(&config::default_paths()?, action)
}

/// The same refusal for a writer that already holds the store lock, so the
/// check and its write cannot interleave with `enable`.
pub fn refuse_app_auth(paths: &config::Paths, action: &str) -> Result<()> {
    if paths
        .codexctl_dir()
        .join("app-auth/state.json")
        .try_exists()?
    {
        bail!(
            "app-auth pins ~/.codex to a server account for the desktop app; {action} would replace that login; run codexctl app-auth disable first"
        );
    }
    Ok(())
}

fn lock(paths: &Paths) -> Result<vault::Lock> {
    store::ensure_private_dir(&paths.state_dir)?;
    vault::lock(&paths.state_dir, "app-auth.lock")
}

fn read_state(paths: &Paths) -> Result<State> {
    match std::fs::read(paths.state()) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("app-auth is not enabled; run codexctl app-auth enable --account <alias>")
        }
        Err(error) => Err(error.into()),
    }
}

fn save_state(paths: &Paths, state: &State) -> Result<()> {
    store::atomic_write(&paths.state(), &serde_json::to_vec_pretty(state)?)
}

fn record(paths: &Paths, state: &mut State, ok: bool, detail: &str) -> Result<()> {
    state.last = Some(Outcome {
        at: chrono::Utc::now().to_rfc3339(),
        ok,
        detail: detail.to_owned(),
    });
    save_state(paths, state)
}

fn enable(
    alias: &str,
    allow_billing: bool,
    no_agent: bool,
    allow_codex_version: bool,
) -> Result<()> {
    if std::env::var_os("CODEX_HOME").is_some() {
        bail!("app-auth writes ~/.codex for the desktop app; unset CODEX_HOME");
    }
    if !no_agent && !cfg!(target_os = "macos") {
        bail!("the refresh agent needs launchd (macOS); pass --no-agent");
    }
    let paths = Paths::new()?;
    let _lock = lock(&paths)?;
    let preconditions = || -> Result<()> {
        if paths.state().try_exists()?
            && file_state(&paths, Some(&read_state(&paths)?))? == FileState::Foreign
        {
            bail!(
                "~/.codex/auth.json changed since app-auth enable; run codexctl app-auth disable first"
            );
        }
        if central_active()? {
            bail!(
                "the central provider is active in ~/.codex; switch it off before app-auth enable"
            );
        }
        check_codex_config(&paths.codex_home)
    };
    preconditions()?;
    check_codex_version(allow_codex_version)?;
    let catalog =
        super::super::remote::catalog()?.context("app-auth requires a connected account server")?;
    let (account, connection) = launch::catalog_connection(&catalog, alias, false)?;
    if account.loan.is_some() {
        bail!("app-auth pins only your own server accounts, not a borrowed one");
    }
    let approval = if allow_billing {
        Approval::Any
    } else {
        Approval::None
    };
    let token = server_token(&connection, &account.alias, approval)?;
    // Take the locks `use`, activation, and recovery write under, in their
    // order, and recheck: one of them may have changed ~/.codex meanwhile.
    let _native = native_lock(&root()?)?;
    let _store = store::try_lock(&config::default_paths()?)?
        .context("local account store is busy; retry app-auth enable")?;
    preconditions()?;
    let previous = paths
        .state()
        .try_exists()?
        .then(|| read_state(&paths))
        .transpose()?;
    // Keep the digest of the file on disk until the new write is confirmed;
    // a failed re-enable must still recognize the previous app-auth file.
    let previous_written = previous.as_ref().and_then(|state| state.written.clone());
    let backup_account = match previous {
        Some(state) => state.backup_account,
        None => back_up_login(&paths)?,
    };
    let account_id = connection.account_id.clone();
    let mut state = State {
        alias: account.alias.clone(),
        connection,
        allow_billing,
        approved_plan: allow_billing
            .then(|| token.chatgpt_plan_type.clone())
            .flatten(),
        approved_class: allow_billing.then_some(token.billing_class).flatten(),
        allow_codex_version,
        backup_account,
        written: previous_written,
        pending: None,
        last: None,
    };
    // Journal first: if the write below never lands, the marker still exists,
    // so the guards hold and disable can undo what enable began.
    write_auth(&paths, &mut state, &account_id, &token.access_token)?;
    record(&paths, &mut state, true, "written by enable")?;
    if !no_agent {
        install_agent()?;
    }
    eprintln!(
        "codexctl: app-auth pinned ~/.codex to {} (plan {}); quit and reopen the desktop app",
        state.alias,
        token.chatgpt_plan_type.as_deref().unwrap_or("unknown")
    );
    Ok(())
}

fn refresh() -> Result<()> {
    let paths = Paths::new()?;
    let _lock = lock(&paths)?;
    let mut state = read_state(&paths)?;
    let result = refresh_locked(&paths, &mut state);
    match &result {
        Ok(detail) => {
            record(&paths, &mut state, true, detail)?;
            eprintln!("codexctl: app-auth {detail}");
        }
        Err(error) => record(&paths, &mut state, false, &format!("{error:#}"))?,
    }
    result.map(|_| ())
}

fn refresh_locked(paths: &Paths, state: &mut State) -> Result<&'static str> {
    match file_state(paths, Some(state))? {
        FileState::Missing => bail!(
            "~/.codex/auth.json is missing (signed out in the app?); refresh stopped; run codexctl app-auth enable again"
        ),
        FileState::Foreign => {
            bail!("~/.codex/auth.json was not written by app-auth (a new login?); refresh stopped")
        }
        FileState::Ours => {}
    }
    check_codex_version(state.allow_codex_version)?;
    let approval = if state.allow_billing {
        Approval::Exact(&state.approved_plan, &state.approved_class)
    } else {
        Approval::None
    };
    let token = server_token(&state.connection, &state.alias, approval)?;
    let current = current_access_token(paths)?.context("~/.codex/auth.json has no access token")?;
    if current == token.access_token {
        return Ok("token unchanged");
    }
    // One account is its workspace and its login together: replace the file
    // only with positive proof that the server token is the same login.
    if !api::token_logins(&current).same(&api::token_logins(&token.access_token)) {
        bail!(
            "the server token for {} is not provably the login app-auth pinned; refresh stopped; run codexctl app-auth enable again",
            state.alias
        );
    }
    // The fetch took time; the app may have written another login meanwhile.
    if file_state(paths, Some(state))? != FileState::Ours {
        bail!("~/.codex/auth.json changed during refresh (a new login?); refresh stopped");
    }
    let account_id = state.connection.account_id.clone();
    write_auth(paths, state, &account_id, &token.access_token)?;
    Ok("token rewritten")
}

fn status() -> Result<()> {
    let paths = Paths::new()?;
    let state = read_state(&paths)?;
    let file = match file_state(&paths, Some(&state))? {
        FileState::Ours => match current_access_token(&paths)?.and_then(|t| claims(&t).ok()) {
            Some(claims) => format!(
                "written by app-auth, token expires in {} min",
                (claims.exp - now()) / 60
            ),
            None => "written by app-auth".to_owned(),
        },
        FileState::Foreign => "replaced by another login".to_owned(),
        FileState::Missing => "missing".to_owned(),
    };
    println!("account   {}", state.alias);
    println!("file      {file}");
    match &state.last {
        Some(last) => println!(
            "last      {} at {}: {}",
            if last.ok { "ok" } else { "failed" },
            last.at,
            last.detail
        ),
        None => println!("last      none"),
    }
    println!(
        "agent     {}",
        if agent_path()?.try_exists()? {
            "installed"
        } else {
            "not installed"
        }
    );
    Ok(())
}

fn disable(restore_login: bool) -> Result<()> {
    let paths = Paths::new()?;
    let _lock = lock(&paths)?;
    let state = read_state(&paths)?;
    remove_agent()?;
    match file_state(&paths, Some(&state))? {
        FileState::Ours => std::fs::remove_file(paths.auth())?,
        FileState::Foreign => {
            eprintln!("codexctl: ~/.codex/auth.json holds another login; left in place");
            if paths.backup().try_exists()? {
                eprintln!(
                    "codexctl: the login app-auth replaced is still at {}",
                    paths.backup().display()
                );
            }
        }
        FileState::Missing => {}
    }
    let backup = paths.backup();
    if backup.try_exists()? && !paths.auth().try_exists()? {
        // A server account's backed-up refresh token would be a second
        // refresh owner beside the server; restore it only when asked.
        let held_by_server = match &state.backup_account {
            Some(account) => super::super::remote::catalog()
                .ok()
                .flatten()
                .is_none_or(|catalog| catalog.accounts.iter().any(|a| &a.account_id == account)),
            // A login whose workspace cannot be read could be a server
            // account's; only --restore-login brings it back.
            None => true,
        };
        if restore_login || !held_by_server {
            store::atomic_write(&paths.auth(), &std::fs::read(&backup)?)?;
            std::fs::remove_file(&backup)?;
            eprintln!("codexctl: restored the login app-auth backed up");
        } else {
            eprintln!(
                "codexctl: the backed-up login is a server account; kept at {} (use --restore-login to restore it)",
                backup.display()
            );
        }
    }
    std::fs::remove_file(paths.state())?;
    eprintln!("codexctl: app-auth disabled; quit and reopen the desktop app");
    Ok(())
}

#[derive(PartialEq)]
enum FileState {
    Ours,
    Foreign,
    Missing,
}

/// Whether `~/.codex/auth.json` is the exact file app-auth last wrote for
/// `state`; with no state, any file in app-auth's mode counts.
fn file_state(paths: &Paths, state: Option<&State>) -> Result<FileState> {
    let bytes = match std::fs::read(paths.auth()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileState::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(FileState::Foreign);
    };
    let ours = document["auth_mode"] == MODE
        // The digest alone proves app-auth wrote these bytes. It must not also
        // require the pinned workspace: an interrupted re-enable to another
        // account leaves the previous account's file under the new pin.
        && state.is_none_or(|state| {
            [&state.written, &state.pending]
                .into_iter()
                .any(|digest| digest.as_deref() == Some(vault::digest(&bytes).as_str()))
        });
    Ok(if ours {
        FileState::Ours
    } else {
        FileState::Foreign
    })
}

fn current_access_token(paths: &Paths) -> Result<Option<String>> {
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(paths.auth())?)?;
    Ok(document["tokens"]["access_token"]
        .as_str()
        .map(str::to_owned))
}

fn check_codex_config(codex_home: &Path) -> Result<()> {
    let document = document(codex_home)?;
    if let Some(store) = document
        .get("cli_auth_credentials_store")
        .and_then(Item::as_str)
        && store != "file"
    {
        bail!(
            "cli_auth_credentials_store = \"{store}\" in ~/.codex/config.toml keeps logins outside auth.json; set it to \"file\" or remove it"
        );
    }
    if let Some(selected) = document.get("profile").and_then(Item::as_str)
        && let Some(provider) = document
            .get("profiles")
            .and_then(|profiles| profiles.get(selected))
            .and_then(|profile| profile.get("model_provider"))
            .and_then(Item::as_str)
        && provider != "openai"
    {
        bail!(
            "the selected Codex profile {selected} overrides model_provider with \"{provider}\"; app-auth needs the built-in openai provider"
        );
    }
    if let Some(provider) = document.get("model_provider").and_then(Item::as_str)
        && provider != "openai"
    {
        bail!(
            "~/.codex/config.toml selects model_provider \"{provider}\"; app-auth needs the built-in openai provider"
        );
    }
    Ok(())
}

fn check_codex_version(allow_other: bool) -> Result<()> {
    let app = std::env::var_os("CODEXCTL_APP_AUTH_APP_CODEX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(APP_CODEX));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("codex"))
        .find(|candidate| candidate.is_file());
    let binaries: Vec<PathBuf> = [Some(app), on_path]
        .into_iter()
        .flatten()
        .filter(|binary| binary.is_file())
        .collect();
    if binaries.is_empty() {
        if allow_other {
            return Ok(());
        }
        bail!("no Codex binary found; app-auth checks the Codex version it writes for");
    }
    for binary in binaries {
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .with_context(|| format!("failed to run {} --version", binary.display()))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = text
            .trim()
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .to_owned();
        if !allow_other && !SUPPORTED_CODEX.contains(&version.as_str()) {
            bail!(
                "{} is Codex {version}; app-auth is tested with {}; pass --allow-codex-version to accept it",
                binary.display(),
                SUPPORTED_CODEX.join(", ")
            );
        }
    }
    Ok(())
}

/// A server token that passes every check before it can be written.
enum Approval<'a> {
    None,
    Any,
    Exact(&'a Option<String>, &'a Option<api::BillingClass>),
}

/// The repository's billing rule: a billing token needs an approval that
/// still names its plan and billing class.
fn billing_approved(approval: &Approval, token: &TokenResponse) -> bool {
    match approval {
        Approval::None => false,
        Approval::Any => true,
        Approval::Exact(plan, class) => {
            **plan == token.chatgpt_plan_type && **class == token.billing_class
        }
    }
}

fn server_token(connection: &Connection, alias: &str, approval: Approval) -> Result<TokenResponse> {
    let mut token = fetch(connection, false)?;
    if claims(&token.access_token)?.exp - now() < MIN_LIFETIME_SECONDS {
        // The server forces a renewal only for a request that names the
        // revision it is replacing.
        let mut renewal = connection.clone();
        renewal.revision = token.revision.clone();
        token = fetch(&renewal, true).with_context(|| {
            format!("the server token for {alias} is below the minimum lifetime and the server could not renew it")
        })?;
    }
    validate_token_account(&token.access_token, &connection.account_id)?;
    let claims = claims(&token.access_token)?;
    if claims.plan.is_none() {
        bail!(
            "the server token for {alias} has no chatgpt_plan_type claim; the app cannot show its plan"
        );
    }
    if claims.account.as_deref() != Some(connection.account_id.as_str())
        || token.chatgpt_account_id != connection.account_id
    {
        bail!("the server token for {alias} names another workspace");
    }
    if claims.exp - now() < MIN_LIFETIME_SECONDS {
        bail!(
            "the server token for {alias} is below the minimum lifetime of {MIN_LIFETIME_SECONDS} s"
        );
    }
    if !billing_approved(&approval, &token) {
        launch::require_headroom(alias, &token)?;
        if token.billing_class != Some(api::BillingClass::RateLimited) {
            bail!(
                "server account {alias} may bill credits (or its billing changed since approval); run codexctl app-auth enable --allow-billing"
            );
        }
    }
    Ok(token)
}

struct Claims {
    plan: Option<String>,
    account: Option<String>,
    exp: i64,
}

fn claims(token: &str) -> Result<Claims> {
    use base64::Engine;
    let payload = token
        .split('.')
        .nth(1)
        .context("access token is not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("access token payload is not base64url")?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let auth = &value[AUTH_CLAIM];
    Ok(Claims {
        plan: auth["chatgpt_plan_type"].as_str().map(str::to_owned),
        account: auth["chatgpt_account_id"].as_str().map(str::to_owned),
        exp: value["exp"]
            .as_i64()
            .context("access token has no exp claim")?,
    })
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Codex's file shape for externally managed ChatGPT tokens
/// (`AuthDotJson::from_external_access_token`): the access token also
/// stands in for the ID token, and the refresh token is empty.
fn auth_document(account_id: &str, access_token: &str) -> serde_json::Value {
    serde_json::json!({
        "auth_mode": MODE,
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": access_token,
            "access_token": access_token,
            "refresh_token": "",
            "account_id": account_id,
        },
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    })
}

/// Write the file, journaling its digest so later runs can prove it is ours.
fn write_auth(
    paths: &Paths,
    state: &mut State,
    account_id: &str,
    access_token: &str,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&auth_document(account_id, access_token))?;
    let digest = vault::digest(&bytes);
    // Journal the digest before the write, so a crash on either side of it
    // leaves a file app-auth still recognizes.
    state.pending = Some(digest.clone());
    save_state(paths, state)?;
    store::atomic_write(&paths.auth(), &bytes)?;
    state.written = Some(digest);
    state.pending = None;
    save_state(paths, state)
}

/// Keep the login enable replaces and return the workspace of the backup.
/// With no login to keep, an earlier backup stays, and its workspace is
/// read back so a later disable still knows whose login it holds.
fn back_up_login(paths: &Paths) -> Result<Option<String>> {
    let bytes = match std::fs::read(paths.auth()) {
        Ok(bytes) => {
            // An earlier backup no state names is a login no one else holds;
            // keep it under its own name rather than overwrite it.
            if paths.backup().try_exists()? {
                let kept = paths.backup().with_extension(format!(
                    "json.{}",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                ));
                std::fs::rename(paths.backup(), &kept)?;
                eprintln!(
                    "codexctl: kept an earlier app-auth backup at {}",
                    kept.display()
                );
            }
            store::atomic_write(&paths.backup(), &bytes)?;
            bytes
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::read(paths.backup()) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|document| document["tokens"]["account_id"].as_str().map(str::to_owned)))
}

fn agent_path() -> Result<PathBuf> {
    Ok(config::default_paths()?
        .home
        .join(format!("Library/LaunchAgents/{AGENT_LABEL}.plist")))
}

fn agent_plist(program: &Path, log: &Path) -> String {
    let escape = |path: &Path| {
        path.display()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>app-auth</string><string>refresh</string></array>
  <key>StartInterval</key><integer>{AGENT_INTERVAL_SECONDS}</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        escape(program),
        escape(log)
    )
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")
}

fn install_agent() -> Result<()> {
    let plist = agent_path()?;
    let log = config::default_paths()?
        .codexctl_dir()
        .join("logs/app-auth.log");
    store::ensure_private_dir(log.parent().context("log path has no parent")?)?;
    let program = std::env::current_exe()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist, agent_plist(&program, &log))?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = launchctl(&["bootout", &format!("{domain}/{AGENT_LABEL}")]);
    let output = launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn remove_agent() -> Result<()> {
    let plist = agent_path()?;
    if !plist.try_exists()? {
        return Ok(());
    }
    let service = format!("gui/{}/{AGENT_LABEL}", unsafe { libc::getuid() });
    // bootout fails when the agent is not loaded, which is the goal; only a
    // service launchd still knows about is a failure.
    let _ = launchctl(&["bootout", &service]);
    if launchctl(&["print", &service])?.status.success() {
        bail!("launchctl bootout left {service} loaded; app-auth stays enabled; retry disable");
    }
    std::fs::remove_file(plist)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(plan: &str, class: api::BillingClass) -> TokenResponse {
        serde_json::from_value(serde_json::json!({
            "accessToken": "a.b.c", "chatgptAccountId": "seat",
            "chatgptPlanType": plan, "revision": "r", "billingClass": class,
        }))
        .unwrap()
    }

    #[test]
    fn billing_approval_holds_only_while_plan_and_class_match() {
        use api::BillingClass::UsageBased;
        let approved = token("team", UsageBased);
        let plan = Some("team".to_owned());
        let class = Some(UsageBased);
        assert!(billing_approved(&Approval::Exact(&plan, &class), &approved));
        assert!(!billing_approved(
            &Approval::Exact(&plan, &class),
            &token("pro", UsageBased)
        ));
        assert!(!billing_approved(&Approval::Exact(&plan, &None), &approved));
        assert!(!billing_approved(&Approval::None, &approved));
        assert!(billing_approved(&Approval::Any, &approved));
    }

    fn jwt(subject: &str) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "sub": subject, "exp": 4102444800_u64,
                AUTH_CLAIM: {"chatgpt_account_id": "seat", "chatgpt_plan_type": "team"},
            }))
            .unwrap(),
        );
        format!("h.{payload}.s")
    }

    fn test_state() -> State {
        serde_json::from_value(serde_json::json!({
            "alias": "team", "allow_billing": false, "allow_codex_version": false,
            "connection": {"server": "https://s", "device_token_file": "/t",
                "account_id": "seat", "revision": ""},
        }))
        .unwrap()
    }

    #[test]
    fn written_and_journaled_files_are_ours_and_other_bytes_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state_dir: dir.path().join("app-auth"),
            codex_home: dir.path().join(".codex"),
        };
        let mut state = test_state();
        write_auth(&paths, &mut state, "seat", &jwt("amir")).unwrap();
        assert!(state.pending.is_none() && state.written.is_some());
        assert!(file_state(&paths, Some(&state)).unwrap() == FileState::Ours);
        // A crash after the journal but before the confirmation: the new
        // file matches the pending digest.
        let bytes = std::fs::read(paths.auth()).unwrap();
        state.pending = Some(vault::digest(&bytes));
        state.written = Some("old".into());
        assert!(file_state(&paths, Some(&state)).unwrap() == FileState::Ours);
        // An interrupted re-enable to another account: the previous file is
        // still app-auth's own under the new pin.
        let mut moved = test_state();
        moved.connection.account_id = "other-seat".into();
        moved.written = Some(vault::digest(&bytes));
        assert!(file_state(&paths, Some(&moved)).unwrap() == FileState::Ours);
        // Same mode and workspace, other bytes: another login.
        let mut other: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        other["tokens"]["refresh_token"] = "user".into();
        std::fs::write(paths.auth(), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(file_state(&paths, Some(&state)).unwrap() == FileState::Foreign);
    }

    #[test]
    fn rotation_needs_positive_proof_of_the_same_login() {
        let pinned = api::token_logins(&jwt("amir"));
        assert!(pinned.same(&api::token_logins(&jwt("amir"))));
        assert!(!pinned.same(&api::token_logins(&jwt("someone-else"))));
    }

    #[test]
    fn auth_document_is_codex_external_tokens_mode() {
        let document = auth_document("seat", "a.b.c");
        assert_eq!(document["auth_mode"], "chatgptAuthTokens");
        assert_eq!(document["tokens"]["refresh_token"], "");
        assert_eq!(document["tokens"]["id_token"], "a.b.c");
        assert!(document["OPENAI_API_KEY"].is_null());
    }

    #[test]
    fn agent_runs_refresh_at_load_and_every_interval() {
        let plist = agent_plist(Path::new("/opt/codexctl"), Path::new("/tmp/a&b.log"));
        assert!(plist.contains(
            "<string>/opt/codexctl</string><string>app-auth</string><string>refresh</string>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<integer>300</integer>"));
        assert!(plist.contains("/tmp/a&amp;b.log"));
    }
}
