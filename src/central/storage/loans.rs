//! Loan grants and their audit log (ADR 0004).
//!
//! File mode keeps both in the encrypted `FileState`. PostgreSQL keeps them in
//! typed columns of `account_loans` and `account_loan_audit`. Dual mode writes
//! PostgreSQL first and mirrors the authoritative grant to the file; an ended
//! grant always wins, so a delayed mirror can never reopen a loan.
//!
//! Every grant, end, and expiry is written together with its audit event.
//! Retention never deletes a row: it sets `deleted_at`, and reads hide it.
use super::{CentralStore, FileStore, PostgresStore, bounded_db};
use crate::central::loans::{
    AuditEvent, AuditKind, CredentialSubject, EndReason, Grant, RETENTION_SECONDS,
};
use anyhow::Result;

/// Bound on history rows read for one caller.
const READ_LIMIT: usize = 10_000;

// Table and column names shared by the DDL and every query.
macro_rules! loans_table {
    () => {
        "account_loans"
    };
}
macro_rules! audit_table {
    () => {
        "account_loan_audit"
    };
}
macro_rules! grant_columns {
    () => {
        "id,account_id,lender_id,borrower_id,lender_email,borrower_email,alias,reference,subject_workspace,subject_uid,subject_sub,created_at,ends_at,ended_at,ended_by,end_reason,deleted_at"
    };
}
macro_rules! audit_columns {
    () => {
        "grant_id,at,kind,actor,machine,reason,coalesce_key,deleted_at"
    };
}
macro_rules! audit_columns_as_a {
    () => {
        "a.grant_id,a.at,a.kind,a.actor,a.machine,a.reason,a.coalesce_key,a.deleted_at"
    };
}

// Servers can start together. The transaction-scoped advisory lock makes
// concurrent `CREATE ... IF NOT EXISTS` runs wait instead of racing.
pub(super) const SCHEMA: &str = concat!(
    "BEGIN;
SELECT pg_advisory_xact_lock(hashtext('codexctl.account_loans.schema'));
CREATE TABLE IF NOT EXISTS ",
    loans_table!(),
    " (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    lender_id TEXT NOT NULL,
    borrower_id TEXT NOT NULL,
    lender_email TEXT NOT NULL,
    borrower_email TEXT NOT NULL,
    alias TEXT NOT NULL,
    reference TEXT NOT NULL,
    subject_workspace TEXT NOT NULL,
    subject_uid TEXT,
    subject_sub TEXT,
    created_at BIGINT NOT NULL,
    ends_at BIGINT NOT NULL,
    ended_at BIGINT,
    ended_by TEXT,
    end_reason TEXT CHECK (end_reason IN ('revoked','returned','expired','account_removed')),
    deleted_at BIGINT,
    CHECK ((ended_at IS NULL) = (end_reason IS NULL)),
    CHECK (deleted_at IS NULL OR ended_at IS NOT NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS account_loans_one_active ON ",
    loans_table!(),
    " (account_id) WHERE ended_at IS NULL;
CREATE INDEX IF NOT EXISTS account_loans_lender_idx ON ",
    loans_table!(),
    " (lender_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS account_loans_borrower_idx ON ",
    loans_table!(),
    " (borrower_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS account_loans_expiry_idx ON ",
    loans_table!(),
    " (ends_at) WHERE ended_at IS NULL;
CREATE INDEX IF NOT EXISTS account_loans_retire_idx ON ",
    loans_table!(),
    " (ended_at) WHERE ended_at IS NOT NULL AND deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS ",
    audit_table!(),
    " (
    id BIGSERIAL PRIMARY KEY,
    grant_id TEXT NOT NULL REFERENCES ",
    loans_table!(),
    "(id),
    at BIGINT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('granted','token_issued','ended','paused')),
    actor TEXT,
    machine TEXT,
    reason TEXT,
    coalesce_key TEXT UNIQUE,
    deleted_at BIGINT
);
CREATE INDEX IF NOT EXISTS account_loan_audit_grant_idx ON ",
    audit_table!(),
    " (grant_id, at) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS account_loan_audit_retire_idx ON ",
    audit_table!(),
    " (at) WHERE deleted_at IS NULL;
