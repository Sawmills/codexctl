//! Server account loans (ADR 0004).
//!
//! A lender grants one server account to one borrower for a limited time. The
//! grant never copies the credential: the account server issues access tokens
//! from the lender's existing owner while the grant is active.
use crate::store;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod client;
pub(super) mod http;

/// Ended grants and audit events are kept this long.
pub(super) const RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
/// Auto-select and recovery skip a borrowed account at this window use.
pub const BORROWER_BACKOFF_PERCENT: f64 = 95.0;

/// A name for a server account: an owned alias or a borrowed reference
/// `<lender>/<alias>`. Aliases stay one path component; only this type
/// accepts the separator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountRef {
    Owned(String),
    Borrowed { lender: String, alias: String },
}

impl AccountRef {
    pub fn parse(name: &str) -> Result<Self> {
        let name = name.trim();
        match name.split_once('/') {
            None => Ok(Self::Owned(store::validate_alias(name)?.to_owned())),
            Some((lender, alias)) => {
                let (Ok(lender), Ok(alias)) =
                    (store::validate_alias(lender), store::validate_alias(alias))
                else {
                    bail!("borrowed account must be <lender>/<alias>");
                };
                Ok(Self::Borrowed {
                    lender: lender.to_ascii_lowercase(),
                    alias: alias.to_ascii_lowercase(),
                })
            }
        }
    }

    pub fn is_borrowed(&self) -> bool {
        matches!(self, Self::Borrowed { .. })
    }

    pub fn name(&self) -> String {
        match self {
            Self::Owned(alias) => alias.clone(),
            Self::Borrowed { lender, alias } => format!("{lender}/{alias}"),
        }
    }

    /// The local connection file. Borrowed accounts stay outside the owned
    /// alias namespace.
    pub fn connection_file(&self, root: &Path) -> PathBuf {
        match self {
            Self::Owned(alias) => root.join(format!("{alias}.json")),
            Self::Borrowed { lender, alias } => root
                .join("borrowed")
                .join(lender)
                .join(format!("{alias}.json")),
        }
    }
}

/// Digest of a credential's workspace and login. A grant records it at
/// creation; a token with another digest pauses the loan.
pub fn credential_subject(workspace: &str, access_token: &str) -> String {
    let login = crate::api::token_subject(access_token).unwrap_or_default();
    super::vault::digest(format!("{workspace}\0{login}").as_bytes())
}

