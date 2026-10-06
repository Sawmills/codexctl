//! Loan routes and borrowed-account resolution on the account server.
//!
//! `Broker::owner` and `resolve_alias` stay lender-only. Only `/v1/token` and
//! the catalog call `borrowed_owner`, so every other path keeps refusing a
//! borrowed reference.
use super::{AccountRef, AuditEvent, EndReason, Grant, GrantRequest};
use crate::central::{
    catalog,
    managed::{self, Broker, HttpError, User, account_key},
    server::{Owner, TokenResponse},
    storage::{CentralStore, StoreMode},
    vault,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(in crate::central) fn routes() -> Router<Broker> {
    Router::new()
        .route("/v1/loans", get(list).post(lend))
        .route("/v1/loans/end", post(end))
        .route("/v1/loans/audit", get(audit))
}

/// A weekly rate-limit window.
const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn owner_subject(owner: &Owner) -> Option<String> {
    Some(super::credential_subject(
        &vault::account(&owner.vault.auth).ok()?,
        vault::token(&owner.vault.auth).ok()?,
    ))
}

/// The record of a borrowed account that `/v1/token` serves.
pub(in crate::central) struct Borrowed {
    pub grant: Grant,
    pub owner: Arc<Mutex<Owner>>,
}

/// A borrowed catalog entry; `paused` is a bounded reason or `None`.
pub(in crate::central) struct BorrowedEntry {
    pub grant: Grant,
    pub key: String,
    pub owner: Arc<Mutex<Owner>>,
    pub paused: Option<&'static str>,
}

impl Broker {
    /// Loans use the configured store, or the state directory's file store.
    pub(in crate::central) fn loan_store(&self) -> CentralStore {
        self.central
            .clone()
            .unwrap_or_else(|| CentralStore::file(&self.state, &self.key))
    }

    fn loan_failure(&self, stage: &'static str, error: anyhow::Error) -> HttpError {
        eprintln!(
            "{}",
            serde_json::json!({"operation":"loan","stage":stage,"error":error.to_string()})
        );
        self.error(StatusCode::SERVICE_UNAVAILABLE, "loan_store_failed")
    }

    async fn company_users(&self) -> Result<Vec<User>, HttpError> {
        let users = if let Some(central) = self
            .central
            .as_ref()
            .filter(|central| central.mode() != StoreMode::File)
        {
            managed::central_registry_users(central).await
        } else if let Some(registry) = self.registry.as_ref() {
            registry
                .read()
                .map(|registry| registry.users.clone())
                .map_err(|_| anyhow::anyhow!("registry lock poisoned"))
        } else {
            managed::users(&self.state)
        };
        users.map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))
    }

    async fn lender_enabled(&self, lender: &str) -> Result<bool, HttpError> {
        if let Some(central) = self
            .central
            .as_ref()
            .filter(|central| central.mode() != StoreMode::File)
        {
            return central
                .enabled_user(lender)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"));
        }
        Ok(self
            .company_users()
            .await?
            .iter()
            .any(|user| user.id == lender && user.enabled))
    }

    async fn audit_event(&self, event: AuditEvent) -> Result<(), HttpError> {
        self.loan_store()
            .append_loan_audit(&event)
            .await
            .map_err(|error| self.loan_failure("audit", error))
    }

    /// End grants past their end time. The store audits each end.
    async fn expire_loans(&self) -> Result<(), HttpError> {
        let expired = self
            .loan_store()
            .expire_loans(now())
            .await
            .map_err(|error| self.loan_failure("expire", error))?;
        if !expired.is_empty() {
            self.prune_loans().await;
        }
        Ok(())
    }

    /// Apply the 90-day retention after a grant or an end. The grant or end
    /// is already committed, so a failed prune is recorded, not returned.
    async fn prune_loans(&self) {
        if let Err(error) = self.loan_store().prune_loans(now()).await {
            self.record_failure("loan_prune_failed", "loan", StatusCode::SERVICE_UNAVAILABLE);
            eprintln!(
                "{}",
                serde_json::json!({"operation":"loan","stage":"prune","error":error.to_string()})
            );
        }
    }

    async fn end_grant(
        &self,
        grant: &Grant,
        by: &str,
        reason: EndReason,
    ) -> Result<Option<Grant>, HttpError> {
        let ended = self
            .loan_store()
            .end_loan(&grant.id, now(), by, reason)
            .await
            .map_err(|error| self.loan_failure("end", error))?;
        if ended.is_some() {
            self.prune_loans().await;
        }
        Ok(ended)
    }

    async fn pause(&self, grant: &Grant, reason: &'static str) -> HttpError {
        if let Err(error) = self
            .audit_event(AuditEvent::paused(now(), &grant.id, reason))
            .await
        {
            return error;
        }
        self.error(StatusCode::CONFLICT, "loan_paused")
    }

    /// The borrower's active grants, after expiry.
    async fn borrowed_grants(&self, borrower: &str) -> Result<Vec<Grant>, HttpError> {
        self.expire_loans().await?;
        let now = now();
        Ok(self
            .loan_store()
            .loans_for_user(borrower)
            .await
            .map_err(|error| self.loan_failure("load", error))?
            .into_iter()
            .filter(|grant| grant.borrower == borrower && grant.active(now))
            .collect())
    }

    /// The lender's owner for a grant. A missing lender alias ends the grant.
    async fn grant_owner(
        &self,
        grant: &Grant,
    ) -> Result<Option<(String, Arc<Mutex<Owner>>)>, HttpError> {
        if let Some(found) = self.resolve_alias(&grant.lender, &grant.alias).await? {
            return Ok(Some(found));
        }
        // An unresolved recovery is not proof that the alias was removed.
        if self
            .ownership_unresolved
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(self.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
        }
        self.end_grant(grant, &grant.lender, EndReason::AccountRemoved)
            .await?;
        Ok(None)
    }

    /// Resolve a borrowed reference for `/v1/token`. The grant must be active
    /// and the lender enabled; the subject is checked against the issued token.
    pub(in crate::central) async fn borrowed_owner(
        &self,
        device: &vault::Device,
        reference: &str,
    ) -> Result<Borrowed, HttpError> {
        let reference = match AccountRef::parse(reference) {
            Ok(reference @ AccountRef::Borrowed { .. }) => reference.name(),
            _ => return Err(self.error(StatusCode::BAD_REQUEST, "invalid_alias")),
        };
        let grants = self.borrowed_grants(&device.user).await?;
        let grant = match super::match_reference(&grants, &device.user, &reference, now()) {
            Ok(Some(grant)) => grant.clone(),
            Ok(None) => return Err(self.ended_or_missing(&device.user, &reference).await?),
            Err(reason) => return Err(self.error(StatusCode::CONFLICT, reason)),
        };
        if !self.lender_enabled(&grant.lender).await? {
            return Err(self.pause(&grant, "lender_disabled").await);
        }
        let Some((_, owner)) = self.grant_owner(&grant).await? else {
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        };
        Ok(Borrowed { grant, owner })
    }

    async fn ended_or_missing(
        &self,
        borrower: &str,
        reference: &str,
    ) -> Result<HttpError, HttpError> {
        let ended = self
            .loan_store()
            .loans_for_user(borrower)
            .await
            .map_err(|error| self.loan_failure("load", error))?
            .iter()
            .any(|grant| {
                grant.borrower == borrower && grant.reference.eq_ignore_ascii_case(reference)
            });
        Ok(if ended {
            self.error(StatusCode::FORBIDDEN, "loan_ended")
        } else {
            self.error(StatusCode::NOT_FOUND, "account_not_found")
        })
    }

    /// Recheck a grant after the token was produced, next to the machine
    /// re-authorization, and record the issue.
    pub(in crate::central) async fn confirm_borrowed_token(
        &self,
        grant: &Grant,
        device: &vault::Device,
        token: &TokenResponse,
    ) -> Result<(), HttpError> {
        let current = self
            .loan_store()
            .load_loan(&grant.id)
            .await
            .map_err(|error| self.loan_failure("recheck", error))?;
        if !current.is_some_and(|current| current.active(now())) {
            self.expire_loans().await?;
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        }
        if !self.lender_enabled(&grant.lender).await? {
            return Err(self.pause(grant, "lender_disabled").await);
        }
        if super::credential_subject(&token.chatgpt_account_id, &token.access_token)
            != grant.subject
        {
            return Err(self.pause(grant, "subject_changed").await);
        }
        self.audit_event(AuditEvent::token_issued(now(), &grant.id, &device.id))
            .await
    }

    /// Borrowed accounts for the borrower's catalog. Ended or removed grants
    /// are left out; a paused grant stays visible as unavailable.
    pub(in crate::central) async fn borrowed_catalog(
        &self,
        borrower: &str,
    ) -> Result<Vec<BorrowedEntry>, HttpError> {
        let mut entries = Vec::new();
        for grant in self.borrowed_grants(borrower).await? {
            let Some((key, owner)) = self.grant_owner(&grant).await? else {
                continue;
            };
            let paused = if !self.lender_enabled(&grant.lender).await? {
                Some("lender_disabled")
            } else if owner_subject(&*owner.lock().await).as_deref() != Some(&grant.subject) {
                Some("subject_changed")
            } else {
                None
            };
            entries.push(BorrowedEntry {
                grant,
                key,
                owner,
                paused,
            });
        }
        Ok(entries)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LendRequest {
    alias: String,
    borrower_email: String,
    #[serde(default)]
    until: Option<i64>,
}

fn grant_status(reason: &str) -> StatusCode {
    match reason {
        "borrower_not_found" => StatusCode::NOT_FOUND,
        "self_loan" | "loan_end_in_past" | "loan_end_after_weekly_reset" => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    }
}

async fn lend(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<LendRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let alias = managed::normalize_alias(&request.alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    let owner = broker.owner(&device, alias).await?;
    // Fresh usage gives the weekly reset; a stale reading refuses the grant.
    let weekly_reset = managed::account_catalog(&broker, &device.user, catalog::Freshness::Cached)
        .await?
        .into_iter()
        .find(|account| account.loan.is_none() && account.alias.eq_ignore_ascii_case(alias))
        .filter(|account| !account.usage_stale)
        // `resets_at` belongs to the longest window; only a weekly one counts.
        .filter(|account| {
            account
                .secondary_window_seconds
                .is_some_and(|seconds| seconds >= WEEK_SECONDS)
        })
        .and_then(|account| account.resets_at);
    let (lender_alias, account_id, subject) = {
        let owner = owner.lock().await;
        (
            owner.vault.alias.trim().to_owned(),
            account_key(&owner.vault.user, &owner.vault.alias),
            owner_subject(&owner).ok_or_else(|| {
                broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
            })?,
        )
    };
    let users = broker.company_users().await?;
    let lender = users
        .iter()
        .find(|user| user.id == device.user)
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "user_disabled"))?;
    let borrower = users.iter().find(|user| {
        user.email
            .eq_ignore_ascii_case(request.borrower_email.trim())
    });
    broker.expire_loans().await?;
    let grant = super::plan_grant(
        GrantRequest {
            lender,
            borrower,
            alias: &lender_alias,
            account_id: &account_id,
            subject: &subject,
            weekly_reset,
            until: request.until,
            now: now(),
        },
        vault::digest(&crate::central::enrollment::random_bytes()),
    )
    .map_err(|reason| broker.error(grant_status(reason), reason))?;
    let store = broker.loan_store();
    if !store
        .create_loan(&grant)
        .await
        .map_err(|error| broker.loan_failure("create", error))?
    {
        return Err(broker.error(StatusCode::CONFLICT, "loan_exists"));
    }
    broker.prune_loans().await;
    Ok((StatusCode::CREATED, Json(grant)).into_response())
}

