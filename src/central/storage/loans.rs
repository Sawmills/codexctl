//! Loan grants and their audit log (ADR 0004).
//!
//! File mode keeps both in the encrypted `FileState`. PostgreSQL keeps them in
//! `account_loans` and `account_loan_audit`. Dual mode writes PostgreSQL first
//! and mirrors to the file.
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
    /// End every active grant whose end time has passed. Returns the grants
    /// this call ended, so the caller can audit each one once.
    pub async fn expire_loans(&self, now: i64) -> Result<Vec<Grant>> {
        match self {
            Self::File(file) => file.expire_loans(now),
            Self::Postgres(db) => bounded_db(db.expire_loans(now)).await,
            Self::Dual { file, postgres, .. } => {
                let expired = bounded_db(postgres.expire_loans(now)).await?;
                if let Err(error) = file.expire_loans(now) {
                    mirror_failure(self, "expire_loans", &error);
                }
                Ok(expired)
            }
        }
    }

    /// Insert a grant. Returns false when the account already has an active one.
    pub async fn create_loan(&self, grant: &Grant) -> Result<bool> {
        match self {
            Self::File(file) => file.create_loan(grant),
            Self::Postgres(db) => bounded_db(db.create_loan(grant)).await,
            Self::Dual { file, postgres, .. } => {
                let created = bounded_db(postgres.create_loan(grant)).await?;
                if created && let Err(error) = file.create_loan(grant) {
                    mirror_failure(self, "create_loan", &error);
                }
                Ok(created)
            }
        }
    }

    /// End an active grant. Returns the ended grant only when this call ended it.
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
                if ended.is_some()
                    && let Err(error) = file.end_loan(id, at, by, reason)
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

    pub async fn loan_audit(&self, grant_ids: &[String]) -> Result<Vec<AuditEvent>> {
        match self {
            Self::File(file) => Ok(file
                .read_state()?
                .loan_audit
                .into_iter()
                .filter(|event| grant_ids.contains(&event.grant_id))
                .take(READ_LIMIT)
                .collect()),
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => {
                bounded_db(db.loan_audit(grant_ids)).await
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
            Ok(true)
        })
    }

    fn end_loan(&self, id: &str, at: i64, by: &str, reason: EndReason) -> Result<Option<Grant>> {
        self.with_lock(|state| {
            Ok(state
                .loans
                .get_mut(id)
                .filter(|grant| grant.ended_at.is_none())
                .map(|grant| {
                    grant.end(at, by, reason);
                    grant.clone()
                }))
        })
    }

    fn append_loan_audit(&self, event: &AuditEvent) -> Result<()> {
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

    /// Persist an ended grant once; a concurrent end wins and returns false.
    async fn store_end(&self, grant: &Grant) -> Result<bool> {
        let client = self.client().await?;
        let changed = client
            .execute(
                "UPDATE account_loans SET ended_at=$2, grant_json=$3 WHERE id=$1 AND ended_at IS NULL",
                &[&grant.id, &grant.ended_at, &serde_json::to_string(grant)?],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn create_loan(&self, grant: &Grant) -> Result<bool> {
        let client = self.client().await?;
        let changed = client
            .execute(
                "INSERT INTO account_loans(id,account_id,lender_id,borrower_id,ends_at,ended_at,grant_json) VALUES($1,$2,$3,$4,$5,NULL,$6) ON CONFLICT DO NOTHING",
                &[
                    &grant.id,
                    &grant.account_id,
                    &grant.lender,
                    &grant.borrower,
                    &grant.ends_at,
                    &serde_json::to_string(grant)?,
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

    async fn loan_audit(&self, grant_ids: &[String]) -> Result<Vec<AuditEvent>> {
        let client = self.client().await?;
        client
            .query(
                "SELECT event_json FROM account_loan_audit WHERE grant_id = ANY($1) ORDER BY at, id LIMIT 10000",
                &[&grant_ids],
            )
            .await?
            .into_iter()
            .map(|row| Ok(serde_json::from_str(row.get(0))?))
            .collect()
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
