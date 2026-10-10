//! Multi-user broker. One process and one persistent disk own every refresh token.
mod renewal;

use super::{
    catalog, enrollment, fast_path, relogin,
    rpc::Rpc,
    server::{Owner, TokenFailure, TokenRequest, TokenResponse},
    storage::{CentralStore, CredentialRecord, StoreMode},
    transport,
    vault::{self, Vault},
};
use crate::{api, store};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

const IMPORT_LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(120);
const IMPORT_LEASE_RENEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const IMPORT_LEASE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const IMPORT_LEASE_SAFETY_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);
const LOGIN_HOLDER_RENEWAL_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub enabled: bool,
    /// A replacement OIDC identity; the company-user ID and all ownership stay stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_identity: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub user_id: String,
    pub alias: String,
    pub label: Option<String>,
    pub account_id: String,
    pub plan: Option<String>,
    pub billing_class: api::BillingClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<api::Credits>,
    pub primary_used: Option<f64>,
    pub secondary_used: Option<f64>,
    pub primary_window_seconds: Option<u64>,
    pub secondary_window_seconds: Option<u64>,
    pub primary_resets_at: Option<i64>,
    pub resets_at: Option<i64>,
    pub available: bool,
    pub usage_score: Option<f64>,
    #[serde(default)]
    pub usage_age_seconds: Option<u64>,
    #[serde(default)]
    pub usage_stale: bool,
    #[serde(default)]
    pub usage_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline_usage: Option<crate::statusline::Usage>,
    /// Recent launch sessions, or None if the session store could not be read.
    #[serde(default = "legacy_live_sessions")]
    pub live_sessions: Option<usize>,
    /// Recent 429 ratio reported by a client, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_429_rate: Option<f64>,
    /// Set when the account is borrowed from another company user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loan: Option<LoanInfo>,
}
/// The loan behind a borrowed catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoanInfo {
    pub id: String,
    pub lender_email: String,
    pub ends_at: i64,
    /// A bounded reason when the loan cannot issue tokens now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}
// Older account servers do not report load; preserve their zero-load tie-break.
fn legacy_live_sessions() -> Option<usize> {
    Some(0)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub alias: String,
    pub label: Option<String>,
    pub auth: Value,
}
#[derive(Default)]
struct Failure {
    count: u64,
    last: i64,
}
#[derive(Clone)]
pub(super) struct AccountIndex {
    pub(super) user: String,
    pub(super) alias: String,
}
type Owners = BTreeMap<String, (AccountIndex, Arc<Mutex<Owner>>)>;

const RELAY_EVENT_BUCKET_CAPACITY: f64 = 32.0;
const RELAY_EVENT_BUCKET_REFILL_PER_SECOND: f64 = 1.0;
const RELAY_EVENT_KINDS: &[&str] = &["rate_429", "overloaded"];
const RELAY_EVENT_ACCOUNT_CLASSES: &[&str] = &["included", "credit", "unknown"];
const RELAY_EVENT_OUTCOMES: &[&str] =
    &["advised", "recovered", "exhausted", "terminal_passthrough"];

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
struct RelayMetricKey {
    kind: String,
    model: String,
    account_class: String,
    outcome: String,
}

struct RelayMetrics {
    accepted: BTreeMap<RelayMetricKey, u64>,
    event_timestamps: BTreeMap<RelayMetricKey, i64>,
    rejected: BTreeMap<&'static str, u64>,
    buckets: BTreeMap<String, RelayTokenBucket>,
}

impl Default for RelayMetrics {
    fn default() -> Self {
        let mut accepted = BTreeMap::new();
        for kind in RELAY_EVENT_KINDS {
            for model in crate::RELAY_KNOWN_MODELS
                .iter()
                .copied()
                .chain(std::iter::once("other"))
            {
                for account_class in RELAY_EVENT_ACCOUNT_CLASSES {
                    accepted.insert(
                        RelayMetricKey {
                            kind: (*kind).into(),
                            model: model.into(),
                            account_class: (*account_class).into(),
                            outcome: "exhausted".into(),
                        },
                        0,
                    );
                }
            }
        }
        Self {
            accepted,
            event_timestamps: BTreeMap::new(),
            rejected: BTreeMap::new(),
            buckets: BTreeMap::new(),
        }
    }
}

struct RelayTokenBucket {
    tokens: f64,
    last: std::time::Instant,
}

impl RelayTokenBucket {
    fn new() -> Self {
        Self {
            tokens: RELAY_EVENT_BUCKET_CAPACITY,
            last: std::time::Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * RELAY_EVENT_BUCKET_REFILL_PER_SECOND)
            .min(RELAY_EVENT_BUCKET_CAPACITY);
        self.last = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayCapacityEvent {
    kind: String,
    model: String,
    account_class: String,
    outcome: String,
}

impl RelayCapacityEvent {
    fn validate(self) -> Result<RelayMetricKey, ()> {
        if !RELAY_EVENT_KINDS.contains(&self.kind.as_str())
            || !RELAY_EVENT_ACCOUNT_CLASSES.contains(&self.account_class.as_str())
            || !RELAY_EVENT_OUTCOMES.contains(&self.outcome.as_str())
        {
            return Err(());
        }
        let model = if crate::RELAY_KNOWN_MODELS.contains(&self.model.as_str()) {
            self.model
        } else {
            "other".into()
        };
        Ok(RelayMetricKey {
            kind: self.kind,
            model,
            account_class: self.account_class,
            outcome: self.outcome,
        })
    }
}
// Overlap retains a seat reservation even if UID evidence is missing or conflicts.
// It is never permission to replace credentials.
pub(super) fn overlaps(left: &Value, right: &Value) -> bool {
    if vault::account(left).ok() != vault::account(right).ok() {
        return false;
    }
    let left = api::token_logins(vault::token(left).unwrap_or(""));
    let right = api::token_logins(vault::token(right).unwrap_or(""));
    let uid = left.uid.as_ref().zip(right.uid.as_ref());
    let sub = left.sub.as_ref().zip(right.sub.as_ref());
    uid.is_some_and(|(a, b)| a == b)
        || sub.is_some_and(|(a, b)| a == b)
        || (uid.is_none() && sub.is_none())
}

#[derive(Clone)]
pub(super) struct Broker {
    pub state: PathBuf,
    pub key: PathBuf,
    pub(super) binary: PathBuf,
    pub(super) read_only: bool,
    pub(super) ownership_unresolved: Arc<AtomicBool>,
    pub(super) owners: Arc<RwLock<Owners>>,
    pub(super) imports: Arc<Mutex<()>>,
    pub sso: Option<Arc<enrollment::Sso>>,
    pub(super) activity: Arc<super::activity::Activity>,
    pub(super) reset_reader: super::resets::Reader,
    pub(super) catalog: Arc<catalog::Reader>,
    failures: Arc<StdMutex<BTreeMap<&'static str, Failure>>>,
    relay_metrics: Arc<StdMutex<RelayMetrics>>,
    metrics_hash: Option<String>,
    pub(super) work: Arc<Semaphore>,
    pub(super) session_writes: Arc<Semaphore>,
    pub(super) stopping: Arc<AtomicBool>,
    pub(super) recovery_stop: Arc<tokio::sync::Notify>,
    pub(super) background_recovery: bool,
    pub(super) relogins: Arc<StdMutex<BTreeMap<String, Arc<AtomicBool>>>>,
    pub(super) shared_login_workers: Arc<StdMutex<BTreeSet<(String, String)>>>,
    pub(super) central: Option<CentralStore>,
    /// Stable for the process: refresh leases outlive a login holder outage.
    pub(super) holder_id: String,
    /// Login incarnation. It changes when a lost holder lease recovers.
    pub(super) login_holder: Arc<StdMutex<String>>,
    pub(super) login_holder_live: Arc<AtomicBool>,
    pub(super) registry: Option<Arc<std::sync::RwLock<RegistryState>>>,
}
impl Broker {
    fn relay_reject(&self, reason: &'static str) {
        *self
            .relay_metrics
            .lock()
            .expect("relay metric lock poisoned")
            .rejected
            .entry(reason)
            .or_default() += 1;
    }

    async fn relay_accept(
        &self,
        device: &str,
        key: RelayMetricKey,
    ) -> std::result::Result<bool, HttpError> {
        let allowed = match self.central.as_ref() {
            Some(store) if !matches!(store.mode(), StoreMode::File) => {
                store.relay_event_allowed(device).await.map_err(|_| {
                    self.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "relay_rate_limiter_unavailable",
                    )
                })?
            }
            Some(_) | None => {
                let mut metrics = self
                    .relay_metrics
                    .lock()
                    .expect("relay metric lock poisoned");
                let bucket = metrics
                    .buckets
                    .entry(device.to_owned())
                    .or_insert_with(RelayTokenBucket::new);
                bucket.take()
            }
        };
        if !allowed {
            return Ok(false);
        }
        let mut metrics = self
            .relay_metrics
            .lock()
            .expect("relay metric lock poisoned");
        *metrics.accepted.entry(key.clone()).or_default() += 1;
        if key.outcome == "exhausted" {
            metrics
                .event_timestamps
                .insert(key, chrono::Utc::now().timestamp());
        }
        Ok(true)
    }

    pub(super) fn login_holder(&self) -> String {
        self.login_holder
            .lock()
            .expect("login holder lock poisoned")
            .clone()
    }

    fn replace_login_holder(&self, holder: String) {
        *self
            .login_holder
            .lock()
            .expect("login holder lock poisoned") = holder;
    }
}
#[derive(Clone)]
pub(super) struct RegistryState {
    pub users: Vec<User>,
    pub devices: Vec<vault::Device>,
}
pub(super) struct HttpError {
    pub(super) status: StatusCode,
    pub(super) reason: &'static str,
    /// The current alias, for `account_renamed`.
    pub(super) alias: Option<String>,
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let body = match self.alias {
            Some(alias) => json!({"error":self.reason,"alias":alias}),
            None => json!({"error":self.reason}),
        };
        let mut response = (self.status, Json(body)).into_response();
        response.extensions_mut().insert(FailureReason);
        response
    }
}
#[derive(Clone)]
struct FailureReason;

pub fn setup(state: &Path, key: &Path) -> Result<()> {
    let _lock = vault::lock(state, "owner.lock")?;
    if state.join("users.json").try_exists()? || state.join("vault.enc").try_exists()? {
        bail!("server state already initialized");
    }
    if key.try_exists()? {
        if vault::private_read(key)?.len() != 32 {
            bail!("vault key must contain exactly 32 bytes");
        }
    } else {
        vault::create_secret(key, &enrollment::random_bytes())?;
    }
    store::ensure_private_dir(&state.join("accounts"))?;
    vault::save_devices(state, &[])?;
    // The pod initializer treats this registry as the completion marker.
    store::atomic_write(&state.join("users.json"), b"[]")
}
pub fn users(state: &Path) -> Result<Vec<User>> {
    Ok(serde_json::from_slice(&vault::private_read(
        &state.join("users.json"),
    )?)?)
}
pub fn set_user(state: &Path, id: &str, enabled: bool) -> Result<()> {
    let _lock = vault::registry_lock(state, "users.lock")?;
    let mut users = users(state)?;
    let user = users
        .iter_mut()
        .find(|u| u.id == id)
        .context("user not found")?;
    user.enabled = enabled;
    store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)
}

/// Mutate one user in PostgreSQL using its entity revision.  The local file is
/// not consulted in PostgreSQL mode, so a stale pod cannot replace another
/// instance's concurrent enrollment or revocation changes.
pub async fn set_user_central(state: &Path, key: &Path, id: &str, enabled: bool) -> Result<()> {
    let central = super::storage::runtime_store(state, key).await?;
    let rows = central.load_registry_entity_revisions("users").await?;
    let Some((_, payload, revision)) = rows.iter().find(|(entity_id, _, _)| entity_id == id) else {
        bail!("user not found");
    };
    let mut user: User = serde_json::from_slice(payload)?;
    user.enabled = enabled;
    let updated = central
        .save_registry_entity_cas("users", id, &serde_json::to_vec(&user)?, Some(*revision))
        .await?;
    if !updated {
        bail!("user changed concurrently; retry");
    }
    Ok(())
}

pub async fn list_users_central(state: &Path, key: &Path) -> Result<Vec<User>> {
    let central = super::storage::runtime_store(state, key).await?;
    central_registry_users(&central).await
}
pub(super) enum UserEnrollment {
    Recorded,
    Disabled,
}
pub(super) fn record_user(state: &Path, id: &str, email: &str) -> Result<UserEnrollment> {
    let _lock = vault::registry_lock(state, "users.lock")?;
    let mut users = users(state)?;
    if let Some(user) = users.iter_mut().find(|u| u.id == id) {
        if !user.enabled {
            return Ok(UserEnrollment::Disabled);
        }
        user.email = email.into();
    } else {
        users.push(User {
            id: id.into(),
            email: email.into(),
            enabled: true,
            oidc_identity: None,
        });
    }
    store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)?;
    Ok(UserEnrollment::Recorded)
}
pub(super) fn normalize_alias(alias: &str) -> Result<&str> {
    store::validate_alias(alias)
}
pub fn account_key(user: &str, alias: &str) -> String {
    vault::digest(format!("{user}\0{}", alias.to_ascii_lowercase()).as_bytes())
}

fn instance_holder_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|v| v.trim().to_owned()))
        .unwrap_or_else(|_| "unknown-host".into());
    let boot_nonce = vault::digest(&enrollment::random_bytes());
    vault::digest(format!("{host}:{}:{boot_nonce}", std::process::id()).as_bytes())
}

fn holder_renewal_lost(renewed: &Result<bool>, since_last_success: std::time::Duration) -> bool {
    match renewed {
        Ok(true) => false,
        Ok(false) => true,
        Err(_) => since_last_success >= LOGIN_HOLDER_RENEWAL_RETRY_WINDOW,
    }
}

fn shared_login_ready(
    local: bool,
    reachable: Option<bool>,
    central: bool,
    holder_live: bool,
) -> bool {
    local && reachable.is_none_or(|value| value) && (!central || holder_live)
}

pub(super) fn account_summary(owner: &Owner) -> Account {
    let limits = owner.limits.as_ref().map(|v| &v["rateLimits"]);
    let usage = owner
        .limits
        .as_ref()
        .and_then(|v| super::server::usage(v).ok());
    let windows = usage.as_ref().and_then(|u| u.rate_limit.as_ref());
    let billing = owner
        .limits
        .as_ref()
        .map(super::server::billing_class)
        .unwrap_or(api::BillingClass::Unknown);
    Account {
        user_id: owner.vault.user.clone(),
        alias: owner.vault.alias.trim().to_owned(),
        label: owner.vault.label.clone(),
        account_id: vault::account(&owner.vault.auth).unwrap_or_default(),
        plan: limits
            .and_then(|v| v["planType"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                api::token_identity(vault::token(&owner.vault.auth).ok()?).and_then(|i| i.plan)
            }),
        billing_class: billing,
        credits: usage.as_ref().and_then(|u| u.credits.clone()),
        statusline_usage: None,
        primary_used: windows
            .and_then(|r| r.short_window())
            .map(|w| w.used_percent),
        secondary_used: windows
            .and_then(|r| r.long_window())
            .map(|w| w.used_percent),
        primary_window_seconds: windows
            .and_then(api::RateLimit::short_window)
            .and_then(api::RateLimitWindow::duration_seconds),
        secondary_window_seconds: windows
            .and_then(api::RateLimit::long_window)
            .and_then(api::RateLimitWindow::duration_seconds),
        primary_resets_at: windows
            .and_then(api::RateLimit::short_window)
            .and_then(api::RateLimitWindow::reset_timestamp),
        resets_at: windows
            .and_then(|r| r.long_window())
            .and_then(|w| w.reset_timestamp()),
        available: owner.selectable(),
        usage_age_seconds: owner
            .limits_observed
            .as_ref()
            .map(|(at, _)| at.elapsed().as_secs()),
        usage_stale: owner
            .limits_observed
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= catalog::TTL),
        usage_error: None,
        usage_score: owner
            .limits
            .as_ref()
            .and_then(|v| super::server::usage(v).ok())
            .and_then(|u| u.rate_limit.map(|r| r.availability_score())),
        live_sessions: Some(0),
        recent_429_rate: None,
        loan: None,
    }
}

pub(super) async fn central_registry_users(central: &CentralStore) -> Result<Vec<User>> {
    let entities = central.load_registry_entities("users").await?;
    entities
        .into_iter()
        .map(|bytes| Ok(serde_json::from_slice(&bytes)?))
        .collect()
}

async fn central_registry_devices_for_tenant(
    central: &CentralStore,
    tenant: &str,
) -> Result<Vec<vault::Device>> {
    central
        .load_device_entity_revisions(tenant)
        .await?
        .into_iter()
        .map(|(_, bytes, _)| Ok(serde_json::from_slice(&bytes)?))
        .collect()
}

