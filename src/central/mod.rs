//! Experimental single-account credential broker and local App Server client.
mod activity;
mod catalog;
mod client;
mod dashboard;
pub mod enrollment;
mod fast_path;
pub mod loans;
pub mod managed;
pub mod native;
mod owner_refresh;
#[cfg(target_os = "linux")]
mod polling;
mod process;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use polling::supervise_login;
mod relogin;
pub mod remote;
mod rename;
pub(crate) mod resets;
mod rpc;
mod server;
mod sessions;
pub mod storage;
mod transport;
mod vault;

pub use client::run_client;
pub use server::serve;

/// Revoke a device in the shared registry with a per-entity CAS revision.
pub async fn revoke_central(state: &Path, key: &Path, tenant: &str, id: &str) -> Result<()> {
    store::validate_alias(tenant)?;
    store::validate_alias(id)?;
    let central = storage::runtime_store(state, key).await?;
    let Some((_, payload, revision)) = central.load_device_entity_revision(tenant, id).await?
    else {
        bail!("device not found");
    };
    let mut device: vault::Device = serde_json::from_slice(&payload)?;
    device.revoked = true;
    if !central
        .save_device_entity_cas(tenant, id, &serde_json::to_vec(&device)?, Some(revision))
        .await?
    {
        bail!("device changed concurrently; retry");
    }
    central.retire_relay_event_limiter(id).await?;
    Ok(())
}

pub async fn register_central(
    state: &Path,
    key: &Path,
    id: &str,
    tenant: &str,
    user: &str,
    token_file: &Path,
) -> Result<()> {
    store::validate_alias(id)?;
    store::validate_alias(tenant)?;
    store::validate_alias(user)?;
    let central = storage::runtime_store(state, key).await?;
    let devices = central.load_device_entity_revisions(tenant).await?;
    if devices.iter().any(|(device_id, _, _)| device_id == id) {
        bail!("device already registered");
    }
    let token = vault::digest(&enrollment::random_bytes());
    vault::create_secret(token_file, token.as_bytes())?;
    let device = vault::Device {
        id: id.into(),
        tenant: tenant.into(),
        user: user.into(),
        token_hash: vault::digest(token.as_bytes()),
        revoked: false,
    };
    let saved = central
        .save_device_entity_cas(tenant, id, &serde_json::to_vec(&device)?, None)
        .await;
    match saved {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = std::fs::remove_file(token_file);
            bail!("device was registered concurrently; retry")
        }
        Err(error) => {
            let _ = std::fs::remove_file(token_file);
            Err(error)
        }
    }
}

use crate::store;
use aes_gcm::aead::{OsRng, rand_core::RngCore};
use anyhow::{Result, bail};
use std::path::Path;

pub fn init(
    state: &Path,
    key: &Path,
    auth: &Path,
    alias: &str,
    tenant: &str,
    user: &str,
) -> Result<()> {
    store::validate_alias(alias)?;
    store::validate_alias(tenant)?;
    store::validate_alias(user)?;
    let _lock = vault::lock(state, "owner.lock")?;
    if state.join("vault.enc").exists() || state.join("devices.json").exists() {
        bail!("state already initialized");
    }
    let auth = serde_json::from_slice(&vault::private_read(auth)?)?;
    vault::validate_auth(&auth)?;
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    vault::create_secret(key, &bytes)?;
    vault::save(
        state,
        key,
        &vault::Vault {
            alias: alias.into(),
            tenant: tenant.into(),
            user: user.into(),
            auth,
            label: None,
            verified: true,
            import_rejected: false,
            revision: 0,
        },
    )?;
    vault::save_devices(state, &[])?;
    Ok(())
}

pub fn register(state: &Path, id: &str, tenant: &str, user: &str, token_file: &Path) -> Result<()> {
    store::validate_alias(id)?;
    store::validate_alias(tenant)?;
    store::validate_alias(user)?;
    let _lock = vault::registry_lock(state, "devices.lock")?;
    let mut devices = vault::devices(state)?;
    if devices.iter().any(|d| d.id == id) {
        bail!("device already registered");
    }
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = vault::digest(&bytes);
    vault::create_secret(token_file, token.as_bytes())?;
    devices.push(vault::Device {
        id: id.into(),
        tenant: tenant.into(),
        user: user.into(),
        token_hash: vault::digest(token.as_bytes()),
        revoked: false,
    });
    vault::save_devices(state, &devices)
}

pub fn revoke(state: &Path, id: &str) -> Result<()> {
    let _lock = vault::registry_lock(state, "devices.lock")?;
    let mut devices = vault::devices(state)?;
    let device = devices
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or_else(|| anyhow::anyhow!("device not found"))?;
    device.revoked = true;
    vault::save_devices(state, &devices)
}
