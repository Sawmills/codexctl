//! Short account-fenced reset journal transactions. Provider I/O stays outside these locks.
use super::*;
use crate::api::ConsumeResetCode;
use crate::central::{relogin::shared::Caller, resets::Refusal};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS account_reset_journals (
    account_id TEXT PRIMARY KEY REFERENCES central_accounts(account_id),
    revision BIGINT NOT NULL,
    journal JSONB NOT NULL,
    deleted_at TIMESTAMPTZ
);
ALTER TABLE account_reset_journals ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
CREATE TABLE IF NOT EXISTS reset_audit (
    id BIGSERIAL PRIMARY KEY,
    at BIGINT NOT NULL,
    user_id TEXT NOT NULL REFERENCES central_users(id),
    actor TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation='account_reset'),
    target TEXT NOT NULL REFERENCES central_accounts(account_id),
    result TEXT NOT NULL,
    coalesce_key TEXT NOT NULL UNIQUE,
    deleted_at TIMESTAMPTZ
);
DO $$ BEGIN
    IF to_regclass(format('%I.%I',current_schema(),'dashboard_audit_history')) IS NULL THEN
        CREATE VIEW dashboard_audit_history AS
            SELECT at,actor,operation,target,result,coalesce_key,deleted_at FROM dashboard_audit
            UNION ALL
            SELECT at,actor,operation,target,result,coalesce_key,deleted_at FROM reset_audit;
    END IF;
END $$;";

#[derive(Clone, Copy)]
pub(in crate::central) struct ResetAudit<'a> {
    pub actor: &'a str,
    pub attempt: &'a str,
}

#[derive(Clone, Copy)]
pub(in crate::central) enum ResetWrite<'a> {
    Reserve {
        caller: &'a Caller,
        account_revision: i64,
        auth_revision: &'a str,
        holder: &'a str,
    },
    Settle {
        audit: Option<ResetAudit<'a>>,
        code: ConsumeResetCode,
    },
}

/// Dropping this dedicated client closes the session and releases execution
/// custody. The journal remains intact when a remote outcome is unknown.
pub(in crate::central) struct ResetCustody(Arc<tokio_postgres::Client>);

