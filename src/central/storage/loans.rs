//! Loan grants and their audit log (ADR 0004).
//!
//! File mode keeps both in the encrypted `FileState`. PostgreSQL keeps them in
//! `account_loans` and `account_loan_audit`. Dual mode writes PostgreSQL first
//! and mirrors the authoritative grant to the file; an ended grant always wins,
//! so a delayed mirror can never reopen a loan.
//!
//! Every grant, end, and expiry is written together with its audit event.
use super::{CentralStore, FileStore, PostgresStore, bounded_db};
use crate::central::loans::{AuditEvent, EndReason, Grant, RETENTION_SECONDS};
use anyhow::Result;

/// Bound on rows read for one caller.
const READ_LIMIT: usize = 10_000;

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
        match self {
            Self::File(file) => file.expire_loans(now),
            Self::Postgres(db) => bounded_db(db.expire_loans(now)).await,
            Self::Dual { file, postgres, .. } => {
                let expired = bounded_db(postgres.expire_loans(now)).await?;
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

    pub async fn load_loan(&self, id: &str) -> Result<Option<Grant>> {
        match self {
            Self::File(file) => Ok(file.read_state()?.loans.get(id).cloned()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.load_loan(id)).await
            }
        }
    }

    /// Grants where the user is the lender or the borrower.
    pub async fn loans_for_user(&self, user: &str) -> Result<Vec<Grant>> {
        match self {
            Self::File(file) => Ok(file
                .read_state()?
                .loans
                .into_values()
                .filter(|grant| grant.involves(user))
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
                .filter(|grant| grant.borrower == borrower && grant.ended_at.is_none())
                .collect()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.active_loans_for_borrower(borrower)).await
            }
        }
    }

    /// Append an audit event. An event whose coalescing key exists is skipped.
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

    /// Audit events of the grants, newest first, recorded before `before`
    /// (when set), at most `limit` of them.
    pub async fn loan_audit(
        &self,
        grant_ids: &[String],
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>> {
        match self {
            Self::File(file) => {
                let mut events: Vec<_> = file
                    .read_state()?
                    .loan_audit
                    .into_iter()
                    .filter(|event| grant_ids.contains(&event.grant_id))
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
                bounded_db(db.loan_audit(grant_ids, before, limit)).await
            }
        }
    }

    /// Delete grants that ended, and audit events recorded, before the
    /// retention window.
    pub async fn prune_loans(&self, now: i64) -> Result<()> {
        let cutoff = now.saturating_sub(RETENTION_SECONDS);
        match self {
            Self::File(file) => file.prune_loans(cutoff),
            Self::Postgres(db) => bounded_db(db.prune_loans(cutoff)).await,
            Self::Dual { file, postgres, .. } => {
                bounded_db(postgres.prune_loans(cutoff)).await?;
                if let Err(error) = file.prune_loans(cutoff) {
                    mirror_failure(self, "prune_loans", &error);
                }
                Ok(())
            }
        }
    }
}

