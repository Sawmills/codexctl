//! Multi-user broker. One process and one persistent disk own every refresh token.
use super::{
    catalog, enrollment, relogin,
    rpc::Rpc,
    server::{Owner, TokenFailure, TokenRequest},
    storage::{CentralStore, CredentialRecord},
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
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, RwLock, Semaphore};

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
// Overlap retains a seat reservation even if UID evidence is missing or conflicts.
// It is never permission to replace credentials.
pub(super) fn overlaps(left: &Value, right: &Value) -> bool {
    vault::account(left).ok() == vault::account(right).ok()
        && api::token_subject(vault::token(left).unwrap_or(""))
            == api::token_subject(vault::token(right).unwrap_or(""))
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
    metrics_hash: Option<String>,
    pub(super) work: Arc<Semaphore>,
    pub(super) stopping: Arc<AtomicBool>,
    pub(super) relogins: Arc<StdMutex<BTreeMap<String, Arc<AtomicBool>>>>,
    pub(super) central: Option<CentralStore>,
    pub(super) holder_id: String,
}
pub(super) struct HttpError {
    status: StatusCode,
    reason: &'static str,
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({"error":self.reason}))).into_response();
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
pub(super) fn account_key(user: &str, alias: &str) -> String {
    vault::digest(format!("{user}\0{}", alias.to_ascii_lowercase()).as_bytes())
}

fn instance_holder_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|v| v.trim().to_owned()))
        .unwrap_or_else(|_| "unknown-host".into());
    let boot_nonce = vault::digest(&enrollment::random_bytes());
    vault::digest(format!("{host}:{}:{boot_nonce}", std::process::id()).as_bytes())
}

fn credential_revision(auth: &Value) -> i64 {
    vault::token(auth)
        .ok()
        .and_then(|token| api::token_issued_at(token).or_else(|| api::token_expiry(token)))
        .unwrap_or(0)
}
fn account_summary(owner: &Owner) -> Account {
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
        available: owner.available && !owner.routing_refused,
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
    }
}

impl Broker {
    async fn persist_owner(&self, owner: &Owner) -> Result<()> {
        let Some(central) = self.central.as_ref() else {
            return Ok(());
        };
        let record = CredentialRecord {
            account_id: account_key(&owner.vault.user, &owner.vault.alias),
            user_id: Some(owner.vault.user.clone()),
            alias: owner.vault.alias.clone(),
            workspace: Some(vault::account(&owner.vault.auth)?),
            login: vault::token(&owner.vault.auth)
                .ok()
                .and_then(api::token_subject),
            vault: serde_json::to_value(&owner.vault)?,
            revision: credential_revision(&owner.vault.auth),
        };
        central.save_account(&record).await
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
        HttpError { status, reason }
    }
    pub fn authorize(&self, headers: &HeaderMap) -> Result<vault::Device, HttpError> {
        let bearer = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        let devices = vault::devices(&self.state)
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
        let hash = vault::digest(bearer.as_bytes());
        let device = devices
            .into_iter()
            .find(|d| d.token_hash == hash && !d.revoked && d.tenant == "sawmills")
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        if !users(&self.state)
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
            .iter()
            .any(|u| u.id == device.user && u.enabled)
        {
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
                    self.error(StatusCode::NOT_FOUND, "account_not_found")
                }
            })
    }
    async fn resolve_alias(
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
        Ok(selected)
    }
    fn owner_failure(&self, error: TokenFailure) -> HttpError {
        match error {
            TokenFailure::AccountMismatch => self.error(StatusCode::CONFLICT, "account_mismatch"),
            TokenFailure::RefreshDisabled => self.error(StatusCode::CONFLICT, "refresh_disabled"),
            TokenFailure::UnsupportedRouting => {
                self.error(StatusCode::CONFLICT, "unsupported_workspace_routing")
            }
            TokenFailure::Unavailable(_) => {
                self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
            }
        }
    }
}