COMMIT;
"
);

fn grant_from_row(row: &tokio_postgres::Row) -> Result<Grant> {
    Ok(Grant {
        id: row.try_get("id")?,
        account_id: row.try_get("account_id")?,
        lender: row.try_get("lender_id")?,
        borrower: row.try_get("borrower_id")?,
        lender_email: row.try_get("lender_email")?,
        borrower_email: row.try_get("borrower_email")?,
        alias: row.try_get("alias")?,
        reference: row.try_get("reference")?,
        subject: CredentialSubject {
            workspace: row.try_get("subject_workspace")?,
            uid: row.try_get("subject_uid")?,
            sub: row.try_get("subject_sub")?,
        },
        created_at: row.try_get("created_at")?,
        ends_at: row.try_get("ends_at")?,
        ended_at: row.try_get("ended_at")?,
        ended_by: row.try_get("ended_by")?,
        end_reason: row
            .try_get::<_, Option<String>>("end_reason")?
            .as_deref()
            .map(EndReason::parse)
            .transpose()?,
        deleted_at: row.try_get("deleted_at")?,
    })
}

fn audit_from_row(row: &tokio_postgres::Row) -> Result<AuditEvent> {
    Ok(AuditEvent {
        grant_id: row.try_get("grant_id")?,
        at: row.try_get("at")?,
        kind: AuditKind::parse(row.try_get("kind")?)?,
        actor: row.try_get("actor")?,
        machine: row.try_get("machine")?,
        reason: row.try_get("reason")?,
        coalesce_key: row.try_get("coalesce_key")?,
        deleted_at: row.try_get("deleted_at")?,
    })
}

