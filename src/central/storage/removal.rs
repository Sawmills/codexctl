//! Scoped removal metadata and the account-row tombstone transaction.
use super::*;
use crate::central::{relogin::shared::Caller, removal::Request};

#[derive(Clone, Copy, PartialEq)]
pub(in crate::central) enum RemovalAction {
    Inspect,
    Commit,
}
pub(in crate::central) enum RemovalResult {
    Ready,
    Removed,
    AlreadyRemoved,
}

pub(in crate::central) struct RemovalTarget {
    pub account_id: String,
    pub alias: String,
    pub revision: i64,
    pub recent_machines: i64,
}

async fn authorize(
    tx: &tokio_postgres::Transaction<'_>,
    caller: &Caller,
) -> Result<std::result::Result<(), &'static str>> {
    let login::LoginAuthority::Browser(session) = &caller.authority else {
        return Ok(Err("browser_sign_in_required"));
    };
    if !login::authority_live(tx, &caller.user, &caller.authority).await? {
        return Ok(Err("browser_sign_in_required"));
    }
    let fresh = tx.query_one(
        "SELECT auth_time_verified AND signed_in_at BETWEEN clock_timestamp()-interval '630 seconds' AND clock_timestamp()+interval '30 seconds' FROM browser_sessions WHERE token_hash=$1",
        &[session],
    ).await?.get::<_, bool>(0);
    Ok(if fresh {
        Ok(())
    } else {
        Err("fresh_sign_in_required")
    })
}

impl CentralStore {
    /// No process wait occurs here. Match ordinary claims' account-before-lease
    /// order; identity admission also serializes login and alias publication.
    pub(in crate::central) async fn remove_account(
        &self,
        caller: &Caller,
        request: &Request,
        holder: &str,
        action: RemovalAction,
    ) -> Result<std::result::Result<RemovalResult, &'static str>> {
        let db = self.browser_store()?;
        bounded_db(async {
            let mut client=db.admission_client().await?;
            let tx=client.transaction().await?;
            let _admission=identity::lock_admission(&tx).await?;
            if let Err(reason)=authorize(&tx,caller).await? {return Ok(Err(reason));}
            if !resets::mutation_allowed(&tx,&request.account_id).await? {return Ok(Err("reset_in_progress"));}
            let row=tx.query_opt("SELECT alias,revision,deleted_at IS NOT NULL FROM central_accounts WHERE account_id=$1 AND user_id=$2 FOR UPDATE", &[&request.account_id,&caller.user]).await?;
            let Some(row)=row else { return Ok(Err("account_not_found")); };
            let alias:String=row.get(0); let revision:i64=row.get(1); let removed:bool=row.get(2);
            if request.alias!=alias || request.confirmation!=alias { return Ok(Err("removal_confirmation_changed")); }
            let actor=caller.dashboard_actor.as_deref().context("browser removal actor missing")?;
            if let Some(receipt)=tx.query_opt("SELECT actor,operation,target,result FROM dashboard_audit WHERE coalesce_key=$1 AND deleted_at IS NULL", &[&request.attempt]).await? {
                if receipt.get::<_,String>(0)==actor && receipt.get::<_,String>(1)=="account_remove" && receipt.get::<_,String>(2)==request.account_id && receipt.get::<_,String>(3)=="ok" && removed && request.revision.checked_add(1)==Some(revision) {
                    return Ok(Ok(RemovalResult::AlreadyRemoved));
                }
                return Ok(Err("removal_attempt_conflict"));
            }
            if removed { return Ok(Err("account_removed")); }
            if revision!=request.revision { return Ok(Err("removal_confirmation_changed")); }
            if tx.query_one("SELECT EXISTS(SELECT 1 FROM account_loans WHERE account_id=$1 AND ended_at IS NULL AND deleted_at IS NULL)", &[&request.account_id]).await?.get::<_,bool>(0) { return Ok(Err("account_lent")); }
            let login_query=concat!("SELECT EXISTS(SELECT 1 FROM central_login_operations WHERE ",login_fence!()," OR (account_id=$1 AND phase IN ('starting','pending')) OR (user_id=$2 AND phase IN ('starting','pending') AND kind='add'))");
            if tx.query_one(login_query, &[&request.account_id,&caller.user]).await?.get::<_,bool>(0) { return Ok(Err("login_pending")); }
            if let Some(lease)=tx.query_opt("SELECT holder_id,released FROM account_refresh_leases WHERE account_id=$1 FOR UPDATE", &[&request.account_id]).await?
                && !lease.get::<_,bool>(1) && lease.get::<_,String>(0)!=holder { return Ok(Err("refresh_in_progress")); }
            if action==RemovalAction::Inspect { return Ok(Ok(RemovalResult::Ready)); }
            // Caller retains the local owner lock and has positively stopped its
            // runtime. No foreign or unproven expired lease can be stolen.
            tx.execute("UPDATE central_accounts SET deleted_at=clock_timestamp(),revision=revision+1,updated_at=clock_timestamp() WHERE account_id=$1", &[&request.account_id]).await?;
            tx.execute("UPDATE account_refresh_leases SET released=true,expires_at=clock_timestamp() WHERE account_id=$1 AND holder_id=$2", &[&request.account_id,&holder]).await?;
            tx.execute("UPDATE account_live_sessions SET deleted_at=clock_timestamp() WHERE account_id=$1 AND deleted_at IS NULL", &[&request.account_id]).await?;
            tx.execute("UPDATE account_reset_journals SET deleted_at=clock_timestamp() WHERE account_id=$1 AND deleted_at IS NULL", &[&request.account_id]).await?;
            let at=chrono::Utc::now().timestamp();
            tx.execute("INSERT INTO dashboard_audit(at,actor,operation,target,result,coalesce_key) VALUES($1,$2,'account_remove',$3,'ok',$4)", &[&at,&actor,&request.account_id,&request.attempt]).await?;
            // Locks serialize sign-out and user changes, but do not stop time.
            // Recheck after the final write wait so expiry rolls everything back.
            if let Err(reason)=authorize(&tx,caller).await? {return Ok(Err(reason));}
            tx.commit().await?;
            super::browser::audit_log(at,actor,&request.account_id,"ok","account_remove");
            Ok(Ok(RemovalResult::Removed))
        }).await
    }

    pub(in crate::central) async fn removal_target(
        &self,
        user: &str,
        alias: &str,
    ) -> Result<Option<RemovalTarget>> {
        let db = self.browser_store()?;
        bounded_db(async {
            Ok(db.client().await?.query_opt(
                "SELECT a.account_id,a.alias,a.revision,(SELECT count(*) FROM account_live_sessions s WHERE s.account_id=a.account_id AND s.deleted_at IS NULL AND s.last_seen>clock_timestamp()-interval '5 minutes') FROM central_accounts a WHERE a.user_id=$1 AND lower(a.alias)=lower($2) AND a.deleted_at IS NULL",
                &[&user, &alias],
            ).await?.map(|r| RemovalTarget { account_id:r.get(0), alias:r.get(1), revision:r.get(2), recent_machines:r.get(3) }))
        }).await
    }
}
