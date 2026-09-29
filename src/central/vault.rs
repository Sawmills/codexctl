use crate::{api, store};
use aes_gcm::{
    Aes256Gcm,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

#[derive(Serialize, Deserialize)]
pub struct Vault {
    pub alias: String,
    pub tenant: String,
    pub user: String,
    pub auth: Value,
}

#[derive(Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub tenant: String,
    pub user: String,
    pub token_hash: String,
    pub revoked: bool,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn private_read(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        bail!("secret must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("secret file must have mode 0600 or stricter");
        }
    }
    std::fs::read(path).context("could not read private file")
}

pub fn create_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .context("could not create secret file; destination must be new")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn lock(state: &Path, name: &str) -> Result<File> {
    store::ensure_private_dir(state)?;
    let path = state.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("lock file must not be a symlink");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock().context("another process owns this state")?;
    Ok(file)
}

fn cipher(key: &Path) -> Result<Aes256Gcm> {
    let bytes = private_read(key)?;
    Aes256Gcm::new_from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("vault key must contain exactly 32 bytes"))
}

pub fn save(state: &Path, key: &Path, vault: &Vault) -> Result<()> {
    let cipher = cipher(key)?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let encrypted = cipher
        .encrypt(&nonce, serde_json::to_vec(vault)?.as_ref())
        .map_err(|_| anyhow::anyhow!("vault encryption failed"))?;
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    store::atomic_write(&state.join("vault.enc"), &bytes)
}

pub fn load(state: &Path, key: &Path) -> Result<Vault> {
    let bytes = private_read(&state.join("vault.enc"))?;
    if bytes.len() < 28 {
        bail!("vault is truncated");
    }
    let plaintext = cipher(key)?
        .decrypt(bytes[..12].into(), &bytes[12..])
        .map_err(|_| anyhow::anyhow!("vault authentication failed"))?;
    serde_json::from_slice(&plaintext).context("invalid vault data")
}

pub fn token(auth: &Value) -> Result<&str> {
    auth.pointer("/tokens/access_token")
        .or_else(|| auth.get("access_token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("missing access token")
}

pub fn account(auth: &Value) -> Result<String> {
    let token = token(auth)?;
    let id = auth
        .pointer("/tokens/account_id")
        .or_else(|| auth.pointer("/tokens/chatgpt_account_id"))
        .or_else(|| auth.get("account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| api::extract_account_id(token))
        .filter(|s| !s.is_empty())
        .context("missing account identifier")?;
    if api::extract_account_id(token).is_some_and(|claimed| claimed != id) {
        bail!("account identifier conflicts with token");
    }
    Ok(id)
}

pub fn validate_auth(auth: &Value) -> Result<()> {
    account(auth)?;
    if auth
        .pointer("/tokens/refresh_token")
        .or_else(|| auth.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        bail!("server auth must include a refresh token");
    }
    if api::token_subject(token(auth)?).is_none() {
        bail!("server auth must identify its login");
    }
    Ok(())
}

pub fn devices(state: &Path) -> Result<Vec<Device>> {
    serde_json::from_slice(&private_read(&state.join("devices.json"))?)
        .context("invalid device registry")
}

pub fn save_devices(state: &Path, devices: &[Device]) -> Result<()> {
    store::atomic_write(&state.join("devices.json"), &serde_json::to_vec(devices)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encrypted_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        create_secret(&key, &[7; 32]).unwrap();
        save(
            root.path(),
            &key,
            &Vault {
                alias: "personal".into(),
                tenant: "personal".into(),
                user: "amir".into(),
                auth: json!({"refresh_token":"synthetic-rotated-refresh"}),
            },
        )
        .unwrap();
        (root, key)
    }

    #[test]
    fn when_ciphertext_changes_then_the_vault_rejects_it() {
        let (root, key) = encrypted_fixture();
        let path = root.path().join("vault.enc");
        let mut bytes = private_read(&path).unwrap();
        bytes[12] ^= 1;
        store::atomic_write(&path, &bytes).unwrap();

        let result = load(root.path(), &key);

        assert!(result.is_err());
    }

    #[test]
    fn when_the_key_is_wrong_then_the_vault_rejects_it() {
        let (root, _) = encrypted_fixture();
        let key = root.path().join("wrong-key");
        create_secret(&key, &[8; 32]).unwrap();

        let result = load(root.path(), &key);

        assert!(result.is_err());
    }
}