fn mirror_failure(store: &CentralStore, stage: &str, error: &anyhow::Error) {
    if let CentralStore::Dual {
        mirror_failures, ..
    } = store
    {
        mirror_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    eprintln!(
        "{}",
        serde_json::json!({"operation":"central_store_mirror","backend":"file","stage":stage,"reason":error.to_string()})
    );
}

impl CentralStore {
    /// End every active grant whose end time has passed, each with its audit
    /// event. Returns the grants this call ended.
    pub async fn expire_loans(&self, now: i64) -> Result<Vec<Grant>> {
        self.expire(None, now).await
    }

    /// Record a token issue only if its grant is still active, and report
    /// whether it was. One atomic step: PostgreSQL locks the grant row for
    /// the statement and file mode holds the store lock, so an end commits
    /// either before (no token) or after (the token was issued first).
    pub async fn issue_token(&self, grant_id: &str, machine: &str) -> Result<bool> {
        match self {
            Self::File(file) => file.issue_token(grant_id, machine),
            Self::Postgres(db) => Ok(bounded_db(db.issue_token(grant_id, machine))
                .await?
                .is_some()),
            Self::Dual { file, postgres, .. } => {
                let issued = bounded_db(postgres.issue_token(grant_id, machine)).await?;
                // Mirror the committed event as is; the grant may already
                // show as ended in the file by now.
                if let Some(at) = issued
                    && let Err(error) =
                        file.mirror_audit(&AuditEvent::token_issued(at, grant_id, machine))
                {
                    mirror_failure(self, "issue_token", &error);
                }
                Ok(issued.is_some())
            }
        }
    }

    /// Whether the account has an active grant. Account-scoped and uncapped,
    /// for guards such as the alias rename.
    pub async fn has_active_loan(&self, account_id: &str, now: i64) -> Result<bool> {
        match self {
            Self::File(file) => Ok(file.read_state()?.loans.values().any(|grant| {
                grant.account_id == account_id && grant.active(now) && grant.deleted_at.is_none()
            })),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.has_active_loan(account_id, now)).await
            }
        }
    }

    /// Whether any unended, unretired grant is past its end time.
    pub async fn has_due_loans(&self, now: i64) -> Result<bool> {
        match self {
            Self::File(file) => Ok(file.read_state()?.loans.values().any(|grant| {
                grant.ended_at.is_none() && grant.deleted_at.is_none() && grant.ends_at <= now
            })),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.has_due_loans(now)).await
            }
        }
    }

    /// Expire one account's past grant, independent of the bounded sweep, so
    /// a new grant for that account is never refused by a stale one.
    pub async fn expire_account_loans(&self, account_id: &str, now: i64) -> Result<Vec<Grant>> {
        self.expire(Some(account_id), now).await
    }

    async fn expire(&self, account: Option<&str>, now: i64) -> Result<Vec<Grant>> {
        match self {
            Self::File(file) => file.expire_loans(account, now),
            Self::Postgres(db) => bounded_db(db.expire_loans(account, now)).await,
            Self::Dual { file, postgres, .. } => {
                let expired = bounded_db(postgres.expire_loans(account, now)).await?;
                for grant in &expired {
                    if let Err(error) = file.mirror_loan(grant, &AuditEvent::ended(grant)) {
                        mirror_failure(self, "expire_loans", &error);
                    }
                }
                Ok(expired)
            }
        }
    }

    /// Insert a grant with its audit event. Returns false when the account
    /// already has an active grant.
    pub async fn create_loan(&self, grant: &Grant) -> Result<bool> {
        match self {
            Self::File(file) => file.create_loan(grant),
            Self::Postgres(db) => bounded_db(db.create_loan(grant)).await,
            Self::Dual { file, postgres, .. } => {
                let created = bounded_db(postgres.create_loan(grant)).await?;
                if created && let Err(error) = file.mirror_loan(grant, &AuditEvent::granted(grant))
                {
                    mirror_failure(self, "create_loan", &error);
                }
                Ok(created)
            }
        }
    }

    /// End an active grant with its audit event. Returns the ended grant only
    /// when this call ended it.
    pub async fn end_loan(
        &self,
        id: &str,
        at: i64,
        by: &str,
        reason: EndReason,
    ) -> Result<Option<Grant>> {
        match self {
            Self::File(file) => file.end_loan(id, at, by, reason),
            Self::Postgres(db) => bounded_db(db.end_loan(id, at, by, reason)).await,
            Self::Dual { file, postgres, .. } => {
                let ended = bounded_db(postgres.end_loan(id, at, by, reason)).await?;
                // A retry after a lost mirror copies the committed end.
                let mirror = match ended.clone() {
                    Some(grant) => Some(grant),
                    None => bounded_db(postgres.load_loan(id))
                        .await?
                        .filter(|grant| grant.ended_at.is_some()),
                };
                if let Some(grant) = mirror.as_ref()
                    && let Err(error) = file.mirror_loan(grant, &AuditEvent::ended(grant))
                {
                    mirror_failure(self, "end_loan", &error);
                }
                Ok(ended)
            }
        }
    }

    /// A grant that retention has not retired.
    pub async fn load_loan(&self, id: &str) -> Result<Option<Grant>> {
        match self {
            Self::File(file) => Ok(file
                .read_state()?
                .loans
                .get(id)
                .filter(|grant| grant.deleted_at.is_none())
                .cloned()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.load_loan(id)).await
            }
        }
    }

    /// Unretired grants where the user is the lender or the borrower.
    pub async fn loans_for_user(&self, user: &str) -> Result<Vec<Grant>> {
        match self {
            Self::File(file) => Ok(file
                .read_state()?
                .loans
                .into_values()
                .filter(|grant| grant.involves(user) && grant.deleted_at.is_none())
                .take(READ_LIMIT)
                .collect()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.loans_for_user(user)).await
            }
        }
    }

    /// Every unended grant of a borrower, for authorization. An account has
    /// at most one, so the number of accounts bounds this read.
    pub async fn active_loans_for_borrower(&self, borrower: &str) -> Result<Vec<Grant>> {
        match self {
            Self::File(file) => Ok(file
                .read_state()?
                .loans
                .into_values()
                .filter(|grant| {
                    grant.borrower == borrower
                        && grant.ended_at.is_none()
                        && grant.deleted_at.is_none()
                })
                .collect()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.active_loans_for_borrower(borrower)).await
            }
        }
    }

    /// Append an audit event. An event whose coalescing key exists is skipped,
    /// and a token issue event is written only while its grant is active.
    pub async fn append_loan_audit(&self, event: &AuditEvent) -> Result<()> {
        match self {
            Self::File(file) => file.append_loan_audit(event),
            Self::Postgres(db) => bounded_db(db.append_loan_audit(event)).await,
            Self::Dual { file, postgres, .. } => {
                bounded_db(postgres.append_loan_audit(event)).await?;
                if let Err(error) = file.append_loan_audit(event) {
                    mirror_failure(self, "append_loan_audit", &error);
                }
                Ok(())
            }
        }
    }

    /// Unretired audit events of the user's unretired grants (or of one of
    /// them), newest first, recorded before `before` (when set), at most
    /// `limit` of them. The user filter is applied in the query itself.
    pub async fn loan_audit(
        &self,
        user: &str,
        grant_id: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>> {
        match self {
            Self::File(file) => {
                let state = file.read_state()?;
                let visible: std::collections::BTreeSet<_> = state
                    .loans
                    .values()
                    .filter(|grant| grant.involves(user) && grant.deleted_at.is_none())
                    .filter(|grant| grant_id.is_none_or(|id| id == grant.id))
                    .map(|grant| grant.id.clone())
                    .collect();
                let mut events: Vec<_> = state
                    .loan_audit
                    .into_iter()
                    .filter(|event| event.deleted_at.is_none())
                    .filter(|event| visible.contains(&event.grant_id))
                    .filter(|event| before.is_none_or(|before| event.at < before))
                    .collect();
                // Newest first; within one timestamp, the latest append first,
                // as PostgreSQL orders by `at DESC, id DESC`.
                events.reverse();
                events.sort_by_key(|event| std::cmp::Reverse(event.at));
                events.truncate(limit);
                Ok(events)
            }
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.loan_audit(user, grant_id, before, limit)).await
            }
        }
    }

    /// Retire grants that ended, and audit events recorded, before the
    /// retention window. Rows stay stored with `deleted_at` set.
    pub async fn retire_loans(&self, now: i64) -> Result<()> {
        let cutoff = now.saturating_sub(RETENTION_SECONDS);
        match self {
            Self::File(file) => file.retire_loans(cutoff, now),
            Self::Postgres(db) => bounded_db(db.retire_loans(cutoff, now)).await,
            Self::Dual { file, postgres, .. } => {
                bounded_db(postgres.retire_loans(cutoff, now)).await?;
                if let Err(error) = file.retire_loans(cutoff, now) {
                    mirror_failure(self, "retire_loans", &error);
                }
                Ok(())
            }
        }
    }
}

