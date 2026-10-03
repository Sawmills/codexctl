//! Durable storage for the central account broker.
//!
//! File storage remains the default.  PostgreSQL schema provisioning is
//! available with `codexctl-central migrate`; server startup rejects
//! `CODEXCTL_CENTRAL_STORE=postgres`; dual mode is opt-in until rollout. The
//! database never receives the vault key:
//! credential and enrollment payloads are nonce-prefixed AES-GCM ciphertext.

use super::vault;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DB_TIMEOUT: Duration = Duration::from_secs(2);

async fn bounded_db<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(DB_TIMEOUT, future)
        .await
        .context("central PostgreSQL operation timed out")?
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreMode {
    File,
    Postgres,
    Dual,
}

impl StoreMode {
    pub fn from_env() -> Result<Self> {
        let mut configured = std::env::var("CODEXCTL_CENTRAL_STORE")
            .unwrap_or_else(|_| "file".into())
            .to_ascii_lowercase();
        let dual_write = std::env::var("CODEXCTL_CENTRAL_DUAL_WRITE")
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if dual_write && configured == "file" {
            configured = "dual".into();
        }
        match configured.as_str() {
            "file" => Ok(Self::File),
            "postgres" => Ok(Self::Postgres),
            "dual" => Ok(Self::Dual),
            value => {
                bail!("invalid CODEXCTL_CENTRAL_STORE value {value:?}; use file, postgres, or dual")
            }
        }
    }

    pub fn dual_write_enabled() -> bool {
        std::env::var("CODEXCTL_CENTRAL_DUAL_WRITE")
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    }
}