async fn list(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    broker.expire_loans().await?;
    let mut grants = broker
        .loan_store()
        .loans_for_user(&device.user)
        .await
        .map_err(|error| broker.loan_failure("load", error))?;
    grants.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    Ok(([("cache-control", "no-store")], Json(grants)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndRequest {
    id: String,
}

async fn end(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<EndRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.expire_loans().await?;
    let store = broker.loan_store();
    let grant = store
        .load_loan(&request.id)
        .await
        .map_err(|error| broker.loan_failure("load", error))?
        .filter(|grant| grant.involves(&device.user))
        .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "loan_not_found"))?;
    let reason = if grant.lender == device.user {
        EndReason::Revoked
    } else {
        EndReason::Returned
    };
    let ended = match broker.end_grant(&grant, &device.user, reason).await? {
        Some(ended) => ended,
        None => store
            .load_loan(&grant.id)
            .await
            .map_err(|error| broker.loan_failure("load", error))?
            .unwrap_or(grant),
    };
    Ok(Json(ended).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    #[serde(default)]
    id: Option<String>,
}

async fn audit(
    State(broker): State<Broker>,
    headers: HeaderMap,
    query: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Query(query) =
        query.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let store = broker.loan_store();
    let ids: Vec<String> = store
        .loans_for_user(&device.user)
        .await
        .map_err(|error| broker.loan_failure("load", error))?
        .into_iter()
        .map(|grant| grant.id)
        .filter(|id| query.id.as_ref().is_none_or(|wanted| wanted == id))
        .collect();
    if query.id.is_some() && ids.is_empty() {
        return Err(broker.error(StatusCode::NOT_FOUND, "loan_not_found"));
    }
    let mut events = store
        .loan_audit(&ids)
        .await
        .map_err(|error| broker.loan_failure("audit", error))?;
    events.sort_by_key(|event| event.at);
    Ok(([("cache-control", "no-store")], Json(events)).into_response())
}

#[cfg(test)]
mod tests;
