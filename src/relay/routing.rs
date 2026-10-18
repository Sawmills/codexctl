//! One host route for every account-server launch, including plain Codex.
use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::{config::Paths, store};

const DIRECT: &str = "https://chatgpt.com/backend-api/codex";
const PROVIDER: &str = "codexctl-central";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Routing {
    listen: Option<SocketAddr>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    routing: Routing,
}

#[derive(Clone, PartialEq, Eq)]
struct RouteState {
    relay: FileState,
    pending: FileState,
}

#[derive(Clone, PartialEq, Eq)]
enum FileState {
    Missing,
    Bytes(Vec<u8>),
    Other(std::io::ErrorKind),
}

pub(crate) struct RouteProof {
    state: RouteState,
    routing: Routing,
}

pub(super) fn receipt_listener(paths: &Paths) -> Result<Option<SocketAddr>> {
    let state = route_state(paths)?;
    let routing: Routing = if let FileState::Bytes(bytes) = state.pending {
        serde_json::from_slice::<Pending>(&bytes)?.routing
    } else if let FileState::Bytes(bytes) = state.relay {
        serde_json::from_slice(&bytes)?
    } else {
        Routing::default()
    };
    routing.url()?;
    Ok(routing.listen)
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

fn route_state(paths: &Paths) -> Result<RouteState> {
    fn read_optional(path: std::path::PathBuf) -> Result<FileState> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(FileState::Bytes(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileState::Missing),
            Err(error) => Ok(FileState::Other(error.kind())),
        }
    }

    Ok(RouteState {
        relay: read_optional(paths.codexctl_dir().join("relay.json"))?,
        pending: read_optional(paths.codexctl_dir().join("relay.pending.json"))?,
    })
}

fn unchanged(paths: &Paths, expected: &RouteState) -> Result<()> {
    if route_state(paths)? != *expected {
        bail!("relay routing changed during listener proof; retry")
    }
    Ok(())
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
    fn require_listener(&self, paths: &Paths) -> Result<()> {
        if let Some(listen) = self.listen {
            super::identity::require_listener(&paths.codexctl_dir(), listen)?;
        }
        Ok(())
    }

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
    pub(crate) listen: Option<SocketAddr>,
}

impl Routing {
    fn provider(
        &self,
        alias: Option<&str>,
        billing: Option<crate::api::BillingClass>,
    ) -> Result<ProviderRoute> {
        let mut headers = toml_edit::InlineTable::new();
        if self.listen.is_some()
            && let Some(alias) = alias.and_then(crate::outcomes::alias)
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
            listen: self.listen,
        })
    }
}

/// Prove the route selected by the current durable state before taking any
/// native, store, or relay locks. The state is checked again by the caller
/// while those locks are held before it is applied or returned.
pub(crate) fn preflight_route(paths: &Paths) -> Result<RouteProof> {
    let state = route_state(paths)?;
    let routing = if let FileState::Bytes(bytes) = &state.pending {
        serde_json::from_slice::<Pending>(bytes)?.routing
    } else if let FileState::Bytes(bytes) = &state.relay {
        serde_json::from_slice::<Routing>(bytes)?
    } else if matches!(&state.pending, FileState::Missing)
        && matches!(&state.relay, FileState::Missing)
    {
        Routing::default()
    } else {
        bail!("relay routing state is not a readable regular file")
    };
    routing.require_listener(paths)?;
    Ok(RouteProof { state, routing })
}

/// Validate a requested configuration before taking any native, store, or
/// relay locks. `None` intentionally skips listener proof so direct disable
/// remains available during rollback from a bad pending enable intent.
pub(crate) fn preflight_configuration(
    paths: &Paths,
    listen: Option<SocketAddr>,
) -> Result<RouteProof> {
    let state = route_state(paths)?;
    let routing = Routing { listen };
    routing.url()?;
    if listen.is_some() {
        routing.require_listener(paths)?;
    }
    Ok(RouteProof { state, routing })
}

pub(crate) fn central_provider(
    paths: &Paths,
    alias: Option<&str>,
    billing: Option<crate::api::BillingClass>,
    proof: &RouteProof,
) -> Result<ProviderRoute> {
    let _lock = lock(paths)?;
    unchanged(paths, &proof.state)?;
    recover(paths)?;
    let path = paths.codexctl_dir().join("relay.json");
    let routing = if path.try_exists()? {
        serde_json::from_slice::<Routing>(&std::fs::read(&path)?)?
    } else {
        Routing::default()
    };
    if routing.listen != proof.routing.listen {
        bail!("relay routing changed during listener proof; retry");
    }
    routing.provider(alias, billing)
}

/// A durable intent is rolled forward before any later route is selected. Both
/// projections use one route lock; unrelated Codex config fields are preserved.
pub(crate) fn configure(paths: &Paths, proof: &RouteProof) -> Result<()> {
    let _lock = lock(paths)?;
    unchanged(paths, &proof.state)?;
    // This explicit intent supersedes any unfinished intent. Recovery remains
    // on route selection, where the selected listener is proved before apply.
    let pending_path = paths.codexctl_dir().join("relay.pending.json");
    let result = store::atomic_write(
        &pending_path,
        &serde_json::to_vec(&Pending {
            routing: proof.routing.clone(),
        })?,
    );
    result?;
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
