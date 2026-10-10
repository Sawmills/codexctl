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

#[derive(Clone, Serialize, Deserialize)]
pub struct Vault {
    /// Wall-clock rotation time, shared with the encrypted credential state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rotation_ms: Option<u64>,
    pub alias: String,
    pub tenant: String,
    pub user: String,
    pub auth: Value,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub import_rejected: bool,
    /// Monotonic durable credential revision assigned by the central store.
    /// It is independent of JWT timestamps, which can collide or be absent.
    #[serde(default)]
    pub revision: i64,
}

#[derive(Clone, Serialize, Deserialize)]
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

pub struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        // A fork can inherit the open description. Release explicitly before closing.
        let _ = self.0.unlock();
    }
}

pub fn registry_lock(state: &Path, name: &str) -> Result<Lock> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match lock(state, name) {
            Ok(guard) => return Ok(guard),
            Err(e)
                if e.downcast_ref::<std::fs::TryLockError>()
                    .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn lock(state: &Path, name: &str) -> Result<Lock> {
    let file = open_lock(state, name)?;
    file.try_lock().context("another process owns this state")?;
    Ok(Lock(file))
}

pub(super) enum LockMode {
    Shared,
    Exclusive,
}

/// A shared lease can stay with a login child after its parent dies.
pub(super) fn mode_lock(state: &Path, mode: LockMode) -> Result<File> {
    let file = open_lock(state, "mode.lock")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let result = match mode {
            LockMode::Shared => file.try_lock_shared(),
            LockMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error @ std::fs::TryLockError::WouldBlock) => {
                return Err(error)
                    .context("credential mode is busy; finish existing local operations");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn open_lock(state: &Path, name: &str) -> Result<File> {
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
    Ok(options.open(path)?)
}

pub(super) fn cipher(key: &Path) -> Result<Aes256Gcm> {
    let bytes = private_read(key)?;
    Aes256Gcm::new_from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("vault key must contain exactly 32 bytes"))
}

/// Encrypt a value for storage outside the local vault file.
///
/// The key is deliberately still read from the mounted secret file. Database
/// rows contain only this nonce-prefixed ciphertext, so a database dump does
/// not grant access to credentials without the vault key.
pub(super) fn encrypt_bytes(key: &Path, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = cipher(key)?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let encrypted = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| anyhow::anyhow!("vault encryption failed"))?;
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    Ok(bytes)
}

pub(super) fn decrypt_bytes(key: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    decrypt_with_cipher(&cipher(key)?, bytes)
}

pub(super) fn decrypt_with_cipher(cipher: &Aes256Gcm, bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 28 {
        bail!("encrypted value is truncated");
    }
    cipher
        .decrypt(bytes[..12].into(), &bytes[12..])
        .map_err(|_| anyhow::anyhow!("vault authentication failed"))
}

pub fn save(state: &Path, key: &Path, vault: &Vault) -> Result<()> {
    let bytes = encrypt_bytes(key, &serde_json::to_vec(vault)?)?;
    store::atomic_write(&state.join("vault.enc"), &bytes)
}

pub fn load(state: &Path, key: &Path) -> Result<Vault> {
    let bytes = private_read(&state.join("vault.enc"))?;
    let plaintext = decrypt_bytes(key, &bytes).map_err(|error| {
        if bytes.len() < 28 {
            anyhow::anyhow!("vault is truncated")
        } else {
            error
        }
    })?;
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
    let id = [
        auth.pointer("/tokens/account_id"),
        auth.pointer("/tokens/chatgpt_account_id"),
        auth.get("account_id"),
        auth.get("chatgpt_account_id"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|id| !id.is_empty())
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
    if api::token_logins(token(auth)?).is_empty() {
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
                last_rotation_ms: None,
                alias: "personal".into(),
                tenant: "personal".into(),
                user: "amir".into(),
                label: None,
                verified: true,
                import_rejected: false,
                revision: 0,
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

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd};
    #[test]
    fn a_duplicate_descriptor_does_not_keep_a_released_lock_owned() {
        let root = tempfile::tempdir().unwrap();
        let guard = lock(root.path(), "owner.lock").unwrap();
        let fd = unsafe { libc::dup(guard.0.as_raw_fd()) };
        assert!(fd >= 0);
        let inherited = unsafe { File::from_raw_fd(fd) };
        assert!(lock(root.path(), "owner.lock").is_err());
        drop(guard);
        let _next = lock(root.path(), "owner.lock").unwrap();
        drop(inherited);
    }
}