impl FileStore {
    fn expire_loans(&self, account: Option<&str>, now: i64) -> Result<Vec<Grant>> {
        let due = |grant: &Grant| {
            grant.ended_at.is_none()
                && grant.deleted_at.is_none()
                && grant.ends_at <= now
                && account.is_none_or(|account| grant.account_id == account)
        };
        // Token requests call this; rewrite the encrypted state only when needed.
        if !self.read_state()?.loans.values().any(due) {
            return Ok(Vec::new());
        }
        self.with_lock(|state| {
            let mut expired = Vec::new();
            for grant in state.loans.values_mut() {
                if due(grant) {
                    grant.end(grant.ends_at, None, EndReason::Expired);
                    expired.push(grant.clone());
                }
            }
            state
                .loan_audit
                .extend(expired.iter().map(AuditEvent::ended));
            Ok(expired)
        })
    }

    fn create_loan(&self, grant: &Grant) -> Result<bool> {
        self.with_lock(|state| {
            if state.loans.contains_key(&grant.id)
                || state
                    .loans
                    .values()
                    .any(|other| other.account_id == grant.account_id && other.ended_at.is_none())
            {
                return Ok(false);
            }
            state.loans.insert(grant.id.clone(), grant.clone());
            state.loan_audit.push(AuditEvent::granted(grant));
            Ok(true)
        })
    }

