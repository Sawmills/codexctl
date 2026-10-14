//! One host route for every account-server launch, including plain Codex.
use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::{config::Paths, store};

const DIRECT: &str = "https://chatgpt.com/backend-api/codex";
const PROVIDER: &str = "codexctl-central";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Routing {
    listen: Option<SocketAddr>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    routing: Routing,
}

fn lock(paths: &Paths) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    store::ensure_private_dir(&paths.codexctl_dir())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(paths.codexctl_dir().join("relay.lock"))?;
    if !file.metadata()?.is_file() {
        bail!("relay routing lock must be a regular file");
    }
    file.try_lock().context("relay routing is busy; retry")?;
    Ok(file)
}

fn recover(paths: &Paths) -> Result<()> {
    let pending = paths.codexctl_dir().join("relay.pending.json");
    if !pending.try_exists()? {
        return Ok(());
    }
    let transaction: Pending = serde_json::from_slice(&std::fs::read(&pending)?)?;
    apply(paths, &transaction.routing).context(
        "pending relay routing change could not be completed; retry after fixing the filesystem",
    )?;
    std::fs::remove_file(pending)?;
    std::fs::File::open(paths.codexctl_dir())?.sync_all()?;
    Ok(())
}

impl Routing {
    fn url(&self) -> Result<String> {
        match self.listen {
            Some(listen) if listen.ip().is_loopback() && listen.port() != 0 => {
                Ok(format!("http://{listen}/backend-api/codex"))
            }
            Some(_) => bail!("relay route requires a loopback address and a nonzero port"),
            None => Ok(DIRECT.into()),
        }
    }
}

pub(crate) struct ProviderRoute {
    pub(crate) url: String,
    pub(crate) headers: toml_edit::InlineTable,
}

impl Routing {
    fn provider(
        &self,
        alias: Option<&str>,
        billing: Option<crate::api::BillingClass>,
    ) -> Result<ProviderRoute> {
        let mut headers = toml_edit::InlineTable::new();
        if self.listen.is_some()
            && let Some(alias) = alias.and_then(super::evidence::alias)
        {
            headers.insert("X-Codexctl-Account-Alias", alias.into());
            let class = match billing {
                Some(crate::api::BillingClass::RateLimited) => "included",
                Some(crate::api::BillingClass::UsageBased) => "credit",
                _ => "unknown",
            };
            headers.insert("X-Codexctl-Account-Class", class.into());
        }
        Ok(ProviderRoute {
            url: self.url()?,
            headers,
        })
    }
}

pub(crate) fn central_provider(
    paths: &Paths,
    alias: Option<&str>,
    billing: Option<crate::api::BillingClass>,
) -> Result<ProviderRoute> {
    let _lock = lock(paths)?;
    recover(paths)?;
    let path = paths.codexctl_dir().join("relay.json");
    let routing = if path.try_exists()? {
        serde_json::from_slice::<Routing>(&std::fs::read(&path)?)?
    } else {
        Routing::default()
    };
    routing.provider(alias, billing)
}

/// A durable intent is rolled forward before any later route is selected. Both
/// projections use one route lock; unrelated Codex config fields are preserved.
pub(crate) fn configure(paths: &Paths, listen: Option<SocketAddr>) -> Result<()> {
    let routing = Routing { listen };
    routing.url()?;
    let _lock = lock(paths)?;
    recover(paths)?;
    store::atomic_write(
        &paths.codexctl_dir().join("relay.pending.json"),
        &serde_json::to_vec(&Pending { routing })?,
    )?;
    recover(paths)
}

fn apply(paths: &Paths, routing: &Routing) -> Result<()> {
    // The shared --active helper can change accounts without a client restart.
    // Static identity/billing headers would then misattribute those requests.
    let route = routing.provider(None, None)?;
    let path = crate::config::codex_config_path(&paths.codex_home())?;
    let before = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut document = before
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()?
        .unwrap_or_default()
        .parse::<DocumentMut>()?;
    let active = document
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        == Some(PROVIDER);
    if active {
        let provider = document
            .get_mut("model_providers")
            .and_then(|providers| providers.get_mut(PROVIDER))
            .context("active account-server provider is missing")?;
        provider["base_url"] = toml_edit::value(&route.url);
        provider["http_headers"] = toml_edit::value(route.headers);
        crate::config::write_codex_config(&path, document.to_string().as_bytes())?;
    }
    store::atomic_write(
        &paths.codexctl_dir().join("relay.json"),
        &serde_json::to_vec(routing)?,
    )
}
