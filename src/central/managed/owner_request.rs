//! Refresh-owner token transactions. The machine handler owns admission, lease
//! retention, native retry, journal capture, publication and deferred settlement.
//! Private probes use the same read policy but cannot substitute a cached token.
#[path = "renewal.rs"]
mod renewal;

use super::*;
use crate::central::{
    rpc::RoutingPolicyError,
    server::{billing_class, supported_native_routing, usage},
};

const NATIVE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);
const FAILED_SETTLEMENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_FAILED_SETTLEMENTS: u8 = 3;

pub(super) async fn initialize_owner(owner: &mut Owner) -> Result<()> {
    owner
        .rpc
        .as_mut()
        .context("missing owner")?
        .initialize_with_deadline(NATIVE_DEADLINE)
        .await?;
    owner.snapshot()?;
    if !owner.vault.verified {
        let status = owner
            .rpc
            .as_mut()
            .context("missing owner")?
            .remember_exportable_login(NATIVE_DEADLINE)
            .await;
        // Even the cached-status call can refresh. Persist its journal and
        // rejection evidence before returning any protocol or loading error.
        owner.snapshot()?;
        if owner.rpc.as_ref().is_some_and(|rpc| rpc.rejected_login()) {
            owner.refresh_failed(None).await;
        }
        let token = status?;
        if token != vault::token(&owner.vault.auth)? {
            bail!("native owner exported a login that differs from its journal");
        }
        owner.verification_input = Some(owner.vault.auth.clone());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum BillingRead {
    Omit,
    Live,
    Recent,
}

impl Owner {
    pub(in crate::central) async fn tokens(
        &mut self,
        request: TokenRequest,
    ) -> Result<TokenResponse, TokenFailure> {
        NativeRead {
            owner: self,
            device: None,
            lease_lost: None,
            retried: false,
        }
        .tokens(request)
        .await
    }

    pub(in crate::central) async fn tokens_for_device(
        &mut self,
        request: TokenRequest,
        device: &str,
    ) -> Result<TokenResponse, TokenFailure> {
        NativeRead {
            owner: self,
            device: Some(device),
            lease_lost: None,
            retried: false,
        }
        .tokens(request)
        .await
    }

    /// Logs and counts a failed owner refresh with the provider's bounded
    /// reason, read before the fence can stop the child.
    pub(in crate::central) async fn refresh_failed(&mut self, error: Option<&anyhow::Error>) {
        let reason = match self.rpc.as_ref() {
            Some(rpc) => rpc.refresh_failure_reason().await,
            None => None,
        };
        if let (Some(rpc), Some(error)) = (self.rpc.as_mut(), error) {
            rpc.classify_owner_read_failure(error, reason.is_some());
        }
        let account = self.vault.alias.to_ascii_lowercase();
        if let Some(reason) = reason {
            crate::central::owner_refresh::record(&account, &self.vault.user, reason);
        }
        let mut log = json!({"operation":"owner_refresh","stage":"provider","reason":reason.unwrap_or("unclassified"),"account":account,"account_key":crate::central::managed::account_key(&self.vault.user, &self.vault.alias)});
        if reason.is_none()
            && error.is_some()
            && !self.rpc.as_ref().is_some_and(Rpc::rejected_login)
        {
            let failure = self
                .rpc
                .as_ref()
                .map(Rpc::failure_diagnostics)
                .unwrap_or_default();
            log["failure_class"] = json!(failure.failure_class);
            log["rpc_code"] = json!(failure.rpc_code);
            log["has_data_retryable"] = json!(failure.has_data_retryable);
            log["native_reason"] = json!(failure.native_reason());
            if let Some(status) = failure.http_status {
                log["http_status"] = json!(status);
            }
            crate::central::owner_refresh::record_native(
                &account,
                &self.vault.user,
                failure.native_reason(),
            );
        }
        eprintln!("{log}");
    }
}

struct NativeRead<'a> {
    owner: &'a mut Owner,
    device: Option<&'a str>,
    lease_lost: Option<&'a AtomicBool>,
    retried: bool,
}

impl NativeRead<'_> {
    fn cached_plain_token(&self) -> Option<TokenResponse> {
        if !self.owner.vault.verified
            || self.owner.routing_refused
            || self.owner.rpc.as_ref().is_some_and(Rpc::rejected_login)
        {
            return None;
        }
        let access_token = vault::token(&self.owner.vault.auth).ok()?.to_owned();
        if !api::token_expiry(&access_token).is_some_and(|at| at > chrono::Utc::now().timestamp()) {
            return None;
        }
        let revision = vault::digest(&serde_json::to_vec(&self.owner.vault.auth).ok()?);
        if self.owner.routing_revision.as_deref() != Some(revision.as_str()) {
            return None;
        }
        Some(TokenResponse {
            user_id: None,
            chatgpt_account_id: vault::account(&self.owner.vault.auth).ok()?,
            chatgpt_plan_type: api::token_identity(&access_token)
                .and_then(|identity| identity.plan),
            access_token,
            revision,
            billing_class: None,
            native_routing_supported: true,
            statusline_usage: None,
            label: self.owner.vault.label.clone(),
        })
    }

    fn native_failure(&mut self, error: anyhow::Error, billing: bool) -> TokenFailure {
        self.owner.retry_requires_billing = billing;
        if error.is::<RoutingPolicyError>() {
            return TokenFailure::UnsupportedRouting;
        }
        if self.owner.rpc.as_ref().is_some_and(Rpc::rejected_login) {
            self.owner.fence(false);
        } else if vault::token(&self.owner.vault.auth)
            .ok()
            .and_then(api::token_expiry)
            .is_some_and(|at| at <= chrono::Utc::now().timestamp())
        {
            let retryable = self.owner.vault.verified
                && self
                    .owner
                    .rpc
                    .as_ref()
                    .is_some_and(Rpc::retryable_owner_read);
            self.owner.fence(retryable);
            if retryable {
                return TokenFailure::Retryable(error);
            }
        }
        TokenFailure::Unavailable(error)
    }

    async fn native_read(&mut self, method: &str, mut params: Value) -> Result<Value> {
        loop {
            if self
                .lease_lost
                .is_some_and(|lost| lost.load(std::sync::atomic::Ordering::Acquire))
            {
                anyhow::bail!("credential owner lease lost");
            }
            let before = self.owner.vault.auth.clone();
            let result = self
                .owner
                .rpc
                .as_mut()
                .context("missing credential owner")?
                .owner_read(method, params.clone(), NATIVE_DEADLINE)
                .await;
            let error = match result {
                Ok(value) => return Ok(value),
                Err(error) if error.is::<RoutingPolicyError>() => return Err(error),
                Err(error) => error,
            };
            self.owner.refresh_failed(Some(&error)).await;
            // Even a completed error may have rotated the owner's journal.
            // Preserve it before retrying, and never force that rotation twice.
            self.owner.snapshot()?;
            let reusable = self
                .owner
                .rpc
                .as_mut()
                .is_some_and(Rpc::resume_completed_native_error);
            if self.retried || !reusable {
                return Err(error);
            }
            self.retried = true;
            if self.owner.vault.auth != before && params.get("refreshToken") == Some(&json!(true)) {
                params["refreshToken"] = json!(false);
            }
            tokio::time::sleep(RETRY_BACKOFF).await;
            if !self
                .owner
                .rpc
                .as_mut()
                .is_some_and(Rpc::resume_completed_native_error)
            {
                if self.owner.rpc.as_ref().is_some_and(Rpc::rejected_login) {
                    self.owner.refresh_failed(None).await;
                }
                return Err(error);
            }
        }
    }

    async fn tokens(&mut self, request: TokenRequest) -> Result<TokenResponse, TokenFailure> {
        self.owner.routing_observed = false;
        let billing = if request.billing {
            BillingRead::Live
        } else {
            BillingRead::Omit
        };
        if !self.owner.available {
            return Err(TokenFailure::Unavailable(anyhow::anyhow!(
                "credential owner unavailable"
            )));
        }
        self.owner.retry_requires_billing = request.billing;
        let current = match self.owner.snapshot() {
            Ok(current) => current,
            Err(error) => {
                self.owner.fence(false);
                return Err(error.into());
            }
        };
        if let Some(previous) = request.previous_revision.as_ref() {
            if previous != &current.revision {
                return self.with_billing(current, billing).await;
            }
            if !self.owner.refresh_enabled {
                return Err(TokenFailure::RefreshDisabled);
            }
            let age_ms = self
                .owner
                .vault
                .last_rotation_ms
                .and_then(|rotated| self.owner.rotation_clock_now().checked_sub(rotated));
            // Verification and reset-auth probes must still exercise the grant.
            // Machine requests cannot force a healthy access token to rotate.
            let expiry = api::token_expiry(&current.access_token);
            let token_valid = self.device.is_some()
                && expiry.is_some_and(|expires| {
                    expires
                        >= chrono::Utc::now()
                            .timestamp()
                            .saturating_add(crate::central::fast_path::MIN_REMAINING_SECONDS)
                });
            let recent = self.device.is_some() && age_ms.is_some_and(|age| age < 60_000);
            if let Some(device) = self.device {
                crate::central::refresh_control::forced(
                    if token_valid {
                        crate::central::refresh_control::Forced::TokenValid
                    } else if expiry.is_none() {
                        crate::central::refresh_control::Forced::UnknownExpiry { recent }
                    } else if recent {
                        crate::central::refresh_control::Forced::ServedRecent
                    } else {
                        crate::central::refresh_control::Forced::Refreshed
                    },
                    &self.owner.vault.alias,
                    &self.owner.vault.user,
                    device,
                    age_ms,
                );
            }
            if token_valid || recent {
                let billing = if request.billing {
                    BillingRead::Recent
                } else {
                    BillingRead::Omit
                };
                return self.with_billing(current, billing).await;
            }
        }
        // Serialize all calls, and persist any rotated credentials even when RPC fails.
        let force = request.previous_revision.is_some();

        let result = if self.owner.refresh_enabled {
            self.native_read("account/read", json!({"refreshToken":force}))
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        if result
            .as_ref()
            .is_err_and(|error| error.is::<RoutingPolicyError>())
        {
            self.owner.routing_refused = true;
        }
        if force
            && !self.owner.vault.verified
            && let Some(rpc) = self.owner.rpc.as_mut()
            && !rpc.verified_login()
        {
            rpc.inspect_rejection(NATIVE_DEADLINE).await;
        }
        let snapshot = self.owner.snapshot();
        if snapshot.is_err() {
            self.owner.fence(false);
        }
        // Persistence/identity failure wins even for a completed routing refusal.
        let current = snapshot?;
        if let Err(error) = result {
            if self.device.is_some()
                && !request.billing
                && !error.is::<RoutingPolicyError>()
                && let Some(token) = self.cached_plain_token()
            {
                return Ok(token);
            }
            return Err(self.native_failure(error, request.billing));
        }
        self.with_billing(current, billing).await
    }

    async fn with_billing(
        &mut self,
        mut token: TokenResponse,
        billing: BillingRead,
    ) -> Result<TokenResponse, TokenFailure> {
        let requested = !matches!(billing, BillingRead::Omit);
        if self.owner.rpc.is_none() {
            if requested {
                if !self.owner.refresh_enabled && self.owner.routing_revision.is_none() {
                    return Err(TokenFailure::UnsupportedRouting);
                }
                return Err(TokenFailure::Unavailable(anyhow::anyhow!(
                    "native billing evidence unavailable"
                )));
            }
            return Ok(token);
        }
        for attempt in 0..2 {
            let revision = token.revision.clone();
            let mut observed_limits = None;
            if requested {
                let cached = if matches!(billing, BillingRead::Recent) {
                    self.owner
                        .limits_observed
                        .as_ref()
                        .filter(|(at, observed_revision)| {
                            observed_revision == &revision
                                && at.elapsed() < std::time::Duration::from_secs(30)
                        })
                        .and_then(|(at, _)| {
                            self.owner
                                .limits
                                .as_ref()
                                .map(|limits| (limits.clone(), *at))
                        })
                } else {
                    None
                };
                let (limits, observed_at) = if let Some(cached) = cached {
                    cached
                } else {
                    // Rate-limit reads can refresh too. Persist on every result.
                    let result = self.native_read("account/rateLimits/read", json!({})).await;
                    let observed_at = std::time::Instant::now();
                    let snapshot = self.owner.snapshot();
                    if snapshot.is_err() {
                        self.owner.fence(false);
                    }
                    token = snapshot?;
                    let limits = match result {
                        Ok(limits) => limits,
                        Err(error) => {
                            return Err(self.native_failure(error, true));
                        }
                    };
                    (limits, observed_at)
                };
                token.billing_class = Some(billing_class(&limits));
                token.chatgpt_plan_type = limits
                    .pointer("/rateLimits/planType")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                observed_limits = Some((limits, observed_at));
            }
            // The command-auth provider bypasses first-party workspace routing.
            // Re-discover after any billing refresh, before exporting credentials.
            let result = self
                .native_read("account/read", json!({"refreshToken":false}))
                .await;
            // This routing read can refresh too, so it reports rejections.
            let snapshot = self.owner.snapshot();
            if snapshot.is_err() {
                self.owner.fence(false);
            }
            let mut current = snapshot?;
            if result
                .as_ref()
                .is_err_and(|error| error.is::<RoutingPolicyError>())
            {
                self.owner.routing_refused = true;
            }
            let account = match result {
                Ok(account) => account,
                Err(error) => {
                    if self.device.is_some()
                        && !requested
                        && !error.is::<RoutingPolicyError>()
                        && let Some(token) = self.cached_plain_token()
                    {
                        return Ok(token);
                    }
                    return Err(self.native_failure(error, requested));
                }
            };
            if supported_native_routing(&account) != Some(current.chatgpt_account_id.as_str()) {
                self.owner.routing_refused = true;
                return Err(TokenFailure::UnsupportedRouting);
            }
            self.owner.routing_refused = false;
            if requested && current.revision != revision {
                // Native status, billing, and routing reads can rotate before OR
                // after fetching their evidence. Cover one stable complete auth
                // revision, or retain the latest journal and refuse delivery.
                token = current;
                if attempt == 0 {
                    continue;
                }
                return Err(TokenFailure::Unavailable(anyhow::anyhow!(
                    "credential state changed while checking billing; retry"
                )));
            }
            current.billing_class = token.billing_class;
            current.chatgpt_plan_type = token.chatgpt_plan_type;
            current.native_routing_supported = true;
            self.owner.routing_revision = Some(current.revision.clone());
            self.owner.routing_observed = true;
            if let Some((limits, observed_at)) = observed_limits {
                let parsed = usage(&limits).ok();
                current.statusline_usage =
                    parsed.as_ref().map(crate::statusline::Usage::from_usage);
                if let Some(usage) = current.statusline_usage.as_mut() {
                    usage.age_seconds = observed_at.elapsed().as_secs();
                }
                self.owner.limits = Some(limits);
                self.owner.limits_observed = Some((observed_at, current.revision.clone()));
            }
            return Ok(current);
        }
        Err(TokenFailure::Unavailable(anyhow::anyhow!(
            "credential state changed while checking billing; retry"
        )))
    }
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
    stopped_owner_record(owner, before)
}

fn stopped_owner_record(owner: &mut Owner, before: &Vault) -> Result<CredentialRecord> {
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

/// Child settlement failures spend the budget; journal or database failures
/// retain the dead child's output and never spend a child termination budget.
#[derive(Default)]
struct BackgroundSettlement {
    failed_attempts: u8,
}

impl BackgroundSettlement {
    async fn record(&mut self, owner: &mut Owner, before: &Vault) -> Result<CredentialRecord> {
        if let Some(rpc) = owner.rpc.as_mut() {
            if self.failed_attempts >= MAX_FAILED_SETTLEMENTS {
                if rpc.kill_and_reap().await? {
                    crate::central::refresh_control::settlement_killed(&before.alias, &before.user);
                    eprintln!(
                        "{}",
                        json!({"operation":"owner_settlement","stage":"child","reason":"settlement_killed","account":before.alias.to_ascii_lowercase(),"account_key":account_key(&before.user, &before.alias),"failed_attempts":self.failed_attempts})
                    );
                }
            } else if !rpc.process_exited() {
                let result = tokio::time::timeout(NATIVE_DEADLINE, rpc.settle_and_stop())
                    .await
                    .context("owner settlement timed out; runtime retained")
                    .and_then(|result| result);
                if let Err(error) = result {
                    self.failed_attempts += 1;
                    return Err(error);
                }
            }
        }
        stopped_owner_record(owner, before)
    }
}

async fn settle_foreground(owner: &mut Owner, before: &Vault) -> Result<CredentialRecord> {
    tokio::time::timeout(
        FAILED_SETTLEMENT_DEADLINE,
        settled_owner_record(owner, before),
    )
    .await
    .context("owner settlement deferred; runtime retained")?
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

/// Keep a recovery lease until the stopped child has been settled and its
/// credentials are durably published. The work permit is held by the caller
/// or transferred to this task, so shutdown drains this work instead of
/// cancelling it.
#[allow(clippy::too_many_arguments)] // Recovery state is explicit at this boundary.
pub(super) async fn settle_background_recovery(
    owner_ref: Arc<Mutex<Owner>>,
    settlements: Arc<Settlements>,
    central: Option<CentralStore>,
    lease: Option<crate::central::storage::Lease>,
    lease_window: Option<Arc<LeaseWindow>>,
    before: Vault,
    recovery: SettlementRecovery,
    recovery_generation: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
    renew_done: Arc<AtomicBool>,
    renew_task: Option<tokio::task::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
) {
    let _settlement = settlements.mark(&owner_ref);
    let mut settlement = BackgroundSettlement::default();
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
                settlement.record(&mut owner, &before).await.map(Some)
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
                crate::central::refresh_control::recovery_failed(&before.alias, &before.user);
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
            crate::central::refresh_control::recovery_failed(&before.alias, &before.user);
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

pub(super) async fn token(
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
    let policy = fast_path::Policy::configured();
    let vault_fast = policy == fast_path::Policy::Vault
        && broker
            .central
            .as_ref()
            .is_some_and(|central| central.mode() == StoreMode::Postgres);
    // Canary reads must see a local fence as a refusal, never a reason to
    // initialize native during on-demand recovery.
    if !vault_fast && broker.on_demand_recovery() {
        let worker = broker.clone();
        let owner_ref = owner.clone();
        let account_id = request.account_id.clone();
        // Detached work retains its permit and settles native refreshes even
        // when the requesting client disconnects.
        tokio::spawn(async move {
            let permit = worker
                .settlements
                .admit(&owner_ref, worker.work.clone().acquire_owned())
                .await
                .map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?
                .map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
            {
                let owner = worker
                    .settlements
                    .admit(&owner_ref, owner_ref.lock())
                    .await
                    .map_err(|_| {
                        worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                    })?;
                owner
                    .validate_account_id(account_id.as_deref())
                    .map_err(|failure| worker.owner_failure(failure))?;
                if owner.available || !owner.retryable_unavailable || owner.routing_refused {
                    return Ok(());
                }
            }
            drop(permit);
            recover_owners(&worker, Some(owner_ref)).await;
            Ok::<_, HttpError>(())
        })
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))??;
    }
    // The fast path and the lease wait run only behind the flag, in
    // PostgreSQL mode. Dual and file mode keep today's path.
    let fast_on = (fast_path::enabled() || vault_fast)
        && broker
            .central
            .as_ref()
            .is_some_and(|central| central.mode() == crate::central::storage::StoreMode::Postgres);
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
                policy,
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
                Err(miss) if vault_fast && miss != fast_path::Miss::Expiring => {
                    return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
                }
                Err(miss) => busy_owner = miss == fast_path::Miss::Busy,
            }
        }
        let permit = broker
            .settlements
            .admit(&owner, broker.work.clone().acquire_owned())
            .await
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
        let worker = broker.clone();
        let owner_ref = owner.clone();
        // A borrowed request names the lender's alias through its grant.
        let lender_alias = borrowed
            .as_ref()
            .map(|borrowed| borrowed.grant.alias.clone());
        let mut request = template.clone();
        let requesting_device = device.id.clone();
        // A request that found the owner busy re-checks under its lock.
        let fast_locked = (busy_owner || vault_fast)
            .then(|| (fast_account.clone(), fast_user.clone(), fast_alias.clone()));
        busy_owner = false;
        let busy = Arc::new(StdMutex::new(None::<(String, i64)>));
        let busy_slot = busy.clone();
        let attempt = tokio::spawn(async move {
        let admission = async { if worker
            .central.as_ref()
            .is_some_and(|central| central.mode() != crate::central::storage::StoreMode::File)
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
        }};
        let (import_guard, mut owner) = worker.settlements.admit(&owner_ref, admission)
            .await.map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?;
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
                    crate::central::rename::renamed_error(&worker, &owner.vault.user, requested)
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
            .is_some_and(|central| central.mode() != crate::central::storage::StoreMode::File)
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
            let mut error = worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable");
            if worker.on_demand_recovery() && owner.retryable_unavailable && !owner.routing_refused {
                error.retry_after = owner.on_demand_retry_after();
            }
            return Err(error);
        }
        // Under the owner lock the local fence is visible, so a request that
        // found the owner busy may still be served without the lease.
        if let Some((fast_account, fast_user, fast_alias)) = fast_locked.as_ref()
            && !owner.routing_refused
        {
            let result = fast_token(
                &worker,
                LocalOwner::Locked(&owner),
                fast_account,
                fast_user,
                fast_alias,
                &request,
                policy,
            )
            .await;
            fast_path::record(result.as_ref().map(|_| ()).map_err(|miss| *miss));
            if let Ok((token, alias)) = result {
                return Ok((token, alias, account_id));
            } else if vault_fast && !matches!(result, Err(fast_path::Miss::Expiring)) {
                return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
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
                        && central.mode() == crate::central::storage::StoreMode::Postgres
                        && central
                            .lease_held_elsewhere(&account_id, &worker.holder_id)
                            .await
                            .unwrap_or(false)
                        && let Ok(mut slot) = busy_slot.lock()
                    {
                        *slot = Some((account_id.clone(), owner.vault.revision));
                        return Err(HttpError {
                            retry_after: None,
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
                .is_some_and(|central| central.mode() != crate::central::storage::StoreMode::File)
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
        // A different lease holder may have refreshed while this request
        // waited. Recheck the committed token before initializing native.
        if vault_fast {
            let result = fast_token(
                &worker,
                LocalOwner::Locked(&owner),
                &account_id,
                &owner.vault.user,
                &owner.vault.alias,
                &request,
                policy,
            ).await;
            match result {
                Err(fast_path::Miss::Expiring) => {}
                result => {
                    if let (Some(central), Some(lease)) = (worker.central.as_ref(), lease.as_ref()) {
                        central.release_lease(lease).await.map_err(|_| {
                            worker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed")
                        })?;
                    }
                    return result.map(|(token, alias)| (token, alias, account_id))
                        .map_err(|_| worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
                }
            }
        }
        let renew_lost = Arc::new(AtomicBool::new(false));
        let lease_signal = Arc::new(Notify::new());
        let lease_window = lease.as_ref().map(|_| Arc::new(LeaseWindow {
            deadline: Arc::new(Mutex::new(std::time::Instant::now() + IMPORT_LEASE_TTL)),
            lost: renew_lost.clone(),
            signal: lease_signal.clone(),
        }));
        let renew_task = match (worker.central.clone(), lease.clone()) {
            (Some(central), Some(lease)) => {
                let lost = renew_lost.clone();
                let window = lease_window.clone().expect("token lease window");
                Some(tokio::spawn(async move {
                    loop {
                        let remaining = window.deadline.lock().await.saturating_duration_since(std::time::Instant::now());
                        if remaining <= IMPORT_LEASE_SAFETY_MARGIN {
                            lost.store(true, Ordering::Release);
                            window.signal.notify_waiters();
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        match central
                            .renew(&lease, std::time::Duration::from_secs(120))
                            .await
                        {
                            Ok(true) => {
                                *window.deadline.lock().await = std::time::Instant::now() + IMPORT_LEASE_TTL;
                            }
                            Ok(false) => {
                                lost.store(true, Ordering::Release);
                                window.signal.notify_waiters();
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
        let native_work = async {
            if lease.is_some()
                && let Some(import_guard) = import_guard
                && let Err(error) = worker.ensure_refresh_owner(&mut owner, import_guard).await
            {
                restore_after_settlement = owner.available && !owner.routing_refused;
                return Err(error);
            }
            let observed_from = std::time::Instant::now();
            let token_result = NativeRead { owner: &mut owner, device: Some(&requesting_device), lease_lost: Some(&renew_lost), retried: false }.tokens(request).await;
            if renew_lost.load(Ordering::Acquire) {
                terminate_lost_import(&mut owner).await;
                return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "refresh_fenced"));
            }
            // A failed native read may mean lost routing or a fenced owner.
            // Withdraw the evidence so no replica serves this account
            // lease-free until the lease path observes it again.
            // A clear that keeps failing leaves at most the 60 s evidence
            // bound; it is counted so the failure alert fires.
            if (token_result.is_err() || !owner.routing_observed)
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
            let settle_required = owner.rpc.as_mut().is_some_and(|rpc| rpc.completion_pending() || rpc.process_exited());
            if settle_required && owner.rpc.is_some() {
                // File-mode recovery must relaunch only after background settlement
                // confirms exit and persists the final journal. This is a temporary
                // serialization barrier, not a rejected refresh grant.
                if owner.available && !owner.routing_refused {
                    if lease.is_some() {
                        restore_after_settlement = true;
                    } else {
                        owner.retryable_unavailable = true;
                        owner.retry_started = None;
                    }
                }
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
                    match settle_foreground(&mut owner, &before).await {
                        Ok(_) => {}
                        Err(error) if failed_rpc => {
                            let exited = owner
                                .rpc
                                .as_mut()
                                .is_some_and(|rpc| rpc.process_exited());
                            let exited = if exited {
                                true
                            } else if let Some(rpc) = owner.rpc.as_mut().filter(|rpc| !rpc.timed_out()) {
                                tokio::time::timeout(std::time::Duration::from_millis(100), rpc.wait_after_eof()).await.unwrap_or(false)
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
                let settlement = if token_result.is_ok() && owner.routing_observed {
                    settled_owner_record(&mut owner, &before).await
                } else {
                    settle_foreground(&mut owner, &before).await
                };
                if let Err(error) = settlement {
                    restore_after_settlement = owner.available && !owner.routing_refused;
                    eprintln!("central owner shutdown: {error:#}");
                    return Err(worker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"));
                }
            }
            let settled_file_owner = settle_required && lease.is_none() && owner.rpc.is_none();
            if settled_file_owner && owner.available && !owner.routing_refused {
                // Preserve a successful plain response, but reopen native reads
                // through admission after the old child is confirmed stopped.
                owner.available = false;
                owner.retryable_unavailable = true;
                owner.retry_started = token_result.is_err().then(|| owner.retry_clock_now());
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
                                restore_after_settlement = owner.available && !owner.routing_refused;
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
                    if !settled_file_owner {
                        owner.retryable_unavailable = false;
                    }
                    token
                }
                Err(error) => {
                    return Err(worker.owner_failure(error));
                }
            };
            Ok::<_, HttpError>((token, owner.vault.alias.trim().to_owned(), account_id.clone()))
        };
        let result = lease_guarded_verification(native_work, renew_lost.clone(), lease_signal).await;
        if renew_lost.load(Ordering::Acquire) {
            terminate_lost_import(&mut owner).await;
            restore_after_settlement = false;
        }
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
                worker.settlements.clone(),
                worker.central.clone(),
                lease,
                lease_window,
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
        .filter(|store| store.mode() != crate::central::storage::StoreMode::File)
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

/// Evidence for the credential this request is about to publish. Only a
/// completed routing check on that exact revision counts.
fn observation(
    owner: &Owner,
    token: &Result<TokenResponse, TokenFailure>,
    observed_from: std::time::Instant,
) -> Option<fast_path::Observation> {
    let Ok(token) = token else { return None };
    let published = vault::digest(&serde_json::to_vec(&owner.vault.auth).ok()?);
    if !owner.routing_observed || !token.native_routing_supported || token.revision != published {
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

pub(super) enum SettlementRecovery {
    Available,
    Retryable,
    Fenced,
}

#[derive(Clone)]
pub(super) struct LeaseWindow {
    deadline: Arc<Mutex<std::time::Instant>>,
    lost: Arc<AtomicBool>,
    signal: Arc<Notify>,
}

pub(super) fn fence_background_owner(owner: &mut Owner) {
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

pub(super) async fn lease_guarded_verification<T, F>(
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
                retry_after: None,
                status: StatusCode::SERVICE_UNAVAILABLE,
                reason: "refresh_fenced",
                alias: None,
            })
        }
    }
}

pub(super) async fn terminate_lost_import(owner: &mut Owner) {
    // Closing the RPC is the first action after lease loss. This prevents an
    // in-flight account/read from rotating credentials after a successor
    // acquires the account lease.
    if let Some(rpc) = owner.rpc.take() {
        rpc.terminate().await;
    }
    fence_background_owner(owner);
}

impl Broker {
    pub(super) async fn verify_migrated_owner(
        &self,
        import_guard: &tokio::sync::MutexGuard<'_, ()>,
        permit: tokio::sync::OwnedSemaphorePermit,
        owner_ref: Arc<Mutex<Owner>>,
        mut owner: tokio::sync::OwnedMutexGuard<Owner>,
        state: PathBuf,
    ) -> Result<Account, HttpError> {
        let verification_lease = if let Some(central) = self
            .central
            .as_ref()
            .filter(|store| store.mode() != crate::central::storage::StoreMode::File)
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
                .filter(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
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
            let settlements = self.settlements.clone();
            // Keep a per-owner reservation while settlement runs independently.
            // Other accounts' token requests must not wait for this child or DB.
            tokio::spawn(async move {
                settle_background_recovery(
                    owner_ref.clone(),
                    settlements,
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

pub(super) async fn recover_unhealthy_owners(broker: &Broker) {
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
        {
            continue;
        }
        let cooldown = if on_demand {
            owner.on_demand_cooldown_active()
        } else {
            owner.retry_cooldown_active()
        };
        if cooldown {
            if on_demand {
                crate::central::refresh_control::recovery(
                    crate::central::refresh_control::Recovery::Backoff,
                );
            }
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
        if !on_demand && owner.retry_started.is_none() {
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
                    .filter(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
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
                let probe = match probe {
                    Err(TokenFailure::Unavailable(error))
                        if owner.available
                            && !owner.routing_refused
                            && owner.rpc.as_ref().is_some_and(Rpc::retryable_owner_read) =>
                    {
                        Err(TokenFailure::Retryable(error))
                    }
                    result => result,
                };
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
                if !probe_ok || !local_saved {
                    crate::central::refresh_control::recovery_failed(
                        &owner.vault.alias,
                        &owner.vault.user,
                    );
                    if on_demand {
                        crate::central::refresh_control::recovery(
                            crate::central::refresh_control::Recovery::Failed,
                        );
                    }
                }
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
                        broker.settlements.clone(),
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
                        if on_demand {
                            crate::central::refresh_control::recovery(
                                crate::central::refresh_control::Recovery::Recovered,
                            );
                        }
                    }
                    (Ok(_), true, false) => {
                        fence_background_owner(&mut owner);
                        let recovery_generation = owner.recovery_generation;
                        let owner_ref = owner_ref.clone();
                        let central = central.clone();
                        let lease = lease.clone();
                        tokio::spawn(settle_background_recovery(
                            owner_ref,
                            broker.settlements.clone(),
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
                            broker.settlements.clone(),
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
                crate::central::refresh_control::recovery_failed(
                    &owner.vault.alias,
                    &owner.vault.user,
                );
                if on_demand {
                    crate::central::refresh_control::recovery(
                        crate::central::refresh_control::Recovery::Failed,
                    );
                }
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
                        broker.settlements.clone(),
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