impl ResetCustody {
    pub(in crate::central) async fn lost(&self) {
        while !self.0.is_closed() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub(super) async fn mutation_allowed<C: tokio_postgres::GenericClient + Sync>(
    client: &C,
    account: &str,
) -> Result<bool> {
    Ok(client
        .query_one(concat!("SELECT ", reset_mutation_guard!()), &[&account])
        .await?
        .get(0))
}

async fn authorize<C: tokio_postgres::GenericClient + Sync>(
    client: &C,
    caller: &Caller,
) -> Result<()> {
    if !login::authority_live(client, &caller.user, &caller.authority).await? {
        return Err(Refusal("reset_authority_ended").into());
    }
    if let login::LoginAuthority::Browser(session) = &caller.authority {
        let fresh=client.query_opt("SELECT auth_time_verified AND signed_in_at BETWEEN clock_timestamp()-interval '630 seconds' AND clock_timestamp()+interval '30 seconds' FROM browser_sessions WHERE token_hash=$1 AND user_id=$2 AND expires_at>clock_timestamp()", &[&session,&caller.user]).await?.is_some_and(|row|row.get::<_,bool>(0));
        if !fresh {
            return Err(Refusal("fresh_sign_in_required").into());
        }
    }
    Ok(())
}

impl CentralStore {
    pub(in crate::central) async fn reset_custody(&self, account: &str) -> Result<ResetCustody> {
        let db = self.browser_store()?;
        bounded_db(async {
            // Never put a session lock on a pooled or cached client.
            let client = db.establish().await?;
            let acquired: bool = client.query_one(
                "SELECT pg_try_advisory_lock(hashtextextended(current_schema()||':'||$1,13322))",
                &[&account],
            ).await?.get(0);
            if !acquired {
                return Err(Refusal("reset_in_progress").into());
            }
            Ok(ResetCustody(client))
        })
        .await
    }

    pub(in crate::central) async fn authorize_reset(&self, caller: &Caller) -> Result<()> {
        let db = self.browser_store()?;
        bounded_db(async { authorize(&*db.client().await?, caller).await }).await
    }
    pub(in crate::central) async fn reset_journal(
        &self,
        user: &str,
        account: &str,
    ) -> Result<(i64, Value)> {
        let db = self.browser_store()?;
        bounded_db(async {
            let row=db.client().await?.query_opt("SELECT j.revision,j.journal::text FROM central_accounts a LEFT JOIN account_reset_journals j ON j.account_id=a.account_id WHERE a.account_id=$1 AND a.user_id=$2 AND a.deleted_at IS NULL AND j.deleted_at IS NULL", &[&account,&user]).await?.ok_or(Refusal("account_not_found"))?;
            Ok(match row.get::<_,Option<i64>>(0) {
                Some(revision) => (revision,serde_json::from_str(&row.get::<_,String>(1))?),
                None => (0,serde_json::json!({"pending":null,"completed":{}})),
            })
        }).await
    }

    pub(in crate::central) async fn write_reset_journal(
        &self,
        user: &str,
        account: &str,
        expected: i64,
        journal: &Value,
        stage: ResetWrite<'_>,
    ) -> Result<bool> {
        let db = self.browser_store()?;
        let payload = serde_json::to_string(journal)?;
        bounded_db(async {
            let mut client=Arc::try_unwrap(db.establish().await?).map_err(|_| anyhow::anyhow!("reset connection is shared"))?;
            let tx=client.transaction().await?;
            if let ResetWrite::Reserve{caller,account_revision,auth_revision,holder}=stage {
                let _admission=identity::lock_admission(&tx).await?;
                if caller.user != user { return Err(Refusal("account_not_found").into()); }
                authorize(&tx,caller).await?;
                let row=tx.query_opt("SELECT revision FROM central_accounts WHERE account_id=$1 AND user_id=$2 AND deleted_at IS NULL FOR UPDATE", &[&account,&user]).await?;
                let row=row.ok_or(Refusal("account_not_found"))?;
                if row.get::<_,i64>(0)!=account_revision {return Err(Refusal("account_changed").into());}
                let login_query=concat!("SELECT EXISTS(SELECT 1 FROM central_login_operations WHERE ",login_fence!()," OR (account_id=$1 AND phase IN ('starting','pending')) OR (user_id=$2 AND phase IN ('starting','pending') AND kind='add'))");
                if tx.query_one(login_query, &[&account,&caller.user]).await?.get::<_,bool>(0) {
                    return Err(Refusal("login_pending").into());
                }
                if tx.query_one("SELECT EXISTS(SELECT 1 FROM account_refresh_leases WHERE account_id=$1 AND NOT released AND holder_id<>$2)", &[&account,&holder]).await?.get::<_,bool>(0) {
                    return Err(Refusal("refresh_in_progress").into());
                }
                let routing_seconds=crate::central::fast_path::EVIDENCE_MAX_AGE.as_secs() as i64;
                let routed:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_token_evidence WHERE account_id=$1 AND account_revision=$2 AND auth_revision=$3 AND routing_supported AND deleted_at IS NULL AND routing_observed_at>clock_timestamp()-($4::bigint*interval '1 second'))",&[&account,&account_revision,&auth_revision,&routing_seconds]).await?.get(0);
                if !routed { return Err(Refusal("reset_routing_unverified").into()); }
            }
            if let ResetWrite::Settle { .. } = stage {
                // Removal takes the same account-row fence before tombstoning.
                tx.query_opt("SELECT account_id FROM central_accounts WHERE account_id=$1 AND user_id=$2 AND deleted_at IS NULL FOR UPDATE", &[&account,&user]).await?.ok_or(Refusal("account_not_found"))?;
            }
            let changed=if expected==0 {
                tx.execute("INSERT INTO account_reset_journals(account_id,revision,journal) VALUES($1,1,$2::text::jsonb) ON CONFLICT DO NOTHING", &[&account,&payload]).await?
            } else {
                tx.execute("UPDATE account_reset_journals SET revision=revision+1,journal=$3::text::jsonb WHERE account_id=$1 AND revision=$2 AND deleted_at IS NULL", &[&account,&expected,&payload]).await?
            };
            let mut audit_log=None;
            if changed==1 && let ResetWrite::Settle{audit,code}=stage {
                let result=match code {
                    ConsumeResetCode::Reset => "ok:reset",
                    ConsumeResetCode::AlreadyRedeemed => "ok:already_redeemed",
                    ConsumeResetCode::NothingToReset => "refused:nothing_to_reset",
                    ConsumeResetCode::NoCredit => "refused:no_credit",
                    ConsumeResetCode::Unknown => "failed:reset_rejected",
                };
                if matches!(code,ConsumeResetCode::Reset|ConsumeResetCode::AlreadyRedeemed|ConsumeResetCode::Unknown) {
                    tx.execute("UPDATE central_token_evidence SET deleted_at=clock_timestamp() WHERE account_id=$1 AND deleted_at IS NULL", &[&account]).await?;
                }
                if let Some(audit)=audit {
                    let at=chrono::Utc::now().timestamp();
                    audit_log=tx.query_opt("INSERT INTO reset_audit(at,user_id,actor,operation,target,result,coalesce_key) VALUES($1,$6,$2,'account_reset',$3,$4,$5) ON CONFLICT(coalesce_key) DO NOTHING RETURNING at,actor,target,result", &[&at,&audit.actor,&account,&result,&audit.attempt,&user]).await?;
                }
            }
            if let ResetWrite::Reserve{caller,..}=stage {authorize(&tx,caller).await?;}
            tx.commit().await?;
            if let Some(row)=audit_log {super::browser::audit_log(row.get(0),row.get(1),row.get(2),row.get(3),"account_reset");}
            Ok(changed==1)
        }).await
    }
}