impl Broker {
    pub(super) fn reject_unshared_workflow(&self, reason: &'static str) -> Option<HttpError> {
        self.central
            .as_ref()
            .filter(|store| store.mode() != super::storage::StoreMode::File)
            .map(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, reason))
    }

    async fn ensure_refresh_owner(
        &self,
        owner: &mut Owner,
        imports: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<(), HttpError> {
        if owner.refresh_enabled && owner.rpc.is_some() {
            return Ok(());
        }
        // Shared-store token paths hold imports before Owner, matching import
        // workflows. File mode never takes this lock or launches on demand.
        if self.read_only
            || self.stopping.load(Ordering::Acquire)
            || self.ownership_unresolved.load(Ordering::Acquire)
            || !owner.available
            || owner.routing_refused
        {
            return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
        }
        let inventory = relogin::identity_inventory(&owner.state, &self.key, &owner.home);
        let proof = if let Some(central) = self
            .central
            .as_ref()
            .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
        {
            let identities = central
                .retained_identities()
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
            inventory.clear_for_shared_launch(
                owner,
                relogin::AdmissionKind::Restore,
                &imports,
                &identities,
            )
        } else {
            inventory.clear_for_launch(owner, relogin::AdmissionKind::Restore, &imports)
        }
        .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        spawn_owner(owner, &self.binary, proof)
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        // Admission and PID evidence are complete. Initialization is covered by
        // the account lease and owner mutex, not the replica-wide import lock.
        drop(imports);
        initialize_owner(owner)
            .await
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        owner.refresh_enabled = true;
        owner.available = true;
        Ok(())
    }

    pub(super) async fn record_user_central(
        &self,
        id: &str,
        email: &str,
    ) -> Result<UserEnrollment> {
        let central = self
            .central
            .as_ref()
            .context("central store is not configured")?;
        let rows = central.load_registry_entity_revisions("users").await?;
        if let Some((_, payload, revision)) = rows.iter().find(|(entity_id, _, _)| entity_id == id)
        {
            let mut user: User = serde_json::from_slice(payload)?;
            if !user.enabled {
                return Ok(UserEnrollment::Disabled);
            }
            user.email = email.to_owned();
            if !central
                .save_registry_entity_cas("users", id, &serde_json::to_vec(&user)?, Some(*revision))
                .await?
            {
                bail!("user changed concurrently; retry");
            }
            return Ok(UserEnrollment::Recorded);
        }
        let user = User {
            id: id.to_owned(),
            email: email.to_owned(),
            enabled: true,
            oidc_identity: None,
        };
        if !central
            .save_registry_entity_cas("users", id, &serde_json::to_vec(&user)?, None)
            .await?
        {
            bail!("user was enrolled concurrently; retry");
        }
        Ok(UserEnrollment::Recorded)
    }

    pub(super) async fn record_device_central(&self, device: &vault::Device) -> Result<()> {
        let central = self
            .central
            .as_ref()
            .context("central store is not configured")?;
        if !central
            .save_device_entity_cas(
                &device.tenant,
                &device.id,
                &serde_json::to_vec(device)?,
                None,
            )
            .await?
        {
            bail!("device was enrolled concurrently; retry");
        }
        Ok(())
    }

    pub(super) async fn sync_registry(&self) -> Result<()> {
        let local_users = users(&self.state)?;
        let local_devices = vault::devices(&self.state)?;
        let (users, devices) = if let Some(central) = self.central.as_ref() {
            (central_registry_users(central).await?, Vec::new())
        } else {
            (local_users, local_devices)
        };
        if let Some(registry) = self.registry.as_ref() {
            *registry
                .write()
                .map_err(|_| anyhow::anyhow!("registry lock poisoned"))? = RegistryState {
                users,
                // PostgreSQL device authorization is queried per request.
                // Keep no cross-tenant device snapshot in the in-memory registry.
                devices,
            };
        }
        Ok(())
    }

    async fn save_owner_record(&self, owner: &mut Owner) -> Result<()> {
        let Some(central) = self.central.as_ref() else {
            return Ok(());
        };
        central.save_account(&Self::owner_record(owner)?).await?;
        owner.persist_shared_revision(owner.vault.revision.max(1))?;
        Ok(())
    }

    fn owner_record(owner: &Owner) -> Result<CredentialRecord> {
        Ok(CredentialRecord {
            account_id: account_key(&owner.vault.user, &owner.vault.alias),
            user_id: Some(owner.vault.user.clone()),
            alias: owner.vault.alias.clone(),
            workspace: Some(vault::account(&owner.vault.auth)?),
            login: vault::token(&owner.vault.auth)
                .ok()
                .and_then(api::token_subject),
            vault: serde_json::to_value(&owner.vault)?,
            revision: owner.vault.revision.max(1),
        })
    }

    async fn ensure_owner_record(&self, owner: &mut Owner) -> Result<()> {
        let Some(central) = self.central.as_ref() else {
            return Ok(());
        };
        let account_id = account_key(&owner.vault.user, &owner.vault.alias);
        if let Some(record) = central.load_account(&account_id).await? {
            if central.mode() == super::storage::StoreMode::Postgres {
                reconcile_owner_from_central(self, owner, &account_id)
                    .await
                    .map_err(|_| anyhow::anyhow!("cannot reconcile shared account"))?;
                return Ok(());
            }
            if record.revision > owner.vault.revision {
                let committed: Vault = serde_json::from_value(record.vault)?;
                vault::validate_auth(&committed.auth)?;
                vault::save(&owner.state, &owner.key, &committed)?;
                store::atomic_write(
                    &owner.home.join("auth.json"),
                    &serde_json::to_vec(&committed.auth)?,
                )?;
                owner.vault = committed;
                owner.vault.revision = record.revision;
                if let Some(rpc) = owner.rpc.as_mut() {
                    rpc.shutdown().await?;
                }
                owner.rpc = None;
                owner.refresh_enabled = false;
            }
            owner.persist_shared_revision(record.revision)?;
            return Ok(());
        }
        owner.vault.revision = owner.vault.revision.max(1);
        self.save_owner_record(owner).await
    }

    pub(super) fn record_failure(
        &self,
        reason: &'static str,
        stage: &'static str,
        status: StatusCode,
    ) {
        let mut failures = self.failures.lock().expect("metrics lock");
        let failure = failures.entry(reason).or_default();
        failure.count += 1;
        failure.last = chrono::Utc::now().timestamp();
        drop(failures);
        eprintln!(
            "{}",
            json!({"operation":"broker_request","stage":stage,"reason":reason,"status":status.as_u16()})
        );
    }
    pub fn error(&self, status: StatusCode, reason: &'static str) -> HttpError {
        self.record_failure(reason, "broker", status);
        HttpError {
            status,
            reason,
            alias: None,
        }
    }
    pub async fn authorize(&self, headers: &HeaderMap) -> Result<vault::Device, HttpError> {
        let bearer = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        let (devices, users) =
            if let Some(central) = self
                .central
                .as_ref()
                .filter(|central| central.mode() != super::storage::StoreMode::File)
            {
                let hash = vault::digest(bearer.as_bytes());
                let Some(device) = central.authorized_device(&hash).await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                })?
                else {
                    return Err(self.error(StatusCode::UNAUTHORIZED, "unauthorized"));
                };
                let enabled = central.enabled_user(&device.user).await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                })?;
                if !enabled {
                    return Err(self.error(StatusCode::FORBIDDEN, "user_disabled"));
                }
                return Ok(device);
            } else {
                (
                    match self.registry.as_ref() {
                        Some(registry) => registry
                            .read()
                            .map_err(|_| {
                                self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                            })?
                            .devices
                            .clone(),
                        None => vault::devices(&self.state).map_err(|_| {
                            self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                        })?,
                    },
                    match self.registry.as_ref() {
                        Some(registry) => registry
                            .read()
                            .map_err(|_| {
                                self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                            })?
                            .users
                            .clone(),
                        None => users(&self.state).map_err(|_| {
                            self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
                        })?,
                    },
                )
            };
        let hash = vault::digest(bearer.as_bytes());
        let device = devices
            .into_iter()
            .find(|d| d.token_hash == hash && !d.revoked && d.tenant == "sawmills")
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        if !users.iter().any(|u| u.id == device.user && u.enabled) {
            return Err(self.error(StatusCode::FORBIDDEN, "user_disabled"));
        }
        Ok(device)
    }
    pub(super) async fn owner(
        &self,
        device: &vault::Device,
        alias: &str,
    ) -> Result<Arc<Mutex<Owner>>, HttpError> {
        self.resolve_alias(&device.user, alias)
            .await?
            .map(|(_, refresh)| refresh)
            .ok_or_else(|| {
                if self.ownership_unresolved.load(Ordering::Acquire) {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed")
                } else {
                    super::rename::renamed_error(self, &device.user, alias)
                        .unwrap_or_else(|| self.error(StatusCode::NOT_FOUND, "account_not_found"))
                }
            })
    }
    pub(super) async fn resolve_alias(
        &self,
        user: &str,
        alias: &str,
    ) -> Result<Option<(String, Arc<Mutex<Owner>>)>, HttpError> {
        let alias = normalize_alias(alias)
            .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
        let entries = self.owners.read().await;
        let mut selected = None;
        // Retain the physical key for server accounts saved before normalization.
        // Ambiguous historical aliases require repair; never choose one silently.
        for (key, (identity, refresh)) in entries.iter() {
            if identity.user != user {
                continue;
            }
            if identity.alias.eq_ignore_ascii_case(alias) {
                if selected.is_some() {
                    return Err(self.error(StatusCode::CONFLICT, "ambiguous_alias"));
                }
                selected = Some((key.clone(), refresh.clone()));
            }
        }
        drop(entries);
        if selected.is_none()
            && let Some(central) = self
                .central
                .as_ref()
                .filter(|central| central.mode() != super::storage::StoreMode::File)
        {
            let record = central
                .load_account_by_alias(user, alias)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            if let Some(record) = record {
                let account_state = self
                    .state
                    .join("accounts")
                    .join(account_key(user, &record.alias));
                let account_vault: Vault = serde_json::from_value(record.vault).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                vault::validate_auth(&account_vault.auth).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                store::ensure_private_dir(&account_state).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                vault::save(&account_state, &self.key, &account_vault).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                let owner = Arc::new(Mutex::new(
                    prepare_owner(&account_state, &self.key, self.read_only).map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                    })?,
                ));
                let key = account_key(user, &record.alias);
                let mut owners = self.owners.write().await;
                if let Some((_, existing)) = owners.get(&key) {
                    selected = Some((key, existing.clone()));
                } else {
                    owners.insert(
                        key.clone(),
                        (
                            AccountIndex {
                                user: user.to_owned(),
                                alias: record.alias.trim().to_owned(),
                            },
                            owner.clone(),
                        ),
                    );
                    selected = Some((key, owner));
                }
            }
        }
        Ok(selected)
    }
    fn owner_failure(&self, error: TokenFailure) -> HttpError {
        match error {
            TokenFailure::AccountMismatch => self.error(StatusCode::CONFLICT, "account_mismatch"),
            TokenFailure::RefreshDisabled => self.error(StatusCode::CONFLICT, "refresh_disabled"),
            TokenFailure::UnsupportedRouting => {
                self.error(StatusCode::CONFLICT, "unsupported_workspace_routing")
            }
            TokenFailure::Unavailable(_) | TokenFailure::Retryable(_) => {
                self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
            }
        }
    }
}

async fn reconcile_owner_from_central(
    broker: &Broker,
    owner: &mut Owner,
    account_id: &str,
) -> Result<bool, HttpError> {
    let Some(central) = broker
        .central
        .as_ref()
        .filter(|central| central.mode() != super::storage::StoreMode::File)
    else {
        return Ok(false);
    };
    let Some(record) = central
        .load_account(account_id)
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
    else {
        return Ok(false);
    };
    // Failed startup preparation has no trustworthy revision baseline. Keep
    // its retained evidence quarantined instead of repairing it through a token request.
    if owner.routing_refused && owner.shared_revision.is_none() {
        return Ok(false);
    }
    // Local snapshots can advance without publication. Renewal evidence must
    // be newer than the last shared revision, not the unpublished local one.
    let shared_revision = owner.shared_revision.unwrap_or(owner.vault.revision);
    let committed: vault::Vault = serde_json::from_value(record.vault)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    vault::validate_auth(&committed.auth)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    // A committed renewal is positive evidence that a prior local rejection is
    // obsolete, even if another replica has since advanced the credential revision.
    let renewed = central.mode() == super::storage::StoreMode::Postgres
        && committed.verified
        && !committed.import_rejected
        && record.revision > shared_revision
        && central
            .login_completed_after(account_id, shared_revision, record.revision)
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    if record.revision <= owner.vault.revision && !renewed {
        owner
            .persist_shared_revision(record.revision)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        return Ok(false);
    }
    if central.mode() == super::storage::StoreMode::Postgres {
        let identities = central
            .retained_identities()
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        let agreement = (|| -> Result<()> {
            let identity = identities
                .get(account_id)
                .context("completed login identity missing")?;
            identity.validate(&owner.vault.auth)?;
            identity.validate(&committed.auth)?;
            if owner.home.try_exists()? {
                previous_owner_exited(&owner.home)?;
                identity.validate(&retained_auth(&owner.home)?)?;
            }
            Ok(())
        })();
        if agreement.is_err() {
            owner.available = false;
            owner.routing_refused = true;
            owner.shared_revision = None;
            return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
        }
    }
    vault::save(&owner.state, &owner.key, &committed)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    store::atomic_write(
        &owner.home.join("auth.json"),
        &serde_json::to_vec(&committed.auth)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
    )
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    owner.vault = committed;
    owner.vault.revision = record.revision;
    // The old child may have cached the spent refresh token. Stop it and let the
    // next lease holder relaunch from the committed vault.
    if let Some(rpc) = owner.rpc.as_mut() {
        rpc.shutdown()
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
    }
    owner.rpc = None;
    owner.refresh_enabled = false;
    // A retained settlement callback belongs to the preceding credential state.
    owner.recovery_generation = owner.recovery_generation.wrapping_add(1);
    if renewed {
        owner.verification_input = None;
        owner.available = true;
        owner.routing_refused = false;
        owner.retryable_unavailable = false;
        owner.retry_requires_billing = false;
        owner.retry_started = None;
        owner.retry_failures = 0;
    }
    owner
        .persist_shared_revision(record.revision)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    Ok(true)
}

// Settlement may finish a refresh after the request deadline. Keep the stopped
// RPC attached through snapshot so its final journal is read as this owner's
// output (including refresh-only rotations with unchanged access claims).
async fn settled_owner_record(owner: &mut Owner, before: &Vault) -> Result<CredentialRecord> {
    if let Some(rpc) = owner.rpc.as_mut()
        && !rpc.process_exited()
    {
        rpc.settle_and_stop().await?;
    }
    owner.snapshot()?;
    if serde_json::to_value(&owner.vault)? != serde_json::to_value(before)? {
        owner.vault.revision = owner
            .vault
            .revision
            .max(before.revision.saturating_add(1))
            .max(1);
    }
    vault::save(&owner.state, &owner.key, &owner.vault)?;
    let record = Broker::owner_record(owner)?;
    owner.rpc = None;
    owner.refresh_enabled = false;
    Ok(record)
}

fn fence_background_owner(owner: &mut Owner) {
    owner.available = false;
    owner.refresh_enabled = false;
    owner.routing_refused = true;
}

fn fence_background_probe(owner: &mut Owner) {
    owner.available = false;
    owner.refresh_enabled = false;
}

