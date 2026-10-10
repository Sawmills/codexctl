//! Shared browser sign-in state and sessions. Only the cookie digest identifies sessions.
use super::{CentralStore, PostgresStore, bounded_db, vault};
use anyhow::{Result, bail};
use std::time::Duration;

pub(in crate::central) enum BrowserIdentity {
    User(String),
    Disabled,
    Conflict,
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
            let resolution = if let Some(user) = users.iter().find(|u| u.oidc_identity.as_deref() == Some(arriving)) {
                if user.enabled { BrowserIdentity::User(user.id.clone()) } else { BrowserIdentity::Disabled }
            } else if let Some(user) = users.iter().find(|u| u.id == arriving) {
                if !user.enabled { BrowserIdentity::Disabled } else if user.oidc_identity.is_some() { BrowserIdentity::Conflict } else { BrowserIdentity::User(user.id.clone()) }
            } else {
                let matches: Vec<_> = users.iter().filter(|u| u.email.eq_ignore_ascii_case(email)).collect();
                if enforce_email_link && (!matches.is_empty() || authorized_source.is_some()) {
                    match (matches.as_slice(), authorized_source) {
                        ([user], Some(source)) if user.id == source && user.oidc_identity.is_none() => {
                            if !user.enabled { BrowserIdentity::Disabled } else {
                                tx.execute("UPDATE central_users SET oidc_identity=$2,revision=revision+1,updated_at=now() WHERE id=$1", &[&user.id,&arriving]).await?;
                                BrowserIdentity::User(user.id.clone())
                            }
                        }
                        _ => BrowserIdentity::Conflict,
                    }
                } else {
                    // Reserve under the same admission lock as resolution. A competing
                    // verified subject must see this email before it can claim another ID.
                    let inserted = tx.query_opt("INSERT INTO central_users(id,email,enabled) VALUES($1,$2,true) ON CONFLICT(id) DO NOTHING RETURNING id", &[&arriving,&email]).await?.is_some();
                    if inserted { BrowserIdentity::User(arriving.to_owned()) } else { BrowserIdentity::Disabled }
                }
            };
            tx.commit().await?;
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
            tx.query_one("SELECT pg_advisory_xact_lock(73912758)", &[]).await?;
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
        signed_in_at: i64,
        ttl: Duration,
    ) -> Result<bool> {
        let db = self.browser_store()?;
        let hash = vault::digest(token.as_bytes());
        bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            // Serialize the count and insertion across replicas, not just this connection.
            tx.query_one("SELECT pg_advisory_xact_lock(73912757)", &[]).await?;
            tx.execute("DELETE FROM browser_sessions WHERE expires_at <= now()", &[]).await?;
            let inserted = tx.execute(
                "INSERT INTO browser_sessions(token_hash,user_id,signed_in_at,expires_at) SELECT $1,$2,to_timestamp($3::double precision),now()+($4::bigint * interval '1 second') WHERE (SELECT count(*) FROM browser_sessions WHERE expires_at > now()) < 1024",
                &[&hash, &user, &(signed_in_at as f64), &(ttl.as_secs() as i64)],
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
