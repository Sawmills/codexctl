//! Shared machine credentials for jobs. Service tokens stay in the environment;
//! serialized connection metadata records only their source, never their value.
use super::{remote::Connection, transport, vault};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MachineCredential {
    File(PathBuf),
    Service(()),
}

impl From<PathBuf> for MachineCredential {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}
impl From<&str> for MachineCredential {
    fn from(path: &str) -> Self {
        Self::File(path.into())
    }
}

struct Environment {
    server: String,
    token: String,
}

fn environment() -> Result<Option<Environment>> {
    let Some(token) = std::env::var_os("CODEXCTL_SERVICE_TOKEN") else {
        if std::env::var_os("CODEXCTL_SERVER_URL").is_some() {
            bail!("CODEXCTL_SERVICE_TOKEN is required with CODEXCTL_SERVER_URL");
        }
        return Ok(None);
    };
    let token = token
        .into_string()
        .map_err(|_| anyhow::anyhow!("CODEXCTL_SERVICE_TOKEN must be UTF-8"))?;
    if token.trim().is_empty() {
        bail!("CODEXCTL_SERVICE_TOKEN is empty");
    }
    let server = std::env::var("CODEXCTL_SERVER_URL")
        .context("CODEXCTL_SERVER_URL is required with CODEXCTL_SERVICE_TOKEN")?;
    let server = transport::origin(&server)?
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    Ok(Some(Environment {
        server,
        token: token.trim().to_owned(),
    }))
}

/// Service jobs use subscription capacity only and never inherit human auth.
pub fn enabled() -> Result<bool> {
    Ok(environment()?.is_some())
}

impl MachineCredential {
    pub fn file(&self) -> Result<&Path> {
        match self {
            Self::File(path) => Ok(path),
            Self::Service(()) => bail!(
                "this operation requires a saved machine connection; unset CODEXCTL_SERVICE_TOKEN"
            ),
        }
    }

    pub fn read_for(&self, server: &str) -> Result<String> {
        match self {
            Self::File(path) => Ok(String::from_utf8(vault::private_read(path)?)?.trim().into()),
            Self::Service(()) => {
                let env = environment()?
                    .context("CODEXCTL_SERVICE_TOKEN is required for this job connection")?;
                let expected = transport::origin(server)?
                    .to_string()
                    .trim_end_matches('/')
                    .to_owned();
                if env.server != expected {
                    bail!("service credential belongs to another account server");
                }
                Ok(env.token)
            }
        }
    }
}

/// An env-only job authenticates as an ordinary machine. A refused or malformed
/// identity is terminal; callers must not fall back to a saved human login.
pub(super) fn connection() -> Result<Option<Connection>> {
    let Some(env) = environment()? else {
        return Ok(None);
    };
    let response = transport::blocking()?
        .get(format!("{}/v1/me", env.server))
        .bearer_auth(&env.token)
        .send()
        .context("service authentication unavailable")?;
    if !response.status().is_success() {
        bail!("service credential rejected (HTTP {})", response.status());
    }
    let identity: serde_json::Value = response.json().context("invalid service identity")?;
    let user_id = identity
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .context("invalid service identity")?
        .to_owned();
    Ok(Some(Connection {
        server: env.server,
        user_id,
        token_file: MachineCredential::Service(()),
    }))
}

const SHARED_MACHINE: &str = "shared-service-account";

/// Local server administration: replace the one shared machine credential. Only
/// its hash is stored, and the caller emits the new token once after commit.
pub async fn issue(state: &Path, key: &Path, company_user: &str) -> Result<String> {
    crate::store::validate_alias(company_user)?;
    if super::storage::StoreMode::from_env()? == super::storage::StoreMode::File {
        let _lock = vault::registry_lock(state, "devices.lock")?;
        if !super::managed::users(state)?
            .iter()
            .any(|user| user.id == company_user && user.enabled)
        {
            bail!("enabled company user not found; list server users first");
        }
        let token = vault::digest(&super::enrollment::random_bytes());
        let device = vault::Device {
            id: SHARED_MACHINE.into(),
            tenant: "sawmills".into(),
            user: company_user.into(),
            token_hash: vault::digest(token.as_bytes()),
            revoked: false,
        };
        let mut devices = vault::devices(state)?;
        devices.retain(|device| device.id != SHARED_MACHINE || device.tenant != "sawmills");
        devices.push(device);
        vault::save_devices(state, &devices)?;
        return Ok(token);
    }
    let central = super::storage::runtime_store(state, key).await?;
    if !central.enabled_user(company_user).await? {
        bail!("enabled company user not found; list server users first");
    }
    let prior = central
        .load_device_entity_revision("sawmills", SHARED_MACHINE)
        .await?;
    let token = vault::digest(&super::enrollment::random_bytes());
    let device = vault::Device {
        id: SHARED_MACHINE.into(),
        tenant: "sawmills".into(),
        user: company_user.into(),
        token_hash: vault::digest(token.as_bytes()),
        revoked: false,
    };
    if !central
        .save_device_entity_cas(
            "sawmills",
            SHARED_MACHINE,
            &serde_json::to_vec(&device)?,
            prior.map(|(_, _, revision)| revision),
        )
        .await?
    {
        bail!("shared credential changed concurrently; issue again");
    }
    Ok(token)
}