impl FileStore {
    fn expire_loans(&self, now: i64) -> Result<Vec<Grant>> {
        // Token requests call this; rewrite the encrypted state only when needed.
        if !self
            .read_state()?
            .loans
            .values()
            .any(|grant| grant.ended_at.is_none() && grant.ends_at <= now)
        {
            return Ok(Vec::new());
        }
        self.with_lock(|state| {
            let mut expired = Vec::new();
            for grant in state.loans.values_mut() {
                if grant.ended_at.is_none() && grant.ends_at <= now {
                    grant.end(grant.ends_at, &grant.lender.clone(), EndReason::Expired);
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

    /// Copy an authoritative grant and its event into the file mirror. An
    /// ended copy is terminal: a delayed active copy never replaces it. The
    /// event is added only when the copy changes the mirror.
    fn mirror_loan(&self, grant: &Grant, event: &AuditEvent) -> Result<()> {
        let changes = |state: &super::FileState| match state.loans.get(&grant.id) {
            None => true,
            Some(existing) => existing.ended_at.is_none() && existing != grant,
        };
        if !changes(&self.read_state()?) {
            return Ok(());
        }
        self.with_lock(|state| {
            if changes(state) {
                state.loans.insert(grant.id.clone(), grant.clone());
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
                .filter(|grant| grant.ended_at.is_none())
                .map(|grant| {
                    grant.end(at, by, reason);
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

    fn prune_loans(&self, cutoff: i64) -> Result<()> {
        self.with_lock(|state| {
            state
                .loans
                .retain(|_, grant| grant.ended_at.is_none_or(|ended| ended >= cutoff));
            state.loan_audit.retain(|event| event.at >= cutoff);
            Ok(())
        })
    }
}

impl PostgresStore {
    async fn expire_loans(&self, now: i64) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT grant_json FROM account_loans WHERE ended_at IS NULL AND ends_at <= $1 LIMIT 1000",
                &[&now],
            )
            .await?;
        let mut expired = Vec::new();
        for row in rows {
            let mut grant: Grant = serde_json::from_str(row.get(0))?;
            grant.end(grant.ends_at, &grant.lender.clone(), EndReason::Expired);
            if self.store_end(&grant).await? {
                expired.push(grant);
            }
        }
        Ok(expired)
    }

    /// Persist an ended grant and its event in one statement. A concurrent
    /// end wins and this returns false.
    async fn store_end(&self, grant: &Grant) -> Result<bool> {
        let event = AuditEvent::ended(grant);
        let client = self.client().await?;
        let changed = client
            .execute(
                "WITH ended AS (UPDATE account_loans SET ended_at=$2, grant_json=$3 WHERE id=$1 AND ended_at IS NULL RETURNING id) INSERT INTO account_loan_audit(grant_id,at,coalesce_key,event_json) SELECT id,$4,NULL,$5 FROM ended",
                &[
                    &grant.id,
                    &grant.ended_at,
                    &serde_json::to_string(grant)?,
                    &event.at,
                    &serde_json::to_string(&event)?,
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
                "WITH created AS (INSERT INTO account_loans(id,account_id,lender_id,borrower_id,ends_at,ended_at,grant_json) VALUES($1,$2,$3,$4,$5,NULL,$6) ON CONFLICT DO NOTHING RETURNING id) INSERT INTO account_loan_audit(grant_id,at,coalesce_key,event_json) SELECT id,$7,NULL,$8 FROM created",
                &[
                    &grant.id,
                    &grant.account_id,
                    &grant.lender,
                    &grant.borrower,
                    &grant.ends_at,
                    &serde_json::to_string(grant)?,
                    &event.at,
                    &serde_json::to_string(&event)?,
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
        grant.end(at, by, reason);
        Ok(self.store_end(&grant).await?.then_some(grant))
    }

    async fn load_loan(&self, id: &str) -> Result<Option<Grant>> {
        let client = self.client().await?;
        client
            .query_opt("SELECT grant_json FROM account_loans WHERE id=$1", &[&id])
            .await?
            .map(|row| Ok(serde_json::from_str(row.get(0))?))
            .transpose()
    }

    async fn loans_for_user(&self, user: &str) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        client
            .query(
                "SELECT grant_json FROM account_loans WHERE lender_id=$1 OR borrower_id=$1 ORDER BY id LIMIT 10000",
                &[&user],
            )
            .await?
            .into_iter()
            .map(|row| Ok(serde_json::from_str(row.get(0))?))
            .collect()
    }

    async fn active_loans_for_borrower(&self, borrower: &str) -> Result<Vec<Grant>> {
        let client = self.client().await?;
        client
            .query(
                "SELECT grant_json FROM account_loans WHERE borrower_id=$1 AND ended_at IS NULL",
                &[&borrower],
            )
            .await?
            .into_iter()
            .map(|row| Ok(serde_json::from_str(row.get(0))?))
            .collect()
    }

    async fn append_loan_audit(&self, event: &AuditEvent) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "INSERT INTO account_loan_audit(grant_id,at,coalesce_key,event_json) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
                &[
                    &event.grant_id,
                    &event.at,
                    &event.coalesce_key,
                    &serde_json::to_string(event)?,
                ],
            )
            .await?;
        Ok(())
    }

    async fn loan_audit(
        &self,
        grant_ids: &[String],
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AuditEvent>> {
        let client = self.client().await?;
        client
            .query(
                "SELECT event_json FROM account_loan_audit WHERE grant_id = ANY($1) AND ($2::bigint IS NULL OR at < $2) ORDER BY at DESC, id DESC LIMIT $3",
                &[&grant_ids, &before, &i64::try_from(limit)?],
            )
            .await?
            .into_iter()
            .map(|row| Ok(serde_json::from_str(row.get(0))?))
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
            let keys: Vec<Option<String>> = events.iter().map(|e| e.coalesce_key.clone()).collect();
            let json = events
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()?;
            let row = client
                .query_one(
                    "WITH created AS (INSERT INTO account_loans(id,account_id,lender_id,borrower_id,ends_at,ended_at,grant_json) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING RETURNING id), copied AS (INSERT INTO account_loan_audit(grant_id,at,coalesce_key,event_json) SELECT created.id, e.at, e.key, e.json FROM created, unnest($8::bigint[], $9::text[], $10::text[]) AS e(at, key, json) ON CONFLICT DO NOTHING) SELECT count(*)::BIGINT FROM created",
                    &[
                        &grant.id,
                        &grant.account_id,
                        &grant.lender,
                        &grant.borrower,
                        &grant.ends_at,
                        &grant.ended_at,
                        &serde_json::to_string(grant)?,
                        &at,
                        &keys,
                        &json,
                    ],
                )
                .await?;
            let created: i64 = row.get(0);
            copied += usize::try_from(created)?;
        }
        Ok(copied)
    }

    async fn prune_loans(&self, cutoff: i64) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "DELETE FROM account_loans WHERE ended_at IS NOT NULL AND ended_at < $1",
                &[&cutoff],
            )
            .await?;
        client
            .execute("DELETE FROM account_loan_audit WHERE at < $1", &[&cutoff])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