async fn wait_for_lease_loss(lost: Arc<AtomicBool>, signal: Arc<Notify>) {
    loop {
        // Create the notification future before checking the flag so a loss
        // between the check and the await cannot be missed.
        let notified = signal.notified();
        if lost.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn lease_guarded_verification<T, F>(
    verification: F,
    lost: Arc<AtomicBool>,
    signal: Arc<Notify>,
) -> Result<T, HttpError>
where
    F: Future<Output = Result<T, HttpError>>,
{
    tokio::select! {
        result = verification => result,
        _ = wait_for_lease_loss(lost, signal) => {
            Err(HttpError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                reason: "refresh_fenced",
                alias: None,
            })
        }
    }
}

async fn terminate_lost_import(owner: &mut Owner) {
    // Closing the RPC is the first action after lease loss. This prevents an
    // in-flight account/read from rotating credentials after a successor
    // acquires the account lease.
    if let Some(rpc) = owner.rpc.take() {
        rpc.terminate().await;
    }
    fence_background_owner(owner);
}

async fn abandon_background_recovery(
    owner_ref: Arc<Mutex<Owner>>,
    recovery_generation: u64,
    renew_done: Arc<AtomicBool>,
    renew_task: Option<tokio::task::JoinHandle<()>>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let mut owner = owner_ref.lock().await;
    if owner.recovery_generation == recovery_generation {
        fence_background_owner(&mut owner);
    }
    drop(owner);
    renew_done.store(true, Ordering::Release);
    if let Some(task) = renew_task {
        task.abort();
    }
    drop(permit);
}

enum SettlementRecovery {
    Available,
    Retryable,
    Fenced,
}

#[derive(Clone)]
struct LeaseWindow {
    deadline: Arc<Mutex<std::time::Instant>>,
    lost: Arc<AtomicBool>,
    signal: Arc<Notify>,
}

/// Keep a recovery lease until the stopped child has been settled and its
/// credentials are durably published. The work permit is held by the caller
/// or transferred to this task, so shutdown drains this work instead of
/// cancelling it.
#[allow(clippy::too_many_arguments)] // Recovery state is explicit at this boundary.
async fn settle_background_recovery(
    owner_ref: Arc<Mutex<Owner>>,
    central: Option<CentralStore>,
    lease: Option<super::storage::Lease>,
    lease_window: Option<Arc<LeaseWindow>>,
    before: Vault,
    recovery: SettlementRecovery,
    recovery_generation: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
    renew_done: Arc<AtomicBool>,
    renew_task: Option<tokio::task::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
) {
    loop {
        // Shared-store shutdown must drain the child's final rotation to the
        // database before releasing its lease and the shutdown work permit.
        if central.is_none() && stopping.load(Ordering::Acquire) {
            abandon_background_recovery(
                owner_ref,
                recovery_generation,
                renew_done,
                renew_task,
                permit,
            )
            .await;
            return;
        }
        if let Some(window) = lease_window.as_ref() {
            let remaining = window
                .deadline
                .lock()
                .await
                .saturating_duration_since(std::time::Instant::now());
            if window.lost.load(Ordering::Acquire) || remaining <= IMPORT_LEASE_SAFETY_MARGIN {
                let mut owner = owner_ref.lock().await;
                if owner.recovery_generation == recovery_generation {
                    terminate_lost_import(&mut owner).await;
                }
                drop(owner);
                abandon_background_recovery(
                    owner_ref,
                    recovery_generation,
                    renew_done,
                    renew_task,
                    permit,
                )
                .await;
                return;
            }
        }
        let record = {
            let settle = async {
                let mut owner = owner_ref.lock().await;
                if owner.recovery_generation != recovery_generation {
                    return Ok(None);
                }
                settled_owner_record(&mut owner, &before).await.map(Some)
            };
            if let Some(window) = lease_window.as_ref() {
                let mut settle = Box::pin(settle);
                loop {
                    let remaining = window
                        .deadline
                        .lock()
                        .await
                        .saturating_duration_since(std::time::Instant::now());
                    if remaining <= IMPORT_LEASE_SAFETY_MARGIN {
                        drop(settle);
                        let mut owner = owner_ref.lock().await;
                        if owner.recovery_generation == recovery_generation {
                            terminate_lost_import(&mut owner).await;
                        }
                        drop(owner);
                        abandon_background_recovery(
                            owner_ref,
                            recovery_generation,
                            renew_done,
                            renew_task,
                            permit,
                        )
                        .await;
                        return;
                    }
                    let check_after = IMPORT_LEASE_RENEW_INTERVAL
                        .min(remaining.saturating_sub(IMPORT_LEASE_SAFETY_MARGIN));
                    tokio::select! {
                        record = &mut settle => break record,
                        _ = wait_for_lease_loss(window.lost.clone(), window.signal.clone()) => {
                            drop(settle);
                            let mut owner = owner_ref.lock().await;
                            if owner.recovery_generation == recovery_generation {
                                terminate_lost_import(&mut owner).await;
                            }
                            drop(owner);
                            abandon_background_recovery(owner_ref, recovery_generation, renew_done, renew_task, permit)
                                .await;
                            return;
                        }
                        _ = tokio::time::sleep(check_after) => {}
                    }
                }
            } else {
                settle.await
            }
        };
        let record = match record {
            Ok(Some(record)) => record,
            Ok(None) => {
                abandon_background_recovery(
                    owner_ref,
                    recovery_generation,
                    renew_done,
                    renew_task,
                    permit,
                )
                .await;
                return;
            }
            Err(error) => {
                eprintln!("central child settlement retry: {error:#}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let durable = match (central.as_ref(), lease.as_ref()) {
            (Some(central), Some(lease)) => match central
                .renew(lease, std::time::Duration::from_secs(120))
                .await
            {
                Ok(true) => match central.fenced_write(lease, &record).await {
                    Ok(true) => true,
                    Ok(false) => {
                        // A tombstone or newer credential is definitive rejection,
                        // not an uncertain query. Stop renewing and let shutdown
                        // drain without ever reopening this owner.
                        let _ = central.release_lease(lease).await;
                        abandon_background_recovery(
                            owner_ref,
                            recovery_generation,
                            renew_done,
                            renew_task,
                            permit,
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        eprintln!("central settlement write retry: {error:#}");
                        false
                    }
                },
                Ok(false) => {
                    abandon_background_recovery(
                        owner_ref,
                        recovery_generation,
                        renew_done,
                        renew_task,
                        permit,
                    )
                    .await;
                    return;
                }
                Err(error) => {
                    // A failed query is not proof that another holder owns
                    // the lease. Retry while retaining the unpublished journal.
                    eprintln!("central settlement renewal retry: {error:#}");
                    false
                }
            },
            _ => true,
        };
        if !durable {
            if central.is_none() && stopping.load(Ordering::Acquire) {
                abandon_background_recovery(
                    owner_ref,
                    recovery_generation,
                    renew_done,
                    renew_task,
                    permit,
                )
                .await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        if let (Some(central), Some(lease)) = (central.as_ref(), lease.as_ref()) {
            let _ = central.release_lease(lease).await;
        }
        {
            let mut owner = owner_ref.lock().await;
            if owner.recovery_generation == recovery_generation && central.is_some() {
                owner.shared_revision = Some(record.revision);
            }
            if owner.recovery_generation == recovery_generation
                && matches!(recovery, SettlementRecovery::Available)
            {
                owner.available = true;
                owner.refresh_enabled = true;
                owner.routing_refused = false;
                owner.retryable_unavailable = false;
                owner.retry_requires_billing = false;
                owner.retry_started = None;
                owner.retry_failures = 0;
            } else if owner.recovery_generation == recovery_generation
                && matches!(recovery, SettlementRecovery::Retryable)
            {
                // Settlement cleared the temporary lease fence. A retryable
                // failure still needs its normal recovery probe before serving.
                owner.routing_refused = false;
            }
        }
        renew_done.store(true, Ordering::Release);
        if let Some(task) = renew_task {
            task.abort();
        }
        drop(permit);
        return;
    }
}

async fn token(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<TokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let session_id = match headers.get("x-codexctl-session") {
        Some(value) => value
            .to_str()
            .ok()
            .filter(|value| value.len() <= 128)
            .ok_or_else(|| broker.error(StatusCode::BAD_REQUEST, "invalid_session_id"))?,
        None => "default",
    };
    let session_id = if session_id.is_empty() {
        "default"
    } else {
        session_id
    };
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let alias = request
        .alias
        .as_deref()
        .ok_or_else(|| broker.error(StatusCode::BAD_REQUEST, "alias_required"))?;
    // A borrowed reference resolves only through an active grant (ADR 0004).
    let borrowed = if alias.contains('/') {
        Some(
            broker
                .borrowed_owner(&device, alias, request.loan_id.as_deref())
                .await?,
        )
    } else {
        None
    };
    let owner = match borrowed.as_ref() {
        Some(borrowed) => borrowed.owner.clone(),
        None => broker.owner(&device, alias).await?,
    };
    if !broker.background_recovery
        && broker
            .central
            .as_ref()
            .is_none_or(|s| s.mode() == super::storage::StoreMode::File)
    {
        let worker = broker.clone();
        let owner_ref = owner.clone();
        let account_id = request.account_id.clone();
        // Detached work retains its permit and settles native refreshes even
        // when the requesting client disconnects.
        tokio::spawn(async move {
            {
                let owner = owner_ref.lock().await;
                owner
                    .validate_account_id(account_id.as_deref())
                    .map_err(|failure| worker.owner_failure(failure))?;
            }
            recover_owners(&worker, Some(owner_ref)).await;
            Ok::<_, HttpError>(())
        })
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))??;
    }
    // The fast path and the lease wait run only behind the flag, in
    // PostgreSQL mode. Dual and file mode keep today's path.
    let fast_on = fast_path::enabled()
        && broker
            .central
            .as_ref()
            .is_some_and(|central| central.mode() == super::storage::StoreMode::Postgres);
    // The account, user, and alias the committed row must carry.
    let (fast_account, fast_user, fast_alias) = match borrowed.as_ref() {
        Some(borrowed) => (
            borrowed.grant.account_id.clone(),
            borrowed.grant.lender.clone(),
            borrowed.grant.alias.clone(),
        ),
        None => (
            account_key(&device.user, alias.trim()),
            device.user.clone(),
            alias.trim().to_owned(),
        ),
    };
    let template = request;
    let mut try_fast = fast_on;
    let mut busy_owner = false;
    let mut wait: Option<(tokio::time::Instant, tokio::time::Instant)> = None;
    let (mut token, alias, account_id) = loop {
        if try_fast {
            let result = fast_token(
                &broker,
                LocalOwner::Probe(&owner),
                &fast_account,
                &fast_user,
                &fast_alias,
                &template,
            )
            .await;
            fast_path::record(result.as_ref().map(|_| ()).map_err(|miss| *miss));
            match result {
                Ok((token, alias)) => {
                    if let Some((started, _)) = wait {
                        fast_path::Wait::Committed.record(started.elapsed());
                    }
                    break (token, alias, fast_account.clone());
                }
                Err(miss) => busy_owner = miss == fast_path::Miss::Busy,
            }
        }
        let permit = broker
            .work
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
        let worker = broker.clone();
        let owner_ref = owner.clone();
        // A borrowed request names the lender's alias through its grant.
        let lender_alias = borrowed
            .as_ref()
            .map(|borrowed| borrowed.grant.alias.clone());
        let mut request = template.clone();
        // A request that found the owner busy re-checks under its lock.
        let fast_locked =
            busy_owner.then(|| (fast_account.clone(), fast_user.clone(), fast_alias.clone()));
        busy_owner = false;
        let busy = Arc::new(StdMutex::new(None::<(String, i64)>));
        let busy_slot = busy.clone();
        let attempt = tokio::spawn(async move {
        let (import_guard, mut owner) = if worker
            .central.as_ref()
            .is_some_and(|central| central.mode() != super::storage::StoreMode::File)
        {
            loop {
                let imports = worker.imports.lock().await;
                if let Ok(owner) = owner_ref.try_lock() {
                    break (Some(imports), owner);
                }
                // Keep the fair Owner queue position when admission is free.
                // Never await imports with Owner held: an importer may already
                // hold imports while waiting for this Owner.
                drop(imports);
                let owner = owner_ref.lock().await;
                if let Ok(imports) = worker.imports.try_lock() {
                    break (Some(imports), owner);
                }
                drop(owner);
            }
        } else {
            (None, owner_ref.lock().await)
        };
        // A rename can re-key this owner while the request waits for its lock.
        // The old alias must never be served once the rename committed.
        let requested = request.alias.as_deref().unwrap_or_default().trim();
        match lender_alias.as_deref() {
            // A rename of a lent account ends the borrower's access here.
            Some(expected) if !owner.vault.alias.trim().eq_ignore_ascii_case(expected) => {
                return Err(worker.error(StatusCode::FORBIDDEN, "loan_ended"));
            }
            Some(_) => {}
            None if !owner.vault.alias.trim().eq_ignore_ascii_case(requested) => {
                return Err(
                    super::rename::renamed_error(&worker, &owner.vault.user, requested)
                        .unwrap_or_else(|| {
                            worker.error(StatusCode::NOT_FOUND, "account_not_found")
                        }),
                );
            }
            None => {}
        }
        let account_id = account_key(&owner.vault.user, &owner.vault.alias);
        if worker
            .central
            .as_ref()
            .is_some_and(|central| central.mode() != super::storage::StoreMode::File)
        {
            reconcile_owner_from_central(&worker, &mut owner, &account_id).await?;
        }
        if !owner.vault.verified {
            return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
        }
        owner
            .validate_account_id(request.account_id.as_deref())
            .map_err(|failure| worker.owner_failure(failure))?;
        if !owner.available {
            return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
        }
        // Under the owner lock the local fence is visible, so a request that
        // found the owner busy may still be served without the lease.
        if let Some((fast_account, fast_user, fast_alias)) = fast_locked.as_ref()
            && !owner.routing_refused
        {
            let result = fast_token(
                &worker,
                LocalOwner::Locked,
                fast_account,
                fast_user,
                fast_alias,
                &request,
            )
            .await;
            fast_path::record(result.as_ref().map(|_| ()).map_err(|miss| *miss));
            if let Ok((token, alias)) = result {
                return Ok((token, alias, account_id));
            }
        }
        // `account/read` may refresh even without a forced request. Every
        // PostgreSQL token path therefore takes the account lease before
        // invoking the native owner or persisting its result.
        let lease = if let Some(central) = worker.central.as_ref() {
            // Ensure the FK target exists before the first request after cutover
            // or a locally imported account.
            worker
                .ensure_owner_record(&mut owner)
                .await
                .map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            match central
                .acquire_lease(
                    &account_id,
                    &worker.holder_id,
                    std::time::Duration::from_secs(120),
                )
                .await
            {
                Ok(lease) => Some(lease),
                Err(_) => {
                    // A lease loser drops its locks and permit and waits for
                    // the holder outside this task. It counts no failure yet.
                    if fast_path::enabled()
                        && central.mode() == super::storage::StoreMode::Postgres
                        && central
                            .lease_held_elsewhere(&account_id, &worker.holder_id)
                            .await
                            .unwrap_or(false)
                        && let Ok(mut slot) = busy_slot.lock()
                    {
                        *slot = Some((account_id.clone(), owner.vault.revision));
                        return Err(HttpError {
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            reason: "refresh_in_progress",
                            alias: None,
                        });
                    }
                    return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress"));
                }
            }
        } else {
            None
        };
        let reconciled = if lease.is_some()
            && worker
                .central
                .as_ref()
                .is_some_and(|central| central.mode() != super::storage::StoreMode::File)
        {
            match reconcile_owner_from_central(&worker, &mut owner, &account_id).await {
                Ok(changed) => changed,
                Err(error) => {
                    if let (Some(central), Some(lease)) = (worker.central.as_ref(), lease.as_ref())
                    {
                        let _ = central.release_lease(lease).await;
                    }
                    return Err(error);
                }
            }
        } else {
            false
        };
        if reconciled {
            request.previous_revision = None;
        }
        let renew_lost = Arc::new(AtomicBool::new(false));
        let renew_task = match (worker.central.clone(), lease.clone()) {
            (Some(central), Some(lease)) => {
                let lost = renew_lost.clone();
                Some(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        match central
                            .renew(&lease, std::time::Duration::from_secs(120))
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                lost.store(true, Ordering::Release);
                                break;
                            }
                            Err(error) => {
                                eprintln!("central lease renewal: {error:#}");
                            }
                        }
                    }
                }))
            }
            _ => None,
        };
        let mut retain_lease = lease.is_some();
        // initialize can rotate credentials too. Capture the committed baseline
        // and renew the lease before launching, then settle even a failed launch.
        let before = owner.vault.clone();
        let mut restore_after_settlement = false;
        let result = async {
            if lease.is_some()
                && let Some(import_guard) = import_guard
                && let Err(error) = worker.ensure_refresh_owner(&mut owner, import_guard).await
            {
                restore_after_settlement = owner.available && !owner.routing_refused;
                return Err(error);
            }
            let observed_from = std::time::Instant::now();
            let token_result = owner.tokens(request).await;
            // A failed native read may mean lost routing or a fenced owner.
            // Withdraw the evidence so no replica serves this account
            // lease-free until the lease path observes it again.
            // A clear that keeps failing leaves at most the 60 s evidence
            // bound; it is counted so the failure alert fires.
            if token_result.is_err()
                && lease.is_some()
                && let Some(central) = worker.central.as_ref()
            {
                let mut cleared = Err(anyhow::anyhow!("not attempted"));
                for attempt in 0..3 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    cleared = central.clear_token_evidence(&account_id).await;
                    if cleared.is_ok() {
                        break;
                    }
                }
                if let Err(error) = cleared {
                    eprintln!(
                        "{}",
                        json!({"operation":"token_evidence","stage":"clear","reason":"clear_failed","error":format!("{error:#}")})
                    );
                    worker.record_failure(
                        "token_evidence_clear_failed",
                        "token_evidence",
                        StatusCode::SERVICE_UNAVAILABLE,
                    );
                }
            }
            if matches!(&token_result, Err(TokenFailure::Retryable(_))) {
                owner.fence(true);
                if owner.retry_started.is_none() {
                    owner.retry_started = Some(owner.retry_clock_now());
                }
            }
            let settle_required = owner.rpc.as_ref().is_some_and(Rpc::completion_pending);
            if settle_required && owner.rpc.is_some() {
                let failed_rpc = owner.rpc.as_ref().is_some_and(Rpc::retryable_or_timed_out);
                let process_exited = owner.rpc.as_mut().is_some_and(Rpc::process_exited);
                // An outstanding request may still have rotated credentials even
                // after a protocol/transport failure. Only an EOF from a child
                // that is confirmed dead can skip the normal settlement path.
                let dead_child =
                    failed_rpc && process_exited && !owner.rpc.as_ref().is_some_and(Rpc::timed_out);
                if dead_child {
                    if let Some(rpc) = owner.rpc.take() {
                        rpc.terminate().await;
                    }
                    owner.refresh_enabled = false;
                    // The failed call may have rotated auth before the protocol
                    // failure. Read the journal after the bounded child wait so
                    // the shared store publishes that completed rotation.
                    if failed_rpc {
                        owner.snapshot().map_err(|error| {
                            eprintln!("central owner failed snapshot: {error:#}");
                            worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                        })?;
                    }
                } else {
                    // Keep the RPC attached and the lease retained until the
                    // request completion is known. This applies to timeouts and
                    // protocol failures alike.
                    retain_lease = true;
                    match settled_owner_record(&mut owner, &before).await {
                        Ok(_) => {}
                        Err(error) if failed_rpc => {
                            let exited = owner
                                .rpc
                                .as_mut()
                                .is_some_and(|rpc| rpc.process_exited());
                            let exited = if exited {
                                true
                            } else if let Some(rpc) = owner.rpc.as_mut() {
                                rpc.wait_after_eof().await
                            } else {
                                false
                            };
                            if !exited {
                                eprintln!("central owner settlement: {error:#}");
                                return Err(worker.error(
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    "owner_unavailable",
                                ));
                            }
                            // The protocol read reached EOF and the child has
                            // now exited. Completion is known impossible, so
                            // discard this dead RPC and publish its journal.
                            if let Some(rpc) = owner.rpc.take() {
                                rpc.terminate().await;
                            }
                            owner.refresh_enabled = false;
                            owner.snapshot().map_err(|snapshot_error| {
                                eprintln!(
                                    "central owner dead-child snapshot: {snapshot_error:#} (settle: {error:#})"
                                );
                                worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                            })?;
                        }
                        Err(error) => {
                            eprintln!("central owner settlement: {error:#}");
                            return Err(worker.error(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "owner_unavailable",
                            ));
                        }
                    }
                }
            }
            if lease.is_some() && owner.rpc.is_some() {
                // No refresh-capable child may outlive its lease, even after a
                // successful cached-token read. Save any exit-time rotation.
                if let Err(error) = settled_owner_record(&mut owner, &before).await {
                    restore_after_settlement = token_result.is_ok()
                        && owner.available
                        && !owner.routing_refused;
                    eprintln!("central owner shutdown: {error:#}");
                    return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
                }
            }
            let auth_changed = owner.vault.auth != before.auth;
            if auth_changed {
                owner.vault.revision = before.revision.saturating_add(1).max(1);
            }
            let record = CredentialRecord {
                account_id: account_id.clone(),
                user_id: Some(owner.vault.user.clone()),
                alias: owner.vault.alias.clone(),
                workspace: Some(vault::account(&owner.vault.auth).map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                })?),
                login: vault::token(&owner.vault.auth)
                    .ok()
                    .and_then(api::token_subject),
                vault: serde_json::to_value(&owner.vault).map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?,
                revision: owner.vault.revision.max(1),
            };
            let observation = observation(&owner, &token_result, observed_from);
            let written = if let Some(central) = worker.central.as_ref() {
                if let Some(lease) = lease.as_ref() {
                    let mut result = None;
                    for attempt in 0..3 {
                        match central
                            .fenced_write_with_evidence(lease, &record, observation.as_ref())
                            .await
                        {
                            Ok(written) => {
                                result = Some(written);
                                break;
                            }
                            Err(error) if attempt < 2 => {
                                eprintln!("central fenced write retry: {error:#}");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                            Err(error) => {
                                retain_lease = true;
                                // Preserve the token outcome until the deferred
                                // settlement is classified below. This DB error
                                // is not a permanent routing refusal.
                                restore_after_settlement = token_result.is_ok()
                                    && owner.available
                                    && !owner.routing_refused;
                                eprintln!("central fenced write deferred: {error:#}");
                                return Err(worker
                                    .error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"));
                            }
                        }
                    }
                    result.expect("fenced write result")
                } else {
                    if let Err(error) = central.save_account(&record).await {
                        // The local vault remains authoritative in dual mode for
                        // cached-token delivery. Readiness exposes the DB outage;
                        // token delivery stays available while the mirror recovers.
                        eprintln!("central store cached-token mirror: {error:#}");
                    }
                    true
                }
            } else {
                true
            };
            if !written || renew_lost.load(Ordering::Acquire) {
                retain_lease = true;
                owner.available = false;
                owner.routing_refused = true;
                owner.refresh_enabled = false;
                return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_fenced"));
            }
            if worker.central.is_some() {
                owner.shared_revision = Some(record.revision);
            }
            if auth_changed {
                vault::save(&owner.state, &owner.key, &owner.vault).map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
            }
            retain_lease = false;
            let token = match token_result {
                Ok(token) => {
                    owner.retry_failures = 0;
                    owner.retry_started = None;
                    owner.retryable_unavailable = false;
                    token
                }
                Err(error) => {
                    return Err(worker.owner_failure(error));
                }
            };
            Ok::<_, HttpError>((token, owner.vault.alias.trim().to_owned(), account_id.clone()))
        }
        .await;
        if retain_lease {
            let recovery = if restore_after_settlement {
                SettlementRecovery::Available
            } else if owner.retryable_unavailable && !owner.routing_refused {
                SettlementRecovery::Retryable
            } else {
                SettlementRecovery::Fenced
            };
            let recovery_generation = owner.recovery_generation;
            owner.available = false;
            owner.routing_refused = true;
            owner.refresh_enabled = false;
            tokio::spawn(settle_background_recovery(
                owner_ref.clone(),
                worker.central.clone(),
                lease,
                None,
                before,
                recovery,
                recovery_generation,
                permit,
                Arc::new(AtomicBool::new(false)),
                renew_task,
                worker.stopping.clone(),
            ));
        } else {
            if let Some(task) = renew_task {
                task.abort();
            }
            if let (Some(central), Some(lease)) = (worker.central.as_ref(), lease.as_ref())
                && let Err(error) = central.release_lease(lease).await
            {
                eprintln!("central lease release: {error:#}");
            }
        }
        result
        })
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
        let error = match attempt {
            Ok(value) => {
                if let Some((started, _)) = wait {
                    fast_path::Wait::Acquired.record(started.elapsed());
                }
                break value;
            }
            Err(error) => error,
        };
        let loser = busy.lock().ok().and_then(|mut slot| slot.take());
        let (Some((account_id, revision)), Some(central)) = (loser, broker.central.as_ref()) else {
            if let Some((started, _)) = wait {
                fast_path::Wait::Refused.record(started.elapsed());
            }
            return Err(error);
        };
        let (started, deadline) = *wait.get_or_insert_with(|| {
            let now = tokio::time::Instant::now();
            (now, now + fast_path::LEASE_WAIT)
        });
        match fast_path::wait_for_holder(
            central,
            &account_id,
            &broker.holder_id,
            revision,
            deadline,
        )
        .await
        {
            fast_path::Seen::Committed => try_fast = true,
            fast_path::Seen::Released => try_fast = false,
            fast_path::Seen::Timeout => {
                fast_path::Wait::Timeout.record(started.elapsed());
                return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress"));
            }
        }
    };
    refresh_legacy_usage(&broker, &mut token, borrowed.is_some()).await;
    // Revocation during a slow refresh must prevent delivery of a new access token.
    broker.authorize(&headers).await?;
    // A loan that ended or paused during the refresh delivers no token either.
    let alias = match borrowed.as_ref() {
        Some(borrowed) => {
            broker
                .confirm_borrowed_token(&borrowed.grant, &token)
                .await?;
            // The checks above can wait; a machine revoked meanwhile gets
            // nothing. The atomic issue step is last before the response.
            broker.authorize(&headers).await?;
            broker
                .issue_borrowed_token(&borrowed.grant, &device)
                .await?;
            borrowed.grant.reference.clone()
        }
        None => alias,
    };
    // A client-supplied launch ID is unique only within its authenticated machine.
    let session_id = format!("{}:{}:{session_id}", device.id.len(), device.id);
    broker.activity.delivered(
        &device,
        alias.clone(),
        session_id.clone(),
        account_id.clone(),
    );
    if let Some(central) = broker
        .central
        .clone()
        .filter(|store| store.mode() != super::storage::StoreMode::File)
    {
        match broker.session_writes.clone().try_acquire_owned() {
            Ok(permit) => {
                let worker = broker.clone();
                let user = device.user.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    // Keyed by the owning account, so a borrower's session
                    // counts on the lender's account too.
                    let account = account_id;
                    let failure = match central
                        .record_live_session(&account, &user, &alias, &session_id)
                        .await
                    {
                        Err(error) => Some(("write", error)),
                        Ok(()) => central
                            .prune_live_sessions(&account, &user)
                            .await
                            .err()
                            .map(|error| ("cleanup", error)),
                    };
                    if let Some((stage, error)) = failure {
                        worker.record_failure(
                            "live_session_failed",
                            "live_session",
                            StatusCode::SERVICE_UNAVAILABLE,
                        );
                        eprintln!(
                            "{}",
                            json!({"operation":"live_session", "stage":stage, "error":error.to_string()})
                        );
                    }
                });
            }
            Err(_) => broker.record_failure(
                "live_session_failed",
                "live_session_queue",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        }
    }
    token.user_id = Some(device.user);
    Ok(([("cache-control", "no-store")], Json(token)).into_response())
}

