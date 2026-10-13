//! Shared browser sign-in state and sessions. Only the cookie digest identifies sessions.
use super::{CentralStore, PostgresStore, bounded_db, vault};
use crate::central::enrollment::identity::{self as browser_identity, Decision};
use anyhow::{Result, bail};
use std::time::Duration;

pub(in crate::central) enum BrowserIdentity {
    User(String),
    Disabled,
    Conflict,
}

#[derive(Clone, Copy)]
pub(in crate::central) struct DashboardAudit<'a> {
    pub actor: &'a str,
    pub attempt: &'a str,
}

pub(in crate::central) enum MachineRevoke {
    Revoked,
    AlreadyRevoked,
    NotFound,
    RegistryChanged,
}
impl MachineRevoke {
    pub(in crate::central) fn audit_result(&self) -> &'static str {
        match self {
            Self::Revoked => "ok",
            Self::AlreadyRevoked => "refused:already_revoked",
            Self::NotFound => "refused:device_not_found",
            Self::RegistryChanged => "refused:registry_changed",
        }
    }
}

impl CentralStore {
    pub(in crate::central) async fn resolve_browser_identity(
        &self,
        arriving: &str,
        email: &str,
        enforce_email_link: bool,
        authorized_source: Option<&str>,
    ) -> Result<BrowserIdentity> {
        let db = self.browser_store()?;
        bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let _timing = super::identity::lock_admission(&tx).await?;
            let users: Vec<_> = tx.query("SELECT id,email,enabled,oidc_identity FROM central_users WHERE deleted_at IS NULL ORDER BY id", &[]).await?
                .into_iter().map(|row| crate::central::managed::User {
                    id: row.get(0), email: row.get(1), enabled: row.get(2), oidc_identity: row.get(3),
                }).collect();
            let mut linked_user = None;
            let resolution = match browser_identity::decide(&users, arriving, email, enforce_email_link, authorized_source) {
                Decision::Known(resolution) => resolution,
                Decision::Link(index) => {
                    let user = &users[index];
                    tx.execute("UPDATE central_users SET oidc_identity=$2,revision=revision+1,updated_at=now() WHERE id=$1", &[&user.id,&arriving]).await?;
                    linked_user = Some(user.id.clone());
                    BrowserIdentity::User(user.id.clone())
                }
                Decision::Reserve => {
                    // Reserve under the same admission lock as resolution. A competing
                    // verified subject must see this email before it can claim another ID.
                    let inserted = tx.query_opt("INSERT INTO central_users(id,email,enabled) VALUES($1,$2,true) ON CONFLICT(id) DO NOTHING RETURNING id", &[&arriving,&email]).await?.is_some();
                    if inserted { BrowserIdentity::User(arriving.to_owned()) } else { BrowserIdentity::Disabled }
                }
            };
            tx.commit().await?;
            if let Some(stable_id) = linked_user {
                eprintln!("SSO_IDENTITY_LINKED company_user={stable_id} oidc_identity={arriving}");
            }
            Ok(resolution)
        }).await
    }

    pub(in crate::central) async fn record_browser_user(
        &self,
        id: &str,
        email: &str,
    ) -> Result<bool> {
        let db = self.browser_store()?;
        bounded_db(async {
            Ok(db.client().await?.query_opt(
                "INSERT INTO central_users(id,email,enabled) VALUES($1,$2,true) ON CONFLICT(id) DO UPDATE SET email=EXCLUDED.email,revision=central_users.revision+1,updated_at=now() WHERE central_users.enabled=true AND central_users.deleted_at IS NULL RETURNING id",
                &[&id,&email],
            ).await?.is_some())
        }).await
    }

    pub(in crate::central) async fn browser_sessions_ready(&self) -> Result<bool> {
        let db = self.browser_store()?;
        bounded_db(async {
            Ok(db
                .client()
                .await?
                .query_one("SELECT to_regclass('browser_sessions') IS NOT NULL", &[])
                .await?
                .get(0))
        })
        .await
    }

    pub(in crate::central) async fn browser_devices(
        &self,
        tenant: &str,
        user: &str,
    ) -> Result<Vec<vault::Device>> {
        let db = self.browser_store()?;
        bounded_db(async {
            let rows = db.client().await?.query(
                "SELECT id,tenant,user_id,token_hash,revoked FROM central_devices WHERE tenant=$1 AND user_id=$2 AND deleted_at IS NULL ORDER BY id",
                &[&tenant, &user],
            ).await?;
            Ok(rows.into_iter().map(|row| vault::Device {
                id: row.get(0), tenant: row.get(1), user: row.get(2),
                token_hash: row.get(3), revoked: row.get(4),
            }).collect())
        }).await
    }

    /// Scope the CAS to the company user and commit its optional dashboard audit atomically.
    /// API callers omit the actor and retain their idempotent response.
    pub(in crate::central) async fn revoke_machine(
        &self,
        tenant: &str,
        user: &str,
        id: &str,
        audit: Option<DashboardAudit<'_>>,
    ) -> Result<MachineRevoke> {
        let db = self.browser_store()?;
        bounded_db(async {
            let mut client = std::sync::Arc::try_unwrap(db.establish().await?)
                .map_err(|_| anyhow::anyhow!("revoke transaction connection is shared"))?;
            let tx = client.transaction().await?;
            let row = tx.query_opt("SELECT revoked,revision FROM central_devices WHERE tenant=$1 AND user_id=$2 AND id=$3 AND deleted_at IS NULL FOR UPDATE", &[&tenant, &user, &id]).await?;
            let outcome = match row {
                None => MachineRevoke::NotFound,
                Some(row) if row.get::<_,bool>(0) => MachineRevoke::AlreadyRevoked,
                Some(row) => {
                    let revision:i64=row.get(1);
                    let changed=tx.execute("UPDATE central_devices SET revoked=true,revision=revision+1,updated_at=now() WHERE tenant=$1 AND user_id=$2 AND id=$3 AND revision=$4 AND deleted_at IS NULL", &[&tenant,&user,&id,&revision]).await?;
                    if changed==1 { MachineRevoke::Revoked } else { MachineRevoke::RegistryChanged }
                }
            };
            if matches!(outcome,MachineRevoke::Revoked | MachineRevoke::AlreadyRevoked) {
                tx.execute("INSERT INTO central_relay_rate_limits(device_id,tokens,updated_at,allowed,deleted_at) VALUES($1,0,clock_timestamp(),false,clock_timestamp()) ON CONFLICT(device_id) DO UPDATE SET deleted_at=COALESCE(central_relay_rate_limits.deleted_at,clock_timestamp()),updated_at=clock_timestamp()", &[&id]).await?;
            }
            // Unresolved request targets are digests, never arbitrary client text.
            let target=if matches!(outcome,MachineRevoke::NotFound) { format!("unresolved:{}",vault::digest(id.as_bytes())) } else { id.to_owned() };
            let at=chrono::Utc::now().timestamp();
            if let Some(audit)=audit {
                tx.execute("INSERT INTO dashboard_audit(at,actor,operation,target,result,coalesce_key) VALUES($1,$2,'machine_revoke',$3,$4,$5)", &[&at,&audit.actor,&target,&outcome.audit_result(),&audit.attempt]).await?;
            }
            tx.commit().await?;
            if let Some(audit)=audit { audit_log(at,audit.actor,&target,outcome.audit_result()); }
            Ok(outcome)
        }).await
    }

    pub(in crate::central) async fn dashboard_audit(
        &self,
        audit: DashboardAudit<'_>,
        target: &str,
        result: &str,
    ) -> Result<()> {
        let db = self.browser_store()?;
        bounded_db(async {
            let at=chrono::Utc::now().timestamp();
            let client=db.client().await?;
            let inserted=client.query_opt("INSERT INTO dashboard_audit(at,actor,operation,target,result,coalesce_key) VALUES($1,$2,'machine_revoke',$3,$4,$5) ON CONFLICT(coalesce_key) DO NOTHING RETURNING at,actor,target,result", &[&at,&audit.actor,&target,&result,&audit.attempt]).await?;
            // A lost COMMIT acknowledgement can have already persisted the real outcome.
            let row=match inserted {
                Some(row)=>row,
                None=>client.query_one("SELECT at,actor,target,result FROM dashboard_audit WHERE coalesce_key=$1 AND deleted_at IS NULL", &[&audit.attempt]).await?,
            };
            audit_log(row.get(0),row.get(1),row.get(2),row.get(3));
            Ok(())
        }).await
    }

    fn browser_store(&self) -> Result<&PostgresStore> {
        match self {
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => Ok(db),
            Self::File(_) => bail!("shared browser sessions require PostgreSQL"),
        }
    }

    pub(in crate::central) async fn create_browser_login(
        &self,
        challenge: &str,
        payload: &[u8],
        ttl: Duration,
    ) -> Result<bool> {
        let db = self.browser_store()?;
        let hash = vault::digest(challenge.as_bytes());
        let encrypted = vault::encrypt_bytes(&db.key, payload)?;
        bounded_db(async {
            let mut connection = db.transaction_client(&db.browser_logins).await?;
            let tx = connection.transaction().await?;
            // Count after the cross-pod lock so each insertion sees its predecessor.
            tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended(current_schema(),73912758))", &[]).await?;
            tx.execute("DELETE FROM enrollment_challenges WHERE expires_at <= now() OR consumed_at IS NOT NULL", &[]).await?;
            let inserted = tx.execute(
                "INSERT INTO enrollment_challenges(challenge_hash,encrypted_payload,expires_at) SELECT $1,$2,now()+($3::bigint * interval '1 second') WHERE (SELECT count(*) FROM enrollment_challenges WHERE expires_at > now() AND consumed_at IS NULL) < 1024",
                &[&hash, &encrypted, &(ttl.as_secs() as i64)],
            ).await?;
            tx.commit().await?;
            Ok(inserted == 1)
        }).await
    }

    pub(in crate::central) async fn create_browser_session(
        &self,
        token: &str,
        user: &str,
        signed_in_at: Option<i64>,
        ttl: Duration,
    ) -> Result<bool> {
        let db = self.browser_store()?;
        let hash = vault::digest(token.as_bytes());
        bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            // Serialize the count and insertion across replicas, not just this connection.
            tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended(current_schema(),73912757))", &[]).await?;
            tx.execute("DELETE FROM browser_sessions WHERE expires_at <= now()", &[]).await?;
            let inserted = tx.execute(
                "INSERT INTO browser_sessions(token_hash,user_id,signed_in_at,expires_at,auth_time_verified) SELECT $1,$2,to_timestamp($3::double precision),now()+($4::bigint * interval '1 second'),$5 WHERE (SELECT count(*) FROM browser_sessions WHERE expires_at > now()) < 1024",
                &[&hash, &user, &(signed_in_at.unwrap_or_else(|| chrono::Utc::now().timestamp()) as f64), &(ttl.as_secs() as i64), &signed_in_at.is_some()],
            ).await?;
            tx.commit().await?;
            Ok(inserted == 1)
        }).await
    }

    pub(in crate::central) async fn browser_session_user(
        &self,
        token: &str,
    ) -> Result<Option<crate::central::managed::User>> {
        let db = self.browser_store()?;
        let hash = vault::digest(token.as_bytes());
        bounded_db(async {
            let client = db.client().await?;
            client.execute("DELETE FROM browser_sessions WHERE token_hash=$1 AND expires_at <= now()", &[&hash]).await?;
            let row = client.query_opt(
                "SELECT u.id,u.email,(u.enabled AND u.deleted_at IS NULL),u.oidc_identity FROM browser_sessions s JOIN central_users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.expires_at > now()",
                &[&hash],
            ).await?;
            Ok(row.map(|row| crate::central::managed::User {
                id: row.get(0), email: row.get(1), enabled: row.get(2), oidc_identity: row.get(3),
            }))
        }).await
    }

    pub(in crate::central) async fn browser_session_signed_in_at(
        &self,
        token: &str,
    ) -> Result<Option<i64>> {
        let db = self.browser_store()?;
        let hash = vault::digest(token.as_bytes());
        bounded_db(async {
            Ok(db.client().await?.query_opt(
                "SELECT extract(epoch FROM s.signed_in_at)::bigint FROM browser_sessions s JOIN central_users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.expires_at > now() AND s.auth_time_verified AND u.enabled AND u.deleted_at IS NULL",
                &[&hash],
            ).await?.map(|row| row.get(0)))
        }).await
    }

    pub(in crate::central) async fn end_browser_session(&self, token: &str) -> Result<()> {
        let db = self.browser_store()?;
        let hash = vault::digest(token.as_bytes());
        bounded_db(async {
            db.client()
                .await?
                .execute("DELETE FROM browser_sessions WHERE token_hash=$1", &[&hash])
                .await?;
            Ok(())
        })
        .await
    }
}

fn audit_log(at: i64, actor: &str, target: &str, result: &str) {
    eprintln!(
        "{}",
        serde_json::json!({"at":at,"actor_hash":vault::digest(actor.as_bytes()),"operation":"machine_revoke","target":target,"result":result})
    );
}
