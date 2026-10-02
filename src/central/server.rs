use super::{
    rpc::{RoutingPolicyError, Rpc},
    vault::{self, Vault},
};
use crate::{api, store};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};
use tokio::sync::Mutex;

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRequest {
    pub previous_revision: Option<String>,
    pub account_id: Option<String>,
    #[serde(default)]
    pub billing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub access_token: String,
    pub chatgpt_account_id: String,
    pub chatgpt_plan_type: Option<String>,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_class: Option<api::BillingClass>,
    #[serde(default)]
    pub native_routing_supported: bool,
}

impl TokenResponse {
    pub fn login(&self) -> Value {
        json!({"type":"chatgptAuthTokens","accessToken":self.access_token,"chatgptAccountId":self.chatgpt_account_id,"chatgptPlanType":self.chatgpt_plan_type})
    }
    pub fn refresh(&self) -> Value {
        json!({"accessToken":self.access_token,"chatgptAccountId":self.chatgpt_account_id,"chatgptPlanType":self.chatgpt_plan_type})
    }
}

pub(super) fn supported_native_routing(account: &Value) -> Option<&str> {
    if account.pointer("/account/type").and_then(Value::as_str) != Some("chatgpt")
        || account
            .pointer("/workspaceRouting/backendOrigin")
            .and_then(Value::as_str)
            != Some("https://chatgpt.com")
        || account
            .pointer("/workspaceRouting/accountRoutingOverride")
            .and_then(Value::as_str)
            != Some("NO_CONSTRAINT")
    {
        return None;
    }
    account
        .pointer("/workspaceRouting/chatgptAccountId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

pub(super) enum TokenFailure {
    AccountMismatch,
    RefreshDisabled,
    UnsupportedRouting,
    Unavailable(anyhow::Error),
}

impl From<anyhow::Error> for TokenFailure {
    fn from(error: anyhow::Error) -> Self {
        if error.is::<RoutingPolicyError>() {
            Self::UnsupportedRouting
        } else {
            Self::Unavailable(error)
        }
    }
}

pub(super) struct Owner {
    pub(super) vault: Vault,
    pub(super) rpc: Option<Rpc>,
    pub(super) home: PathBuf,
    pub(super) state: PathBuf,
    pub(super) key: PathBuf,
    pub(super) available: bool,
    pub(super) refresh_enabled: bool,
    pub(super) limits: Option<Value>,
    pub(super) verification_input: Option<Value>,
}

pub(super) fn validate_owned_identity(original_auth: &Value, auth: &Value) -> Result<()> {
    vault::validate_auth(auth)?;
    if vault::account(auth)? != vault::account(original_auth)?
        || api::token_subject(vault::token(auth)?)
            != api::token_subject(vault::token(original_auth)?)
    {
        bail!("central credential owner changed identity");
    }
    let original =
        api::token_identity(vault::token(original_auth)?).context("missing original identity")?;
    let updated = api::token_identity(vault::token(auth)?).context("missing updated identity")?;
    if original.user_id.is_some() && original.user_id != updated.user_id {
        bail!("central credential owner changed login identity");
    }
    Ok(())
}

impl Owner {
    pub(super) fn validate_owned_auth(&self, auth: &Value) -> Result<()> {
        validate_owned_identity(&self.vault.auth, auth)
    }

    pub(super) fn reconcile_journal(&mut self) -> Result<()> {
        let journal: Value =
            serde_json::from_slice(&vault::private_read(&self.home.join("auth.json"))?)?;
        self.validate_owned_auth(&journal)?;
        if journal == self.vault.auth {
            return Ok(());
        }
        // A confirmed unchanged rejection permits a replacement grant. The journal
        // is written before its vault, so a crash here must retain that replacement.
        if !self.vault.verified && self.vault.import_rejected {
            return Ok(());
        }
        let saved = vault::token(&self.vault.auth)?;
        let retained = vault::token(&journal)?;
        let order = api::token_issued_at(saved)
            .zip(api::token_issued_at(retained))
            .or_else(|| api::token_expiry(saved).zip(api::token_expiry(retained)));
        match order {
            Some((old, new)) if new > old => {}
            Some((old, new)) if new < old => store::atomic_write(
                &self.home.join("auth.json"),
                &serde_json::to_vec(&self.vault.auth)?,
            )?,
            _ => {
                bail!("cannot order retained and encrypted credentials; reconcile before recovery")
            }
        }
        Ok(())
    }

    pub(super) fn snapshot(&mut self) -> Result<TokenResponse> {
        if self.rpc.is_none() {
            self.reconcile_journal()?;
        }
        let auth: Value = serde_json::from_slice(&vault::private_read(
            &self.home.as_path().join("auth.json"),
        )?)?;
        self.validate_owned_auth(&auth)?;
        if self.rpc.as_ref().is_some_and(Rpc::verified_login) {
            self.vault.verified = true;
            self.vault.import_rejected = false;
        } else if !self.vault.verified {
            if let Some(rpc) = self.rpc.as_ref() {
                self.vault.import_rejected =
                    rpc.rejected_login() && self.verification_input.as_ref() == Some(&auth);
            } else if self.vault.auth != auth {
                self.vault.import_rejected = false;
            }
        }
        self.vault.auth = auth;
        vault::save(&self.state, &self.key, &self.vault)?;
        let access_token = vault::token(&self.vault.auth)?.to_owned();
        Ok(TokenResponse {
            user_id: None,
            chatgpt_account_id: vault::account(&self.vault.auth)?,
            chatgpt_plan_type: api::token_identity(&access_token).and_then(|i| i.plan),
            revision: vault::digest(&serde_json::to_vec(&self.vault.auth)?),
            access_token,
            billing_class: None,
            native_routing_supported: false,
        })
    }

    pub(super) async fn tokens(
        &mut self,
        request: TokenRequest,
    ) -> Result<TokenResponse, TokenFailure> {
        if !self.available {
            return Err(TokenFailure::Unavailable(anyhow::anyhow!(
                "credential owner unavailable; restart after diagnosis"
            )));
        }
        let current = self.snapshot()?;
        if request
            .account_id
            .as_ref()
            .is_some_and(|id| *id != current.chatgpt_account_id)
        {
            return Err(TokenFailure::AccountMismatch);
        }
        if let Some(previous) = request.previous_revision.as_ref() {
            if previous != &current.revision {
                return self.with_billing(current, request.billing).await;
            }
            if !self.refresh_enabled {
                return Err(TokenFailure::RefreshDisabled);
            }
        }
        // Serialize all calls, and persist any rotated credentials even when RPC fails.
        let force = request.previous_revision.is_some();

        let result = if self.refresh_enabled {
            self.rpc
                .as_mut()
                .context("missing credential owner")?
                .call("account/read", json!({"refreshToken":force}))
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        if result
            .as_ref()
            .is_err_and(|error| !error.is::<RoutingPolicyError>())
        {
            self.available = false;
        }
        if force
            && !self.vault.verified
            && let Some(rpc) = self.rpc.as_mut()
            && !rpc.verified_login()
        {
            rpc.inspect_rejection().await;
        }
        let snapshot = self.snapshot();
        if snapshot.is_err() {
            self.available = false;
        }
        // Persistence/identity failure wins even for a completed routing refusal.
        let current = snapshot?;
        result?;
        self.with_billing(current, request.billing).await
    }

    async fn with_billing(
        &mut self,
        mut token: TokenResponse,
        requested: bool,
    ) -> Result<TokenResponse, TokenFailure> {
        if self.rpc.is_none() {
            if requested {
                token.billing_class = Some(api::BillingClass::Unknown);
            }
            return Ok(token);
        }
        for attempt in 0..2 {
            let revision = token.revision.clone();
            let mut observed_limits = None;
            if requested {
                // Rate-limit reads can refresh too. Persist on every result.
                let result = self
                    .rpc
                    .as_mut()
                    .context("missing owner")?
                    .call("account/rateLimits/read", json!({}))
                    .await;
                let snapshot = self.snapshot();
                if result.is_err() || snapshot.is_err() {
                    self.available = false;
                }
                let limits = result?;
                token = snapshot?;
                token.billing_class = Some(billing_class(&limits));
                token.chatgpt_plan_type = limits
                    .pointer("/rateLimits/planType")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                observed_limits = Some(limits);
            }
            // The command-auth provider bypasses first-party workspace routing.
            // Re-discover after any billing refresh, before exporting credentials.
            let result = self
                .rpc
                .as_mut()
                .context("missing owner")?
                .call("account/read", json!({"refreshToken":false}))
                .await;
            let snapshot = self.snapshot();
            if result
                .as_ref()
                .is_err_and(|error| !error.is::<RoutingPolicyError>())
                || snapshot.is_err()
            {
                self.available = false;
            }
            let mut current = snapshot?;
            let account = result?;
            if supported_native_routing(&account) != Some(current.chatgpt_account_id.as_str()) {
                return Err(TokenFailure::UnsupportedRouting);
            }
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
            if let Some(limits) = observed_limits {
                self.limits = Some(limits);
            }
            return Ok(current);
        }
        Err(TokenFailure::Unavailable(anyhow::anyhow!(
            "credential state changed while checking billing; retry"
        )))
    }
}

pub(super) fn usage(response: &Value) -> Result<api::RateLimitResponse> {
    let limits = &response["rateLimits"];
    let window = |name: &str| {
        let window = &limits[name];
        window.get("usedPercent").and_then(Value::as_f64).map(|used| {
            json!({"used_percent": used, "window_minutes": window.get("windowDurationMins"), "resets_at": window.get("resetsAt")})
        })
    };
    let credits = limits.get("credits").filter(|c| !c.is_null()).map(|c| {
        json!({"has_credits": c.get("hasCredits"), "unlimited": c.get("unlimited"), "overage_limit_reached": c.get("overageLimitReached").and_then(Value::as_bool).unwrap_or(false), "balance": c.get("balance")})
    });
    let spend = limits
        .get("spendControl")
        .or_else(|| limits.get("spend_control"))
        .map(|s| json!({"reached":s.get("reached")}));
    let usage = json!({"spend_control":spend,"plan_type":limits.get("planType"), "rate_limit":{"primary":window("primary"), "secondary":window("secondary")}, "credits":credits});
    Ok(serde_json::from_value::<api::RateLimitResponse>(usage)?)
}

pub(super) fn billing_class(response: &Value) -> api::BillingClass {
    usage(response).map_or(api::BillingClass::Unknown, |u| {
        let class = u.billing_class();
        // Every reported window must have valid included headroom. Inspect the
        // protocol fields too: usage() omits windows with malformed percentages.
        let headroom = ["primary", "secondary"].into_iter().all(|name| {
            let window = &response["rateLimits"][name];
            window.is_null()
                || window["usedPercent"]
                    .as_f64()
                    .is_some_and(|used| (0.0..100.0).contains(&used))
        });
        if class == api::BillingClass::RateLimited && !headroom {
            return api::BillingClass::Unknown;
        }
        let personal_subscription = matches!(
            u.plan_type.as_deref(),
            Some("plus" | "pro" | "prolite" | "promax")
        );
        if class == api::BillingClass::RateLimited
            && personal_subscription
            && !u.credits.as_ref().is_some_and(|c| c.overage_limit_reached)
        {
            return class;
        }
        let organization = matches!(
            u.plan_type.as_deref(),
            Some("team" | "business" | "enterprise" | "edu")
        );
        let credits = u
            .credits
            .as_ref()
            .is_some_and(|c| c.has_credits || c.unlimited || c.overage_limit_reached);
        // Included subscription credits do not prove that further spending is
        // disabled. Keep entitlement classification separate from approval risk.
        if class == api::BillingClass::RateLimited
            && !u.spend_control.as_ref().is_some_and(|s| s.reached)
            && (organization || credits || u.spend_control.is_some())
        {
            api::BillingClass::Unknown
        } else {
            class
        }
    })
}

#[derive(Clone)]
struct Broker {
    owner: Arc<Mutex<Owner>>,
    state: PathBuf,
    tenant: String,
    user: String,
    failures: Arc<StdMutex<BTreeMap<&'static str, u64>>>,
}

type HttpError = (StatusCode, Json<Value>);

impl Broker {
    fn error(&self, status: StatusCode, reason: &'static str) -> HttpError {
        *self
            .failures
            .lock()
            .expect("failure counter lock")
            .entry(reason)
            .or_default() += 1;
        eprintln!(
            "{}",
            json!({"operation":"token_request","stage":"broker","reason":reason,"status":status.as_u16()})
        );
        (status, Json(json!({"error":reason})))
    }
    fn authorize(&self, headers: &HeaderMap) -> Result<(), HttpError> {
        let bearer = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        let devices = vault::devices(&self.state)
            .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
        let hash = vault::digest(bearer.as_bytes());
        let device = devices
            .iter()
            .find(|d| d.token_hash == hash && !d.revoked)
            .ok_or_else(|| self.error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
        if device.tenant != self.tenant || device.user != self.user {
            return Err(self.error(StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(())
    }
}

async fn tokens(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<TokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    // A disconnected HTTP client must not cancel a refresh after OpenAI rotates its token.
    let owner = broker.owner.clone();
    let task = tokio::spawn(async move { owner.lock().await.tokens(request).await });
    let result = task
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?
        .map_err(|error| match error {
            TokenFailure::AccountMismatch => broker.error(StatusCode::BAD_REQUEST, "account_mismatch"),
            TokenFailure::RefreshDisabled => broker.error(StatusCode::CONFLICT, "refresh_disabled"),
            TokenFailure::UnsupportedRouting => broker.error(StatusCode::CONFLICT, "unsupported_workspace_routing"),
            TokenFailure::Unavailable(error) => {
                eprintln!("{}", json!({"operation":"token_request","stage":"owner","reason":"owner_unavailable","detail":error.to_string()}));
                broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
            }
        })?;
    Ok(([("cache-control", "no-store")], Json(result)).into_response())
}

async fn metrics(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    broker.authorize(&headers)?;
    let counters = broker.failures.lock().expect("failure counter lock");
    let output: String = counters
        .iter()
        .map(|(reason, count)| {
            format!("codexctl_central_failed_requests_total{{reason=\"{reason}\"}} {count}\n")
        })
        .collect();
    Ok((
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
        .into_response())
}

pub async fn serve(
    state: &Path,
    key: &Path,
    address: SocketAddr,
    binary: &Path,
    read_only: bool,
) -> Result<()> {
    if !address.ip().is_loopback() {
        bail!("prototype binds only to loopback; use an SSH tunnel for other machines");
    }
    let _lock = vault::lock(state, "owner.lock")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("could not bind broker")?;
    let vault = vault::load(state, key)?;
    vault::validate_auth(&vault.auth)?;
    if std::fs::read_dir(state)?.any(|entry| {
        entry.is_ok_and(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("owner-runtime-")
        })
    }) {
        bail!("unfinished owner runtime exists; recover its refreshed auth before restarting");
    }
    let mut runtime = tempfile::Builder::new()
        .prefix("owner-runtime-")
        .tempdir_in(state)?;
    let home = runtime.path().to_path_buf();
    store::ensure_private_dir(home.as_path())?;
    store::atomic_write(
        &home.as_path().join("auth.json"),
        &serde_json::to_vec(&vault.auth)?,
    )?;
    store::atomic_write(&home.join("spawn-failed"), b"not-started")?;
    let tenant = vault.tenant.clone();
    let user = vault.user.clone();
    let mut refresh = Owner {
        vault,
        rpc: None,
        home,
        state: state.into(),
        key: key.into(),
        available: true,
        refresh_enabled: !read_only,
        limits: None,
        verification_input: None,
    };
    if !read_only {
        let migration_lock = Mutex::new(());
        let guard = migration_lock.lock().await;
        let proof = super::relogin::identity_inventory(state, key, &refresh.home)
            .clear_for_launch(&refresh, super::relogin::AdmissionKind::Restore, &guard)?;
        runtime.disable_cleanup(true);
        if let Err(error) = super::managed::launch_owner(&mut refresh, binary, proof).await {
            // Retain every runtime that might have refreshed credentials. A
            // proven pre-spawn failure can safely remove this temporary copy.
            if refresh.rpc.is_none() && super::managed::definitely_not_started(&refresh.home) {
                runtime.disable_cleanup(false);
            }
            return Err(error);
        }
    }
    runtime.disable_cleanup(true);
    let broker = Broker {
        state: state.into(),
        tenant,
        user,
        owner: Arc::new(Mutex::new(refresh)),
        failures: Arc::new(StdMutex::new(BTreeMap::new())),
    };
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    println!("{}", json!({"listening": listener.local_addr()?}));
    let app = Router::new()
        .route("/v1/token", post(tokens))
        .route("/metrics", get(metrics))
        .with_state(broker.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)] {
                tokio::select! { _ = interrupt.recv() => {}, _ = term.recv() => {}, _ = hup.recv() => {} }
            }
            #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
        })
        .await?;
    let mut owner = broker.owner.lock().await;
    if let Some(rpc) = owner.rpc.as_mut() {
        rpc.shutdown().await?;
    }
    owner.snapshot()?;
    if !owner.available {
        bail!("owner unavailable; runtime retained for credential recovery");
    }
    std::fs::remove_dir_all(owner.home.as_path())
        .context("could not remove private owner runtime")?;
    Ok(())
}

#[cfg(test)]
mod token_compatibility_tests {
    use super::*;
    #[test]
    fn when_the_client_has_no_managed_alias_then_it_preserves_the_legacy_request() {
        let request = TokenRequest::default();
        let encoded = serde_json::to_value(request).unwrap();
        assert!(encoded.get("alias").is_none());
    }
}

#[cfg(test)]
mod billing_tests {
    use super::*;
    #[test]
    fn organizational_limits_need_a_closed_spend_cap_to_prove_included_usage() {
        for plan in ["team", "business", "enterprise", "edu"] {
            let mut limits = json!({"rateLimits":{"planType":plan,"primary":{"usedPercent":0,"windowDurationMins":300},"credits":{"hasCredits":false,"unlimited":false}}});
            assert_eq!(billing_class(&limits), api::BillingClass::Unknown);
            limits["rateLimits"]["spendControl"] = json!({"reached":false});
            assert_eq!(billing_class(&limits), api::BillingClass::Unknown);
            limits["rateLimits"]["spendControl"] = json!({"reached":true});
            assert_eq!(billing_class(&limits), api::BillingClass::RateLimited);
        }
    }
    #[test]
    fn overage_evidence_prevents_automatic_selection() {
        let limits = json!({"rateLimits":{"planType":"pro","primary":{"usedPercent":0,"windowDurationMins":300},"credits":{"hasCredits":false,"unlimited":false,"overageLimitReached":true}}});
        assert_eq!(billing_class(&limits), api::BillingClass::Unknown);
    }
    #[test]
    fn subscription_credits_with_headroom_do_not_require_spend_control() {
        for plan in ["plus", "pro", "prolite", "promax"] {
            let mut limits = json!({"rateLimits":{"planType":plan,"primary":{"usedPercent":0,"windowDurationMins":300},"credits":{"hasCredits":true,"unlimited":false},"spendControl":{"reached":true}}});
            assert_eq!(billing_class(&limits), api::BillingClass::RateLimited);
            limits["rateLimits"]["spendControl"]["reached"] = json!(false);
            assert_eq!(billing_class(&limits), api::BillingClass::RateLimited);
            limits["rateLimits"]
                .as_object_mut()
                .unwrap()
                .remove("spendControl");
            assert_eq!(billing_class(&limits), api::BillingClass::RateLimited);
        }
    }

    macro_rules! headroom_case {
        ($name:ident, $primary:expr, $secondary:expr) => {
            #[test]
            fn $name() {
                let limits = json!({"rateLimits":{"planType":"pro",
                    "primary":$primary,"secondary":$secondary,
                    "spendControl":{"reached":true}}});
                assert_eq!(billing_class(&limits), api::BillingClass::Unknown);
            }
        };
    }
    headroom_case!(
        when_primary_exhausted_then_consent_required,
        json!({"usedPercent":100}),
        json!({"usedPercent":15})
    );
    headroom_case!(
        when_secondary_exhausted_then_consent_required,
        json!({"usedPercent":15}),
        json!({"usedPercent":100})
    );
    headroom_case!(
        when_usage_negative_then_consent_required,
        json!({"usedPercent":-1}),
        json!({"usedPercent":15})
    );
    headroom_case!(
        when_usage_missing_then_consent_required,
        json!({}),
        json!({"usedPercent":15})
    );
    headroom_case!(
        when_usage_malformed_then_consent_required,
        json!({"usedPercent":"15"}),
        json!({"usedPercent":15})
    );
}