/// Build the borrowed reference stored on a grant at creation.
pub(super) fn reference(lender_email: &str, alias: &str) -> Result<String> {
    let local = lender_email.split('@').next().unwrap_or_default();
    let name = format!("{local}/{alias}");
    match AccountRef::parse(&name)? {
        borrowed @ AccountRef::Borrowed { .. } => Ok(borrowed.name()),
        AccountRef::Owned(_) => bail!("lender email has no local part"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Revoked,
    Returned,
    Expired,
    AccountRemoved,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Revoked => "revoked",
            Self::Returned => "returned",
            Self::Expired => "expired",
            Self::AccountRemoved => "account_removed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: String,
    /// The lender's account key, `account_key(lender, alias)`.
    pub account_id: String,
    pub lender: String,
    pub borrower: String,
    pub lender_email: String,
    pub borrower_email: String,
    /// The lender's alias at grant time.
    pub alias: String,
    /// The borrowed reference, `<lender>/<alias>`, fixed at grant time.
    pub reference: String,
    /// Digest of the credential workspace and login at grant time.
    pub subject: String,
    pub created_at: i64,
    pub ends_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub ended_by: Option<String>,
    #[serde(default)]
    pub end_reason: Option<EndReason>,
}

impl Grant {
    /// True while the grant can issue tokens, before any pause check.
    pub fn active(&self, now: i64) -> bool {
        self.ended_at.is_none() && now < self.ends_at
    }

    pub fn involves(&self, user: &str) -> bool {
        self.lender == user || self.borrower == user
    }

    pub(super) fn end(&mut self, at: i64, by: &str, reason: EndReason) {
        self.ended_at = Some(at);
        self.ended_by = Some(by.to_owned());
        self.end_reason = Some(reason);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    Granted,
    TokenIssued,
    Ended,
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub at: i64,
    pub grant_id: String,
    pub kind: AuditKind,
    /// The company user who caused the event, when one did.
    #[serde(default)]
    pub actor: Option<String>,
    /// The machine that received a token.
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Events with the same key are recorded once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesce_key: Option<String>,
}

impl AuditEvent {
    pub(super) fn new(at: i64, grant_id: &str, kind: AuditKind) -> Self {
        Self {
            at,
            grant_id: grant_id.to_owned(),
            kind,
            actor: None,
            machine: None,
            reason: None,
            coalesce_key: None,
        }
    }

    /// The event stored together with a new grant.
    pub(crate) fn granted(grant: &Grant) -> Self {
        Self {
            actor: Some(grant.lender.clone()),
            ..Self::new(grant.created_at, &grant.id, AuditKind::Granted)
        }
    }

    /// The event stored together with an ended grant.
    pub(crate) fn ended(grant: &Grant) -> Self {
        Self {
            actor: grant.ended_by.clone(),
            reason: grant.end_reason.map(|reason| reason.as_str().to_owned()),
            ..Self::new(
                grant.ended_at.unwrap_or(grant.ends_at),
                &grant.id,
                AuditKind::Ended,
            )
        }
    }

    /// One token issue event per grant, machine, and UTC hour.
    pub(super) fn token_issued(at: i64, grant_id: &str, machine: &str) -> Self {
        Self {
            machine: Some(machine.to_owned()),
            coalesce_key: Some(format!("{grant_id}:{machine}:{}", at.div_euclid(3600))),
            ..Self::new(at, grant_id, AuditKind::TokenIssued)
        }
    }

    /// One pause event per grant, reason, and UTC hour.
    pub(super) fn paused(at: i64, grant_id: &str, reason: &'static str) -> Self {
        Self {
            reason: Some(reason.to_owned()),
            coalesce_key: Some(format!("{grant_id}:{reason}:{}", at.div_euclid(3600))),
            ..Self::new(at, grant_id, AuditKind::Paused)
        }
    }
}

/// What the lender asks for, with the facts the server read for it.
pub(super) struct GrantRequest<'a> {
    pub lender: &'a super::managed::User,
    pub borrower: Option<&'a super::managed::User>,
    pub alias: &'a str,
    pub account_id: &'a str,
    pub subject: &'a str,
    /// The account's next weekly reset, from fresh usage only.
    pub weekly_reset: Option<i64>,
    pub until: Option<i64>,
    pub now: i64,
}

/// Apply the grant rules. The error is a bounded reason for the HTTP answer.
pub(super) fn plan_grant(request: GrantRequest<'_>, id: String) -> Result<Grant, &'static str> {
    let borrower = request.borrower.ok_or("borrower_not_found")?;
    if borrower.id == request.lender.id {
        return Err("self_loan");
    }
    if !borrower.enabled {
        return Err("borrower_disabled");
    }
    let reset = request
        .weekly_reset
        .filter(|reset| *reset > request.now)
        .ok_or("weekly_reset_unknown")?;
    let ends_at = match request.until {
        None => reset,
        Some(until) if until <= request.now => return Err("loan_end_in_past"),
        Some(until) if until > reset => return Err("loan_end_after_weekly_reset"),
        Some(until) => until,
    };
    let reference =
        reference(&request.lender.email, request.alias).map_err(|_| "lender_name_invalid")?;
    Ok(Grant {
        id,
        account_id: request.account_id.to_owned(),
        lender: request.lender.id.clone(),
        borrower: borrower.id.clone(),
        lender_email: request.lender.email.clone(),
        borrower_email: borrower.email.clone(),
        alias: request.alias.to_owned(),
        reference,
        subject: request.subject.to_owned(),
        created_at: request.now,
        ends_at,
        ended_at: None,
        ended_by: None,
        end_reason: None,
    })
}

/// Pick the borrower's one active grant for a reference.
pub(super) fn match_reference<'a>(
    grants: &'a [Grant],
    borrower: &str,
    reference: &str,
    now: i64,
) -> Result<Option<&'a Grant>, &'static str> {
    let mut matches = grants.iter().filter(|grant| {
        grant.borrower == borrower
            && grant.active(now)
            && grant.reference.eq_ignore_ascii_case(reference)
    });
    let first = matches.next();
    if matches.next().is_some() {
        return Err("ambiguous_loan");
    }
    Ok(first)
}

/// True when a borrowed account has room for the borrower's auto-placement.
pub fn below_borrower_backoff(account: &super::managed::Account) -> bool {
    [account.primary_used, account.secondary_used]
        .into_iter()
        .flatten()
        .all(|used| used.is_finite() && used < BORROWER_BACKOFF_PERCENT)
}

/// Rank owned accounts before borrowed ones. A borrowed account enters only
/// when no owned account qualifies, and only below the borrower backoff.
pub fn owned_then_borrowed<T>(
    accounts: &[super::managed::Account],
    pick: impl Fn(&[super::managed::Account]) -> Result<T>,
) -> Result<T> {
    if accounts.iter().all(|account| account.loan.is_none()) {
        return pick(accounts);
    }
    let (owned, borrowed): (Vec<_>, Vec<_>) = accounts
        .iter()
        .cloned()
        .partition(|account| account.loan.is_none());
    pick(&owned).or_else(|error| {
        let roomy: Vec<_> = borrowed
            .into_iter()
            .filter(below_borrower_backoff)
            .collect();
        pick(&roomy).map_err(|_| error)
    })
}

#[cfg(test)]
mod tests;