    /// Copy an authoritative grant and its grant or end event into the file
    /// mirror. An ended copy is terminal: a delayed active copy never replaces
    /// it. Each grant has one event of each kind, added once in any order.
    fn mirror_loan(&self, grant: &Grant, event: &AuditEvent) -> Result<()> {
        let replaces = |state: &super::FileState| {
            state
                .loans
                .get(&grant.id)
                .is_none_or(|existing| existing.ended_at.is_none() && existing != grant)
        };
        let missing = |state: &super::FileState| {
            !state
                .loan_audit
                .iter()
                .any(|existing| existing.grant_id == event.grant_id && existing.kind == event.kind)
        };
        let current = self.read_state()?;
        if !replaces(&current) && !missing(&current) {
            return Ok(());
        }
        self.with_lock(|state| {
            if replaces(state) {
                state.loans.insert(grant.id.clone(), grant.clone());
            }
            if missing(state) {
                state.loan_audit.push(event.clone());
            }
            Ok(())
        })
    }

    fn end_loan(&self, id: &str, at: i64, by: &str, reason: EndReason) -> Result<Option<Grant>> {
        self.with_lock(|state| {
            let ended = state
                .loans
                .get_mut(id)
                .filter(|grant| grant.ended_at.is_none() && grant.deleted_at.is_none())
                .map(|grant| {
                    grant.end(at, Some(by), reason);
                    grant.clone()
                });
            state.loan_audit.extend(ended.iter().map(AuditEvent::ended));
            Ok(ended)
        })
    }

    fn append_loan_audit(&self, event: &AuditEvent) -> Result<()> {
        // Most token issue events are duplicates; skip the encrypted rewrite.
        let duplicate = |state: &super::FileState| {
            event.coalesce_key.is_some()
                && state
                    .loan_audit
                    .iter()
                    .any(|existing| existing.coalesce_key == event.coalesce_key)
        };
        if duplicate(&self.read_state()?) {
            return Ok(());
        }
        self.with_lock(|state| {
            let ended = event.kind == AuditKind::TokenIssued
                && !state
                    .loans
                    .get(&event.grant_id)
                    .is_some_and(|grant| grant.active(event.at) && grant.deleted_at.is_none());
            if !duplicate(state) && !ended {
                state.loan_audit.push(event.clone());
            }
            Ok(())
        })
    }

    fn issue_token(&self, grant_id: &str, machine: &str) -> Result<bool> {
        self.with_lock(|state| {
            // The time is read under the lock, after any wait for it.
            let event = AuditEvent::token_issued(chrono::Utc::now().timestamp(), grant_id, machine);
            let active = state
                .loans
                .get(grant_id)
                .is_some_and(|grant| grant.active(event.at) && grant.deleted_at.is_none());
            let duplicate = state
                .loan_audit
                .iter()
                .any(|existing| existing.coalesce_key == event.coalesce_key);
            if active && !duplicate {
                state.loan_audit.push(event);
            }
            Ok(active)
        })
    }

    /// Copy an event another store already committed, coalesced, with no
    /// grant-state check.
    fn mirror_audit(&self, event: &AuditEvent) -> Result<()> {
        self.with_lock(|state| {
            if event.coalesce_key.is_none()
                || !state
                    .loan_audit
                    .iter()
                    .any(|existing| existing.coalesce_key == event.coalesce_key)
            {
                state.loan_audit.push(event.clone());
            }
            Ok(())
        })
    }

    fn retire_loans(&self, cutoff: i64, now: i64) -> Result<()> {
        let due = |state: &super::FileState| {
            state.loans.values().any(|grant| {
                grant.deleted_at.is_none() && grant.ended_at.is_some_and(|ended| ended < cutoff)
            }) || state
                .loan_audit
                .iter()
                .any(|event| event.deleted_at.is_none() && event.at < cutoff)
        };
        if !due(&self.read_state()?) {
            return Ok(());
        }
        self.with_lock(|state| {
            for grant in state.loans.values_mut() {
                if grant.deleted_at.is_none() && grant.ended_at.is_some_and(|ended| ended < cutoff)
                {
                    grant.deleted_at = Some(now);
                }
            }
            for event in &mut state.loan_audit {
                if event.deleted_at.is_none() && event.at < cutoff {
                    event.deleted_at = Some(now);
                }
            }
            Ok(())
        })
    }
}

