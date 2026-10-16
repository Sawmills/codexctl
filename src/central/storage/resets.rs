//! Short account-fenced reset journal transactions. Provider I/O stays outside these locks.
use super::*;
use crate::central::{relogin::shared::Caller, resets::Refusal};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS account_reset_journals (
    account_id TEXT PRIMARY KEY REFERENCES central_accounts(account_id),
    revision BIGINT NOT NULL,
    journal JSONB NOT NULL
);
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid='dashboard_audit'::regclass AND conname='dashboard_audit_operation_check' AND pg_get_constraintdef(oid) NOT LIKE '%account_reset%') THEN
        ALTER TABLE dashboard_audit DROP CONSTRAINT dashboard_audit_operation_check;
        ALTER TABLE dashboard_audit ADD CONSTRAINT dashboard_audit_operation_check CHECK (operation IN ('account_add','account_renew','account_remove','machine_revoke','account_reset'));
    END IF;
END $$;";

#[derive(Clone, Copy)]
pub(in crate::central) enum ResetWrite<'a> {
    Reserve {
        caller: &'a Caller,
        account_revision: i64,
        holder: &'a str,
    },
    Settle {
        audit: Option<DashboardAudit<'a>>,
        result: &'a str,
    },
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
    pub(in crate::central) async fn authorize_reset(&self, caller: &Caller) -> Result<()> {
        let db = self.browser_store()?;
        bounded_db(async { authorize(&*db.client().await?, caller).await }).await
    }
    pub(in crate::central) async fn reset_journal(&self, account: &str) -> Result<(i64, Value)> {
        let db = self.browser_store()?;
        bounded_db(async {
            Ok(match db.client().await?.query_opt("SELECT revision,journal::text FROM account_reset_journals WHERE account_id=$1", &[&account]).await? {
                Some(row) => (row.get(0),serde_json::from_str(&row.get::<_,String>(1))?),
                None => (0,serde_json::json!({"pending":null,"completed":{}})),
            })
        }).await
    }

    pub(in crate::central) async fn write_reset_journal(
        &self,
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
            if let ResetWrite::Reserve{caller,account_revision,holder}=stage {
                authorize(&tx,caller).await?;
                let row=tx.query_opt("SELECT revision FROM central_accounts WHERE account_id=$1 AND user_id=$2 AND deleted_at IS NULL FOR SHARE", &[&account,&caller.user]).await?;
                let row=row.ok_or(Refusal("account_not_found"))?;
                if row.get::<_,i64>(0)!=account_revision {return Err(Refusal("account_changed").into());}
                if tx.query_one("SELECT EXISTS(SELECT 1 FROM account_refresh_leases WHERE account_id=$1 AND NOT released AND holder_id<>$2)", &[&account,&holder]).await?.get::<_,bool>(0) {
                    return Err(Refusal("refresh_in_progress").into());
                }
            }
            let changed=if expected==0 {
                tx.execute("INSERT INTO account_reset_journals(account_id,revision,journal) VALUES($1,1,$2::text::jsonb) ON CONFLICT DO NOTHING", &[&account,&payload]).await?
            } else {
                tx.execute("UPDATE account_reset_journals SET revision=revision+1,journal=$3::text::jsonb WHERE account_id=$1 AND revision=$2", &[&account,&expected,&payload]).await?
            };
            let mut audit_log=None;
            if changed==1 && let ResetWrite::Settle{audit,result}=stage {
                tx.execute("UPDATE central_token_evidence SET deleted_at=clock_timestamp() WHERE account_id=$1 AND deleted_at IS NULL", &[&account]).await?;
                if let Some(audit)=audit {
                    let at=chrono::Utc::now().timestamp();
                    audit_log=tx.query_opt("INSERT INTO dashboard_audit(at,actor,operation,target,result,coalesce_key) VALUES($1,$2,$6,$3,$4,$5) ON CONFLICT(coalesce_key) DO NOTHING RETURNING at,actor,target,result", &[&at,&audit.actor,&account,&result,&audit.attempt,&audit.operation.as_str()]).await?;
                }
            }
            if let ResetWrite::Reserve{caller,..}=stage {authorize(&tx,caller).await?;}
            tx.commit().await?;
            if let Some(row)=audit_log {super::browser::audit_log(row.get(0),row.get(1),row.get(2),row.get(3),"account_reset");}
            Ok(changed==1)
        }).await
    }
}