/// Serve a committed token from PostgreSQL without the lease or a child. The
/// row must carry the expected user and alias, and no fence may cover it.
/// How the fast path sees this replica's own owner.
enum LocalOwner<'a> {
    /// Probe it without waiting; a busy owner hides its fence, so it misses.
    Probe(&'a Arc<Mutex<Owner>>),
    /// The caller holds the owner lock and has checked its fence.
    Locked,
}

async fn fast_token(
    broker: &Broker,
    local: LocalOwner<'_>,
    account_id: &str,
    user: &str,
    alias: &str,
    request: &TokenRequest,
) -> Result<(TokenResponse, String), fast_path::Miss> {
    use fast_path::Miss;
    let central = broker.central.as_ref().ok_or(Miss::Disabled)?;
    let read = central
        .fast_token_read(account_id)
        .await
        .map_err(|_| Miss::ReadFailed)?
        .ok_or(Miss::Identity)?;
    if read.fenced {
        return Err(Miss::Fenced);
    }
    if read.user_id.as_deref() != Some(user) || !read.alias.trim().eq_ignore_ascii_case(alias) {
        return Err(Miss::Identity);
    }
    let vault: Vault = serde_json::from_value(read.vault).map_err(|_| Miss::Identity)?;
    if !vault.verified {
        return Err(Miss::Fenced);
    }
    if let Some(requested) = request.account_id.as_deref()
        && vault::account(&vault.auth).ok().as_deref() != Some(requested)
    {
        return Err(Miss::Identity);
    }
    if let LocalOwner::Probe(owner) = local {
        let fenced = owner
            .try_lock()
            .map(|owner| !owner.available || owner.routing_refused)
            .map_err(|_| Miss::Busy)?;
        if fenced {
            return Err(Miss::Fenced);
        }
    }
    let token = fast_path::decide(
        &fast_path::Candidate {
            auth: &vault.auth,
            account_revision: read.revision,
            label: vault.label.clone(),
            now: read.now,
            evidence: read.evidence.as_ref(),
        },
        request,
    )?;
    Ok((token, read.alias))
}

/// Evidence for the credential this request is about to publish. Only a
/// completed routing check on that exact revision counts.
fn observation(
    owner: &Owner,
    token: &Result<TokenResponse, TokenFailure>,
    observed_from: std::time::Instant,
) -> Option<fast_path::Observation> {
    let Ok(token) = token else { return None };
    let published = vault::digest(&serde_json::to_vec(&owner.vault.auth).ok()?);
    if !token.native_routing_supported || token.revision != published {
        return None;
    }
    let billing = token.billing_class.and_then(|class| {
        let (at, revision) = owner.limits_observed.as_ref()?;
        (revision == &token.revision).then(|| fast_path::Billing {
            class,
            plan_type: token.chatgpt_plan_type.clone(),
            usage: token.statusline_usage.clone(),
            peak_used_percent: owner.limits.as_ref().and_then(fast_path::peak_used_percent),
            age: at.elapsed(),
        })
    });
    Some(fast_path::Observation {
        auth_revision: token.revision.clone(),
        routing_age: observed_from.elapsed(),
        billing,
    })
}

/// `need_all_windows`: a borrowed token's placement checks need the
/// all-window maximum, which native usage cannot give.
async fn refresh_legacy_usage(broker: &Broker, token: &mut TokenResponse, need_all_windows: bool) {
    let Some(usage) = token.statusline_usage.as_ref() else {
        return;
    };
    if usage.allowed.is_some()
        && usage.limit_reached.is_some()
        && (!need_all_windows || usage.max_used_percent.is_some())
    {
        return;
    }
    let authoritative = match broker
        .catalog
        .fetch_direct(&token.access_token, &token.chatgpt_account_id)
        .await
    {
        Ok(usage) => usage,
        Err(reason) => {
            eprintln!("central token usage refresh failed reason={reason}");
            return;
        }
    };
    token.billing_class = Some(super::server::usage_billing_class(&authoritative));
    token.chatgpt_plan_type = authoritative
        .plan_type
        .clone()
        .or(token.chatgpt_plan_type.take());
    token.statusline_usage = Some(crate::statusline::Usage::from_usage(&authoritative));
}

async fn accounts(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let result = account_catalog(&broker, &device.user, catalog::Freshness::Cached).await?;
    broker.authorize(&headers).await?;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}

pub(super) async fn account_catalog(
    broker: &Broker,
    user: &str,
    freshness: catalog::Freshness,
) -> Result<Vec<Account>, HttpError> {
    if let Some(central) = broker
        .central
        .as_ref()
        .filter(|store| store.mode() != super::storage::StoreMode::File)
    {
        let aliases = central
            .list_account_aliases(user)
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        for alias in aliases {
            let _ = broker.resolve_alias(user, &alias).await?;
        }
    }
    let mut owners: Vec<_> = broker
        .owners
        .read()
        .await
        .iter()
        .filter(|(_, (identity, _))| identity.user == user)
        .map(|(key, (_, owner))| (key.clone(), owner.clone(), None))
        .collect();
    // A loan-store fault hides borrowed entries only; owned accounts stay
    // listed. The failure is already counted, and token issue stays strict.
    match broker.borrowed_catalog(user).await {
        Ok(borrowed) => owners.extend(
            borrowed
                .into_iter()
                .map(|entry| (entry.key, entry.owner, Some((entry.grant, entry.paused)))),
        ),
        Err(error) => eprintln!(
            "{}",
            json!({"operation":"account_catalog","stage":"borrowed","error":error.reason})
        ),
    }
    // Borrowed entries count too: a borrower with no owned account can still
    // list a healthy loan while an unrelated account awaits recovery.
    if owners.is_empty() && broker.ownership_unresolved.load(Ordering::Acquire) {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
    }
    let user = user.to_owned();
    let tasks = owners.into_iter().map(|(key, owner, loan)| {
        let broker = broker.clone();
        let user = user.clone();
        tokio::spawn(async move {
            // Copy only the access credential. Listing never snapshots, refreshes,
            // persists, or changes the credential owner's availability.
            let (mut summary, revision, access, seed, account) = {
                let owner = owner.lock().await;
                let revision = vault::digest(owner.vault.auth.to_string().as_bytes());
                let seed = owner
                    .limits_observed
                    .as_ref()
                    .filter(|(_, observed)| observed == &revision)
                    .and_then(|(at, _)| {
                        Some((super::server::usage(owner.limits.as_ref()?).ok()?, *at))
                    });
                (
                    account_summary(&owner),
                    revision,
                    vault::token(&owner.vault.auth).ok().map(str::to_owned),
                    seed,
                    account_key(&owner.vault.user, &owner.vault.alias),
                )
            };
            let failure = broker
                .catalog
                .read(
                    &key,
                    &revision,
                    access.as_deref(),
                    seed,
                    &mut summary,
                    freshness,
                )
                .await;
            if let Some(reason) = failure {
                broker.record_failure(reason, "catalog_usage", StatusCode::SERVICE_UNAVAILABLE);
            }
            // Renewal or a token request may have changed the credential while
            // the independent usage request was in flight. Do not publish its evidence.
            let current = owner.lock().await;
            summary.available = current.selectable();
            if vault::digest(current.vault.auth.to_string().as_bytes()) != revision {
                summary.usage_stale = true;
                summary.usage_error = Some("credentials_changed".into());
            }
            if summary.usage_stale {
                summary.statusline_usage = None;
                summary.billing_class = api::BillingClass::Unknown;
                summary.credits = None;
                summary.usage_score = None;
            }
            drop(current);
            summary.live_sessions = Some(broker.activity.live_sessions(&account, 10 * 60));
            if let Some(central) = broker.central.as_ref()
                && central.mode() != super::storage::StoreMode::File
            {
                match central
                    .live_session_count(&key, std::time::Duration::from_secs(10 * 60))
                    .await
                {
                    Ok(count) => {
                        summary.live_sessions =
                            Some(if central.mode() == super::storage::StoreMode::Dual {
                                count.max(summary.live_sessions.unwrap_or(0))
                            } else {
                                count
                            });
                    }
                    Err(error) => {
                        broker.record_failure(
                            "live_session_failed",
                            "live_session",
                            StatusCode::SERVICE_UNAVAILABLE,
                        );
                        eprintln!("central live session count: {error:#}");
                        if central.mode() == super::storage::StoreMode::Postgres {
                            summary.live_sessions = None;
                        }
                    }
                }
            }
            if let Some((grant, paused)) = loan {
                // A native seed has no all-window maximum, which borrowed
                // placement needs; fetch complete usage for this entry only.
                if !summary.usage_stale
                    && summary
                        .statusline_usage
                        .as_ref()
                        .is_some_and(|usage| usage.max_used_percent.is_none())
                    && let Some(access) = access.as_deref()
                    && let Some(snapshot) = broker
                        .complete_usage(&key, access, &summary.account_id)
                        .await
                {
                    summary.statusline_usage = Some(snapshot);
                }
                summary.alias = grant.reference;
                summary.user_id = user;
                if paused.is_some() {
                    summary.available = false;
                }
                summary.loan = Some(LoanInfo {
                    id: grant.id,
                    lender_email: grant.lender_email,
                    ends_at: grant.ends_at,
                    paused: paused.map(str::to_owned),
                });
            }
            summary
        })
    });
    let mut result = Vec::new();
    for task in futures::future::join_all(tasks).await {
        result.push(
            task.map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "catalog_task_failed"))?,
        );
    }
    result.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(result)
}

