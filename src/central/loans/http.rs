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

fn owner_subject(owner: &Owner) -> Option<super::CredentialSubject> {
    Some(super::credential_subject(
        &vault::account(&owner.vault.auth).ok()?,
        vault::token(&owner.vault.auth).ok()?,
    ))
}

/// The lender's owner for a grant, with the login the shared store holds now.
struct GrantOwner {
    key: String,
    owner: Arc<Mutex<Owner>>,
    shared_subject: Option<super::CredentialSubject>,
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

/// Complete usage for borrowed catalog entries, per server state and account.
type UsageEntry = std::sync::Arc<
    tokio::sync::Mutex<Option<(std::time::Instant, Option<crate::statusline::Usage>)>>,
>;
static COMPLETE_USAGE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, UsageEntry>>,
> = std::sync::OnceLock::new();
const COMPLETE_USAGE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

impl Broker {
    /// A complete (all-window) usage snapshot for a borrowed entry. Results
    /// and failures are kept for 60 seconds. A per-account lock lets
    /// concurrent polls of one account share a single upstream request,
    /// while other accounts never wait behind it.
    pub(in crate::central) async fn complete_usage(
        &self,
        key: &str,
        access: &str,
        account_id: &str,
    ) -> Option<crate::statusline::Usage> {
        let id = format!("{}\0{key}", self.state.display());
        let entry = COMPLETE_USAGE
            .get_or_init(Default::default)
            .lock()
            .expect("complete usage map")
            .entry(id)
            .or_default()
            .clone();
        let mut entry = entry.lock().await;
        if let Some((at, usage)) = entry.as_ref()
            && at.elapsed() < COMPLETE_USAGE_TTL
        {
            return usage.clone();
        }
        let usage = match self.catalog.fetch_direct(access, account_id).await {
            Ok(usage) => Some(crate::statusline::Usage::from_usage(&usage)),
            Err(reason) => {
                self.record_failure(reason, "borrowed_usage", StatusCode::SERVICE_UNAVAILABLE);
                None
            }
        };
        *entry = Some((std::time::Instant::now(), usage.clone()));
        usage
    }

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

    /// End grants past their end time. The store audits each end. One store
    /// sweep is bounded, so repeat it until no due grant is left; every round
    /// ends or skips each row it selected.
    async fn expire_loans(&self) -> Result<(), HttpError> {
        let now = now();
        let store = self.loan_store();
        let mut any = false;
        loop {
            let expired = store
                .expire_loans(now)
                .await
                .map_err(|error| self.loan_failure("expire", error))?;
            any |= !expired.is_empty();
            // A concurrent request can win every row of a batch, so an empty
            // result alone does not prove that no due grant is left.
            if expired.is_empty()
                && !store
                    .has_due_loans(now)
                    .await
                    .map_err(|error| self.loan_failure("expire", error))?
            {
                break;
            }
        }
        if any {
            self.retire_loans().await;
        }
        Ok(())
    }

    /// Expire due grants and apply the retention before a history read, so
    /// rows past 90 days are hidden even when no loan changed meanwhile.
    /// A failed retirement fails the read, so no row past the window shows.
    async fn expire_and_retire(&self) -> Result<(), HttpError> {
        self.expire_loans().await?;
        self.loan_store()
            .retire_loans(now())
            .await
            .map_err(|error| self.loan_failure("retire", error))
    }

