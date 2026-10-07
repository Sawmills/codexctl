//! Durable storage for the central account broker.
//!
//! File storage remains the default. PostgreSQL is authoritative when
//! `CODEXCTL_CENTRAL_STORE=postgres`; dual mode is an explicit migration
//! compatibility mode. The
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

pub(in crate::central) mod login;

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
    observation_client: Arc<tokio::sync::Mutex<Option<Arc<tokio_postgres::Client>>>>,
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
    deleted_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE central_accounts ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
CREATE TABLE IF NOT EXISTS account_refresh_leases (
    account_id TEXT PRIMARY KEY REFERENCES central_accounts(account_id) ON DELETE CASCADE,
    holder_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS account_live_sessions (
    account_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    device_id TEXT NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (account_id, device_id)
);
ALTER TABLE account_live_sessions ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM pg_constraint
        WHERE conrelid='account_live_sessions'::regclass
        AND conname='account_live_sessions_account_id_fkey' AND confdeltype='c') THEN
        ALTER TABLE account_live_sessions DROP CONSTRAINT account_live_sessions_account_id_fkey;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
        WHERE conrelid='account_live_sessions'::regclass
        AND conname='account_live_sessions_account_id_fkey') THEN
        ALTER TABLE account_live_sessions
            ADD CONSTRAINT account_live_sessions_account_id_fkey
            FOREIGN KEY (account_id) REFERENCES central_accounts(account_id);
    END IF;
END $$;
CREATE INDEX IF NOT EXISTS account_live_sessions_recent_idx
    ON account_live_sessions (account_id, last_seen) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS account_live_sessions_retention_idx
    ON account_live_sessions (account_id, last_seen);