impl PostgresStore {
    /// Expire up to 1,000 due grants and audit each end, in one statement.
    async fn expire_loans(&self, account: Option<&str>, now: i64) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        client
            .query(
                concat!(
                    "WITH due AS (SELECT id AS due_id FROM ",
                    loans_table!(),
                    " WHERE ended_at IS NULL AND deleted_at IS NULL AND ends_at <= $1 AND ($2::text IS NULL OR account_id = $2) LIMIT 1000 FOR UPDATE SKIP LOCKED), ended AS (UPDATE ",
                    loans_table!(),
                    " SET ended_at=ends_at, ended_by=NULL, end_reason='expired' FROM due WHERE id = due.due_id AND ended_at IS NULL RETURNING ",
                    grant_columns!(),
                    "), audit AS (INSERT INTO ",
                    audit_table!(),
                    "(",
                    audit_columns!(),
                    ") SELECT id, ended_at, 'ended', NULL, NULL, 'expired', NULL, NULL FROM ended) SELECT ",
                    grant_columns!(),
                    " FROM ended"
                ),
                &[&now, &account],
            )
            .await?
            .iter()
            .map(grant_from_row)
            .collect()
    }

    /// Returns the issue time when the grant was active. The time comes
    /// from `clock_timestamp()`, which PostgreSQL re-evaluates after any wait
    /// for the grant row lock.
    async fn issue_token(&self, grant_id: &str, machine: &str) -> Result<Option<i64>> {
        let client = self.client().await?;
        Ok(client
            .query_one(
                concat!(
                    "WITH grant_row AS (SELECT id, extract(epoch FROM clock_timestamp())::bigint AS at FROM ",
                    loans_table!(),
                    " WHERE id=$1 AND ended_at IS NULL AND deleted_at IS NULL AND ends_at > extract(epoch FROM clock_timestamp())::bigint FOR SHARE), issued AS (INSERT INTO ",
                    audit_table!(),
                    "(",
                    audit_columns!(),
                    ") SELECT id, at, 'token_issued', NULL, $2, NULL, 'token:' || id || ':' || $2 || ':' || (at / 3600), NULL FROM grant_row ON CONFLICT DO NOTHING) SELECT (SELECT at FROM grant_row)"
                ),
                &[&grant_id, &machine],
            )
            .await?
            .get(0))
    }

    async fn has_active_loan(&self, account_id: &str, now: i64) -> Result<bool> {
        let client = self.client().await?;
        Ok(client
            .query_one(
                concat!(
                    "SELECT EXISTS(SELECT 1 FROM ",
                    loans_table!(),
                    " WHERE account_id=$1 AND ended_at IS NULL AND deleted_at IS NULL AND ends_at > $2)"
                ),
                &[&account_id, &now],
            )
            .await?
            .get(0))
    }

    async fn has_due_loans(&self, now: i64) -> Result<bool> {
        let client = self.client().await?;
        Ok(client
            .query_one(
                concat!(
                    "SELECT EXISTS(SELECT 1 FROM ",
                    loans_table!(),
                    " WHERE ended_at IS NULL AND deleted_at IS NULL AND ends_at <= $1)"
                ),
                &[&now],
            )
            .await?
            .get(0))
    }

    /// Persist an ended grant and its event in one statement. A concurrent
    /// end wins and this returns false.
    async fn store_end(&self, grant: &Grant) -> Result<bool> {
        let event = AuditEvent::ended(grant);
        let client = self.client().await?;
        let changed = client
            .execute(
                concat!(
                    "WITH ended AS (UPDATE ",
                    loans_table!(),
                    " SET ended_at=$2, ended_by=$3, end_reason=$4 WHERE id=$1 AND ended_at IS NULL AND deleted_at IS NULL RETURNING id) INSERT INTO ",
                    audit_table!(),
                    "(",
                    audit_columns!(),
                    ") SELECT id,$5,$6,$7,NULL,$8,NULL,NULL FROM ended"
                ),
                &[
                    &grant.id,
                    &grant.ended_at,
                    &grant.ended_by,
                    &grant.end_reason.map(EndReason::as_str),
                    &event.at,
                    &event.kind.as_str(),
                    &event.actor,
                    &event.reason,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    /// Insert a grant and its event in one statement.
    async fn create_loan(&self, grant: &Grant) -> Result<bool> {
        let event = AuditEvent::granted(grant);
        let client = self.client().await?;
        let changed = client
            .execute(
                concat!(
                    "WITH created AS (INSERT INTO ",
                    loans_table!(),
                    "(",
                    grant_columns!(),
                    ") VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,NULL,NULL,NULL,NULL) ON CONFLICT DO NOTHING RETURNING id) INSERT INTO ",
                    audit_table!(),
                    "(",
                    audit_columns!(),
                    ") SELECT id,$14,$15,$16,NULL,NULL,NULL,NULL FROM created"
                ),
                &[
                    &grant.id,
                    &grant.account_id,
                    &grant.lender,
                    &grant.borrower,
                    &grant.lender_email,
                    &grant.borrower_email,
                    &grant.alias,
                    &grant.reference,
                    &grant.subject.workspace,
                    &grant.subject.uid,
                    &grant.subject.sub,
                    &grant.created_at,
                    &grant.ends_at,
                    &event.at,
                    &event.kind.as_str(),
                    &event.actor,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn end_loan(
        &self,
        id: &str,
        at: i64,
        by: &str,
        reason: EndReason,
    ) -> Result<Option<Grant>> {
        let Some(mut grant) = self.load_loan(id).await? else {
            return Ok(None);
        };
        if grant.ended_at.is_some() {
            return Ok(None);
        }
        grant.end(at, Some(by), reason);
        Ok(self.store_end(&grant).await?.then_some(grant))
    }

    async fn load_loan(&self, id: &str) -> Result<Option<Grant>> {
        let client = self.client().await?;
        client
            .query_opt(
                concat!(
                    "SELECT ",
                    grant_columns!(),
                    " FROM ",
                    loans_table!(),
                    " WHERE id=$1 AND deleted_at IS NULL"
                ),
                &[&id],
            )
            .await?
            .map(|row| grant_from_row(&row))
            .transpose()
    }

    async fn loans_for_user(&self, user: &str) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        client
            .query(
                concat!(
                    "SELECT ",
                    grant_columns!(),
                    " FROM ",
                    loans_table!(),
                    " WHERE (lender_id=$1 OR borrower_id=$1) AND deleted_at IS NULL ORDER BY id LIMIT 10000"
                ),
                &[&user],
            )
            .await?
            .iter()
            .map(grant_from_row)
            .collect()
    }

    async fn active_loans_for_borrower(&self, borrower: &str) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        client
            .query(
                concat!(
                    "SELECT ",
                    grant_columns!(),
                    " FROM ",
                    loans_table!(),
                    " WHERE borrower_id=$1 AND ended_at IS NULL AND deleted_at IS NULL"
                ),
                &[&borrower],
            )
            .await?
            .iter()
            .map(grant_from_row)
            .collect()
    }

    async fn append_loan_audit(&self, event: &AuditEvent) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                concat!(
                    "INSERT INTO ",
                    audit_table!(),
                    "(",
                    audit_columns!(),
                    ") SELECT $1,$2,$3,$4,$5,$6,$7,NULL WHERE $3 <> 'token_issued' OR EXISTS (SELECT 1 FROM ",
                    loans_table!(),
                    " WHERE id=$1 AND ended_at IS NULL AND deleted_at IS NULL AND ends_at > $2) ON CONFLICT DO NOTHING"
                ),
                &[
                    &event.grant_id,
                    &event.at,
                    &event.kind.as_str(),
                    &event.actor,
                    &event.machine,
                    &event.reason,
                    &event.coalesce_key,
                ],
            )
            .await?;
        Ok(())
    }

    async fn loan_audit(
        &self,
        user: &str,
        grant_id: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>> {
        let client = self.client().await?;
        client
            .query(
                concat!(
                    "SELECT ",
                    audit_columns_as_a!(),
                    " FROM ",
                    audit_table!(),
                    " a JOIN ",
                    loans_table!(),
                    " l ON l.id = a.grant_id WHERE (l.lender_id=$1 OR l.borrower_id=$1) AND l.deleted_at IS NULL AND a.deleted_at IS NULL AND ($2::text IS NULL OR a.grant_id=$2) AND ($3::bigint IS NULL OR a.at < $3) ORDER BY a.at DESC, a.id DESC LIMIT $4"
                ),
                &[&user, &grant_id, &before, &i64::try_from(limit)?],
            )
            .await?
            .iter()
            .map(audit_from_row)
            .collect()
    }

    /// Copy file-mode grants with their audit events. A grant already in the
    /// table is skipped with its events, so a repeated backfill adds nothing.
    /// Returns the number of grants copied.
    pub(super) async fn import_loans(
        &self,
        grants: &std::collections::BTreeMap<String, Grant>,
        audit: &[AuditEvent],
    ) -> Result<usize> {
        let client = self.client().await?;
        let mut copied = 0;
        for grant in grants.values() {
            let events: Vec<_> = audit.iter().filter(|e| e.grant_id == grant.id).collect();
            let at: Vec<i64> = events.iter().map(|e| e.at).collect();
            let kind: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
            let actor: Vec<Option<String>> = events.iter().map(|e| e.actor.clone()).collect();
            let machine: Vec<Option<String>> = events.iter().map(|e| e.machine.clone()).collect();
            let reason: Vec<Option<String>> = events.iter().map(|e| e.reason.clone()).collect();
            let key: Vec<Option<String>> = events.iter().map(|e| e.coalesce_key.clone()).collect();
            let deleted: Vec<Option<i64>> = events.iter().map(|e| e.deleted_at).collect();
            let row = client
                .query_one(
                    concat!(
                        "WITH created AS (INSERT INTO ",
                        loans_table!(),
                        "(",
                        grant_columns!(),
                        ") VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17) ON CONFLICT DO NOTHING RETURNING id), copied AS (INSERT INTO ",
                        audit_table!(),
                        "(",
                        audit_columns!(),
                        ") SELECT created.id, e.at, e.kind, e.actor, e.machine, e.reason, e.key, e.deleted FROM created, unnest($18::bigint[], $19::text[], $20::text[], $21::text[], $22::text[], $23::text[], $24::bigint[]) AS e(at, kind, actor, machine, reason, key, deleted) ON CONFLICT DO NOTHING) SELECT count(*)::BIGINT FROM created"
                    ),
                    &[
                        &grant.id,
                        &grant.account_id,
                        &grant.lender,
                        &grant.borrower,
                        &grant.lender_email,
                        &grant.borrower_email,
                        &grant.alias,
                        &grant.reference,
                        &grant.subject.workspace,
                        &grant.subject.uid,
                        &grant.subject.sub,
                        &grant.created_at,
                        &grant.ends_at,
                        &grant.ended_at,
                        &grant.ended_by,
                        &grant.end_reason.map(EndReason::as_str),
                        &grant.deleted_at,
                        &at,
                        &kind,
                        &actor,
                        &machine,
                        &reason,
                        &key,
                        &deleted,
                    ],
                )
                .await?;
            let created: i64 = row.get(0);
            copied += usize::try_from(created)?;
        }
        Ok(copied)
    }

    async fn retire_loans(&self, cutoff: i64, now: i64) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                concat!(
                    "UPDATE ",
                    loans_table!(),
                    " SET deleted_at=$2 WHERE ended_at IS NOT NULL AND ended_at < $1 AND deleted_at IS NULL"
                ),
                &[&cutoff, &now],
            )
            .await?;
        client
            .execute(
                concat!(
                    "UPDATE ",
                    audit_table!(),
                    " SET deleted_at=$2 WHERE at < $1 AND deleted_at IS NULL"
                ),
                &[&cutoff, &now],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