async fn import(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Import>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    normalize_alias(&input.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    store::validate_label(input.label.as_deref().unwrap_or(""))
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_label"))?;
    vault::validate_auth(&input.auth)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_auth"))?;
    // Only admitted imports continue on disconnect. Waiting handlers retain no
    // detached task; settlement can inherit this same permit after admission.
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let worker = broker.clone();
    let result =
        tokio::spawn(async move { worker.import_account(permit, &device.user, input).await })
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "import_unavailable"))??;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}
impl Broker {
    fn storage_admission_failure(&self, error: anyhow::Error) -> HttpError {
        match error.downcast_ref::<super::storage::identity::IdentityDenied>() {
            Some(denied) => self.error(StatusCode::CONFLICT, denied.reason()),
            None => self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"),
        }
    }
    async fn lock_import_owner<'a>(
        &self,
        owner: &'a Mutex<Owner>,
    ) -> Result<tokio::sync::MutexGuard<'a, Owner>, HttpError> {
        // Admission audits every local journal, including possible identity
        // changes. Refuse an unstable inventory instead of holding imports
        // while waiting for native work; existing accounts can still serve.
        let owner = if self
            .central
            .as_ref()
            .is_some_and(|central| central.mode() != super::storage::StoreMode::File)
        {
            owner
                .try_lock()
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress"))?
        } else {
            owner.lock().await
        };
        if owner.import_settling {
            return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress"));
        }
        Ok(owner)
    }

    async fn import_account(
        &self,
        permit: tokio::sync::OwnedSemaphorePermit,
        user: &str,
        input: Import,
    ) -> Result<Account, HttpError> {
        // Lock order is imports, then Owner. Token requests take the same
        // imports guard before locking their selected Owner.
        let import_guard = self.imports.lock().await;
        self.import_account_locked(&import_guard, permit, user, input)
            .await
    }

    /// Import admission for a caller that already holds the imports guard, such
    /// as a server-managed new-account login that must not release it between
    /// retiring its own reservation and admission.
    pub(super) async fn import_account_locked(
        &self,
        import_guard: &tokio::sync::MutexGuard<'_, ()>,
        permit: tokio::sync::OwnedSemaphorePermit,
        user: &str,
        mut input: Import,
    ) -> Result<Account, HttpError> {
        input.alias = normalize_alias(&input.alias)
            .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
            .to_owned();
        let resolved = self
            .resolve_alias(user, &input.alias)
            .await?
            .map(|(key, _)| key);
        if resolved.is_none()
            && super::rename::renamed_to(&self.state, user, &input.alias)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .is_some()
        {
            return Err(self.error(StatusCode::CONFLICT, "alias_renamed"));
        }
        if let Some(central) = self
            .central
            .as_ref()
            .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
            && central
                .renamed_alias(user, &input.alias)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .is_some()
        {
            return Err(self.error(StatusCode::CONFLICT, "alias_renamed"));
        }
        let id = resolved.unwrap_or_else(|| account_key(user, &input.alias));
        if let Some(central) = self
            .central
            .as_ref()
            .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
        {
            central
                .check_import_identity(&id, &input.auth)
                .await
                .map_err(|error| self.storage_admission_failure(error))?;
        }
        let selected = self.state.join("accounts").join(&id);
        if self.read_only {
            return Err(self.error(StatusCode::CONFLICT, "verification_requires_refresh"));
        }
        if self.stopping.load(Ordering::Acquire) {
            return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"));
        }
        if self.ownership_unresolved.load(Ordering::Acquire) {
            return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
        }
        let owners = self.owners.read().await.clone();
        // Inventory journals before filtering by a vault identity. A failed refresh
        // can leave a different seat in the journal while its vault stays unchanged.
        for (_, owner) in owners.values() {
            let mut owner = self.lock_import_owner(owner).await?;
            let inventory = async {
                let inventory = relogin::identity_inventory(
                    &owner.state,
                    &self.key,
                    &owner.state.join("runtime"),
                );
                inventory
                    .saved
                    .as_ref()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                inventory
                    .journal
                    .as_ref()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                inventory
                    .candidates
                    .as_ref()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if !inventory.journal_conflicts() {
                    return Ok(());
                }
                // Settle before trusting a conflicting journal, then inventory it
                // again through the same reader used by renewal and startup.
                owner.available = false;
                if let Some(rpc) = owner.rpc.as_mut() {
                    // A nonzero status is still a confirmed stopped owner.
                    // Unknown request completion or process exit remains fenced.
                    rpc.settle_and_stop().await?;
                } else {
                    previous_owner_exited(&owner.home)?;
                }
                owner.rpc = None;
                let settled = relogin::identity_inventory(
                    &owner.state,
                    &self.key,
                    &owner.state.join("runtime"),
                );
                if settled.runtime != relogin::ProcessState::Stopped {
                    bail!("conflicting owner did not stop");
                }
                settled.saved?;
                settled.journal?.context("missing conflicting journal")?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if inventory.is_err() {
                self.ownership_unresolved.store(true, Ordering::Release);
                return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
            }
        }
        for (key, (_, owner)) in &owners {
            if key == &id {
                continue;
            }
            let mut owner = self.lock_import_owner(owner).await?;
            let same = vault::account(&owner.vault.auth).ok() == vault::account(&input.auth).ok()
                && api::token_subject(vault::token(&owner.vault.auth).unwrap_or(""))
                    == api::token_subject(vault::token(&input.auth).unwrap_or(""));
            if !same {
                continue;
            }
            if !owner.vault.verified {
                // Settle an unverified refresh process before the shared admission
                // rule decides whether another migration can claim its identity.
                if let Some(rpc) = owner.rpc.as_mut() {
                    rpc.shutdown().await.map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                    })?;
                } else {
                    previous_owner_exited(&owner.home).map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                    })?;
                }
                owner.available = false;
                if owner
                    .home
                    .try_exists()
                    .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"))?
                {
                    owner.snapshot().map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed")
                    })?;
                }
            }
        }
        let identities =
            if let Some(central) = self
                .central
                .as_ref()
                .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
            {
                Some(central.retained_identities().await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?)
            } else {
                None
            };
        let admission = if let Some(identities) = identities.as_ref() {
            relogin::clear_shared_registry(
                &self.state.join("accounts"),
                &self.key,
                &selected,
                &input.auth,
                relogin::AdmissionKind::Migration,
                identities,
            )
        } else {
            relogin::clear_registry(
                &self.state.join("accounts"),
                &self.key,
                &selected,
                &input.auth,
                relogin::AdmissionKind::Migration,
            )
        }
        .map_err(
            |error| match error.downcast_ref::<relogin::AdmissionDenied>() {
                Some(relogin::AdmissionDenied::Reserved) => {
                    self.error(StatusCode::CONFLICT, "relogin_reserved")
                }
                Some(relogin::AdmissionDenied::Owned) => {
                    self.error(StatusCode::CONFLICT, "account_already_owned")
                }
                Some(relogin::AdmissionDenied::IdentityConflict) => {
                    self.error(StatusCode::CONFLICT, "alias_identity_conflict")
                }
                Some(relogin::AdmissionDenied::Unsettled) => {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                }
                None => self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"),
            },
        )?;
        if let Some((_, owner)) = owners.get(&id) {
            let mut owner = self.lock_import_owner(owner).await?;
            let original_uid = api::token_identity(vault::token(&owner.vault.auth).unwrap_or(""))
                .and_then(|identity| identity.user_id);
            let incoming_uid = api::token_identity(vault::token(&input.auth).unwrap_or(""))
                .and_then(|identity| identity.user_id);
            if vault::account(&owner.vault.auth).ok() != vault::account(&input.auth).ok()
                || api::token_subject(vault::token(&owner.vault.auth).unwrap_or(""))
                    != api::token_subject(vault::token(&input.auth).unwrap_or(""))
                || matches!((&original_uid, &incoming_uid), (Some(a), Some(b)) if a != b)
            {
                return Err(self.error(StatusCode::CONFLICT, "alias_identity_conflict"));
            }
            if self.background_recovery
                && owner.retryable_unavailable
                && owner.retry_failures < 3
                && !owner.routing_refused
            {
                return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
            }
            if owner.vault.verified
                && owner.available
                && !admission.quarantine_repair
                && self
                    .central
                    .as_ref()
                    .is_none_or(|central| central.mode() == super::storage::StoreMode::File)
            {
                // A retained proof owns the grant, but cannot establish current
                // routing or billing eligibility after a restart or policy change.
                owner
                    .tokens(TokenRequest {
                        billing: true,
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| self.owner_failure(e))?;
                return Ok(account_summary(&owner));
            }
            if owner.rpc.as_mut().is_some_and(Rpc::process_exited) {
                owner.rpc.take();
            } else if let Some(rpc) = owner.rpc.as_mut() {
                rpc.shutdown().await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                })?;
            } else {
                previous_owner_exited(&owner.home).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                })?;
            }
            if owner
                .home
                .try_exists()
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
            {
                owner.snapshot().map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
            }
            if self
                .central
                .as_ref()
                .is_some_and(|central| central.mode() != super::storage::StoreMode::File)
            {
                // Requests may already hold this Arc while waiting for imports.
                // Retire it before replacement so an orphan cannot reenter the
                // holder's lease or be reopened by an older recovery task.
                owner.fence(false);
            }
            // Keep the reservation until the prepared replacement is inserted below.
            // Validation or preparation can still fail after this owner has stopped.
        }
        let state = self.state.join("accounts").join(&id);
        if state
            .try_exists()
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
        {
            let mut saved = vault::load(&state, &self.key)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            if saved.user != user
                || !normalize_alias(&saved.alias).is_ok_and(|alias| alias == input.alias)
                || vault::account(&saved.auth).ok() != vault::account(&input.auth).ok()
                || api::token_subject(vault::token(&saved.auth).unwrap_or(""))
                    != api::token_subject(vault::token(&input.auth).unwrap_or(""))
            {
                return Err(self.error(StatusCode::CONFLICT, "alias_identity_conflict"));
            }
            let original_login = api::token_identity(vault::token(&saved.auth).unwrap_or(""))
                .and_then(|i| i.user_id);
            let incoming_login = api::token_identity(vault::token(&input.auth).unwrap_or(""))
                .and_then(|i| i.user_id);
            if matches!((&original_login, &incoming_login), (Some(a), Some(b)) if a != b) {
                return Err(self.error(StatusCode::CONFLICT, "alias_identity_conflict"));
            }
            if !saved.verified && saved.import_rejected && saved.auth != input.auth {
                // A retry retains the server's learned claims. A fresh grant must
                // also preserve a known UID before replacing those credentials.
                if original_login.is_some() && original_login != incoming_login {
                    return Err(self.error(StatusCode::CONFLICT, "alias_identity_conflict"));
                }
                // The stopped owner rejected this exact grant without changing it.
                // Preserve the new input in the journal first for crash recovery.
                store::atomic_write(
                    &state.join("runtime/auth.json"),
                    &serde_json::to_vec(&input.auth)
                        .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_auth"))?,
                )
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
                saved.auth = input.auth;
                saved.import_rejected = false;
                saved.label = input.label;
                saved.revision = saved.revision.saturating_add(1).max(1);
                vault::save(&state, &self.key, &saved).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                let owner_record = CredentialRecord {
                    account_id: account_key(&saved.user, &saved.alias),
                    user_id: Some(saved.user.clone()),
                    alias: saved.alias.clone(),
                    workspace: Some(
                        vault::account(&saved.auth)
                            .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_auth"))?,
                    ),
                    login: vault::token(&saved.auth).ok().and_then(api::token_subject),
                    vault: serde_json::to_value(&saved).map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                    })?,
                    revision: saved.revision.max(1),
                };
                if let Some(central) = self.central.as_ref() {
                    central
                        .save_account(&owner_record)
                        .await
                        .map_err(|error| self.storage_admission_failure(error))?;
                }
            }
        } else {
            store::ensure_private_dir(&state)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            let vault = Vault {
                alias: input.alias,
                tenant: "sawmills".into(),
                user: user.into(),
                auth: input.auth,
                label: input.label,
                verified: false,
                import_rejected: false,
                revision: 1,
            };
            vault::save(&state, &self.key, &vault)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            let owner_record = CredentialRecord {
                account_id: account_key(&vault.user, &vault.alias),
                user_id: Some(vault.user.clone()),
                alias: vault.alias.clone(),
                workspace: Some(
                    vault::account(&vault.auth)
                        .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_auth"))?,
                ),
                login: vault::token(&vault.auth).ok().and_then(api::token_subject),
                vault: serde_json::to_value(&vault).map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?,
                revision: vault.revision.max(1),
            };
            if let Some(central) = self.central.as_ref() {
                central
                    .save_account(&owner_record)
                    .await
                    .map_err(|error| self.storage_admission_failure(error))?;
            }
        }
        let owner = Arc::new(Mutex::new(
            prepare_owner(&state, &self.key, self.read_only)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?,
        ));
        let guard = owner.clone().lock_owned().await;
        // Retain ownership before initialization or any potentially uncertain refresh.
        let identity = AccountIndex {
            user: user.into(),
            alias: guard.vault.alias.trim().into(),
        };
        self.owners
            .write()
            .await
            .insert(id, (identity, owner.clone()));
        let owner_ref = owner;
        let mut owner = guard;
        let verification_lease = if let Some(central) = self
            .central
            .as_ref()
            .filter(|store| store.mode() != super::storage::StoreMode::File)
        {
            self.ensure_owner_record(&mut owner)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            Some(
                central
                    .acquire_lease(
                        &account_key(&owner.vault.user, &owner.vault.alias),
                        &self.holder_id,
                        IMPORT_LEASE_TTL,
                    )
                    .await
                    .map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress")
                    })?,
            )
        } else {
            None
        };
        if let (Some(central), Some(lease)) = (self.central.as_ref(), verification_lease.as_ref())
            && let Err(error) =
                reconcile_owner_from_central(self, &mut owner, &lease.account_id).await
        {
            let _ = central.release_lease(lease).await;
            return Err(error);
        }
        let before = owner.vault.clone();
        let renew_lost = Arc::new(AtomicBool::new(false));
        let renew_done = Arc::new(AtomicBool::new(false));
        let lease_lost_signal = Arc::new(Notify::new());
        let lease_window = verification_lease.as_ref().map(|_| {
            Arc::new(LeaseWindow {
                deadline: Arc::new(Mutex::new(std::time::Instant::now() + IMPORT_LEASE_TTL)),
                lost: renew_lost.clone(),
                signal: lease_lost_signal.clone(),
            })
        });
        let renew_task = match (self.central.clone(), verification_lease.clone()) {
            (Some(central), Some(lease)) => {
                let done = renew_done.clone();
                let lost = renew_lost.clone();
                let signal = lease_lost_signal.clone();
                let window = lease_window.clone().expect("central lease window");
                Some(tokio::spawn(async move {
                    while !done.load(Ordering::Acquire) {
                        let remaining = window
                            .deadline
                            .lock()
                            .await
                            .saturating_duration_since(std::time::Instant::now());
                        if remaining <= IMPORT_LEASE_SAFETY_MARGIN {
                            lost.store(true, Ordering::Release);
                            signal.notify_waiters();
                            break;
                        }
                        let wait = IMPORT_LEASE_RENEW_INTERVAL
                            .min(remaining.saturating_sub(IMPORT_LEASE_SAFETY_MARGIN));
                        tokio::time::sleep(wait).await;
                        if done.load(Ordering::Acquire) {
                            break;
                        }
                        match central.renew(&lease, IMPORT_LEASE_TTL).await {
                            Ok(true) => {
                                *window.deadline.lock().await =
                                    std::time::Instant::now() + IMPORT_LEASE_TTL;
                            }
                            Ok(false) => {
                                lost.store(true, Ordering::Release);
                                signal.notify_waiters();
                                break;
                            }
                            Err(error) => {
                                eprintln!("central import lease renewal: {error:#}");
                                let remaining = window
                                    .deadline
                                    .lock()
                                    .await
                                    .saturating_duration_since(std::time::Instant::now());
                                if remaining <= IMPORT_LEASE_SAFETY_MARGIN {
                                    lost.store(true, Ordering::Release);
                                    signal.notify_waiters();
                                    break;
                                }
                                tokio::time::sleep(
                                    IMPORT_LEASE_RETRY_INTERVAL
                                        .min(remaining.saturating_sub(IMPORT_LEASE_SAFETY_MARGIN)),
                                )
                                .await;
                            }
                        }
                    }
                }))
            }
            _ => None,
        };
        let verification_required = !owner.vault.verified;
        let mut central_published = false;
        let verification_work = async {
            if self.read_only {
                return Err(self.error(StatusCode::CONFLICT, "verification_requires_refresh"));
            }
            // The imports guard was acquired before the new Owner guard.
            let inventory = relogin::identity_inventory(&owner.state, &self.key, &owner.home);
            let proof = if let Some(central) = self
                .central
                .as_ref()
                .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
            {
                let identities = central.retained_identities().await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                inventory.clear_for_shared_launch(
                    &owner,
                    relogin::AdmissionKind::Migration,
                    import_guard,
                    &identities,
                )
            } else {
                inventory.clear_for_launch(&owner, relogin::AdmissionKind::Migration, import_guard)
            }
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
            launch_owner(&mut owner, &self.binary, proof)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
            owner.refresh_enabled = true;
            let revision = owner
                .snapshot()
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .revision;
            let probe = owner
                .tokens(TokenRequest {
                    previous_revision: Some(revision),
                    billing: true,
                    ..Default::default()
                })
                .await;
            if verification_lease.is_some() && matches!(&probe, Err(TokenFailure::Retryable(_))) {
                owner.fence(true);
                if owner.retry_started.is_none() {
                    owner.retry_started = Some(owner.retry_clock_now());
                }
            }
            probe.map_err(|e| self.owner_failure(e))?;
            if !owner.rpc.as_ref().is_some_and(Rpc::verified_login) {
                return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
            }
            owner.vault.verified = true;
            owner.vault.revision = owner.vault.revision.saturating_add(1).max(1);
            if verification_lease.is_some() {
                // Initialization, verification, and process exit may all rotate
                // credentials. Stop and snapshot before publishing or releasing.
                settled_owner_record(&mut owner, &before)
                    .await
                    .map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                    })?;
            }
            vault::save(&state, &self.key, &owner.vault)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            vault::save(&owner.state, &self.key, &owner.vault)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            if let (Some(central), Some(lease)) =
                (self.central.as_ref(), verification_lease.as_ref())
            {
                let written = central
                    .fenced_write(
                        lease,
                        &Self::owner_record(&owner).map_err(|_| {
                            self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                        })?,
                    )
                    .await
                    .map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                    })?;
                if !written {
                    return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_fenced"));
                }
                owner.shared_revision = Some(owner.vault.revision.max(1));
                central_published = true;
            } else {
                self.save_owner_record(&mut owner).await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
            }
            relogin::retire_reservations(&self.state.join("accounts"), &owner.vault.auth, &state)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            owner.verification_input = None;
            Ok(account_summary(&owner))
        };
        let mut verification = if verification_lease.is_some() {
            lease_guarded_verification(
                verification_work,
                renew_lost.clone(),
                lease_lost_signal.clone(),
            )
            .await
        } else {
            verification_work.await
        };
        // If the provider completed at the same time as the renewal failed,
        // prefer the lease-loss path before any result is published.
        let lease_lost = renew_lost.load(Ordering::Acquire);
        if lease_lost {
            verification = Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_fenced"));
        }
        if lease_lost {
            terminate_lost_import(&mut owner).await;
            renew_done.store(true, Ordering::Release);
        }
        let restore_after_settlement = before.verified
            && !owner.routing_refused
            && !owner.rpc.as_ref().is_some_and(Rpc::rejected_login);
        // The probe may fence the owner and advance its generation. Bind the
        // settlement to that completed probe, before adding our temporary fence.
        let recovery_generation = owner.recovery_generation;
        let recovery = if restore_after_settlement && owner.retryable_unavailable {
            SettlementRecovery::Retryable
        } else if restore_after_settlement {
            SettlementRecovery::Available
        } else {
            SettlementRecovery::Fenced
        };
        if verification.is_err() {
            owner.available = false;
            if verification_required {
                // A failed import remains reserved for its original alias. It
                // must complete verification through import retry before token
                // recovery can become eligible.
                if !central_published {
                    owner.vault.verified = false;
                    if vault::save(&owner.state, &self.key, &owner.vault).is_err() {
                        verification =
                            Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"));
                    }
                }
                owner.retryable_unavailable = false;
                owner.retry_started = None;
                owner.retry_failures = 0;
            }
        }
        if verification.is_err() && verification_lease.is_some() && !lease_lost {
            fence_background_owner(&mut owner);
            owner.import_settling = true;
            drop(owner);
            let central = self.central.clone();
            let stopping = self.stopping.clone();
            // Keep a per-owner reservation while settlement runs independently.
            // Other accounts' token requests must not wait for this child or DB.
            tokio::spawn(async move {
                settle_background_recovery(
                    owner_ref.clone(),
                    central,
                    verification_lease,
                    lease_window.clone(),
                    before,
                    recovery,
                    recovery_generation,
                    permit,
                    renew_done,
                    renew_task,
                    stopping,
                )
                .await;
                owner_ref.lock().await.import_settling = false;
            });
        } else if lease_lost {
            // The lease is no longer live, so deferred settlement must not
            // retain or persist a child under the expired epoch.
            drop(owner);
            renew_done.store(true, Ordering::Release);
            if let Some(task) = renew_task {
                task.abort();
            }
            drop(permit);
        } else {
            if let (Some(central), Some(lease)) =
                (self.central.as_ref(), verification_lease.as_ref())
                && let Err(error) = central.release_lease(lease).await
            {
                eprintln!("central import lease release: {error:#}");
            }
            renew_done.store(true, Ordering::Release);
            if let Some(task) = renew_task {
                task.abort();
            }
        }
        verification
    }
}
async fn me(State(broker): State<Broker>, headers: HeaderMap) -> Result<Json<Value>, HttpError> {
    let device = broker.authorize(&headers).await?;
    Ok(Json(json!({"id":device.user})))
}
async fn devices(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let current = broker.authorize(&headers).await?;
    let devices = if let Some(central) = broker
        .central
        .as_ref()
        .filter(|central| central.mode() != super::storage::StoreMode::File)
    {
        central_registry_devices_for_tenant(central, &current.tenant)
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
    } else {
        vault::devices(&broker.state)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
    };
    Ok(Json(json!(
        devices
            .into_iter()
            .filter(|d| d.user == current.user)
            .map(|d| json!({"id":d.id,"revoked":d.revoked}))
            .collect::<Vec<_>>()
    )))
}
#[derive(Deserialize, Serialize)]
pub struct RevokeDevice {
    pub id: String,
}
async fn revoke_device(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Json(input): Json<RevokeDevice>,
) -> Result<StatusCode, HttpError> {
    let current = broker.authorize(&headers).await?;
    if let Some(central) = broker
        .central
        .as_ref()
        .filter(|central| central.mode() != super::storage::StoreMode::File)
    {
        let (_, payload, revision) = central
            .load_device_entity_revision(&current.tenant, &input.id)
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
            .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "device_not_found"))?;
        let mut device: vault::Device = serde_json::from_slice(&payload)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
        if device.user != current.user {
            return Err(broker.error(StatusCode::NOT_FOUND, "device_not_found"));
        }
        device.revoked = true;
        let updated = central
            .save_device_entity_cas(
                &current.tenant,
                &input.id,
                &serde_json::to_vec(&device).map_err(|_| {
                    broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?,
                Some(revision),
            )
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
        if !updated {
            return Err(broker.error(StatusCode::CONFLICT, "registry_changed"));
        }
        central
            .retire_relay_event_limiter(&input.id)
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    } else {
        let _lock = vault::registry_lock(&broker.state, "devices.lock")
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_busy"))?;
        let mut devices = vault::devices(&broker.state)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
        let device = devices
            .iter_mut()
            .find(|d| d.id == input.id && d.user == current.user)
            .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "device_not_found"))?;
        device.revoked = true;
        vault::save_devices(&broker.state, &devices)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    }
    broker
        .sync_registry()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn metrics(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    if let Some(expected) = &broker.metrics_hash {
        let actual = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|v| vault::digest(v.as_bytes()));
        if actual.as_ref() != Some(expected) {
            return Err(broker.error(StatusCode::UNAUTHORIZED, "metrics_unauthorized"));
        }
    } else {
        broker.authorize(&headers).await?;
    }
    let mut output: String = broker
        .failures
        .lock()
        .expect("metrics lock")
        .iter()
        .map(|(reason, count)| {
            format!("codexctl_central_failed_requests_total{{reason=\"{reason}\"}} {}\ncodexctl_central_last_failure_timestamp_seconds{{reason=\"{reason}\"}} {}\n", count.count, count.last)
        })
        .collect();
    output.push_str(&super::storage::identity::admission_metrics());
    output.push_str(&fast_path::metrics());
    output.push_str(&super::owner_refresh::metrics());
    output.push_str(&format!(
        "codexctl_central_ownership_unresolved{{reason=\"recovery_failed\"}} {}\n",
        u8::from(broker.ownership_unresolved.load(Ordering::Acquire))
    ));
    let relay_metrics = broker
        .relay_metrics
        .lock()
        .expect("relay metric lock poisoned");
    output.push_str("# TYPE codexctl_central_relay_capacity_events_total counter\n");
    for (key, count) in &relay_metrics.accepted {
        output.push_str(&format!(
            "codexctl_central_relay_capacity_events_total{{account_class=\"{}\",kind=\"{}\",model=\"{}\",outcome=\"{}\"}} {}\n",
            key.account_class, key.kind, key.model, key.outcome, count
        ));
    }
    output.push_str("# TYPE codexctl_central_relay_capacity_event_timestamp_seconds gauge\n");
    for (key, timestamp) in &relay_metrics.event_timestamps {
        output.push_str(&format!(
            "codexctl_central_relay_capacity_event_timestamp_seconds{{account_class=\"{}\",kind=\"{}\",model=\"{}\",outcome=\"{}\"}} {}\n",
            key.account_class, key.kind, key.model, key.outcome, timestamp
        ));
    }
    output.push_str("# TYPE codexctl_central_relay_events_rejected_total counter\n");
    for (reason, count) in &relay_metrics.rejected {
        output.push_str(&format!(
            "codexctl_central_relay_events_rejected_total{{reason=\"{reason}\"}} {count}\n"
        ));
    }
    Ok((
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
        .into_response())
}