CREATE TABLE IF NOT EXISTS enrollment_challenges (
    challenge_hash TEXT PRIMARY KEY,
    encrypted_payload BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS enrollment_challenges_expiry_idx ON enrollment_challenges (expires_at);
CREATE TABLE IF NOT EXISTS central_users (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    enabled BOOLEAN NOT NULL,
    oidc_identity TEXT,
    revision BIGINT NOT NULL DEFAULT 0,
    deleted_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS central_devices (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    user_id TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    revoked BOOLEAN NOT NULL,
    revision BIGINT NOT NULL DEFAULT 0,
    deleted_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS central_devices_authorize_idx
    ON central_devices (tenant, token_hash) WHERE deleted_at IS NULL AND revoked = false;
CREATE INDEX IF NOT EXISTS central_users_enabled_idx
    ON central_users (id) WHERE deleted_at IS NULL AND enabled = true;
"#;

impl CentralStore {
    #[cfg(all(test, feature = "central-real-db-tests"))]
    pub(super) async fn settle_test_observations(&self) -> Result<()> {
        if let Self::Postgres(db) | Self::Dual { postgres: db, .. } = self {
            let client = db.cached_client(&db.observation_client).await?;
            tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await??;
        }
        Ok(())
    }

    #[cfg(all(test, feature = "central-real-db-tests"))]
    pub(super) async fn isolated_test_schema(
        &self,
    ) -> Result<(Self, Arc<tokio_postgres::Client>, String)> {
        let mut isolated = self.clone();
        let db = match &mut isolated {
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => db,
            Self::File(_) => bail!("PostgreSQL required for isolated schema"),
        };
        let schema = format!(
            "sessions_{}",
            &vault::digest(&super::enrollment::random_bytes())[..16]
        );
        db.client()
            .await?
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await?;
        let mut url = reqwest::Url::parse(&db.url)?;
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        db.url = url.to_string();
        db.client = Arc::new(tokio::sync::Mutex::new(None));
        db.observation_client = Arc::new(tokio::sync::Mutex::new(None));
        let control = db.establish().await?;
        Ok((isolated, control, schema))
    }

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
        if payload.len() > 1024 * 1024 {
            bail!("central registry payload exceeds the 1 MiB bound");
        }
        match self {
            Self::File(file) => file.save_registry(name, payload),
            Self::Postgres(_) | Self::Dual { .. } => {
                bail!("generic registry blobs are unsupported; use typed entities")
            }
        }
    }

    pub async fn load_registry(&self, _name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(_) => Ok(None),
            Self::Postgres(_) | Self::Dual { .. } => Ok(None),
        }
    }

    /// Read typed registry entities. PostgreSQL is authoritative in shared mode.
    pub async fn load_registry_entities(&self, name: &str) -> Result<Vec<Vec<u8>>> {
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.load_registry_entities(name)).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.load_registry_entities(name)).await,
        }
    }

    /// Authorize against typed, indexed PostgreSQL rows. The encrypted legacy
    /// registry blob is never used as an authorization source in shared mode.
    pub async fn authorized_device(&self, token_hash: &str) -> Result<Option<vault::Device>> {
        match self {
            Self::File(_) => Ok(None),
            Self::Postgres(db) => bounded_db(db.authorized_device(token_hash)).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.authorized_device(token_hash)).await,
        }
    }

    pub async fn enabled_user(&self, id: &str) -> Result<bool> {
        match self {
            Self::File(_) => Ok(false),
            Self::Postgres(db) => bounded_db(db.enabled_user(id)).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.enabled_user(id)).await,
        }
    }

    /// Read registry entities together with their per-entity revision.  The
    /// revision is a compare-and-swap token for administrative mutations.
    pub async fn load_registry_entity_revisions(
        &self,
        name: &str,
    ) -> Result<Vec<(String, Vec<u8>, i64)>> {
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.load_registry_entity_revisions(name)).await,
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.load_registry_entity_revisions(name)).await
            }
        }
    }

    /// Read device entities for one authenticated tenant only. Administrative
    /// startup snapshots may enumerate all devices, but request handlers use
    /// this scoped path before listing or mutating a device.
    pub async fn load_device_entity_revisions(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<u8>, i64)>> {
        crate::store::validate_alias(tenant)?;
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.load_device_entity_revisions(tenant)).await,
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.load_device_entity_revisions(tenant)).await
            }
        }
    }

    pub async fn load_device_entity_revision(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<(String, Vec<u8>, i64)>> {
        crate::store::validate_alias(tenant)?;
        crate::store::validate_alias(entity_id)?;
        match self {
            Self::File(_) => Ok(None),
            Self::Postgres(db) => {
                bounded_db(db.load_device_entity_revision(tenant, entity_id)).await
            }
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.load_device_entity_revision(tenant, entity_id)).await
            }
        }
    }

    /// Atomically upsert each registry entity in one SQL statement. This keeps
    /// a stale pod from replacing the shared users/devices set with its local
    /// snapshot and gives every entity its own revision.
    pub async fn save_registry_entities(
        &self,
        name: &str,
        entries: &[(String, Vec<u8>)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if entries
            .iter()
            .any(|(_, payload)| payload.len() > 1024 * 1024)
        {
            bail!("central registry entity exceeds the 1 MiB bound");
        }
        match self {
            Self::File(_) => Ok(()),
            Self::Postgres(db) => bounded_db(db.save_registry_entities(name, entries)).await,
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.save_registry_entities(name, entries)).await
            }
        }
    }

    /// Update one entity without replacing a stale registry snapshot.  A
    /// missing entity is inserted; an existing entity is replaced only when
    /// `expected_revision` still matches.
    pub async fn save_registry_entity_cas(
        &self,
        name: &str,
        entity_id: &str,
        payload: &[u8],
        expected_revision: Option<i64>,
    ) -> Result<bool> {
        if payload.len() > 1024 * 1024 {
            bail!("central registry entity exceeds the 1 MiB bound");
        }
        match self {
            Self::File(_) => Ok(true),
            Self::Postgres(db) => {
                bounded_db(db.save_registry_entity_cas(name, entity_id, payload, expected_revision))
                    .await
            }
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.save_registry_entity_cas(
                    name,
                    entity_id,
                    payload,
                    expected_revision,
                ))
                .await
            }
        }
    }

    pub async fn save_device_entity_cas(
        &self,
        tenant: &str,
        entity_id: &str,
        payload: &[u8],
        expected_revision: Option<i64>,
    ) -> Result<bool> {
        crate::store::validate_alias(tenant)?;
        crate::store::validate_alias(entity_id)?;
        if payload.len() > 1024 * 1024 {
            bail!("central registry entity exceeds the 1 MiB bound");
        }
        match self {
            Self::File(_) => Ok(true),
            Self::Postgres(db) => {
                bounded_db(db.save_device_entity_cas(tenant, entity_id, payload, expected_revision))
                    .await
            }
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.save_device_entity_cas(
                    tenant,
                    entity_id,
                    payload,
                    expected_revision,
                ))
                .await
            }
        }
    }

    pub async fn backfill(&self, state: &Path, key: &Path) -> Result<BackfillCounts> {
        let target = match self {
            Self::Postgres(db) => db,
            Self::Dual { postgres, .. } => postgres,
            Self::File(_) => bail!("backfill requires PostgreSQL or dual central storage"),
        };
        if super::relogin::add::pending_logins(state)? {
            bail!("finish pending logins in file mode before switching storage");
        }
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
                let mut value = crate::central::vault::load(&account_state, key)?;
                let workspace = crate::central::vault::account(&value.auth)?;
                let login = crate::central::vault::token(&value.auth)
                    .ok()
                    .and_then(crate::api::token_subject);
                let revision = value.revision.max(1);
                value.revision = revision;
                let record = CredentialRecord {
                    account_id: crate::central::managed::account_key(&value.user, &value.alias),
                    user_id: Some(value.user.clone()),
                    alias: value.alias.clone(),
                    workspace: Some(workspace),
                    login,
                    vault: serde_json::to_value(value)?,
                    revision,
                };
                if target
                    .load_account(&record.account_id)
                    .await?
                    .is_some_and(|existing| existing.revision >= record.revision)
                {
                    continue;
                }
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
                if name == "users" {
                    let users =
                        serde_json::from_slice::<Vec<crate::central::managed::User>>(&bytes)?;
                    for user in &users {
                        target
                            .save_registry_entity_cas(
                                "users",
                                &user.id,
                                &serde_json::to_vec(user)?,
                                None,
                            )
                            .await?;
                    }
                    counts.users = users.len();
                } else {
                    let devices =
                        serde_json::from_slice::<Vec<crate::central::vault::Device>>(&bytes)?;
                    for device in &devices {
                        target
                            .save_device_entity_cas(
                                &device.tenant,
                                &device.id,
                                &serde_json::to_vec(device)?,
                                None,
                            )
                            .await?;
                    }
                    counts.devices = devices.len();
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
            Self::Postgres(db) => bounded_db(db.load_account(account_id)).await,
            Self::Dual { file, postgres, .. } => Ok(bounded_db(postgres.load_account(account_id))
                .await?
                .or(file.load_account(account_id)?)),
        }
    }

    pub async fn load_account_by_alias(
        &self,
        user_id: &str,
        alias: &str,
    ) -> Result<Option<CredentialRecord>> {
        match self {
            Self::File(_) => Ok(None),
            Self::Postgres(db) => bounded_db(db.load_account_by_alias(user_id, alias)).await,
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.load_account_by_alias(user_id, alias)).await
            }
        }
    }

    pub async fn list_accounts(&self) -> Result<Vec<CredentialRecord>> {
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.list_accounts()).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.list_accounts()).await,
        }
    }

    pub async fn list_account_aliases(&self, user_id: &str) -> Result<Vec<String>> {
        match self {
            Self::File(_) => Ok(Vec::new()),
            Self::Postgres(db) => bounded_db(db.list_account_aliases(user_id)).await,
            Self::Dual { postgres, .. } => bounded_db(postgres.list_account_aliases(user_id)).await,
        }
    }

    pub async fn record_live_session(
        &self,
        account_id: &str,
        user_id: &str,
        alias: &str,
        device_id: &str,
    ) -> Result<()> {
        match self {
            Self::File(file) => file.record_live_session(account_id, user_id, alias, device_id),
            Self::Postgres(db) => {
                bounded_db(db.record_live_session(account_id, user_id, alias, device_id)).await
            }
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.record_live_session(account_id, user_id, alias, device_id))
                    .await
            }
        }
    }

    pub(super) async fn prune_live_sessions(&self, account_id: &str, user_id: &str) -> Result<()> {
        match self {
            Self::File(_) => Ok(()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.prune_live_sessions(account_id, user_id)).await
            }
        }
    }

    pub async fn live_session_count(&self, account_id: &str, window: Duration) -> Result<usize> {
        match self {
            Self::File(file) => file.live_session_count(account_id, window),
            Self::Postgres(db) => bounded_db(db.live_session_count(account_id, window)).await,
            Self::Dual { postgres, .. } => {
                bounded_db(postgres.live_session_count(account_id, window)).await
            }
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
            Self::Postgres(db) => bounded_db(db.create_enrollment(challenge, payload, ttl)).await,
            Self::Dual { file, postgres, .. } => {
                file.create_enrollment(challenge, payload, ttl)?;
                bounded_db(postgres.create_enrollment(challenge, payload, ttl)).await
            }
        }
    }

    /// Atomically consume a challenge. Exactly one concurrent caller receives
    /// the payload; expired and already-consumed challenges return `None`.
    pub async fn consume_enrollment(&self, challenge: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::File(file) => file.consume_enrollment(challenge),
            Self::Postgres(db) => bounded_db(db.consume_enrollment(challenge)).await,
            Self::Dual {
                file,
                postgres,
                mirror_failures,
            } => {
                let value = bounded_db(postgres.consume_enrollment(challenge)).await?;
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

    fn record_live_session(
        &self,
        _account_id: &str,
        _user_id: &str,
        _alias: &str,
        _device_id: &str,
    ) -> Result<()> {
        Ok(())
    }

    fn live_session_count(&self, _account_id: &str, _window: Duration) -> Result<usize> {
        Ok(0)
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
            state
                .leases
                .get_mut(&lease.account_id)
                .expect("lease checked above")
                .expires_at = 0;
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
                .is_some_and(|old| old.revision == record.revision && old.vault == record.vault));
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
        let url = std::env::var("DATABASE_URL").unwrap_or_default();
        let tls_enabled = std::env::var("CODEXCTL_CENTRAL_DB_TLS")
            .map(|value| value != "0" && value != "false" && value != "disable")
            .unwrap_or(true);
        let store = Self {
            url,
            tls_enabled,
            client: Arc::new(tokio::sync::Mutex::new(None)),
            observation_client: Arc::new(tokio::sync::Mutex::new(None)),
            key: key.into(),
        };
        let _ = store.client().await?;
        Ok(store)
    }

    async fn establish(&self) -> Result<Arc<tokio_postgres::Client>> {
        let mut config =
            if self.url.is_empty() {
                let mut config = tokio_postgres::Config::new();
                config.host(&std::env::var("DB_HOST").context(
                    "DATABASE_URL or DB_HOST is required for PostgreSQL central storage",
                )?);
                config.port(
                    std::env::var("DB_PORT")
                        .context("DB_PORT is required when DATABASE_URL is unset")?
                        .parse()
                        .context("invalid DB_PORT for PostgreSQL central storage")?,
                );
                config.dbname(
                    &std::env::var("DB_NAME")
                        .context("DB_NAME is required when DATABASE_URL is unset")?,
                );
                config.user(
                    &std::env::var("DB_USER")
                        .context("DB_USER is required when DATABASE_URL is unset")?,
                );
                config.password(
                    std::env::var("DB_PASSWORD")
                        .context("DB_PASSWORD is required when DATABASE_URL is unset")?,
                );
                config
            } else {
                self.url
                    .parse()
                    .context("invalid DATABASE_URL for PostgreSQL central storage")?
            };
        config.connect_timeout(DB_TIMEOUT);
        config.keepalives(true);
        config.keepalives_idle(Duration::from_secs(10));
        let client = if self.tls_enabled {
            if !self.url.is_empty()
                && config.get_ssl_mode() != tokio_postgres::config::SslMode::Require
            {
                bail!(
                    "TLS is enabled for central PostgreSQL; DATABASE_URL must set sslmode=require"
                );
            }
            config.ssl_mode(tokio_postgres::config::SslMode::Require);
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
        self.cached_client(&self.client).await
    }

    async fn cached_client(
        &self,
        cache: &tokio::sync::Mutex<Option<Arc<tokio_postgres::Client>>>,
    ) -> Result<Arc<tokio_postgres::Client>> {
        let mut slot = cache.lock().await;
        if let Some(client) = slot.as_ref().filter(|client| !client.is_closed()) {
            return Ok(client.clone());
        }
        *slot = None;
        drop(slot);
        let client = tokio::time::timeout(Duration::from_secs(5), self.establish())
            .await
            .context("central PostgreSQL connect timed out")??;
        let mut slot = cache.lock().await;
        *slot = Some(client.clone());
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
            "UPDATE account_refresh_leases SET expires_at=now() WHERE account_id=$1 AND holder_id=$2 AND epoch=$3",
            &[&lease.account_id, &lease.holder_id, &lease.epoch],
        ).await?;
        Ok(changed == 1)
    }

    async fn migrate(&self) -> Result<()> {
        let client = self.client().await?;
        client
            .batch_execute(&format!("{SCHEMA}{}", login::LOGIN_SCHEMA))
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
        let serialized = serde_json::to_vec(&record.vault)?;
        let encrypted = vault::encrypt_bytes(&self.key, &serialized)?;
        let client = self.client().await?;
        let changed = client.execute(
            "INSERT INTO central_accounts(account_id,user_id,alias,workspace,login,encrypted_vault,revision) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(account_id) DO UPDATE SET user_id=EXCLUDED.user_id,alias=EXCLUDED.alias,workspace=EXCLUDED.workspace,login=EXCLUDED.login,encrypted_vault=EXCLUDED.encrypted_vault,revision=EXCLUDED.revision,updated_at=now() WHERE central_accounts.revision < EXCLUDED.revision AND central_accounts.deleted_at IS NULL",
            &[&record.account_id, &record.user_id, &record.alias, &record.workspace, &record.login, &encrypted, &record.revision],
        ).await?;
        if changed == 0 {
            let current = client
                .query_opt(
                    "SELECT encrypted_vault,deleted_at IS NOT NULL FROM central_accounts WHERE account_id=$1",
                    &[&record.account_id],
                )
                .await?;
            if current.as_ref().is_some_and(|row| row.get::<_, bool>(1)) {
                bail!("central account is tombstoned");
            }
            if current.is_none_or(|row| {
                let stored: Vec<u8> = row.get(0);
                vault::decrypt_bytes(&self.key, &stored)
                    .ok()
                    .is_some_and(|plain| plain == serialized)
            }) {
                return Ok(());
            }
            bail!("central account write was fenced or tombstoned");
        }
        Ok(())
    }

    async fn authorized_device(&self, token_hash: &str) -> Result<Option<vault::Device>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT id,tenant,user_id,token_hash,revoked FROM central_devices WHERE tenant=$1 AND token_hash=$2 AND revoked=false AND deleted_at IS NULL",
                &[&"sawmills", &token_hash],
            )
            .await?;
        Ok(row.map(|row| vault::Device {
            id: row.get(0),
            tenant: row.get(1),
            user: row.get(2),
            token_hash: row.get(3),
            revoked: row.get(4),
        }))
    }

    async fn enabled_user(&self, id: &str) -> Result<bool> {
        let client = self.client().await?;
        Ok(client
            .query_opt(
                "SELECT 1 FROM central_users WHERE id=$1 AND enabled=true AND deleted_at IS NULL",
                &[&id],
            )
            .await?
            .is_some())
    }

    async fn load_registry_entities(&self, name: &str) -> Result<Vec<Vec<u8>>> {
        let client = self.client().await?;
        if name == "users" {
            let rows = client
                .query("SELECT id,email,enabled,oidc_identity FROM central_users WHERE deleted_at IS NULL ORDER BY id", &[])
                .await?;
            return rows
                .into_iter()
                .map(|row| {
                    Ok(serde_json::to_vec(&crate::central::managed::User {
                        id: row.get(0),
                        email: row.get(1),
                        enabled: row.get(2),
                        oidc_identity: row.get(3),
                    })?)
                })
                .collect();
        }
        bail!("unsupported registry entity kind: {name}; use typed entities")
    }

    async fn load_registry_entity_revisions(
        &self,
        name: &str,
    ) -> Result<Vec<(String, Vec<u8>, i64)>> {
        let client = self.client().await?;
        if name == "users" {
            let rows = client
                .query("SELECT id,email,enabled,oidc_identity,revision FROM central_users WHERE deleted_at IS NULL ORDER BY id", &[])
                .await?;
            return rows
                .into_iter()
                .map(|row| {
                    Ok((
                        row.get(0),
                        serde_json::to_vec(&crate::central::managed::User {
                            id: row.get(0),
                            email: row.get(1),
                            enabled: row.get(2),
                            oidc_identity: row.get(3),
                        })?,
                        row.get(4),
                    ))
                })
                .collect();
        }
        bail!("unsupported registry entity kind: {name}; use typed entities")
    }

    async fn load_device_entity_revisions(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<u8>, i64)>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT id,tenant,user_id,token_hash,revoked,revision FROM central_devices WHERE tenant=$1 AND deleted_at IS NULL ORDER BY id",
                &[&tenant],
            )
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.get(0),
                    serde_json::to_vec(&vault::Device {
                        id: row.get(0),
                        tenant: row.get(1),
                        user: row.get(2),
                        token_hash: row.get(3),
                        revoked: row.get(4),
                    })?,
                    row.get(5),
                ))
            })
            .collect()
    }

    async fn load_device_entity_revision(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<(String, Vec<u8>, i64)>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT id,tenant,user_id,token_hash,revoked,revision FROM central_devices WHERE tenant=$1 AND id=$2 AND deleted_at IS NULL",
                &[&tenant, &entity_id],
            )
            .await?;
        row.map(|row| {
            Ok((
                row.get(0),
                serde_json::to_vec(&vault::Device {
                    id: row.get(0),
                    tenant: row.get(1),
                    user: row.get(2),
                    token_hash: row.get(3),
                    revoked: row.get(4),
                })?,
                row.get(5),
            ))
        })
        .transpose()
    }

    async fn save_registry_entities(
        &self,
        name: &str,
        entries: &[(String, Vec<u8>)],
    ) -> Result<()> {
        if name == "users" {
            for (id, payload) in entries {
                let inserted = self
                    .save_registry_entity_cas(name, id, payload, None)
                    .await?;
                if !inserted {
                    bail!("registry entity already exists: {name}/{id}");
                }
            }
            return Ok(());
        }
        let _ = entries;
        bail!("unsupported registry entity kind: {name}; use typed entities")
    }

    async fn save_registry_entity_cas(
        &self,
        name: &str,
        entity_id: &str,
        payload: &[u8],
        expected_revision: Option<i64>,
    ) -> Result<bool> {
        if name == "users" {
            let user: crate::central::managed::User = serde_json::from_slice(payload)?;
            let client = self.client().await?;
            let changed = match expected_revision {
                None => client.execute(
                    "INSERT INTO central_users(id,email,enabled,oidc_identity,revision) VALUES($1,$2,$3,$4,0) ON CONFLICT(id) DO NOTHING",
                    &[&user.id, &user.email, &user.enabled, &user.oidc_identity],
                ).await?,
                Some(revision) => client.execute(
                    "UPDATE central_users SET email=$2,enabled=$3,oidc_identity=$4,revision=revision+1,updated_at=now() WHERE id=$1 AND revision=$5 AND deleted_at IS NULL",
                    &[&user.id, &user.email, &user.enabled, &user.oidc_identity, &revision],
                ).await?,
            };
            return Ok(changed == 1);
        }
        let _ = (entity_id, payload, expected_revision);
        bail!("unsupported registry entity kind: {name}; use typed entities")
    }

    async fn save_device_entity_cas(
        &self,
        tenant: &str,
        entity_id: &str,
        payload: &[u8],
        expected_revision: Option<i64>,
    ) -> Result<bool> {
        let device: vault::Device = serde_json::from_slice(payload)?;
        if device.id != entity_id || device.tenant != tenant {
            bail!("device entity tenant or id does not match the scoped operation");
        }
        let client = self.client().await?;
        let changed = match expected_revision {
            None => client
                .execute(
                    "INSERT INTO central_devices(id,tenant,user_id,token_hash,revoked,revision) VALUES($2,$1,$3,$4,$5,0) ON CONFLICT(id) DO NOTHING",
                    &[&tenant, &device.id, &device.user, &device.token_hash, &device.revoked],
                )
                .await?,
            Some(revision) => client
                .execute(
                    "UPDATE central_devices SET user_id=$3,token_hash=$4,revoked=$5,revision=revision+1,updated_at=now() WHERE tenant=$1 AND id=$2 AND revision=$6 AND deleted_at IS NULL",
                    &[&tenant, &device.id, &device.user, &device.token_hash, &device.revoked, &revision],
                )
                .await?,
        };
        Ok(changed == 1)
    }

    async fn load_account(&self, account_id: &str) -> Result<Option<CredentialRecord>> {
        let client = self.client().await?;
        let row = client.query_opt("SELECT user_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts WHERE account_id=$1 AND deleted_at IS NULL", &[&account_id]).await?;
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
        let rows = client.query("SELECT account_id,user_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts WHERE deleted_at IS NULL ORDER BY account_id LIMIT 10001", &[]).await?;
        if rows.len() > 10_000 {
            bail!("central account hydration exceeds the 10000-account startup bound");
        }
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

    async fn list_account_aliases(&self, user_id: &str) -> Result<Vec<String>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT alias FROM central_accounts WHERE user_id=$1 AND deleted_at IS NULL ORDER BY alias LIMIT 10000",
                &[&user_id],
            )
            .await?;
        Ok(rows.into_iter().map(|row| row.get(0)).collect())
    }

    async fn record_live_session(
        &self,
        account_id: &str,
        user_id: &str,
        alias: &str,
        device_id: &str,
    ) -> Result<()> {
        // A blocked observation must not queue credential queries behind it.
        let client = self.cached_client(&self.observation_client).await?;
        client
            .execute(
                "INSERT INTO account_live_sessions(account_id,user_id,alias,device_id,last_seen,deleted_at) VALUES($1,$2,$3,$4,now(),NULL) ON CONFLICT(account_id,device_id) DO UPDATE SET user_id=EXCLUDED.user_id,alias=EXCLUDED.alias,last_seen=EXCLUDED.last_seen WHERE account_live_sessions.deleted_at IS NULL",
                &[&account_id, &user_id, &alias, &device_id],
            )
            .await?;
        Ok(())
    }

    async fn prune_live_sessions(&self, account_id: &str, user_id: &str) -> Result<()> {
        let client = self.cached_client(&self.observation_client).await?;
        // Reclaim ephemeral history after 24 hours, but keep recent deletion
        // evidence through the live window even when its last heartbeat is old.
        client.execute(
            "WITH expired AS (SELECT account_id,device_id FROM account_live_sessions WHERE account_id=$1 AND user_id=$2 AND last_seen < now()-interval '1 day' AND (deleted_at IS NULL OR deleted_at < now()-interval '10 minutes') ORDER BY last_seen LIMIT 1000 FOR UPDATE SKIP LOCKED) DELETE FROM account_live_sessions AS sessions USING expired WHERE sessions.account_id=expired.account_id AND sessions.device_id=expired.device_id AND sessions.user_id=$2",
            &[&account_id, &user_id],
        ).await?;
        Ok(())
    }

    async fn live_session_count(&self, account_id: &str, window: Duration) -> Result<usize> {
        let client = self.cached_client(&self.observation_client).await?;
        let row = client
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM account_live_sessions WHERE account_id=$1 AND deleted_at IS NULL AND last_seen >= now()-($2::bigint * interval '1 second')",
                &[&account_id, &(window.as_secs() as i64)],
            )
            .await?;
        let count: i64 = row.get(0);
        usize::try_from(count).context("live session count overflow")
    }

    async fn load_account_by_alias(
        &self,
        user_id: &str,
        alias: &str,
    ) -> Result<Option<CredentialRecord>> {
        let client = self.client().await?;
        let row = client
            .query_opt("SELECT account_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts WHERE user_id=$1 AND lower(alias)=lower($2) AND deleted_at IS NULL", &[&user_id, &alias])
            .await?;
        row.map(|row| {
            let encrypted: Vec<u8> = row.get(4);
            Ok(CredentialRecord {
                account_id: row.get(0),
                user_id: Some(user_id.into()),
                alias: row.get(1),
                workspace: row.get(2),
                login: row.get(3),
                vault: serde_json::from_slice(&vault::decrypt_bytes(&self.key, &encrypted)?)?,
                revision: row.get(5),
            })
        })
        .transpose()
    }

    async fn acquire_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        ttl: Duration,
    ) -> Result<Lease> {
        self.acquire_login_lease(account_id, holder_id, ttl, None)
            .await?
            .context("refresh lease is held by another instance")
    }

    async fn acquire_login_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        ttl: Duration,
        login_id: Option<&str>,
    ) -> Result<Option<Lease>> {
        let client = self.client().await?;
        let row = client.query_opt(
            "INSERT INTO account_refresh_leases(account_id,holder_id,epoch,expires_at) SELECT $1,$2,1,clock_timestamp()+($3::bigint * interval '1 second') WHERE NOT EXISTS (SELECT 1 FROM central_login_operations WHERE ((account_id=$1 AND selected_reserved) OR (candidate_workspace=(SELECT workspace FROM central_accounts WHERE account_id=$1) AND (candidate_login IS NULL OR (SELECT login FROM central_accounts WHERE account_id=$1) IS NULL OR candidate_login=(SELECT login FROM central_accounts WHERE account_id=$1)))) AND phase IN ('candidate','verifying','unresolved','rejected') AND ($4::text IS NULL OR (phase<>'rejected' AND (id<>$4 OR account_id<>$1)))) ON CONFLICT(account_id) DO UPDATE SET holder_id=EXCLUDED.holder_id,epoch=account_refresh_leases.epoch+1,expires_at=EXCLUDED.expires_at WHERE account_refresh_leases.expires_at <= clock_timestamp() OR account_refresh_leases.holder_id=EXCLUDED.holder_id RETURNING epoch",
            &[&account_id, &holder_id, &(ttl.as_secs() as i64), &login_id],
        ).await?;
        Ok(row.map(|row| Lease {
            account_id: account_id.into(),
            holder_id: holder_id.into(),
            epoch: row.get(0),
        }))
    }

    async fn fenced_write(&self, lease: &Lease, record: &CredentialRecord) -> Result<bool> {
        if lease.account_id != record.account_id {
            bail!("lease account does not match credential account")
        }
        let serialized = serde_json::to_vec(&record.vault)?;
        let encrypted = vault::encrypt_bytes(&self.key, &serialized)?;
        let client = self.client().await?;
        let changed = client.execute(
            "WITH valid_lease AS (SELECT account_id FROM account_refresh_leases WHERE account_id=$1 AND holder_id=$8 AND epoch=$9 AND expires_at > now() FOR UPDATE) UPDATE central_accounts SET user_id=$2,alias=$3,workspace=$4,login=$5,encrypted_vault=$6,revision=$7,updated_at=now() WHERE central_accounts.account_id=$1 AND central_accounts.deleted_at IS NULL AND central_accounts.revision < $7 AND EXISTS (SELECT 1 FROM valid_lease WHERE valid_lease.account_id=central_accounts.account_id)",
            &[&record.account_id, &record.user_id, &record.alias, &record.workspace, &record.login, &encrypted, &record.revision, &lease.holder_id, &lease.epoch],
        ).await?;
        if changed == 1 {
            return Ok(true);
        }
        let row = client.query_opt("SELECT encrypted_vault FROM central_accounts JOIN account_refresh_leases USING(account_id) WHERE central_accounts.account_id=$1 AND central_accounts.deleted_at IS NULL AND account_refresh_leases.holder_id=$2 AND account_refresh_leases.epoch=$3 AND account_refresh_leases.expires_at > now() AND central_accounts.revision=$4", &[&record.account_id, &lease.holder_id, &lease.epoch, &record.revision]).await?;
        Ok(row.is_some_and(|row| {
            let stored: Vec<u8> = row.get(0);
            vault::decrypt_bytes(&self.key, &stored)
                .ok()
                .is_some_and(|plain| plain == serialized)
        }))
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

/// Open the configured runtime store. PostgreSQL is authoritative in phase 3;
/// dual mode remains an explicitly acknowledged migration compatibility mode.
pub async fn runtime_store(state: &Path, key: &Path) -> Result<CentralStore> {
    match StoreMode::from_env()? {
        StoreMode::File => CentralStore::from_mode(StoreMode::File, state, key).await,
        StoreMode::Postgres => CentralStore::from_mode(StoreMode::Postgres, state, key).await,
        StoreMode::Dual => {
            if std::env::var("CODEXCTL_CENTRAL_DUAL_ACK").ok().as_deref() != Some("1") {
                bail!("dual central storage requires CODEXCTL_CENTRAL_DUAL_ACK=1")
            }
            CentralStore::from_mode(StoreMode::Dual, state, key).await
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
        if let CentralStore::Postgres(db) = &first {
            let client = db.client().await.unwrap();
            let query = "SELECT oid FROM pg_constraint WHERE conrelid='account_live_sessions'::regclass AND conname='account_live_sessions_account_id_fkey'";
            let before: u32 = client.query_one(query, &[]).await.unwrap().get(0);
            first.migrate().await.unwrap();
            let after: u32 = client.query_one(query, &[]).await.unwrap().get(0);
            assert_eq!(
                before, after,
                "startup must not rebuild a valid live-session foreign key"
            );
        }

        let legacy_state = root.path().join("accounts").join("legacy-seat");
        crate::store::ensure_private_dir(&legacy_state).unwrap();
        let legacy_vault = crate::central::vault::Vault {
            alias: "legacy-seat".into(),
            tenant: "sawmills".into(),
            user: "legacy-user".into(),
            auth: serde_json::json!({"access_token":"legacy-access","account_id":"legacy-account"}),
            label: None,
            verified: false,
            import_rejected: false,
            revision: 0,
        };
        crate::central::vault::save(&legacy_state, &key, &legacy_vault).unwrap();
        let counts = first.backfill(root.path(), &key).await.unwrap();
        assert_eq!(counts.accounts, 1);
        let legacy_id = crate::central::managed::account_key("legacy-user", "legacy-seat");
        let imported = first.load_account(&legacy_id).await.unwrap().unwrap();
        assert_eq!(imported.revision, 1);
        assert_eq!(imported.vault["revision"], serde_json::json!(1));
        let repeated = first.backfill(root.path(), &key).await.unwrap();
        assert_eq!(repeated.accounts, 0);
        let second = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let id = format!(
            "test-{}",
            vault::digest(&crate::central::enrollment::random_bytes())
        );
        first.save_account(&record(&id, 1)).await.unwrap();
        first
            .record_live_session(&id, "user", "seat", "launch-a")
            .await
            .unwrap();
        first
            .record_live_session(&id, "user", "seat", "launch-b")
            .await
            .unwrap();
        first
            .record_live_session(&id, "user", "seat", "launch-a")
            .await
            .unwrap();
        assert_eq!(
            second
                .live_session_count(&id, Duration::from_secs(60))
                .await
                .unwrap(),
            2
        );
        if let CentralStore::Postgres(db) = &first {
            assert!(
                db.client()
                    .await
                    .unwrap()
                    .execute("DELETE FROM central_accounts WHERE account_id=$1", &[&id])
                    .await
                    .is_err(),
                "session rows must not cascade-delete with their account"
            );
            db.client().await.unwrap().execute(
                "UPDATE account_live_sessions SET deleted_at=now() WHERE account_id=$1 AND device_id='launch-a'",
                &[&id],
            ).await.unwrap();
        }
        first
            .record_live_session(&id, "user", "seat", "launch-a")
            .await
            .unwrap();
        assert_eq!(
            second
                .live_session_count(&id, Duration::from_secs(60))
                .await
                .unwrap(),
            1,
            "a routine heartbeat must not restore a tombstoned session"
        );
        if let CentralStore::Postgres(db) = &first {
            let client = db.client().await.unwrap();
            for (other_id, user) in [
                (format!("{id}-other-user"), "other-user"),
                (format!("{id}-other-account"), "user"),
            ] {
                first.save_account(&record(&other_id, 1)).await.unwrap();
                first
                    .record_live_session(&other_id, user, "seat", "old")
                    .await
                    .unwrap();
                client.execute("UPDATE account_live_sessions SET last_seen=now()-interval '2 days' WHERE account_id=$1", &[&other_id]).await.unwrap();
            }
            client.execute(
                "UPDATE account_live_sessions SET last_seen=now()-interval '2 days' WHERE account_id=$1",
                &[&id],
            ).await.unwrap();
            first
                .record_live_session(&id, "user", "seat", "launch-c")
                .await
                .unwrap();
            first.prune_live_sessions(&id, "wrong-user").await.unwrap();
            let count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM account_live_sessions WHERE account_id=$1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 3, "mismatched owner must not prune any rows");
            first.prune_live_sessions(&id, "user").await.unwrap();
            for other_id in [format!("{id}-other-user"), format!("{id}-other-account")] {
                let count: i64 = client
                    .query_one(
                        "SELECT COUNT(*) FROM account_live_sessions WHERE account_id=$1",
                        &[&other_id],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(
                    count, 1,
                    "retention must not delete another account's observations"
                );
            }
            let rows = client.query(
                "SELECT device_id,deleted_at IS NOT NULL FROM account_live_sessions WHERE account_id=$1 ORDER BY device_id",
                &[&id],
            ).await.unwrap();
            let retained: Vec<(String, bool)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            assert_eq!(
                retained,
                vec![("launch-a".into(), true), ("launch-c".into(), false)],
                "retention must remove expired live rows while preserving tombstones and fresh sessions"
            );
            first
                .record_live_session(&id, "user", "seat", "launch-a")
                .await
                .unwrap();
            assert_eq!(
                second
                    .live_session_count(&id, Duration::from_secs(60))
                    .await
                    .unwrap(),
                1
            );
            client.execute("UPDATE account_live_sessions SET deleted_at=now()-interval '11 minutes' WHERE account_id=$1 AND device_id='launch-a'", &[&id]).await.unwrap();
            first.prune_live_sessions(&id, "user").await.unwrap();
            let count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM account_live_sessions WHERE account_id=$1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(
                count, 1,
                "expired tombstones outside the live window must be reclaimed"
            );
            client.execute("INSERT INTO account_live_sessions(account_id,user_id,alias,device_id,last_seen) SELECT $1,'user','seat','expired-' || n,now()-interval '2 days' FROM generate_series(1,1005) n", &[&id]).await.unwrap();
            first.prune_live_sessions(&id, "user").await.unwrap();
            let count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM account_live_sessions WHERE account_id=$1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(
                count, 6,
                "one cleanup must reclaim at most 1,000 observations"
            );
            first.prune_live_sessions(&id, "user").await.unwrap();
            first
                .record_live_session(&id, "user", "seat", "launch-a")
                .await
                .unwrap();
            assert_eq!(
                first
                    .live_session_count(&id, Duration::from_secs(60))
                    .await
                    .unwrap(),
                2,
                "after physical expiry an authenticated same-ID heartbeat is a fresh observation"
            );
        }
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
        // A second broker must observe the committed credential revision before
        // it attempts another refresh, rather than using its startup snapshot.
        assert_eq!(second.load_account(&id).await.unwrap().unwrap().revision, 3);
        let registry_id = format!("registry-{}", vault::digest(id.as_bytes()));
        let device = vault::Device {
            id: registry_id.clone(),
            tenant: "sawmills".into(),
            user: "user".into(),
            token_hash: vault::digest(b"synthetic-device-token"),
            revoked: true,
        };
        let device_bytes = serde_json::to_vec(&device).unwrap();
        first
            .save_device_entity_cas("sawmills", &registry_id, &device_bytes, None)
            .await
            .unwrap();
        let seen = second
            .load_device_entity_revisions("sawmills")
            .await
            .unwrap();
        assert!(seen.iter().any(|(id, _, _)| id == &registry_id));
        let other_registry_id = format!("other-{}", vault::digest(id.as_bytes()));
        let other_device = vault::Device {
            id: other_registry_id.clone(),
            tenant: "other-tenant".into(),
            user: "other-user".into(),
            token_hash: vault::digest(b"other-device-token"),
            revoked: false,
        };
        first
            .save_device_entity_cas(
                "other-tenant",
                &other_registry_id,
                &serde_json::to_vec(&other_device).unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(
            second
                .load_device_entity_revisions("sawmills")
                .await
                .unwrap()
                .iter()
                .all(|(device_id, _, _)| device_id != &other_registry_id)
        );
        assert!(
            second
                .load_device_entity_revisions("other-tenant")
                .await
                .unwrap()
                .iter()
                .any(|(device_id, _, _)| device_id == &other_registry_id)
        );
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
                .execute(
                    "DELETE FROM account_live_sessions WHERE account_id=$1",
                    &[&id],
                )
                .await
                .unwrap();
            client
                .execute("DELETE FROM central_accounts WHERE account_id=$1", &[&id])
                .await
                .unwrap();
            client
                .execute("DELETE FROM central_devices WHERE id=$1", &[&registry_id])
                .await
                .unwrap();
            client
                .execute(
                    "DELETE FROM central_devices WHERE id=$1",
                    &[&other_registry_id],
                )
                .await
                .unwrap();
            client
                .execute(
                    "DELETE FROM central_accounts WHERE account_id=$1",
                    &[&legacy_id],
                )
                .await
                .unwrap();
        }
    }
}