impl std::fmt::Display for StoreMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::File => "file",
            Self::Postgres => "postgres",
            Self::Dual => "dual",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CredentialRecord {
    /// Stable broker owner key (`account_key(user, alias)`), not the workspace.
    pub account_id: String,
    pub user_id: Option<String>,
    pub alias: String,
    pub workspace: Option<String>,
    pub login: Option<String>,
    pub vault: Value,
    pub revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    pub account_id: String,
    pub holder_id: String,
    pub epoch: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileState {
    accounts: BTreeMap<String, Vec<u8>>,
    leases: BTreeMap<String, FileLeaseOnDisk>,
    enrollments: BTreeMap<String, EnrollmentOnDisk>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileLeaseOnDisk {
    holder_id: String,
    epoch: i64,
    expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EnrollmentOnDisk {
    payload: Vec<u8>,
    expires_at: u64,
    consumed: bool,
}

#[derive(Clone)]
pub struct FileStore {
    state: PathBuf,
    key: PathBuf,
}

#[derive(Clone)]
pub struct PostgresStore {
    url: String,
    tls_enabled: bool,
    client: Arc<tokio::sync::Mutex<Option<Arc<tokio_postgres::Client>>>>,
    key: PathBuf,
}

#[derive(Clone)]
pub enum CentralStore {
    File(FileStore),
    Postgres(PostgresStore),
    Dual {
        file: FileStore,
        postgres: PostgresStore,
        mirror_failures: Arc<std::sync::atomic::AtomicU64>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct BackfillCounts {
    pub accounts: usize,
    pub users: usize,
    pub devices: usize,
    pub observed_relogins: usize,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS central_schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS central_accounts (
    account_id TEXT PRIMARY KEY,
    user_id TEXT,
    alias TEXT NOT NULL,
    workspace TEXT,
    login TEXT,
    encrypted_vault BYTEA NOT NULL,
    revision BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS account_refresh_leases (
    account_id TEXT PRIMARY KEY REFERENCES central_accounts(account_id) ON DELETE CASCADE,
    holder_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS enrollment_challenges (
    challenge_hash TEXT PRIMARY KEY,
    encrypted_payload BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS enrollment_challenges_expiry_idx ON enrollment_challenges (expires_at);
CREATE TABLE IF NOT EXISTS central_registry (
    name TEXT PRIMARY KEY,
    encrypted_payload BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

impl CentralStore {
    /// Open the configured backend.  `file` does not require a database URL;
    /// PostgreSQL and dual mode fail early when one is not configured.
    pub async fn from_env(state: &Path, key: &Path) -> Result<Self> {
        Self::from_mode(StoreMode::from_env()?, state, key).await
    }

    pub async fn from_mode(mode: StoreMode, state: &Path, key: &Path) -> Result<Self> {
        let file = FileStore {
            state: state.into(),
            key: key.into(),
        };
        match mode {
            StoreMode::File => Ok(Self::File(file)),
            StoreMode::Postgres => Ok(Self::Postgres(PostgresStore::connect(key).await?)),
            StoreMode::Dual => Ok(Self::Dual {
                file,
                postgres: PostgresStore::connect(key).await?,
                mirror_failures: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }),
        }
    }

    pub async fn migrate(&self) -> Result<()> {
        match self {
            Self::File(file) => file.migrate(),
            Self::Postgres(db) => db.migrate().await,
            Self::Dual { file, postgres, .. } => {
                file.migrate()?;
                postgres.migrate().await
            }
        }
    }

    pub fn mode(&self) -> StoreMode {
        match self {
            Self::File(_) => StoreMode::File,
            Self::Postgres(_) => StoreMode::Postgres,
            Self::Dual { .. } => StoreMode::Dual,
        }
    }

    pub async fn reachable(&self) -> bool {
        match self {
            Self::File(file) => file.path().exists() || file.state.exists(),
            Self::Postgres(db) => bounded_db(db.client()).await.is_ok(),
            Self::Dual { postgres, .. } => bounded_db(postgres.client()).await.is_ok(),
        }
    }

    pub async fn save_registry(&self, name: &str, payload: &[u8]) -> Result<()> {
        match self {
            Self::File(file) => file.save_registry(name, payload),
            Self::Postgres(db) => db.save_registry(name, payload).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                postgres.save_registry(name, payload).await?;
                if let Err(error) = file.save_registry(name, payload) {
                    mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "{}",
                        serde_json::json!({"operation":"central_store_mirror","backend":"file","stage":"registry","reason":error.to_string()})
                    );
                }
                Ok(())
            }
        }
    }

    pub async fn load_registry(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(_) => Ok(None),
            Self::Postgres(db) => bounded_db(db.load_registry(name)).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.load_registry(name)).await,
        }
    }

    pub async fn backfill(&self, state: &Path, key: &Path) -> Result<BackfillCounts> {
        let target = match self {
            Self::Postgres(db) => db,
            Self::Dual { postgres, .. } => postgres,
            Self::File(_) => bail!("backfill requires PostgreSQL or dual central storage"),
        };
        target.migrate().await?;
        let mut counts = BackfillCounts {
            accounts: 0,
            users: 0,
            devices: 0,
            observed_relogins: 0,
        };
        let accounts = state.join("accounts");
        if accounts.exists() {
            for entry in std::fs::read_dir(accounts)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let account_state = entry.path();
                if !account_state.join("vault.enc").exists() {
                    continue;
                }
                let value = crate::central::vault::load(&account_state, key)?;
                let workspace = crate::central::vault::account(&value.auth)?;
                let login = crate::central::vault::token(&value.auth)
                    .ok()
                    .and_then(crate::api::token_subject);
                let revision = crate::central::vault::token(&value.auth)
                    .ok()
                    .and_then(|token| {
                        crate::api::token_issued_at(token)
                            .or_else(|| crate::api::token_expiry(token))
                    })
                    .unwrap_or(0);
                let record = CredentialRecord {
                    account_id: crate::central::managed::account_key(&value.user, &value.alias),
                    user_id: Some(value.user.clone()),
                    alias: value.alias.clone(),
                    workspace: Some(workspace),
                    login,
                    vault: serde_json::to_value(value)?,
                    revision,
                };
                target.save_account(&record).await?;
                counts.accounts += 1;
            }
        }
        for (name, path) in [
            ("users", state.join("users.json")),
            ("devices", state.join("devices.json")),
        ] {
            if path.exists() {
                let bytes = crate::central::vault::private_read(&path)?;
                let user_count = if name == "users" {
                    Some(
                        serde_json::from_slice::<Vec<crate::central::managed::User>>(&bytes)?.len(),
                    )
                } else {
                    None
                };
                let device_count = if name == "devices" {
                    Some(
                        serde_json::from_slice::<Vec<crate::central::vault::Device>>(&bytes)?.len(),
                    )
                } else {
                    None
                };
                target.save_registry(name, &bytes).await?;
                if name == "users" {
                    counts.users = user_count.unwrap_or_default();
                } else {
                    counts.devices = device_count.unwrap_or_default();
                }
            }
        }
        let relogin = state.join("relogin");
        if relogin.exists() {
            counts.observed_relogins = std::fs::read_dir(relogin)?
                .filter_map(Result::ok)
                .filter(|e| e.path().join("record.json").exists())
                .count();
        }
        Ok(counts)
    }

    pub async fn save_account(&self, record: &CredentialRecord) -> Result<()> {
        match self {
            Self::File(file) => file.save_account(record),
            Self::Postgres(db) => bounded_db(db.save_account(record)).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                bounded_db(postgres.save_account(record)).await?;
                let mirror = file.clone();
                let record = record.clone();
                let result =
                    tokio::task::spawn_blocking(move || mirror.save_account_if_newer(&record))
                        .await
                        .context("central file mirror task failed")?;
                if let Err(error) = result {
                    mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "{}",
                        serde_json::json!({"operation":"central_store_mirror","backend":"file","stage":"save_account","reason":error.to_string()})
                    );
                }
                Ok(())
            }
        }
    }

    pub async fn load_account(&self, account_id: &str) -> Result<Option<CredentialRecord>> {
        match self {
            Self::File(file) => file.load_account(account_id),
            Self::Postgres(db) => db.load_account(account_id).await,
            Self::Dual { file, postgres, .. } => Ok(postgres
                .load_account(account_id)
                .await?
                .or(file.load_account(account_id)?)),
        }
    }

    pub async fn list_accounts(&self) -> Result<Vec<CredentialRecord>> {
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.list_accounts()).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.list_accounts()).await,
        }
    }

    pub async fn acquire_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        ttl: Duration,
    ) -> Result<Lease> {
        match self {
            Self::File(file) => file.acquire_lease(account_id, holder_id, ttl),
            Self::Postgres(db) => bounded_db(db.acquire_lease(account_id, holder_id, ttl)).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                // PostgreSQL is the fencing authority in dual mode.  Mirroring
                // to disk is useful during migration, but never grants a lease.
                let lease = bounded_db(postgres.acquire_lease(account_id, holder_id, ttl)).await?;
                let mirror = file.clone();
                let lease_copy = lease.clone();
                let result =
                    tokio::task::spawn_blocking(move || mirror.mirror_lease(&lease_copy, ttl))
                        .await
                        .context("central file lease mirror task failed")?;
                if let Err(error) = result {
                    mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "operation": "central_store_mirror",
                            "backend": "file",
                            "stage": "acquire_lease",
                            "reason": error.to_string(),
                        })
                    );
                }
                Ok(lease)
            }
        }
    }

    pub async fn fenced_write(&self, lease: &Lease, record: &CredentialRecord) -> Result<bool> {
        match self {
            Self::File(file) => file.fenced_write(lease, record),
            Self::Postgres(db) => bounded_db(db.fenced_write(lease, record)).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                let written = bounded_db(postgres.fenced_write(lease, record)).await?;
                if written {
                    let mirror = file.clone();
                    let lease_copy = lease.clone();
                    let record_copy = record.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        mirror.fenced_write(&lease_copy, &record_copy)
                    })
                    .await
                    .context("central file fenced mirror task failed")?;
                    match result {
                        Ok(true) => {}
                        Ok(false) => {
                            mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "operation": "central_store_mirror",
                                    "backend": "file",
                                    "stage": "fenced_write",
                                    "reason": "fence rejected",
                                })
                            );
                        }
                        Err(error) => {
                            mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "operation": "central_store_mirror",
                                    "backend": "file",
                                    "stage": "fenced_write",
                                    "reason": error.to_string(),
                                })
                            );
                        }
                    }
                }
                Ok(written)
            }
        }
    }

    pub async fn release_lease(&self, lease: &Lease) -> Result<bool> {
        match self {
            Self::File(file) => file.release_lease(lease),
            Self::Postgres(db) => bounded_db(db.release_lease(lease)).await,
            Self::Dual { file, postgres, .. } => {
                let released = bounded_db(postgres.release_lease(lease)).await?;
                let mirror = file.clone();
                let lease = lease.clone();
                let _ = tokio::task::spawn_blocking(move || mirror.release_lease(&lease)).await;
                Ok(released)
            }
        }
    }

    pub async fn create_enrollment(
        &self,
        challenge: &str,
        payload: &[u8],
        ttl: Duration,
    ) -> Result<()> {
        match self {
            Self::File(file) => file.create_enrollment(challenge, payload, ttl),
            Self::Postgres(db) => db.create_enrollment(challenge, payload, ttl).await,
            Self::Dual { file, postgres, .. } => {
                file.create_enrollment(challenge, payload, ttl)?;
                postgres.create_enrollment(challenge, payload, ttl).await
            }
        }
    }

    /// Atomically consume a challenge. Exactly one concurrent caller receives
    /// the payload; expired and already-consumed challenges return `None`.
    pub async fn consume_enrollment(&self, challenge: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(file) => file.consume_enrollment(challenge),
            Self::Postgres(db) => db.consume_enrollment(challenge).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                let value = postgres.consume_enrollment(challenge).await?;
                if value.is_some() {
                    match file.consume_enrollment(challenge) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "operation": "central_store_mirror",
                                    "backend": "file",
                                    "stage": "consume_enrollment",
                                    "reason": "challenge missing from file mirror",
                                })
                            );
                        }
                        Err(error) => {
                            mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "operation": "central_store_mirror",
                                    "backend": "file",
                                    "stage": "consume_enrollment",
                                    "reason": error.to_string(),
                                })
                            );
                        }
                    }
                }
                Ok(value)
            }
        }
    }

    pub async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<bool> {
        match self {
            Self::File(file) => file.renew(lease, ttl),
            Self::Postgres(db) => bounded_db(db.renew(lease, ttl)).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                let renewed = bounded_db(postgres.renew(lease, ttl)).await?;
                if renewed {
                    match file.renew(lease, ttl) {
                        Ok(true) => {}
                        Ok(false) | Err(_) => {
                            mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "operation": "central_store_mirror",
                                    "backend": "file",
                                    "stage": "renew",
                                    "reason": "file lease renewal rejected",
                                })
                            );
                        }
                    }
                }
                Ok(renewed)
            }
        }
    }

    pub fn mirror_failures(&self) -> u64 {
        match self {
            Self::Dual {
                mirror_failures, ..
            } => mirror_failures.load(std::sync::atomic::Ordering::Relaxed),
            _ => 0,
        }
    }
}

