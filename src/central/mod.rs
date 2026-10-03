//! Experimental single-account credential broker and local App Server client.
mod activity;
mod catalog;
mod client;
mod dashboard;
pub mod enrollment;
pub mod managed;
pub mod native;
mod process;
mod relogin;
pub mod remote;
pub(crate) mod resets;
mod rpc;
mod server;
mod sessions;
pub mod storage;
mod transport;
mod vault;

pub use client::run_client;
pub use server::serve;

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