async fn relay_capacity_event(
    State(broker): State<Broker>,
    request: Request,
) -> Result<StatusCode, HttpError> {
    let device = broker.authorize(request.headers()).await?;
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_relay_event"))?;
    let event = serde_json::from_slice::<RelayCapacityEvent>(&body)
        .ok()
        .and_then(|event| event.validate().ok());
    let Some(event) = event else {
        broker.relay_reject("invalid_label");
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_relay_event"));
    };
    if !broker.relay_accept(&device.id, event).await? {
        broker.relay_reject("rate_limited");
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "relay_event_rate_limited"));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn observe(State(broker): State<Broker>, request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if (response.status().is_client_error() || response.status().is_server_error())
        && response.extensions().get::<FailureReason>().is_none()
    {
        broker.record_failure("http_rejected", "http", response.status());
    }
    response
}
async fn ready(State(broker): State<Broker>) -> Response {
    let local = users(&broker.state).is_ok() && vault::devices(&broker.state).is_ok();
    let reachable = match broker.central.as_ref() {
        Some(store) => Some(store.reachable().await),
        None => None,
    };
    let healthy = shared_login_ready(
        local,
        reachable,
        broker.central.is_some(),
        broker.login_holder_live.load(Ordering::Acquire),
    );
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mode = broker
        .central
        .as_ref()
        .map_or_else(|| "file".to_owned(), |store| store.mode().to_string());
    (
        status,
        Json(json!({
            "ready": healthy,
            "storeMode": mode,
            "databaseReachable": reachable,
        })),
    )
        .into_response()
}

pub(super) fn readiness(broker: &Broker) -> StatusCode {
    let local = users(&broker.state).is_ok() && vault::devices(&broker.state).is_ok();
    if !shared_login_ready(
        local,
        None,
        broker.central.is_some(),
        broker.login_holder_live.load(Ordering::Acquire),
    ) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

async fn hydrate_accounts(
    state: &Path,
    key: &Path,
    central: &CentralStore,
) -> Result<std::collections::BTreeSet<String>> {
    let mut quarantined = std::collections::BTreeSet::new();
    let identities = if central.mode() == super::storage::StoreMode::Postgres {
        Some(central.retained_identities().await?)
    } else {
        None
    };
    for record in central.list_accounts().await? {
        let account_vault: Vault = serde_json::from_value(record.vault.clone())?;
        vault::validate_auth(&account_vault.auth)?;
        let id = account_key(&account_vault.user, &account_vault.alias);
        let account_state = state.join("accounts").join(&id);
        let replace = if account_state.join("vault.enc").exists() {
            let local = vault::load(&account_state, key)?;
            let baseline = match Owner::read_shared_revision(&account_state) {
                Ok(revision) => Some(revision.unwrap_or(local.revision)),
                Err(_) => {
                    quarantined.insert(id.clone());
                    eprintln!(
                        "{}",
                        json!({"operation":"account_reconcile","stage":"startup","reason":"shared_revision_unreadable"})
                    );
                    None
                }
            };
            let renewed = if let Some(baseline) = baseline {
                identities.is_some()
                    && account_vault.verified
                    && !account_vault.import_rejected
                    && record.revision > baseline
                    && central
                        .login_completed_after(&record.account_id, baseline, record.revision)
                        .await?
            } else {
                false
            };
            let home = account_state.join("runtime");
            let advance = local.revision < record.revision || renewed;
            if let Some(identities) = identities.as_ref() {
                let agreement = (|| -> Result<()> {
                    let identity = identities
                        .get(&record.account_id)
                        .context("shared identity missing")?;
                    identity.validate(&local.auth)?;
                    identity.validate(&account_vault.auth)?;
                    if home.try_exists()? {
                        identity.validate(&retained_auth(&home)?)?;
                        if advance {
                            previous_owner_exited(&home)?;
                        }
                    }
                    Ok(())
                })();
                if agreement.is_err() {
                    quarantined.insert(id.clone());
                    eprintln!(
                        "{}",
                        json!({"operation":"account_reconcile","stage":"startup","reason":"journal_identity_unresolved"})
                    );
                    false
                } else if quarantined.contains(&id) || !advance {
                    false
                } else {
                    if renewed && home.try_exists()? {
                        store::atomic_write(
                            &home.join("auth.json"),
                            &serde_json::to_vec(&account_vault.auth)?,
                        )?;
                    }
                    true
                }
            } else {
                !quarantined.contains(&id) && advance
            }
        } else {
            true
        };
        if replace {
            store::ensure_private_dir(&account_state)?;
            vault::save(&account_state, key, &account_vault)?;
        }
    }
    Ok(quarantined)
}

pub(super) fn retained_auth(home: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&vault::private_read(
        &home.join("auth.json"),
    )?)?)
}

pub(super) fn definitely_not_started(home: &Path) -> bool {
    matches!(home.join("pid").try_exists(), Ok(false))
        && vault::private_read(&home.join("spawn-failed")).is_ok_and(|v| v == b"not-started")
}
pub(super) fn previous_owner_exited(home: &Path) -> Result<()> {
    if !home.try_exists()? {
        return Ok(());
    }
    if definitely_not_started(home) {
        return Ok(());
    }
    let pid_evidence = (|| -> Result<()> {
        let process: super::process::Process =
            serde_json::from_slice(&vault::private_read(&home.join("pid"))?)?;
        if process.alive()? {
            bail!("previous credential owner still exists");
        }
        Ok(())
    })();
    if pid_evidence.is_ok() {
        return pid_evidence;
    }
    if let Some(state) = home.parent()
        && relogin::verifier_parent_exited(state)?
    {
        return Ok(());
    }
    pid_evidence
}

pub(super) fn prepare_owner(state: &Path, key: &Path, read_only: bool) -> Result<Owner> {
    let vault = vault::load(state, key)?;
    vault::validate_auth(&vault.auth)?;
    let home = state.join("runtime");
    let exists = home.try_exists()?;
    store::ensure_private_dir(&home)?;
    if exists {
        if !read_only || home.join("pid").try_exists()? {
            previous_owner_exited(&home)?;
        }
        if !home.join("auth.json").try_exists()? {
            bail!("unfinished runtime has no credentials");
        }
    } else {
        store::atomic_write(&home.join("auth.json"), &serde_json::to_vec(&vault.auth)?)?;
        store::atomic_write(&home.join("spawn-failed"), b"not-started")?;
    }
    let mut owner = Owner {
        vault,
        rpc: None,
        home,
        state: state.into(),
        key: key.into(),
        available: true,
        retryable_unavailable: false,
        retry_requires_billing: false,
        retry_started: None,
        retry_failures: 0,
        recovery_generation: 0,
        shared_revision: Owner::read_shared_revision(state)?,
        routing_refused: false,
        refresh_enabled: !read_only,
        limits: None,
        limits_observed: None,
        verification_input: None,
        import_settling: false,
        #[cfg(test)]
        retry_clock: None,
    };
    // Reconcile the latest disk credentials before any new refresh or reseeding.
    owner.snapshot()?;
    Ok(owner)
}
pub(super) async fn launch_owner(
    owner: &mut Owner,
    binary: &Path,
    proof: relogin::ClearedIdentity<'_>,
) -> Result<()> {
    spawn_owner(owner, binary, proof)?;
    initialize_owner(owner).await
}

fn spawn_owner(
    owner: &mut Owner,
    binary: &Path,
    proof: relogin::ClearedIdentity<'_>,
) -> Result<()> {
    proof.validate(owner)?;
    if !owner.vault.verified {
        owner.vault.import_rejected = false;
        owner.verification_input = Some(owner.vault.auth.clone());
        vault::save(&owner.state, &owner.key, &owner.vault)?;
    }
    let failed = owner.home.join("spawn-failed");
    // Neither a previous dead PID nor a previous failed start proves anything about
    // the next child. Invalidate both records durably before attempting spawn.
    for evidence in [&failed, &owner.home.join("pid")] {
        if evidence.try_exists()? {
            std::fs::remove_file(evidence)?;
        }
    }
    store::sync_directory(&owner.home)?;
    let rpc = match Rpc::spawn_refresh(binary, &owner.home, proof) {
        Ok(rpc) => rpc,
        Err(error) => {
            // Rpc::spawn only returns errors before a Codex process has successfully started.
            store::atomic_write(&failed, b"not-started")?;
            return Err(error);
        }
    };
    owner.rpc = Some(rpc);
    let pid = owner.rpc.as_ref().context("missing owner")?.pid()?;
    store::atomic_write(
        &owner.home.join("pid"),
        &serde_json::to_vec(&super::process::Process::capture(pid)?)?,
    )?;
    Ok(())
}

async fn initialize_owner(owner: &mut Owner) -> Result<()> {
    owner
        .rpc
        .as_mut()
        .context("missing owner")?
        .initialize()
        .await?;
    owner.snapshot()?;
    if !owner.vault.verified {
        let status = owner
            .rpc
            .as_mut()
            .context("missing owner")?
            .remember_exportable_login()
            .await;
        // Even the cached-status call can refresh. Persist its journal and
        // rejection evidence before returning any protocol or loading error.
        owner.snapshot()?;
        if owner.rpc.as_ref().is_some_and(|rpc| rpc.rejected_login()) {
            owner.refresh_failed().await;
        }
        let token = status?;
        if token != vault::token(&owner.vault.auth)? {
            bail!("native owner exported a login that differs from its journal");
        }
        owner.verification_input = Some(owner.vault.auth.clone());
    }
    Ok(())
}

async fn launch_startup_owner(
    owner: &mut Owner,
    binary: &Path,
    imports: &tokio::sync::MutexGuard<'_, ()>,
) -> Result<()> {
    let proof = relogin::identity_inventory(&owner.state, &owner.key, &owner.home)
        .clear_for_launch(owner, relogin::AdmissionKind::Restore, imports)?;
    launch_owner(owner, binary, proof).await?;
    owner.refresh_enabled = true;
    owner.available = true;
    Ok(())
}

async fn recover_unhealthy_owners(broker: &Broker) {
    recover_owners(broker, None).await;
}