async fn token(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<TokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let alias = request
        .alias
        .as_deref()
        .ok_or_else(|| broker.error(StatusCode::BAD_REQUEST, "alias_required"))?;
    let owner = broker.owner(&device, alias).await?;
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let worker = broker.clone();
    let (mut token, alias) = tokio::spawn(async move {
        let _permit = permit;
        let mut owner = owner.lock().await;
        let account_id = account_key(&owner.vault.user, &owner.vault.alias);
        let lease = if request.previous_revision.is_some() {
            if let Some(central) = worker.central.as_ref() {
                // Ensure the FK target exists before the first forced refresh
                // after cutover or a locally imported account.
                worker.persist_owner(&owner).await.map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
                Some(
                    central
                        .acquire_lease(
                            &account_id,
                            &worker.holder_id,
                            std::time::Duration::from_secs(120),
                        )
                        .await
                        .map_err(|_| {
                            worker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_in_progress")
                        })?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let token = owner
            .tokens(request)
            .await
            .map_err(|e| worker.owner_failure(e))?;
        let record = CredentialRecord {
            account_id: account_id.clone(),
            user_id: Some(owner.vault.user.clone()),
            alias: owner.vault.alias.clone(),
            workspace: Some(
                vault::account(&owner.vault.auth).map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                })?,
            ),
            login: vault::token(&owner.vault.auth)
                .ok()
                .and_then(api::token_subject),
            vault: serde_json::to_value(&owner.vault)
                .map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?,
            revision: credential_revision(&owner.vault.auth),
        };
        let written = if let Some(central) = worker.central.as_ref() {
            if let Some(lease) = lease {
                central.fenced_write(&lease, &record).await.map_err(|_| {
                    worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?
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
        if !written {
            return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_fenced"));
        }
        Ok::<_, HttpError>((token, owner.vault.alias.trim().to_owned()))
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))??;
    // Revocation during a slow refresh must prevent delivery of a new access token.
    broker.authorize(&headers)?;
    broker.activity.delivered(&device, alias);
    token.user_id = Some(device.user);
    Ok(([("cache-control", "no-store")], Json(token)).into_response())
}
async fn accounts(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers)?;
    let result = account_catalog(&broker, &device.user).await?;
    broker.authorize(&headers)?;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}

pub(super) async fn account_catalog(
    broker: &Broker,
    user: &str,
) -> Result<Vec<Account>, HttpError> {
    let owners: Vec<_> = broker
        .owners
        .read()
        .await
        .iter()
        .filter(|(_, (identity, _))| identity.user == user)
        .map(|(key, (_, owner))| (key.clone(), owner.clone()))
        .collect();
    if owners.is_empty() && broker.ownership_unresolved.load(Ordering::Acquire) {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
    }
    let tasks = owners.into_iter().map(|(key, owner)| {
        let broker = broker.clone();
        tokio::spawn(async move {
            // Copy only the access credential. Listing never snapshots, refreshes,
            // persists, or changes the credential owner's availability.
            let (mut summary, revision, access, seed) = {
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
                )
            };
            let failure = broker
                .catalog
                .read(&key, &revision, access.as_deref(), seed, &mut summary)
                .await;
            if let Some(reason) = failure {
                broker.record_failure(reason, "catalog_usage", StatusCode::SERVICE_UNAVAILABLE);
            }
            // Renewal or a token request may have changed the credential while
            // the independent usage request was in flight. Do not publish its evidence.
            let current = owner.lock().await;
            summary.available = current.available && !current.routing_refused;
            if vault::digest(current.vault.auth.to_string().as_bytes()) != revision {
                summary.usage_stale = true;
                summary.usage_error = Some("credentials_changed".into());
            }
            if summary.usage_stale {
                summary.statusline_usage = None;
                summary.billing_class = api::BillingClass::Unknown;
                summary.usage_score = None;
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
    let device = broker.authorize(&headers)?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    normalize_alias(&input.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    store::validate_label(input.label.as_deref().unwrap_or(""))
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_label"))?;
    vault::validate_auth(&input.auth)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_auth"))?;
    // Import commits continue on disconnect. The client can safely retry the same identity.
    let worker = broker.clone();
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let result = tokio::spawn(async move {
        let _permit = permit;
        worker.import_account(&device.user, input).await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "import_unavailable"))??;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}
impl Broker {
    async fn import_account(&self, user: &str, mut input: Import) -> Result<Account, HttpError> {
        input.alias = normalize_alias(&input.alias)
            .map_err(|_| self.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
            .to_owned();
        let _import = self.imports.lock().await;
        let id = self
            .resolve_alias(user, &input.alias)
            .await?
            .map(|(key, _)| key)
            .unwrap_or_else(|| account_key(user, &input.alias));
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
            let mut owner = owner.lock().await;
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
            let mut owner = owner.lock().await;
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
        let admission = relogin::clear_registry(
            &self.state.join("accounts"),
            &self.key,
            &selected,
            &input.auth,
            relogin::AdmissionKind::Migration,
        )
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
            let mut owner = owner.lock().await;
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
            if owner.vault.verified && owner.available && !admission.quarantine_repair {
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
            if let Some(rpc) = owner.rpc.as_mut() {
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
                    revision: credential_revision(&saved.auth),
                };
                if let Some(central) = self.central.as_ref() {
                    central.save_account(&owner_record).await.map_err(|_| {
                        self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                    })?;
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
                revision: credential_revision(&vault.auth),
            };
            if let Some(central) = self.central.as_ref() {
                central.save_account(&owner_record).await.map_err(|_| {
                    self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                })?;
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
        self.owners.write().await.insert(id, (identity, owner));
        let mut owner = guard;
        let verification = async {
            if self.read_only {
                return Err(self.error(StatusCode::CONFLICT, "verification_requires_refresh"));
            }
            let proof = relogin::identity_inventory(&owner.state, &self.key, &owner.home)
                .clear_for_launch(&owner, relogin::AdmissionKind::Migration, &_import)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
            launch_owner(&mut owner, &self.binary, proof)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
            let revision = owner
                .snapshot()
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
                .revision;
            owner
                .tokens(TokenRequest {
                    previous_revision: Some(revision),
                    billing: true,
                    ..Default::default()
                })
                .await
                .map_err(|e| self.owner_failure(e))?;
            if !owner.rpc.as_ref().is_some_and(Rpc::verified_login) {
                return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
            }
            owner.vault.verified = true;
            vault::save(&state, &self.key, &owner.vault)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            self.persist_owner(&owner)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            // Verification and the durable vault precede retirement. A crash or
            // retirement failure leaves reservations for an explicit migration retry.
            relogin::retire_reservations(&self.state.join("accounts"), &owner.vault.auth, &state)
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
            Ok(account_summary(&owner))
        }
        .await;
        if verification.is_err() {
            owner.available = false;
        }
        verification
    }
}
async fn me(State(broker): State<Broker>, headers: HeaderMap) -> Result<Json<Value>, HttpError> {
    let device = broker.authorize(&headers)?;
    Ok(Json(json!({"id":device.user})))
}
async fn devices(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let current = broker.authorize(&headers)?;
    let devices = vault::devices(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
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
    let current = broker.authorize(&headers)?;
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
        broker.authorize(&headers)?;
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
    output.push_str(&format!(
        "codexctl_central_ownership_unresolved{{reason=\"recovery_failed\"}} {}\n",
        u8::from(broker.ownership_unresolved.load(Ordering::Acquire))
    ));
    Ok((
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
        .into_response())
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
        Some(store) => store.reachable().await,
        None => true,
    };
    let status = if local && reachable {
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
        Json(json!({"mode": mode, "databaseReachable": reachable})),
    )
        .into_response()
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

fn prepare_owner(state: &Path, key: &Path, read_only: bool) -> Result<Owner> {
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
        routing_refused: false,
        refresh_enabled: !read_only,
        limits: None,
        limits_observed: None,
        verification_input: None,
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
        let token = status?;
        if token != vault::token(&owner.vault.auth)? {
            bail!("native owner exported a login that differs from its journal");
        }
        owner.verification_input = Some(owner.vault.auth.clone());
    }
    Ok(())
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
    let configured = super::storage::runtime_store(state, key).await?;
    // File mode deliberately keeps the central store detached: the existing
    // vault remains the authoritative local path with no token-request cost.
    let central = match configured {
        CentralStore::File(_) => None,
        store => {
            // These rows are migration snapshots only. Authorization always
            // reads the local registry in this phase and never this snapshot.
            store
                .save_registry("users", &vault::private_read(&state.join("users.json"))?)
                .await?;
            store
                .save_registry(
                    "devices",
                    &vault::private_read(&state.join("devices.json"))?,
                )
                .await?;
            Some(store)
        }
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
    // Resolve all durable commits before inventorying quarantines from other accounts.
    for entry in std::fs::read_dir(state.join("accounts"))? {
        if relogin::recover(&entry?.path(), key).is_err() {
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
        let prepared = if pending && matches!(entry.path().join("runtime").try_exists(), Ok(false))
        {
            Err(anyhow::anyhow!("candidate has not started"))
        } else {
            prepare_owner(&entry.path(), key, read_only || pending)
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
                    routing_refused: false,
                    refresh_enabled: false,
                    limits: None,
                    limits_observed: None,
                    verification_input: None,
                }
            }
        };
        if pending || repair.blocked || (read_only && repair.verify) {
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
            && async {
                let proof = relogin::identity_inventory(&owner.state, key, &owner.home)
                    .clear_for_launch(&owner, relogin::AdmissionKind::Restore, &startup_import)?;
                launch_owner(&mut owner, binary, proof).await
            }
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
        work: Arc::new(Semaphore::new(128)),
        stopping: Arc::new(AtomicBool::new(false)),
        relogins: Arc::new(StdMutex::new(BTreeMap::new())),
        holder_id: instance_holder_id(),
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
    for _ in 0..recovery_failures {
        broker.record_failure(
            "recovery_failed",
            "startup",
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    let app = Router::new()
        .route("/v1/token", post(token))
        .route("/v1/accounts", get(accounts).post(import))
        .merge(super::resets::routes())
        .route("/v1/me", get(me))
        .route("/v1/devices", get(devices))
        .route("/v1/devices/revoke", post(revoke_device))
        .route("/v1/relogin/start", post(relogin::start))
        .route("/v1/relogin/status", post(relogin::status))
        .route("/v1/relogin/cancel", post(relogin::cancel))
        .route("/metrics", get(metrics))
        .route("/ready", get(ready))
        .route("/health", get(|| async { StatusCode::OK }));
    let app = enrollment::routes(app.merge(super::dashboard::routes(public_url)))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(broker.clone(), observe))
        .with_state(broker.clone());
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    println!("{}", json!({"listening":listener.local_addr()?}));
    let stopping = broker.stopping.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                tokio::select! {_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
                stopping.store(true, Ordering::Release);
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                stopping.store(true, Ordering::Release);
            }
        })
        .await?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
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
            metrics_hash: None,
            work: Arc::new(Semaphore::new(128)),
            stopping: Arc::new(AtomicBool::new(false)),
            relogins: Arc::new(StdMutex::new(BTreeMap::new())),
            central: Some(central),
            holder_id: "test-holder".into(),
        };
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = json!({"sub":"synthetic-login","iat":2000000000_u64,"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-seat","chatgpt_plan_type":"pro"}});
        let auth = json!({"tokens":{"access_token":format!("header.{}.",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())),"refresh_token":"synthetic-refresh","account_id":"synthetic-seat"}});
        let mut imported = Box::pin(broker.import_account(
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
