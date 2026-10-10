//! Shared browser sessions. Only the cookie digest enters PostgreSQL.
use super::{CentralStore, PostgresStore, bounded_db, vault};
use anyhow::{Result, bail};
use std::time::Duration;

impl CentralStore {
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