impl FileStore {
    fn path(&self) -> PathBuf {
        self.state.join("central-storage.enc")
    }

    fn read_state(&self) -> Result<FileState> {
        let path = self.path();
        if !path.exists() {
            return Ok(FileState::default());
        }
        let bytes = vault::private_read(&path)?;
        Ok(serde_json::from_slice(&vault::decrypt_bytes(
            &self.key, &bytes,
        )?)?)
    }

    fn write_state(&self, state: &FileState) -> Result<()> {
        std::fs::create_dir_all(&self.state)?;
        let plain = serde_json::to_vec(state)?;
        let encrypted = vault::encrypt_bytes(&self.key, &plain)?;
        crate::store::atomic_write(&self.path(), &encrypted)
    }

    fn with_lock<T>(&self, f: impl FnOnce(&mut FileState) -> Result<T>) -> Result<T> {
        let _lock = vault::registry_lock(&self.state, "central-storage.lock")?;
        let mut state = self.read_state()?;
        let result = f(&mut state)?;
        self.write_state(&state)?;
        Ok(result)
    }

    fn migrate(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state)?;
        if !self.path().exists() {
            self.write_state(&FileState::default())?;
        }
        Ok(())
    }

    fn save_account(&self, record: &CredentialRecord) -> Result<()> {
        self.save_account_if_newer(record).map(|_| ())
    }

    fn save_account_if_newer(&self, record: &CredentialRecord) -> Result<bool> {
        let _lock = vault::registry_lock(&self.state, "central-storage.lock")?;
        let mut state = self.read_state()?;
        let replace = state.accounts.get(&record.account_id).is_none_or(|bytes| {
            let old = vault::decrypt_bytes(&self.key, bytes)
                .ok()
                .and_then(|plain| serde_json::from_slice::<CredentialRecord>(&plain).ok());
            old.is_none_or(|old| record.revision > old.revision)
        });
        if !replace {
            return Ok(false);
        }
        state.accounts.insert(
            record.account_id.clone(),
            vault::encrypt_bytes(&self.key, &serde_json::to_vec(record)?)?,
        );
        self.write_state(&state)?;
        Ok(true)
    }

    fn load_account(&self, account_id: &str) -> Result<Option<CredentialRecord>> {
        let state = self.read_state()?;
        state
            .accounts
            .get(account_id)
            .map(|bytes| {
                Ok(serde_json::from_slice(&vault::decrypt_bytes(
                    &self.key, bytes,
                )?)?)
            })
            .transpose()
    }

    fn save_registry(&self, name: &str, payload: &[u8]) -> Result<()> {
        self.with_lock(|state| {
            state.accounts.insert(
                format!("registry:{name}"),
                vault::encrypt_bytes(&self.key, payload)?,
            );
            Ok(())
        })
    }

    fn acquire_lease(&self, account_id: &str, holder_id: &str, ttl: Duration) -> Result<Lease> {
        let now = now_secs();
        self.with_lock(|state| {
            let existing = state.leases.get(account_id);
            if existing.is_some_and(|lease| lease.expires_at > now && lease.holder_id != holder_id)
            {
                bail!("refresh lease is held by another instance")
            }
            let epoch = existing.map_or(1, |lease| lease.epoch + 1);
            state.leases.insert(
                account_id.into(),
                FileLeaseOnDisk {
                    holder_id: holder_id.into(),
                    epoch,
                    expires_at: now + ttl.as_secs(),
                },
            );
            Ok(Lease {
                account_id: account_id.into(),
                holder_id: holder_id.into(),
                epoch,
            })
        })
    }

    fn mirror_lease(&self, lease: &Lease, ttl: Duration) -> Result<()> {
        self.with_lock(|state| {
            state.leases.insert(
                lease.account_id.clone(),
                FileLeaseOnDisk {
                    holder_id: lease.holder_id.clone(),
                    epoch: lease.epoch,
                    expires_at: now_secs() + ttl.as_secs(),
                },
            );
            Ok(())
        })
    }

    fn renew(&self, lease: &Lease, ttl: Duration) -> Result<bool> {
        let now = now_secs();
        self.with_lock(|state| {
            let Some(current) = state.leases.get_mut(&lease.account_id) else {
                return Ok(false);
            };
            if current.holder_id != lease.holder_id
                || current.epoch != lease.epoch
                || current.expires_at <= now
            {
                return Ok(false);
            }
            current.expires_at = now + ttl.as_secs();
            Ok(true)
        })
    }

    fn release_lease(&self, lease: &Lease) -> Result<bool> {
        let _lock = vault::registry_lock(&self.state, "central-storage.lock")?;
        let mut state = self.read_state()?;
        let released = state.leases.get(&lease.account_id).is_some_and(|current| {
            current.holder_id == lease.holder_id && current.epoch == lease.epoch
        });
        if released {
            state.leases.remove(&lease.account_id);
            self.write_state(&state)?;
        }
        Ok(released)
    }

    fn fenced_write(&self, lease: &Lease, record: &CredentialRecord) -> Result<bool> {
        let now = now_secs();
        let _lock = vault::registry_lock(&self.state, "central-storage.lock")?;
        let mut state = self.read_state()?;
        let Some(current) = state.leases.get(&lease.account_id) else {
            return Ok(false);
        };
        if current.holder_id != lease.holder_id
            || current.epoch != lease.epoch
            || current.expires_at <= now
        {
            return Ok(false);
        }
        if lease.account_id != record.account_id {
            bail!("lease account does not match credential account")
        }
        let replace = state.accounts.get(&record.account_id).is_none_or(|bytes| {
            let old = vault::decrypt_bytes(&self.key, bytes)
                .ok()
                .and_then(|plain| serde_json::from_slice::<CredentialRecord>(&plain).ok());
            old.is_none_or(|old| record.revision > old.revision)
        });
        if !replace {
            return Ok(state
                .accounts
                .get(&record.account_id)
                .and_then(|bytes| vault::decrypt_bytes(&self.key, bytes).ok())
                .and_then(|plain| serde_json::from_slice::<CredentialRecord>(&plain).ok())
                .is_some_and(|old| old.revision == record.revision));
        }
        state.accounts.insert(
            record.account_id.clone(),
            vault::encrypt_bytes(&self.key, &serde_json::to_vec(record)?)?,
        );
        self.write_state(&state)?;
        Ok(true)
    }

    fn create_enrollment(&self, challenge: &str, payload: &[u8], ttl: Duration) -> Result<()> {
        let hash = vault::digest(challenge.as_bytes());
        self.with_lock(|state| {
            state.enrollments.insert(
                hash,
                EnrollmentOnDisk {
                    payload: vault::encrypt_bytes(&self.key, payload)?,
                    expires_at: now_secs() + ttl.as_secs(),
                    consumed: false,
                },
            );
            Ok(())
        })
    }

    fn consume_enrollment(&self, challenge: &str) -> Result<Option<Vec<u8>>> {
        let hash = vault::digest(challenge.as_bytes());
        self.with_lock(|state| {
            let Some(entry) = state.enrollments.get_mut(&hash) else {
                return Ok(None);
            };
            if entry.consumed || entry.expires_at <= now_secs() {
                return Ok(None);
            }
            entry.consumed = true;
            Ok(Some(vault::decrypt_bytes(&self.key, &entry.payload)?))
        })
    }
}