    /// Apply the 90-day retention after a grant or an end. The grant or end
    /// is already committed, so a failed retirement is recorded, not returned.
    async fn retire_loans(&self) {
        if let Err(error) = self.loan_store().retire_loans(now()).await {
            self.record_failure(
                "loan_retire_failed",
                "loan",
                StatusCode::SERVICE_UNAVAILABLE,
            );
            eprintln!(
                "{}",
                serde_json::json!({"operation":"loan","stage":"retire","error":error.to_string()})
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
            self.retire_loans().await;
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
            .active_loans_for_borrower(borrower)
            .await
            .map_err(|error| self.loan_failure("load", error))?
            .into_iter()
            .filter(|grant| grant.borrower == borrower && grant.active(now))
            .collect())
    }

    /// The lender's owner for a grant. A missing lender alias ends the grant.
    async fn grant_owner(&self, grant: &Grant) -> Result<Option<GrantOwner>, HttpError> {
        // A cached owner can outlive a tombstoned or replaced shared-store
        // account, so the shared store decides whether the lender's account
        // still exists and which login it holds now.
        let mut shared_subject = None;
        let removed = match self
            .central
            .as_ref()
            .filter(|central| central.mode() != StoreMode::File)
        {
            Some(central) => match central
                .load_account_by_alias(&grant.lender, &grant.alias)
                .await
                .map_err(|_| self.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
            {
                Some(record) => {
                    shared_subject = serde_json::from_value::<vault::Vault>(record.vault)
                        .ok()
                        .and_then(|vault| {
                            Some(super::credential_subject(
                                &vault::account(&vault.auth).ok()?,
                                vault::token(&vault.auth).ok()?,
                            ))
                        });
                    false
                }
                None => true,
            },
            None => false,
        };
        if !removed
            && let Some((key, owner)) = self.resolve_alias(&grant.lender, &grant.alias).await?
        {
            return Ok(Some(GrantOwner {
                key,
                owner,
                shared_subject,
            }));
        }
        // An unresolved recovery is not proof that the alias was removed.
        if !removed
            && self
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
        loan_id: Option<&str>,
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
        // A connection selected with an earlier grant never moves to a later
        // grant of the same name; that needs a new selection.
        if loan_id.is_some_and(|id| id != grant.id) {
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        }
        if !self.lender_enabled(&grant.lender).await? {
            return Err(self.pause(&grant, "lender_disabled").await);
        }
        let Some(found) = self.grant_owner(&grant).await? else {
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        };
        // Pause before the owned-account checks when the lender's credential
        // already names another login. The shared store's credential decides
        // when there is one; otherwise a busy owner skips this early check.
        // The check after the refresh still applies.
        // `None` means the owner is busy and the early check is skipped.
        let current = match found.shared_subject.clone() {
            Some(subject) => Some(Some(subject)),
            None => found
                .owner
                .try_lock()
                .ok()
                .map(|owner| owner_subject(&owner)),
        };
        if let Some(subject) = current
            && !subject.is_some_and(|subject| grant.subject.same_login(&subject))
        {
            return Err(self.pause(&grant, "subject_changed").await);
        }
        Ok(Borrowed {
            grant,
            owner: found.owner,
        })
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
        // The token must be the lender's current login.
        if !grant.subject.same_login(&super::credential_subject(
            &token.chatgpt_account_id,
            &token.access_token,
        )) {
            return Err(self.pause(grant, "subject_changed").await);
        }
        // The lender's account may have been removed or replaced while the
        // token was prepared. This can wait for the owner, so it runs before
        // the grant and lender checks below.
        let Some(found) = self.grant_owner(grant).await? else {
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        };
        let now_subject = match found.shared_subject {
            Some(subject) => Some(subject),
            None => owner_subject(&*found.owner.lock().await),
        };
        if !now_subject.is_some_and(|subject| grant.subject.same_login(&subject)) {
            return Err(self.pause(grant, "subject_changed").await);
        }
        if !self.lender_enabled(&grant.lender).await? {
            return Err(self.pause(grant, "lender_disabled").await);
        }
        self.audit_event(AuditEvent::token_issued(now(), &grant.id, &device.id))
            .await?;
        // The grant check is the last store read before delivery; only the
        // caller's machine re-authorization follows it.
        let current = self
            .loan_store()
            .load_loan(&grant.id)
            .await
            .map_err(|error| self.loan_failure("recheck", error))?;
        if !current.is_some_and(|current| current.active(now())) {
            self.expire_loans().await?;
            return Err(self.error(StatusCode::FORBIDDEN, "loan_ended"));
        }
        Ok(())
    }

    /// Borrowed accounts for the borrower's catalog. Ended or removed grants
    /// are left out; a paused grant stays visible as unavailable.
    pub(in crate::central) async fn borrowed_catalog(
        &self,
        borrower: &str,
    ) -> Result<Vec<BorrowedEntry>, HttpError> {
        let mut entries = Vec::new();
        let grants = self.borrowed_grants(borrower).await?;
        for grant in grants.iter().cloned() {
            let Some(GrantOwner {
                key,
                owner,
                shared_subject,
            }) = self.grant_owner(&grant).await?
            else {
                continue;
            };
            let ambiguous = grants
                .iter()
                .filter(|other| other.reference.eq_ignore_ascii_case(&grant.reference))
                .count()
                > 1;
            // Token issue refuses these too; the catalog keeps them unselectable.
            let paused = if ambiguous {
                Some("ambiguous_loan")
            } else if !self.lender_enabled(&grant.lender).await? {
                Some("lender_disabled")
            } else if !match shared_subject {
                Some(subject) => Some(subject),
                None => owner_subject(&*owner.lock().await),
            }
            .is_some_and(|subject| grant.subject.same_login(&subject))
            {
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
    // Loans cover Codex (OpenAI) server accounts only. Server accounts have no
    // provider field today; when one is added (SAW-12454), refuse a loan here
    // for any provider other than OpenAI.
    let owner = broker.owner(&device, alias).await?;
    // Fresh usage gives the weekly reset; a stale reading refuses the grant.
    let weekly_reset = managed::account_catalog(&broker, &device.user, catalog::Freshness::Cached)
        .await?
        .into_iter()
        .find(|account| account.loan.is_none() && account.alias.eq_ignore_ascii_case(alias))
        .filter(|account| !account.usage_stale)
        // `resets_at` belongs to the longest window; only an exact week counts.
        .filter(|account| {
            account
                .secondary_window_seconds
                .is_some_and(|seconds| seconds == WEEK_SECONDS)
        })
        .and_then(|account| {
            // A relative reset is re-anchored to now on every read of a cached
            // sample. Taking off the sample's age never ends a loan late; an
            // absolute reset ends at most one cache period early.
            let age = i64::try_from(account.usage_age_seconds?).ok()?;
            Some(account.resets_at?.saturating_sub(age))
        });
    let (lender_alias, account_id, subject) = {
        let owner = owner.lock().await;
        (
            owner.vault.alias.trim().to_owned(),
            account_key(&owner.vault.user, &owner.vault.alias),
            // A grant must name a login, or no later token could match it.
            owner_subject(&owner)
                .filter(|subject| subject.uid.is_some() || subject.sub.is_some())
                .ok_or_else(|| {
                    broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable")
                })?,
        )
    };
    let users = broker.company_users().await?;
    let lender = users
        .iter()
        .find(|user| user.id == device.user)
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "user_disabled"))?;
    // An email held by two company-user records names no one for certain.
    let mut matches = users.iter().filter(|user| {
        user.email
            .eq_ignore_ascii_case(request.borrower_email.trim())
    });
    let borrower = matches.next();
    if matches.next().is_some() {
        return Err(broker.error(StatusCode::CONFLICT, "borrower_ambiguous"));
    }
    broker.expire_loans().await?;
    let expired = broker
        .loan_store()
        .expire_account_loans(&account_id, now())
        .await
        .map_err(|error| broker.loan_failure("expire", error))?;
    if !expired.is_empty() {
        broker.retire_loans().await;
    }
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
    // Renames hold the import lock; take it so a rename cannot slip between
    // this alias check and the grant insert.
    let _renames = broker.imports.lock().await;
    if !owner
        .lock()
        .await
        .vault
        .alias
        .trim()
        .eq_ignore_ascii_case(&grant.alias)
    {
        return Err(broker.error(StatusCode::CONFLICT, "account_changed"));
    }
    let store = broker.loan_store();
    if !store
        .create_loan(&grant)
        .await
        .map_err(|error| broker.loan_failure("create", error))?
    {
        return Err(broker.error(StatusCode::CONFLICT, "loan_exists"));
    }
    broker.retire_loans().await;
    Ok((StatusCode::CREATED, Json(grant)).into_response())
}

async fn list(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    broker.expire_and_retire().await?;
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
    broker.expire_and_retire().await?;
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
    /// Return events recorded before this time; the previous page's cursor.
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

const AUDIT_PAGE: usize = 1_000;
const AUDIT_PAGE_MAX: usize = 10_000;

/// One page of audit events, newest first. `before` is set when older
/// events remain; pass it back to read them.
#[derive(serde::Serialize)]
struct AuditPage {
    events: Vec<AuditEvent>,
    before: Option<i64>,
}

/// Cut a newest-first page of `limit + 1` events. Events that share the
/// boundary time all move to the next page, so the time cursor loses none.
/// `None` means every event on the page shares the boundary time.
fn audit_page(mut events: Vec<AuditEvent>, limit: usize) -> Option<AuditPage> {
    if events.len() <= limit {
        return Some(AuditPage {
            events,
            before: None,
        });
    }
    let boundary = events[limit].at;
    let kept = events
        .iter()
        .take_while(|event| event.at > boundary)
        .count();
    if kept == 0 {
        return None;
    }
    events.truncate(kept);
    Some(AuditPage {
        events,
        before: Some(boundary.saturating_add(1)),
    })
}

async fn audit(
    State(broker): State<Broker>,
    headers: HeaderMap,
    query: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, HttpError> {
    let device = broker.authorize(&headers).await?;
    let Query(query) =
        query.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    broker.expire_and_retire().await?;
    let store = broker.loan_store();
    if let Some(id) = query.id.as_deref()
        && !store
            .load_loan(id)
            .await
            .map_err(|error| broker.loan_failure("load", error))?
            .is_some_and(|grant| grant.involves(&device.user))
    {
        return Err(broker.error(StatusCode::NOT_FOUND, "loan_not_found"));
    }
    let wanted = query.id.as_deref();
    let limit = query.limit.unwrap_or(AUDIT_PAGE).clamp(1, AUDIT_PAGE_MAX);
    let events = store
        .loan_audit(&device.user, wanted, query.before, limit + 1)
        .await
        .map_err(|error| broker.loan_failure("audit", error))?;
    let newest = events.first().map(|event| event.at);
    let page = match (audit_page(events, limit), newest) {
        (Some(page), _) => page,
        // More than one page in one second: return that whole second.
        (None, Some(newest)) => AuditPage {
            events: store
                .loan_audit(
                    &device.user,
                    wanted,
                    Some(newest.saturating_add(1)),
                    AUDIT_PAGE_MAX,
                )
                .await
                .map_err(|error| broker.loan_failure("audit", error))?
                .into_iter()
                .filter(|event| event.at == newest)
                .collect(),
            before: Some(newest),
        },
        (None, None) => AuditPage {
            events: Vec::new(),
            before: None,
        },
    };
    Ok(([("cache-control", "no-store")], Json(page)).into_response())
}

#[cfg(test)]
mod tests;