async fn recover_owners(broker: &Broker, requested: Option<Arc<Mutex<Owner>>>) {
    let on_demand = requested.is_some();
    if broker.read_only
        || broker.stopping.load(Ordering::Acquire)
        || broker.ownership_unresolved.load(Ordering::Acquire)
    {
        return;
    }
    let owners = match requested {
        Some(owner) => vec![owner],
        None => broker
            .owners
            .read()
            .await
            .values()
            .map(|(_, owner)| owner.clone())
            .collect(),
    };
    for owner_ref in owners {
        if broker.stopping.load(Ordering::Acquire) {
            return;
        }
        let permit = tokio::select! {
            _ = broker.recovery_stop.notified() => return,
            permit = broker.work.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
        };
        if broker.stopping.load(Ordering::Acquire) {
            drop(permit);
            return;
        }
        let mut owner = owner_ref.lock().await;
        let pending = owner.rpc.as_ref().is_some_and(Rpc::completion_pending);
        let exited = owner.rpc.as_mut().is_some_and(Rpc::process_exited);
        if !owner.vault.verified
            || owner.available
            || !owner.retryable_unavailable
            || owner.routing_refused
            || (if on_demand {
                owner.on_demand_cooldown_active()
            } else {
                owner.retry_cooldown_active()
            })
        {
            continue;
        }
        if pending && !exited {
            continue;
        }
        if let Some(rpc) = owner.rpc.as_mut()
            && !rpc.retryable_or_timed_out()
            && !rpc.process_exited()
        {
            continue;
        }
        let central = broker.central.clone();
        let account_id = account_key(&owner.vault.user, &owner.vault.alias);
        let lease = if let Some(central) = central.as_ref() {
            match central
                .acquire_lease(
                    &account_id,
                    &broker.holder_id,
                    std::time::Duration::from_secs(120),
                )
                .await
            {
                Ok(lease) => Some(lease),
                Err(error) => {
                    eprintln!("central background owner lease: {error:#}");
                    continue;
                }
            }
        } else {
            None
        };
        if let (Some(central), Some(lease)) = (central.as_ref(), lease.as_ref())
            && let Err(error) = broker.ensure_owner_record(&mut owner).await
        {
            eprintln!("central background owner record: {error:#}");
            let _ = central.release_lease(lease).await;
            drop(permit);
            continue;
        }
        // Capture the committed baseline before stopping or relaunching the
        // child. launch_owner snapshots its journal, so taking this later can
        // hide a refresh-token rotation completed by the old child.
        let before = owner.vault.clone();
        let renew_done = Arc::new(AtomicBool::new(false));
        let mut renew_task = match (central.clone(), lease.clone()) {
            (Some(central), Some(lease)) => {
                let done = renew_done.clone();
                Some(tokio::spawn(async move {
                    while !done.load(Ordering::Acquire) {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        if done.load(Ordering::Acquire) {
                            break;
                        }
                        match central
                            .renew(&lease, std::time::Duration::from_secs(120))
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => eprintln!("central background lease renewal: {error:#}"),
                        }
                    }
                }))
            }
            _ => None,
        };
        if let Some(mut rpc) = owner.rpc.take() {
            // `process_exited` reaps a failed startup child. Do not wait on a
            // child that is already dead; a live unhealthy child still gets
            // the bounded termination path before the startup launch path.
            if !rpc.process_exited() {
                rpc.terminate().await;
            }
        }
        owner.refresh_enabled = false;
        if owner.retry_started.is_none() {
            owner.retry_started = Some(owner.retry_clock_now());
        }
        // Reacquire in the established imports -> Owner order. Never await the
        // imports mutex while retaining an Owner guard.
        drop(owner);
        let launched = {
            let imports = broker.imports.lock().await;
            let mut owner = owner_ref.lock().await;
            if !owner.vault.verified
                || owner.available
                || !owner.retryable_unavailable
                || owner.routing_refused
                || (if on_demand {
                    owner.on_demand_cooldown_active()
                } else {
                    owner.retry_cooldown_active()
                })
            {
                None
            } else {
                if on_demand {
                    owner.retry_started = Some(owner.retry_clock_now());
                }
                let identities = if let Some(central) = central
                    .as_ref()
                    .filter(|s| s.mode() == super::storage::StoreMode::Postgres)
                {
                    central.retained_identities().await.map(Some)
                } else {
                    Ok(None)
                };
                let result = match identities {
                    Err(error) => Err(error),
                    Ok(identities) => {
                        let inventory =
                            relogin::identity_inventory(&owner.state, &owner.key, &owner.home);
                        let clearance = if let Some(identities) = identities.as_ref() {
                            inventory.clear_for_shared_launch(
                                &owner,
                                relogin::AdmissionKind::Restore,
                                &imports,
                                identities,
                            )
                        } else {
                            inventory.clear_for_launch(
                                &owner,
                                relogin::AdmissionKind::Restore,
                                &imports,
                            )
                        };
                        match clearance {
                            Ok(proof) => {
                                let spawned = spawn_owner(&mut owner, &broker.binary, proof);
                                drop(imports);
                                match spawned {
                                    Ok(()) => initialize_owner(&mut owner).await,
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => {
                                owner.routing_refused = true;
                                Err(error)
                            }
                        }
                    }
                };
                Some(result)
            }
        };
        let mut owner = owner_ref.lock().await;
        let Some(launched) = launched else {
            renew_done.store(true, Ordering::Release);
            if let Some(task) = renew_task.take() {
                task.abort();
            }
            if let (Some(central), Some(lease)) = (central.as_ref(), lease.as_ref()) {
                let _ = central.release_lease(lease).await;
            }
            drop(permit);
            continue;
        };
        if let Err(error) = &launched {
            eprintln!("central background owner identity or launch failure: {error:#}");
        }
        match launched {
            Ok(()) => {
                // Launching proves only that a child exists. Keep the owner
                // fenced until the same account probe that failed succeeds on
                // this new RPC. Billing failures replay the rate-limit read.
                owner.refresh_enabled = true;
                // The gated launch has completed. The owner must be selectable
                // for the probe itself; failures below immediately re-fence it.
                owner.available = true;
                let billing_probe = owner.retry_requires_billing;
                let probe = owner
                    .tokens(TokenRequest {
                        billing: billing_probe,
                        ..Default::default()
                    })
                    .await;
                let auth_changed = owner.vault.auth != before.auth;
                let mut local_saved = true;
                if auth_changed {
                    owner.vault.revision = before.revision.saturating_add(1).max(1);
                    if let Err(error) = vault::save(&owner.state, &owner.key, &owner.vault) {
                        local_saved = false;
                        fence_background_owner(&mut owner);
                        owner.retryable_unavailable = false;
                        eprintln!("central background owner vault save: {error:#}");
                    }
                }
                let probe_pending = owner.rpc.as_ref().is_some_and(Rpc::completion_pending);
                let probe_ok = probe.is_ok();
                if probe_pending || !local_saved || lease.is_some() {
                    if matches!(probe, Err(TokenFailure::Retryable(_))) {
                        owner.retryable_unavailable = owner.vault.verified;
                        owner.retry_failures = owner.retry_failures.saturating_add(1);
                        if owner.retry_started.is_none() {
                            owner.retry_started = Some(owner.retry_clock_now());
                        }
                    }
                    let recovery = if probe_ok && local_saved {
                        SettlementRecovery::Available
                    } else if local_saved && owner.retryable_unavailable && !owner.routing_refused {
                        SettlementRecovery::Retryable
                    } else {
                        SettlementRecovery::Fenced
                    };
                    if lease.is_some() || !local_saved {
                        // Reserve this owner until settlement finishes. A second
                        // same-holder acquire would invalidate the retained epoch.
                        fence_background_owner(&mut owner);
                    } else {
                        fence_background_probe(&mut owner);
                    }
                    let recovery_generation = owner.recovery_generation;
                    let owner_ref = owner_ref.clone();
                    let central = central.clone();
                    let lease = lease.clone();
                    tokio::spawn(settle_background_recovery(
                        owner_ref,
                        central,
                        lease,
                        None,
                        before,
                        recovery,
                        recovery_generation,
                        permit,
                        renew_done,
                        renew_task.take(),
                        broker.stopping.clone(),
                    ));
                    continue;
                }
                let written = if let (Some(central), Some(lease)) =
                    (central.as_ref(), lease.as_ref())
                {
                    let lease_valid = central
                        .renew(lease, std::time::Duration::from_secs(120))
                        .await
                        .unwrap_or(false);
                    match (lease_valid, Broker::owner_record(&owner)) {
                        (true, Ok(record)) => match central.fenced_write(lease, &record).await {
                            Ok(written) => written,
                            Err(error) => {
                                eprintln!("central background owner fenced write: {error:#}");
                                false
                            }
                        },
                        (false, _) => false,
                        (true, Err(error)) => {
                            eprintln!("central background owner record: {error:#}");
                            false
                        }
                    }
                } else {
                    true
                };
                if written && central.is_some() {
                    owner.shared_revision = Some(owner.vault.revision.max(1));
                }
                match (probe, local_saved, written) {
                    (Ok(_), true, true) => {
                        owner.available = true;
                        owner.refresh_enabled = true;
                        owner.routing_refused = false;
                        owner.retryable_unavailable = false;
                        owner.retry_requires_billing = false;
                        owner.retry_started = None;
                        owner.retry_failures = 0;
                    }
                    (Ok(_), true, false) => {
                        fence_background_owner(&mut owner);
                        let recovery_generation = owner.recovery_generation;
                        let owner_ref = owner_ref.clone();
                        let central = central.clone();
                        let lease = lease.clone();
                        tokio::spawn(settle_background_recovery(
                            owner_ref,
                            central,
                            lease,
                            None,
                            before,
                            SettlementRecovery::Available,
                            recovery_generation,
                            permit,
                            renew_done,
                            renew_task.take(),
                            broker.stopping.clone(),
                        ));
                        continue;
                    }
                    (Err(TokenFailure::Retryable(error)), true, true) => {
                        owner.available = false;
                        owner.refresh_enabled = false;
                        owner.retryable_unavailable = owner.vault.verified;
                        owner.retry_failures = owner.retry_failures.saturating_add(1);
                        if owner.retry_started.is_none() {
                            owner.retry_started = Some(owner.retry_clock_now());
                        }
                        eprintln!("central background owner probe failed: {error:#}");
                    }
                    (Err(_error), true, true) => {
                        owner.available = false;
                        owner.refresh_enabled = false;
                        owner.retryable_unavailable = false;
                        owner.retry_started = None;
                        eprintln!("central background owner probe refused");
                    }
                    (Err(_), true, false) => {
                        fence_background_owner(&mut owner);
                        let recovery_generation = owner.recovery_generation;
                        let owner_ref = owner_ref.clone();
                        let central = central.clone();
                        let lease = lease.clone();
                        tokio::spawn(settle_background_recovery(
                            owner_ref,
                            central,
                            lease,
                            None,
                            before,
                            SettlementRecovery::Fenced,
                            recovery_generation,
                            permit,
                            renew_done,
                            renew_task.take(),
                            broker.stopping.clone(),
                        ));
                        continue;
                    }
                    (_, false, _) => unreachable!("local save failure enters settlement path"),
                }
            }
            Err(error) => {
                owner.available = false;
                owner.retry_failures = owner.retry_failures.saturating_add(1);
                if owner.retry_started.is_none() {
                    owner.retry_started = Some(owner.retry_clock_now());
                }
                eprintln!("central background owner recovery failed: {error:#}");
                if lease.is_some() {
                    let recovery = if owner.retryable_unavailable && !owner.routing_refused {
                        SettlementRecovery::Retryable
                    } else {
                        SettlementRecovery::Fenced
                    };
                    fence_background_owner(&mut owner);
                    let recovery_generation = owner.recovery_generation;
                    drop(owner);
                    tokio::spawn(settle_background_recovery(
                        owner_ref.clone(),
                        central.clone(),
                        lease.clone(),
                        None,
                        before,
                        recovery,
                        recovery_generation,
                        permit,
                        renew_done,
                        renew_task.take(),
                        broker.stopping.clone(),
                    ));
                    continue;
                }
            }
        }
        renew_done.store(true, Ordering::Release);
        if let Some(task) = renew_task.take() {
            task.abort();
        }
        if let (Some(central), Some(lease)) = (central.as_ref(), lease.as_ref()) {
            let _ = central.release_lease(lease).await;
        }
        drop(permit);
    }
}

fn recovery_interval() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Some(milliseconds) = std::env::var("CENTRAL_TEST_RECOVERY_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return std::time::Duration::from_millis(milliseconds);
    }
    std::time::Duration::from_secs(60)
}

#[allow(clippy::too_many_arguments)] // Server configuration, not a request API.
pub async fn serve(
    state: &Path,
    key: &Path,
    address: SocketAddr,
    binary: &Path,
    read_only: bool,
    public_url: &str,
    sso_config: Option<&Path>,
    metrics_token_file: Option<&Path>,
) -> Result<()> {
    transport::origin(public_url)?;
    if !address.ip().is_loopback() && reqwest::Url::parse(public_url)?.scheme() != "https" {
        bail!("network listener requires an HTTPS ingress origin");
    }
    let _lock = vault::lock(state, "owner.lock")?;
    // Login records are replica-local; a shared store cannot resume or retire
    // them, so it refuses to start beside one rather than strand its account.
    if super::storage::StoreMode::from_env()? != super::storage::StoreMode::File
        && (relogin::add::pending_logins(state)? || super::rename::pending(state)?)
    {
        bail!(
            "pending server login or rename operations exist; finish pending logins in file mode before switching storage"
        );
    }
    let configured = super::storage::runtime_store(state, key).await?;
    // File mode deliberately keeps the central store detached: the existing
    // vault remains the authoritative local path with no token-request cost.
    let central = match configured {
        CentralStore::File(_) => None,
        store => Some(store),
    };
    let hydration_quarantines = if let Some(central) = central.as_ref() {
        central.require_identity_ready().await?;
        hydrate_accounts(state, key, central).await?
    } else {
        Default::default()
    };
    let listener = tokio::net::TcpListener::bind(address).await?;
    users(state)?;
    vault::devices(state)?;
    let sso = match sso_config {
        Some(path) => Some(Arc::new(enrollment::Sso::load(path, public_url).await?)),
        None if address.ip().is_loopback() => None,
        None => bail!("network server requires company SSO configuration"),
    };
    let imports = Arc::new(Mutex::new(()));
    let startup_import = imports.lock().await;
    let mut owners = BTreeMap::new();
    let mut recovery_failures = 0;
    let mut ownership_unresolved = false;
    let mut replacements_blocked = false;
    let mut conflicting_journals = Vec::new();
    let mut repairing = std::collections::BTreeSet::new();
    // Finish interrupted renames first: the inventory below requires each
    // directory name to match its vault alias.
    if super::rename::recover(state, key).is_err() {
        replacements_blocked = true;
        ownership_unresolved = true;
    }
    // Resolve all durable commits before inventorying quarantines from other accounts.
    for entry in std::fs::read_dir(state.join("accounts"))? {
        if relogin::recover(&entry?.path(), key).is_err() {
            replacements_blocked = true;
            ownership_unresolved = true;
        }
    }
    // A saved new-account grant fences every owner it overlaps until its
    // admission resumes. An unidentified login child can hold any identity.
    match relogin::add::recover(state, key) {
        Ok(reserved) => conflicting_journals.extend(reserved),
        Err(_) => {
            replacements_blocked = true;
            ownership_unresolved = true;
        }
    }
    for entry in std::fs::read_dir(state.join("accounts"))? {
        let entry = entry?;
        let inventory =
            relogin::identity_inventory(&entry.path(), key, &entry.path().join("runtime"));
        let journal_conflicts = inventory.journal_conflicts();
        let stopped = inventory.runtime == relogin::ProcessState::Stopped;
        match inventory.candidates {
            Ok(candidates) => conflicting_journals.extend(candidates.into_iter().map(|c| c.auth)),
            Err(_) => {
                replacements_blocked = true;
                ownership_unresolved = true;
            }
        }
        let stored = inventory.saved;
        let account_vault = match stored {
            Ok(v)
                if entry.file_type()?.is_dir()
                    && entry.file_name() == account_key(&v.user, &v.alias).as_str() =>
            {
                v
            }
            _ => {
                recovery_failures += 1;
                ownership_unresolved = true;
                replacements_blocked |= !stopped;
                match inventory.journal {
                    Ok(Some(auth)) => conflicting_journals.push(auth),
                    Ok(None) => {}
                    Err(_) => replacements_blocked = true,
                }
                continue;
            }
        };
        let repair = relogin::recover(&entry.path(), key);
        if repair.is_err() {
            replacements_blocked = true;
            ownership_unresolved = true;
        }
        let repair = repair.unwrap_or_default();
        let pending = !account_vault.verified && !repair.verify;
        let id = account_key(&account_vault.user, &account_vault.alias);
        if repair.verify {
            repairing.insert(id.clone());
        }
        let prepared = if hydration_quarantines.contains(&id) {
            Err(anyhow::anyhow!("shared hydration identity unresolved"))
        } else {
            // Shared-store replicas hydrate credentials without starting a native
            // refresh owner. The token path acquires the account's database lease
            // before launching one, including its initialize call.
            prepare_owner(
                &entry.path(),
                key,
                read_only || pending || central.is_some(),
            )
        };
        let mut owner = match prepared {
            Ok(o) => o,
            Err(_) => {
                recovery_failures += 1;
                Owner {
                    vault: account_vault,
                    rpc: None,
                    home: entry.path().join("runtime"),
                    state: entry.path(),
                    key: key.into(),
                    available: false,
                    retryable_unavailable: false,
                    retry_requires_billing: false,
                    retry_started: None,
                    retry_failures: 0,
                    recovery_generation: 0,
                    shared_revision: None,
                    routing_refused: true,
                    refresh_enabled: false,
                    limits: None,
                    limits_observed: None,
                    verification_input: None,
                    import_settling: false,
                    #[cfg(test)]
                    retry_clock: None,
                }
            }
        };
        if pending || repair.blocked || ((read_only || central.is_some()) && repair.verify) {
            owner.available = false;
        }
        // Use the same complete identity inventory as live import and renewal.
        if journal_conflicts {
            replacements_blocked |= !stopped;
            match inventory.journal {
                Ok(Some(auth)) if stopped => {
                    conflicting_journals.push(owner.vault.auth.clone());
                    conflicting_journals.push(auth);
                }
                _ => ownership_unresolved = true,
            }
        }
        let identity = AccountIndex {
            user: owner.vault.user.clone(),
            alias: owner.vault.alias.trim().into(),
        };
        let owner = Arc::new(Mutex::new(owner));
        owners.insert(id, (identity, owner));
    }
    // Inventory every retained runtime before starting replacements. An unresolved
    // process could hold any seat, so do not launch against incomplete evidence.
    for (_, owner) in owners.values() {
        let mut owner = owner.lock().await;
        if replacements_blocked
            || (!repairing.contains(&account_key(&owner.vault.user, &owner.vault.alias))
                && conflicting_journals.iter().any(|auth| {
                    // This is an overlap fence, not permission to replace auth.
                    // Missing or contradictory UID evidence cannot free a seat
                    // whose workspace and subject already agree.
                    overlaps(&owner.vault.auth, auth)
                }))
        {
            owner.available = false;
            continue;
        }
        if owner.available
            && owner.refresh_enabled
            && !repairing.contains(&account_key(&owner.vault.user, &owner.vault.alias))
            && launch_startup_owner(&mut owner, binary, &startup_import)
                .await
                .is_err()
        {
            owner.available = false;
            recovery_failures += 1;
        }
        if owner.available
            && owner.refresh_enabled
            && relogin::needs_verification(&owner.state)?
            && relogin::verify_replacement(&mut owner, binary, &startup_import)
                .await
                .is_err()
        {
            owner.available = false;
            recovery_failures += 1;
        }
    }
    drop(startup_import);
    let registry = if let Some(central) = central.as_ref() {
        // Convert the phase-two array snapshot to entity rows once.  After
        // this point authorization and mutations use only per-entity rows;
        // no pod can replace the shared registry with its local array.
        let existing_users = central
            .load_registry_entity_revisions("users")
            .await?
            .into_iter()
            .map(|(id, _, _)| id)
            .collect::<std::collections::BTreeSet<_>>();
        for user in self::users(state).unwrap_or_default() {
            if !existing_users.contains(&user.id) {
                central
                    .save_registry_entity_cas("users", &user.id, &serde_json::to_vec(&user)?, None)
                    .await?;
            }
        }
        for device in vault::devices(state).unwrap_or_default() {
            // Insert-only CAS preserves an existing central row without
            // reading a cross-tenant device snapshot during startup.
            let _ = central
                .save_device_entity_cas(
                    &device.tenant,
                    &device.id,
                    &serde_json::to_vec(&device)?,
                    None,
                )
                .await?;
        }
        let users = central_registry_users(central).await?;
        Some(Arc::new(std::sync::RwLock::new(RegistryState {
            users,
            // PostgreSQL device authorization is queried per request.
            devices: Vec::new(),
        })))
    } else {
        None
    };
    let holder = instance_holder_id();
    let broker = Broker {
        activity: Arc::default(),
        state: state.into(),
        key: key.into(),
        binary: binary.into(),
        read_only,
        ownership_unresolved: Arc::new(AtomicBool::new(ownership_unresolved)),
        owners: Arc::new(RwLock::new(owners)),
        imports,
        sso,
        reset_reader: super::resets::Reader::new()?,
        catalog: Arc::new(catalog::Reader::new()?),
        failures: Arc::new(StdMutex::new(
            [
                "owner_unavailable",
                "catalog_owner_unavailable",
                "reset_read_failed",
                "reset_redeem_failed",
                "reset_rejected",
                "reset_auth_rejected",
                "catalog_usage_failed",
                "catalog_usage_timeout",
                "live_session_failed",
                "catalog_task_failed",
                "persistence_failed",
                "registry_unavailable",
                "registry_busy",
                "import_unavailable",
                "recovery_failed",
            ]
            .into_iter()
            .map(|r| (r, Failure::default()))
            .collect(),
        )),
        relay_metrics: Arc::new(StdMutex::new(RelayMetrics::default())),
        work: Arc::new(Semaphore::new(128)),
        session_writes: Arc::new(Semaphore::new(32)),
        stopping: Arc::new(AtomicBool::new(false)),
        recovery_stop: Arc::new(tokio::sync::Notify::new()),
        background_recovery: background_recovery_enabled(),
        relogins: Arc::new(StdMutex::new(BTreeMap::new())),
        shared_login_workers: Arc::new(StdMutex::new(Default::default())),
        holder_id: holder.clone(),
        login_holder: Arc::new(StdMutex::new(holder)),
        login_holder_live: Arc::new(AtomicBool::new(true)),
        registry,
        central,
        metrics_hash: metrics_token_file
            .map(|p| {
                let bytes = vault::private_read(p)?;
                let token = std::str::from_utf8(&bytes)
                    .context("metrics token must contain UTF-8 text")?
                    .trim();
                if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
                    bail!("metrics token must contain at least 32 visible ASCII characters");
                }
                Ok(vault::digest(token.as_bytes()))
            })
            .transpose()?,
    };
    let (holder_stop, mut holder_stop_rx) = tokio::sync::oneshot::channel();
    let holder_task = if let Some(shared) = broker.central.as_ref() {
        let initial_holder = broker.login_holder();
        shared.login_register_holder(&initial_holder).await?;
        let holder_store = shared.clone();
        let holder_broker = broker.clone();
        Some(tokio::spawn(async move {
            let mut last_renewal = std::time::Instant::now();
            let mut holder_id = initial_holder;
            loop {
                tokio::select! {
                    _ = &mut holder_stop_rx => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                        let renewed = holder_store.login_renew_holder(&holder_id).await;
                        if holder_renewal_lost(&renewed, last_renewal.elapsed()) {
                            holder_broker.login_holder_live.store(false, Ordering::Release);
                            let error = match renewed {
                                Err(error) => format!("{error:#}"),
                                _ => "login holder incarnation expired".into(),
                            };
                            eprintln!("{}", json!({"operation":"login_holder","stage":"heartbeat","reason":"lease_lost","error":error}));
                            holder_broker.record_failure("relogin_failed", "login_holder", StatusCode::SERVICE_UNAVAILABLE);
                            loop {
                                tokio::select! {
                                    _ = &mut holder_stop_rx => return,
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                                }
                                let replacement = instance_holder_id();
                                match holder_store.login_register_holder(&replacement).await {
                                    Ok(()) => {
                                        // Give up the abandoned incarnation now, so other
                                        // replicas can recover its operations at once.
                                        if let Err(error) = holder_store.login_release_holder(&holder_id).await {
                                            eprintln!("{}", json!({"operation":"login_holder","stage":"heartbeat","reason":"abandoned_release_failed","error":format!("{error:#}")}));
                                        }
                                        holder_broker.replace_login_holder(replacement.clone());
                                        holder_id = replacement;
                                        last_renewal = std::time::Instant::now();
                                        holder_broker.login_holder_live.store(true, Ordering::Release);
                                        eprintln!("{}", json!({"operation":"login_holder","stage":"heartbeat","reason":"lease_recovered"}));
                                        break;
                                    }
                                    Err(error) => {
                                        eprintln!("{}", json!({"operation":"login_holder","stage":"heartbeat","reason":"lease_recovery_retry","error":format!("{error:#}")}));
                                    }
                                }
                            }
                            continue;
                        }
                        match renewed {
                            Ok(true) => last_renewal = std::time::Instant::now(),
                            Ok(false) => {}
                            Err(error) => {
                                eprintln!("{}", json!({"operation":"login_holder","stage":"heartbeat","reason":"lease_renewal_retry","error":format!("{error:#}")}));
                            }
                        }
                    }
                }
            }
        }))
    } else {
        None
    };
    for _ in 0..recovery_failures {
        broker.record_failure(
            "recovery_failed",
            "startup",
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    let recovery_broker = broker.clone();
    let recovery_task = tokio::spawn(async move {
        if !recovery_broker.background_recovery {
            return;
        }
        loop {
            if recovery_broker.stopping.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {
                _ = recovery_broker.recovery_stop.notified() => break,
                _ = tokio::time::sleep(recovery_interval()) => {
                    if recovery_broker.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    recover_unhealthy_owners(&recovery_broker).await;
                }
            }
        }
    });
    let app = enrollment::routes(api_routes().merge(super::dashboard::routes(public_url)))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(broker.clone(), observe))
        .with_state(broker.clone());
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    println!("{}", json!({"listening":listener.local_addr()?}));
    let stopping = broker.stopping.clone();
    let recovery_stop = broker.recovery_stop.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                tokio::select! {_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
                stopping.store(true, Ordering::Release);
                recovery_stop.notify_one();
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                stopping.store(true, Ordering::Release);
                recovery_stop.notify_one();
            }
        })
        .await?;
    broker.recovery_stop.notify_one();
    let _ = recovery_task.await;
    let _drain = broker.work.clone().acquire_many_owned(128).await?;
    let owners: Vec<_> = broker
        .owners
        .read()
        .await
        .values()
        .map(|(_, owner)| owner.clone())
        .collect();
    let tasks = owners.into_iter().map(|owner| async move {
        let mut owner = owner.lock().await;
        if !owner.available && owner.rpc.is_none() {
            return Ok(());
        }
        if let Some(rpc) = owner.rpc.as_mut() {
            rpc.shutdown().await?;
        }
        owner.snapshot()?;
        // Retain the process incarnation as evidence. A dead incarnation cannot block restart.
        Ok::<(), anyhow::Error>(())
    });
    for result in futures::future::join_all(tasks).await {
        result?;
    }
    if let Some(task) = holder_task {
        let _ = holder_stop.send(());
        task.await.context("login holder heartbeat task failed")?;
        if let Some(shared) = broker.central.as_ref() {
            let holder_id = broker.login_holder();
            shared.login_release_holder(&holder_id).await?;
        }
    }
    Ok(())
}