impl PostgresStore {
    async fn connect(key: &Path) -> Result<Self> {
        let url = std::env::var("DATABASE_URL")
            .context("DATABASE_URL is required for PostgreSQL central storage")?;
        let tls_enabled = std::env::var("CODEXCTL_CENTRAL_DB_TLS")
            .map(|value| value != "0" && value != "false" && value != "disable")
            .unwrap_or(true);
        let store = Self {
            url,
            tls_enabled,
            client: Arc::new(tokio::sync::Mutex::new(None)),
            key: key.into(),
        };
        let _ = store.client().await?;
        Ok(store)
    }

    async fn establish(&self) -> Result<Arc<tokio_postgres::Client>> {
        let mut config: tokio_postgres::Config = self
            .url
            .parse()
            .context("invalid DATABASE_URL for PostgreSQL central storage")?;
        config.connect_timeout(DB_TIMEOUT);
        let client = if self.tls_enabled {
            if config.get_ssl_mode() != tokio_postgres::config::SslMode::Require {
                bail!(
                    "TLS is enabled for central PostgreSQL; DATABASE_URL must set sslmode=require"
                );
            }
            let (client, connection) = config
                .connect(tls_connector()?)
                .await
                .context("connect to central PostgreSQL over TLS")?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("central PostgreSQL connection: {error}");
                }
            });
            client
        } else {
            config.ssl_mode(tokio_postgres::config::SslMode::Disable);
            let (client, connection) = config
                .connect(tokio_postgres::NoTls)
                .await
                .context("connect to central PostgreSQL")?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("central PostgreSQL connection: {error}");
                }
            });
            client
        };
        tokio::time::timeout(
            DB_TIMEOUT,
            client.batch_execute("SET statement_timeout = '2s'"),
        )
        .await
        .context("central PostgreSQL statement timeout setup timed out")??;
        Ok(Arc::new(client))
    }

    async fn client(&self) -> Result<Arc<tokio_postgres::Client>> {
        let current = self.client.lock().await.clone();
        if let Some(client) = current
            && tokio::time::timeout(DB_TIMEOUT, client.simple_query("SELECT 1"))
                .await
                .is_ok_and(|result| result.is_ok())
        {
            return Ok(client);
        }
        let client = tokio::time::timeout(DB_TIMEOUT, self.establish())
            .await
            .context("central PostgreSQL connect timed out")??;
        *self.client.lock().await = Some(client.clone());
        Ok(client)
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<bool> {
        let client = self.client().await?;
        let changed = client
            .execute(
                "UPDATE account_refresh_leases SET expires_at=now()+($3::bigint * interval '1 second') WHERE account_id=$1 AND holder_id=$2 AND epoch=$4 AND expires_at > now()",
                &[&lease.account_id, &lease.holder_id, &(ttl.as_secs() as i64), &lease.epoch],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn release_lease(&self, lease: &Lease) -> Result<bool> {
        let client = self.client().await?;
        let changed = client.execute(
            "DELETE FROM account_refresh_leases WHERE account_id=$1 AND holder_id=$2 AND epoch=$3",
            &[&lease.account_id, &lease.holder_id, &lease.epoch],
        ).await?;
        Ok(changed == 1)
    }

    async fn migrate(&self) -> Result<()> {
        let client = self.client().await?;
        client
            .batch_execute(SCHEMA)
            .await
            .context("migrate central PostgreSQL schema")?;
        client
            .execute(
                "INSERT INTO central_schema_migrations(version) VALUES (1) ON CONFLICT DO NOTHING",
                &[],
            )
            .await?;
        Ok(())
    }

    async fn save_account(&self, record: &CredentialRecord) -> Result<()> {
        let encrypted = vault::encrypt_bytes(&self.key, &serde_json::to_vec(&record.vault)?)?;
        let client = self.client().await?;
        client.execute(
            "INSERT INTO central_accounts(account_id,user_id,alias,workspace,login,encrypted_vault,revision) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(account_id) DO UPDATE SET user_id=EXCLUDED.user_id,alias=EXCLUDED.alias,workspace=EXCLUDED.workspace,login=EXCLUDED.login,encrypted_vault=EXCLUDED.encrypted_vault,revision=EXCLUDED.revision,updated_at=now() WHERE central_accounts.revision < EXCLUDED.revision",
            &[&record.account_id, &record.user_id, &record.alias, &record.workspace, &record.login, &encrypted, &record.revision],
        ).await?;
        Ok(())
    }

    async fn save_registry(&self, name: &str, payload: &[u8]) -> Result<()> {
        let encrypted = vault::encrypt_bytes(&self.key, payload)?;
        let client = self.client().await?;
        client.execute("INSERT INTO central_registry(name,encrypted_payload) VALUES($1,$2) ON CONFLICT(name) DO UPDATE SET encrypted_payload=EXCLUDED.encrypted_payload,updated_at=now()", &[&name, &encrypted]).await?;
        Ok(())
    }

    async fn load_registry(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let client = self.client().await?;
        let Some(row) = client
            .query_opt(
                "SELECT encrypted_payload FROM central_registry WHERE name=$1",
                &[&name],
            )
            .await?
        else {
            return Ok(None);
        };
        let encrypted: Vec<u8> = row.get(0);
        Ok(Some(vault::decrypt_bytes(&self.key, &encrypted)?))
    }

    async fn load_account(&self, account_id: &str) -> Result<Option<CredentialRecord>> {
        let client = self.client().await?;
        let row = client.query_opt("SELECT user_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts WHERE account_id=$1", &[&account_id]).await?;
        row.map(|row| {
            let vault_bytes: Vec<u8> = row.get(4);
            Ok(CredentialRecord {
                account_id: account_id.into(),
                user_id: row.get(0),
                alias: row.get(1),
                workspace: row.get(2),
                login: row.get(3),
                vault: serde_json::from_slice(&vault::decrypt_bytes(&self.key, &vault_bytes)?)?,
                revision: row.get(5),
            })
        })
        .transpose()
    }

    async fn list_accounts(&self) -> Result<Vec<CredentialRecord>> {
        let client = self.client().await?;
        let rows = client.query("SELECT account_id,user_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts", &[]).await?;
        rows.into_iter()
            .map(|row| {
                let encrypted: Vec<u8> = row.get(5);
                Ok(CredentialRecord {
                    account_id: row.get(0),
                    user_id: row.get(1),
                    alias: row.get(2),
                    workspace: row.get(3),
                    login: row.get(4),
                    vault: serde_json::from_slice(&vault::decrypt_bytes(&self.key, &encrypted)?)?,
                    revision: row.get(6),
                })
            })
            .collect()
    }

    async fn acquire_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        ttl: Duration,
    ) -> Result<Lease> {
        let client = self.client().await?;
        let row = client.query_opt(
            "INSERT INTO account_refresh_leases(account_id,holder_id,epoch,expires_at) VALUES($1,$2,1,now()+($3::bigint * interval '1 second')) ON CONFLICT(account_id) DO UPDATE SET holder_id=EXCLUDED.holder_id,epoch=account_refresh_leases.epoch+1,expires_at=EXCLUDED.expires_at WHERE account_refresh_leases.expires_at <= now() OR account_refresh_leases.holder_id=EXCLUDED.holder_id RETURNING epoch",
            &[&account_id, &holder_id, &(ttl.as_secs() as i64)],
        ).await?;
        let epoch: i64 = row
            .context("refresh lease is held by another instance")?
            .get(0);
        Ok(Lease {
            account_id: account_id.into(),
            holder_id: holder_id.into(),
            epoch,
        })
    }

    async fn fenced_write(&self, lease: &Lease, record: &CredentialRecord) -> Result<bool> {
        if lease.account_id != record.account_id {
            bail!("lease account does not match credential account")
        }
        let encrypted = vault::encrypt_bytes(&self.key, &serde_json::to_vec(&record.vault)?)?;
        let client = self.client().await?;
        let changed = client.execute(
            "UPDATE central_accounts SET user_id=$2,alias=$3,workspace=$4,login=$5,encrypted_vault=$6,revision=$7,updated_at=now() FROM account_refresh_leases WHERE central_accounts.account_id=$1 AND account_refresh_leases.account_id=$1 AND account_refresh_leases.holder_id=$8 AND account_refresh_leases.epoch=$9 AND account_refresh_leases.expires_at > now() AND central_accounts.revision < $7",
            &[&record.account_id, &record.user_id, &record.alias, &record.workspace, &record.login, &encrypted, &record.revision, &lease.holder_id, &lease.epoch],
        ).await?;
        if changed == 1 {
            return Ok(true);
        }
        let row = client.query_opt("SELECT 1 FROM central_accounts JOIN account_refresh_leases USING(account_id) WHERE central_accounts.account_id=$1 AND account_refresh_leases.holder_id=$2 AND account_refresh_leases.epoch=$3 AND account_refresh_leases.expires_at > now() AND central_accounts.revision=$4", &[&record.account_id, &lease.holder_id, &lease.epoch, &record.revision]).await?;
        Ok(row.is_some())
    }

    async fn create_enrollment(
        &self,
        challenge: &str,
        payload: &[u8],
        ttl: Duration,
    ) -> Result<()> {
        let hash = vault::digest(challenge.as_bytes());
        let encrypted = vault::encrypt_bytes(&self.key, payload)?;
        let client = self.client().await?;
        client.execute("INSERT INTO enrollment_challenges(challenge_hash,encrypted_payload,expires_at) VALUES($1,$2,now()+($3::bigint * interval '1 second')) ON CONFLICT(challenge_hash) DO UPDATE SET encrypted_payload=EXCLUDED.encrypted_payload,expires_at=EXCLUDED.expires_at,consumed_at=NULL", &[&hash, &encrypted, &(ttl.as_secs() as i64)]).await?;
        Ok(())
    }

    async fn consume_enrollment(&self, challenge: &str) -> Result<Option<Vec<u8>>> {
        let hash = vault::digest(challenge.as_bytes());
        let client = self.client().await?;
        let row = client.query_opt("UPDATE enrollment_challenges SET consumed_at=now() WHERE challenge_hash=$1 AND consumed_at IS NULL AND expires_at > now() RETURNING encrypted_payload", &[&hash]).await?;
        row.map(|row| {
            let encrypted: Vec<u8> = row.get(0);
            vault::decrypt_bytes(&self.key, &encrypted)
        })
        .transpose()
    }
}

fn tls_connector() -> Result<tokio_postgres_rustls::MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Ok(path) = std::env::var("CODEXCTL_CENTRAL_DB_CA_FILE") {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("read CODEXCTL_CENTRAL_DB_CA_FILE {path:?}"))?;
        let reader = BufReader::new(file);
        use rustls::pki_types::pem::PemObject;
        for certificate in rustls::pki_types::CertificateDer::pem_reader_iter(reader) {
            let certificate = certificate.context("parse central PostgreSQL CA bundle")?;
            roots
                .add(certificate)
                .context("add central PostgreSQL CA certificate")?;
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Open the configured runtime store. PostgreSQL remains deliberately refused;
/// dual mode needs an explicit acknowledgement because it changes the source
/// of truth for shared state.
pub async fn runtime_store(state: &Path, key: &Path) -> Result<CentralStore> {
    match StoreMode::from_env()? {
        StoreMode::File => CentralStore::from_mode(StoreMode::File, state, key).await,
        StoreMode::Postgres => {
            bail!("central storage mode postgres remains disabled until a later phase")
        }
        StoreMode::Dual => {
            if std::env::var("CODEXCTL_CENTRAL_DUAL_ACK").ok().as_deref() != Some("1") {
                bail!("dual central storage requires CODEXCTL_CENTRAL_DUAL_ACK=1")
            }
            let store = CentralStore::from_mode(StoreMode::Dual, state, key).await?;
            store.migrate().await?;
            Ok(store)
        }
    }
}

pub async fn maybe_migrate(_state: &Path, _key: &Path) -> Result<()> {
    match StoreMode::from_env()? {
        StoreMode::File => Ok(()),
        mode => bail!("prototype central server supports file storage only (requested {mode})"),
    }
}

/// Apply the configured schema migration. This is intentionally separate from
/// server startup while the phase 1 runtime still uses file storage.
pub async fn migrate(state: &Path, key: &Path) -> Result<()> {
    let store = CentralStore::from_env(state, key).await?;
    store.migrate().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, revision: i64) -> CredentialRecord {
        CredentialRecord {
            account_id: id.into(),
            user_id: Some("user".into()),
            alias: "seat".into(),
            workspace: Some("workspace".into()),
            login: Some("login".into()),
            vault: serde_json::json!({"refresh":"secret"}),
            revision,
        }
    }

    #[tokio::test]
    async fn file_store_fences_stale_lease_and_consumes_challenge_once() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[3; 32]).unwrap();
        let store = CentralStore::File(FileStore {
            state: root.path().into(),
            key: key.clone(),
        });
        store.migrate().await.unwrap();
        store.save_account(&record("a", 1)).await.unwrap();
        let first = store
            .acquire_lease("a", "one", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(store.renew(&first, Duration::from_secs(120)).await.unwrap());
        assert!(store.fenced_write(&first, &record("a", 2)).await.unwrap());
        let bumped = store
            .acquire_lease("a", "one", Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(bumped.epoch, first.epoch + 1);
        let second = store
            .acquire_lease("a", "two", Duration::from_secs(60))
            .await;
        assert!(second.is_err());
        store
            .create_enrollment("challenge", b"payload", Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            store.consume_enrollment("challenge").await.unwrap(),
            Some(b"payload".to_vec())
        );
        assert_eq!(store.consume_enrollment("challenge").await.unwrap(), None);
    }

    #[tokio::test]
    async fn two_file_instances_have_one_live_lease_holder() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[4; 32]).unwrap();
        let first = CentralStore::File(FileStore {
            state: root.path().into(),
            key: key.clone(),
        });
        let second = first.clone();
        first.migrate().await.unwrap();
        first
            .save_account(&record("lease-account", 1))
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            first.acquire_lease("lease-account", "pod-a:boot-a", Duration::from_secs(60)),
            second.acquire_lease("lease-account", "pod-b:boot-b", Duration::from_secs(60)),
        );
        assert!(
            left.is_ok() ^ right.is_ok(),
            "exactly one instance may hold a live lease"
        );
    }

    #[tokio::test]
    async fn releasing_a_lease_allows_a_restarted_holder_to_refresh_immediately() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[7; 32]).unwrap();
        let first = CentralStore::File(FileStore {
            state: root.path().into(),
            key: key.clone(),
        });
        let restarted = first.clone();
        first.migrate().await.unwrap();
        first
            .save_account(&record("restart-account", 1))
            .await
            .unwrap();
        let lease = first
            .acquire_lease("restart-account", "pod-a:boot-a", Duration::from_secs(120))
            .await
            .unwrap();
        assert!(first.release_lease(&lease).await.unwrap());
        let replacement = restarted
            .acquire_lease("restart-account", "pod-b:boot-b", Duration::from_secs(120))
            .await
            .unwrap();
        assert_eq!(replacement.holder_id, "pod-b:boot-b");
    }

    #[tokio::test]
    async fn distinct_owner_keys_preserve_two_users_in_one_workspace() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[5; 32]).unwrap();
        let store = CentralStore::File(FileStore {
            state: root.path().into(),
            key: key.clone(),
        });
        store.migrate().await.unwrap();
        let mut first = record("user-a/seat", 10);
        first.user_id = Some("user-a".into());
        first.workspace = Some("shared-workspace".into());
        let mut second = record("user-b/seat", 10);
        second.user_id = Some("user-b".into());
        second.workspace = Some("shared-workspace".into());
        store.save_account(&first).await.unwrap();
        store.save_account(&second).await.unwrap();
        assert!(store.load_account("user-a/seat").await.unwrap().is_some());
        assert!(store.load_account("user-b/seat").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn credential_revision_rejects_a_skewed_unleased_write() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[6; 32]).unwrap();
        let store = CentralStore::File(FileStore {
            state: root.path().into(),
            key: key.clone(),
        });
        store.migrate().await.unwrap();
        store.save_account(&record("skewed", 200)).await.unwrap();
        store.save_account(&record("skewed", 100)).await.unwrap();
        assert_eq!(
            store
                .load_account("skewed")
                .await
                .unwrap()
                .unwrap()
                .revision,
            200
        );
    }

    #[cfg(feature = "central-real-db-tests")]
    #[tokio::test]
    async fn postgres_real_store_scenarios() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        if std::env::var("DATABASE_URL").is_err() {
            if std::env::var("CI").ok().as_deref() == Some("true") {
                panic!("DATABASE_URL must be set for PostgreSQL scenarios in CI");
            }
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[9; 32]).unwrap();
        let first = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        first.migrate().await.unwrap();
        let second = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let id = format!(
            "test-{}",
            vault::digest(&crate::central::enrollment::random_bytes())
        );
        first.save_account(&record(&id, 1)).await.unwrap();
        let (a, b) = tokio::join!(
            first.acquire_lease(&id, "a", Duration::from_secs(60)),
            second.acquire_lease(&id, "b", Duration::from_secs(60))
        );
        assert!(a.is_ok() ^ b.is_ok());
        let upstream_refreshes = AtomicUsize::new(0);
        let refreshed = |counter: &AtomicUsize| {
            counter.fetch_add(1, Ordering::Relaxed);
            record(&id, 2)
        };
        let (lease, refreshed_record) = match (a, b) {
            (Ok(lease), Err(_)) => (lease, refreshed(&upstream_refreshes)),
            (Err(_), Ok(lease)) => (lease, refreshed(&upstream_refreshes)),
            _ => unreachable!("lease race must have exactly one winner"),
        };
        assert_eq!(upstream_refreshes.load(Ordering::Relaxed), 1);
        assert!(first.fenced_write(&lease, &refreshed_record).await.unwrap());
        assert!(first.renew(&lease, Duration::from_secs(120)).await.unwrap());
        assert!(first.fenced_write(&lease, &record(&id, 3)).await.unwrap());
        let renewed = first
            .acquire_lease(&id, &lease.holder_id, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(renewed.epoch, lease.epoch + 1);
        let stale = Lease {
            epoch: lease.epoch - 1,
            ..lease.clone()
        };
        assert!(!second.fenced_write(&stale, &record(&id, 4)).await.unwrap());
        first
            .create_enrollment(&id, b"once", Duration::from_secs(60))
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            first.consume_enrollment(&id),
            second.consume_enrollment(&id)
        );
        assert_eq!(
            left.unwrap().is_some() as u8 + right.unwrap().is_some() as u8,
            1
        );
        if let CentralStore::Postgres(db) = &first {
            let client = db.client().await.unwrap();
            client
                .execute(
                    "DELETE FROM enrollment_challenges WHERE challenge_hash=$1",
                    &[&vault::digest(id.as_bytes())],
                )
                .await
                .unwrap();
            client
                .execute(
                    "DELETE FROM account_refresh_leases WHERE account_id=$1",
                    &[&id],
                )
                .await
                .unwrap();
            client
                .execute("DELETE FROM central_accounts WHERE account_id=$1", &[&id])
                .await
                .unwrap();
        }
    }
}