fn background_recovery_enabled() -> bool {
    std::env::var("CODEXCTL_CENTRAL_BACKGROUND_RECOVERY")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

impl Broker {
    /// A file-mode broker over prepared owners, for tests outside this module.
    #[cfg(test)]
    pub(super) fn testing(
        state: PathBuf,
        key: PathBuf,
        owners: Vec<(String, AccountIndex, Owner)>,
        catalog: catalog::Reader,
    ) -> Self {
        Self {
            state,
            key,
            binary: "unused".into(),
            read_only: true,
            ownership_unresolved: Arc::new(AtomicBool::new(false)),
            owners: Arc::new(RwLock::new(
                owners
                    .into_iter()
                    .map(|(key, index, owner)| (key, (index, Arc::new(Mutex::new(owner)))))
                    .collect(),
            )),
            imports: Arc::new(Mutex::new(())),
            sso: None,
            activity: Arc::default(),
            reset_reader: super::resets::Reader::new().expect("reset reader"),
            catalog: Arc::new(catalog),
            failures: Arc::new(StdMutex::new(BTreeMap::new())),
            relay_metrics: Arc::new(StdMutex::new(RelayMetrics::default())),
            metrics_hash: None,
            work: Arc::new(Semaphore::new(128)),
            session_writes: Arc::new(Semaphore::new(32)),
            stopping: Arc::new(AtomicBool::new(false)),
            recovery_stop: Arc::new(tokio::sync::Notify::new()),
            background_recovery: false,
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
            shared_login_workers: Arc::new(StdMutex::new(Default::default())),
            central: None,
            holder_id: "test-holder".into(),
            login_holder: Arc::new(StdMutex::new("test-holder".into())),
            login_holder_live: Arc::new(AtomicBool::new(true)),
            registry: None,
        }
    }
}

/// The machine-facing API. The dashboard and enrollment routes are added by `serve`.
pub(super) fn api_routes() -> Router<Broker> {
    Router::new()
        .route("/v1/token", post(token))
        .route("/v1/accounts", get(accounts).post(import))
        .route("/v1/accounts/rename", post(super::rename::rename))
        .route("/v1/accounts/renamed", get(super::rename::renamed_aliases))
        .merge(super::resets::routes())
        .merge(super::loans::http::routes())
        .route("/v1/me", get(me))
        .route("/v1/devices", get(devices))
        .route("/v1/devices/revoke", post(revoke_device))
        .route("/v1/relogin/start", post(relogin::start))
        .route("/v1/relogin/status", post(relogin::status))
        .route("/v1/relogin/cancel", post(relogin::cancel))
        .route("/v1/accounts/login/start", post(relogin::add::start))
        .route("/v1/accounts/login/status", post(relogin::add::status))
        .route("/v1/accounts/login/cancel", post(relogin::add::cancel))
        .route("/v1/relay/capacity-events", post(relay_capacity_event))
        .route("/metrics", get(metrics))
        .route("/ready", get(ready))
        .route("/health", get(|| async { StatusCode::OK }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn fenced_shared_login_replica_is_not_ready() {
        assert!(!super::shared_login_ready(true, Some(true), true, false));
        assert!(super::shared_login_ready(true, Some(true), true, true));
        assert!(super::shared_login_ready(true, None, false, false));
    }

    #[test]
    fn transient_holder_renewal_error_keeps_admission_live_within_lease_window() {
        let error = Err(anyhow::Error::msg("database unavailable"));

        assert!(!super::holder_renewal_lost(
            &error,
            std::time::Duration::from_secs(19),
        ));
        assert!(super::holder_renewal_lost(
            &error,
            std::time::Duration::from_secs(20),
        ));
        assert!(super::holder_renewal_lost(
            &Ok(false),
            std::time::Duration::ZERO,
        ));
        assert!(!super::holder_renewal_lost(
            &Ok(true),
            std::time::Duration::ZERO,
        ));
    }

    #[test]
    fn holder_id_changes_for_each_boot() {
        assert_ne!(super::instance_holder_id(), super::instance_holder_id());
    }

    #[test]
    fn interrupted_registry_setup_keeps_the_completion_marker_absent_and_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        std::fs::create_dir_all(state.join("devices.json")).unwrap();
        assert!(super::setup(&state, &key).is_err());
        assert!(!state.join("users.json").exists());
        let original_key = std::fs::read(&key).unwrap();
        std::fs::remove_dir(state.join("devices.json")).unwrap();
        super::setup(&state, &key).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), original_key);
        assert!(super::users(&state).unwrap().is_empty());
        assert!(crate::central::vault::devices(&state).unwrap().is_empty());
    }

    use super::*;
    #[tokio::test]
    async fn catalog_discovery_cannot_take_an_import_before_owner_initialization() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let key = root.path().join("key");
        setup(&state, &key).unwrap();
        record_user(&state, "test-user", "synthetic@sawmills.ai").unwrap();
        let credential = root.path().join("device");
        crate::central::register(&state, "device", "sawmills", "test-user", &credential).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                std::fs::read_to_string(credential).unwrap().trim()
            )
            .parse()
            .unwrap(),
        );
        let mode = root.path().join("mode");
        let counter = root.path().join("counter");
        store::atomic_write(&mode, b"").unwrap();
        store::atomic_write(&counter, b"0").unwrap();
        let binary = root.path().join("synthetic-codex");
        let quoted = |p: &Path| format!("'{}'", p.to_str().unwrap().replace('\'', "'\"'\"'"));
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        store::atomic_write(&binary,format!("#!/bin/sh\nexport CENTRAL_TEST_MODE_FILE={}\nexport CENTRAL_TEST_REFRESH_COUNTER={}\nexec {} \"$@\"\n",quoted(&mode),quoted(&counter),quoted(&fixture)).as_bytes()).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let central =
            CentralStore::from_mode(crate::central::storage::StoreMode::File, &state, &key)
                .await
                .unwrap();
        central.migrate().await.unwrap();
        let broker = Broker {
            state,
            key,
            binary,
            read_only: false,
            ownership_unresolved: Arc::new(AtomicBool::new(false)),
            owners: Arc::new(RwLock::new(BTreeMap::new())),
            imports: Arc::new(Mutex::new(())),
            sso: None,
            activity: Arc::default(),
            reset_reader: crate::central::resets::Reader::new().unwrap(),
            catalog: Arc::new(catalog::Reader::new().unwrap()),
            failures: Arc::new(StdMutex::new(BTreeMap::new())),
            relay_metrics: Arc::new(StdMutex::new(RelayMetrics::default())),
            metrics_hash: None,
            work: Arc::new(Semaphore::new(128)),
            session_writes: Arc::new(Semaphore::new(32)),
            stopping: Arc::new(AtomicBool::new(false)),
            recovery_stop: Arc::new(tokio::sync::Notify::new()),
            background_recovery: false,
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
            shared_login_workers: Arc::new(StdMutex::new(Default::default())),
            central: Some(central),
            holder_id: "test-holder".into(),
            login_holder: Arc::new(StdMutex::new("test-holder".into())),
            login_holder_live: Arc::new(AtomicBool::new(true)),
            registry: None,
        };
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = json!({"sub":"synthetic-login","iat":2000000000_u64,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat","chatgpt_plan_type":"pro"}});
        let auth = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":"synthetic-refresh","account_id":"synthetic-seat"}});
        let mut imported = Box::pin(broker.import_account(
            broker.work.clone().acquire_owned().await.unwrap(),
            "test-user",
            Import {
                alias: "personal".into(),
                label: None,
                auth,
            },
        ));
        // Force scheduler preemption at publication's last lock boundary. A catalog
        // task has a fresh cooperative budget and can run before import resumes.
        tokio::task::yield_now().await;
        for _ in 0..125 {
            tokio::task::consume_budget().await;
        }
        assert!(futures::poll!(&mut imported).is_pending());
        let catalog_broker = broker.clone();
        let catalog = tokio::spawn(async move { accounts(State(catalog_broker), headers).await })
            .await
            .unwrap();
        assert!(catalog.is_ok());
        let result = imported.await;
        assert!(
            result.is_ok(),
            "catalog discovery invalidated an initializing import"
        );
        assert!(result.unwrap_or_else(|_| unreachable!()).available);
        for (_, owner) in broker.owners.read().await.values() {
            if let Some(rpc) = owner.lock().await.rpc.as_mut() {
                rpc.shutdown().await.unwrap();
            }
        }
    }

    pub(super) fn recovery_auth(generation: Option<i64>, refresh: &str) -> Value {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = serde_json::json!({"sub":"synthetic-login", "iat":generation, "https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat"}});
        serde_json::json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":refresh,"account_id":"synthetic-seat"}})
    }
    fn recovery_fixture(saved: Value, journal: Value) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[7; 32]).unwrap();
        vault::save(
            root.path(),
            &key,
            &vault::Vault {
                alias: "test".into(),
                tenant: "test".into(),
                user: "user".into(),
                auth: saved,
                label: None,
                verified: true,
                import_rejected: false,
                revision: 0,
            },
        )
        .unwrap();
        store::atomic_write(
            &root.path().join("runtime/auth.json"),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        (root, key)
    }
    #[test]
    fn recovery_preserves_the_newer_vault_and_reseeds_the_dead_journal() {
        let newer = recovery_auth(Some(2), "new-refresh");
        let (root, key) = recovery_fixture(newer.clone(), recovery_auth(Some(1), "old-refresh"));
        let owner = prepare_owner(root.path(), &key, true).unwrap();
        assert_eq!(owner.vault.auth, newer);
        assert_eq!(vault::load(root.path(), &key).unwrap().auth, newer);
        assert_eq!(
            serde_json::from_slice::<Value>(
                &vault::private_read(&root.path().join("runtime/auth.json")).unwrap()
            )
            .unwrap(),
            newer
        );
    }
    #[test]
    fn recovery_refuses_unordered_credentials_without_overwriting_either_copy() {
        let saved = recovery_auth(None, "saved-refresh");
        let journal = recovery_auth(None, "journal-refresh");
        let (root, key) = recovery_fixture(saved.clone(), journal.clone());
        assert!(prepare_owner(root.path(), &key, true).is_err());
        assert_eq!(vault::load(root.path(), &key).unwrap().auth, saved);
        assert_eq!(
            serde_json::from_slice::<Value>(
                &vault::private_read(&root.path().join("runtime/auth.json")).unwrap()
            )
            .unwrap(),
            journal
        );
    }
    #[tokio::test]
    async fn a_definite_spawn_failure_allows_retry_without_relaxing_unknown_process_fences() {
        let auth = recovery_auth(Some(1), "refresh");
        let (root, key) = recovery_fixture(auth.clone(), auth);
        store::atomic_write(&root.path().join("runtime/spawn-failed"), b"not-started").unwrap();
        let mut owner = prepare_owner(root.path(), &key, true).unwrap();
        let lock = Mutex::new(());
        let guard = lock.lock().await;
        let proof = relogin::identity_inventory(&owner.state, &key, &owner.home)
            .clear_for_launch(&owner, relogin::AdmissionKind::Restore, &guard)
            .unwrap();
        assert!(
            launch_owner(&mut owner, &root.path().join("missing-codex"), proof)
                .await
                .is_err()
        );
        assert!(owner.rpc.is_none());
        assert!(previous_owner_exited(&owner.home).is_ok());
        assert!(prepare_owner(root.path(), &key, false).is_ok());
    }
    #[tokio::test]
    async fn replacement_launch_invalidates_the_previous_process_record_before_spawn() {
        let auth = recovery_auth(Some(1), "refresh");
        let (root, key) = recovery_fixture(auth.clone(), auth);
        let home = root.path().join("runtime");
        store::atomic_write(
            &home.join("pid"),
            &serde_json::to_vec(
                &serde_json::json!({"pid":std::process::id(),"incarnation":"dead-incarnation"}),
            )
            .unwrap(),
        )
        .unwrap();
        let mut owner = prepare_owner(root.path(), &key, false).unwrap();
        let lock = Mutex::new(());
        let guard = lock.lock().await;
        let proof = relogin::identity_inventory(&owner.state, &key, &owner.home)
            .clear_for_launch(&owner, relogin::AdmissionKind::Restore, &guard)
            .unwrap();
        assert!(
            launch_owner(&mut owner, &root.path().join("missing-codex"), proof)
                .await
                .is_err()
        );
        assert!(!home.join("pid").exists());
        assert!(previous_owner_exited(&home).is_ok());
    }
    #[tokio::test]
    async fn launch_clearance_rejects_changed_journal_before_invalidating_exit_evidence() {
        let auth = recovery_auth(Some(1), "refresh");
        let (root, key) = recovery_fixture(auth.clone(), auth);
        store::atomic_write(&root.path().join("runtime/spawn-failed"), b"not-started").unwrap();
        let mut refresh = prepare_owner(root.path(), &key, true).unwrap();
        let lock = Mutex::new(());
        let guard = lock.lock().await;
        let proof = relogin::identity_inventory(&refresh.state, &key, &refresh.home)
            .clear_for_launch(&refresh, relogin::AdmissionKind::Restore, &guard)
            .unwrap();
        let changed = recovery_auth(Some(2), "unexpected-refresh");
        store::atomic_write(
            &refresh.home.join("auth.json"),
            &serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        let error = launch_owner(&mut refresh, &root.path().join("missing-codex"), proof)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("journal changed"));
        assert!(refresh.rpc.is_none());
        assert!(previous_owner_exited(&refresh.home).is_ok());
    }

    #[test]
    fn surviving_process_identity_blocks_recovery_even_without_an_rpc_handle() {
        let root = tempfile::tempdir().unwrap();
        let process = super::super::process::Process::capture(std::process::id()).unwrap();
        store::atomic_write(
            &root.path().join("pid"),
            &serde_json::to_vec(&process).unwrap(),
        )
        .unwrap();
        assert!(previous_owner_exited(root.path()).is_err());
    }
    #[test]
    fn missing_process_identity_blocks_recovery_of_an_existing_journal() {
        let root = tempfile::tempdir().unwrap();
        store::atomic_write(&root.path().join("auth.json"), b"{}").unwrap();
        assert!(previous_owner_exited(root.path()).is_err());
    }
}

#[cfg(test)]
#[path = "reset_tests.rs"]
mod reset_tests;

#[cfg(test)]
mod catalog_tests;
