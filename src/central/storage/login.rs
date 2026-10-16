//! Shared login journal. Device polling has its own lease, separate from refresh.
use super::*;

pub(in crate::central) const LOGIN_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS central_login_holders (
    holder_id TEXT PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL,
    polling_bound BOOLEAN NOT NULL,
    deleted_at TIMESTAMPTZ
);
ALTER TABLE account_refresh_leases ADD COLUMN IF NOT EXISTS legacy_handoff BOOLEAN NOT NULL DEFAULT false;
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid='account_refresh_leases'::regclass AND attname='released' AND NOT attisdropped) THEN
        UPDATE account_refresh_leases SET legacy_handoff=true;
    END IF;
END $$;
ALTER TABLE account_refresh_leases ADD COLUMN IF NOT EXISTS released BOOLEAN NOT NULL DEFAULT false;
CREATE TABLE IF NOT EXISTS central_login_operations (
    user_id TEXT NOT NULL,
    id TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES central_accounts(account_id),
    alias TEXT NOT NULL,
    device_id TEXT NOT NULL,
    phase TEXT NOT NULL,
    sequence BIGINT NOT NULL DEFAULT 0,
    holder_id TEXT NOT NULL,
    epoch BIGINT NOT NULL DEFAULT 1,
    expires_at TIMESTAMPTZ NOT NULL,
    deadline TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp() + interval '900 seconds',
    cancel_requested BOOLEAN NOT NULL DEFAULT false,
    encrypted_payload BYTEA NOT NULL,
    selected_reserved BOOLEAN NOT NULL DEFAULT true,
    repair_evidence JSONB NOT NULL DEFAULT '[]'::jsonb,
    completed_revision BIGINT,
    candidate_workspace TEXT,
    candidate_login TEXT,
    PRIMARY KEY (user_id, id)
);
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS browser_session_hash TEXT;
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS polling_clear BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS failure_reported BOOLEAN NOT NULL DEFAULT false;
CREATE UNIQUE INDEX IF NOT EXISTS central_login_active_account
    ON central_login_operations(account_id)
    WHERE phase NOT IN ('completed','failed','canceled','rejected','replica_lost');
CREATE INDEX IF NOT EXISTS central_login_completed_revision
    ON central_login_operations(account_id, completed_revision) WHERE phase='completed';
ALTER TABLE central_login_operations ALTER COLUMN account_id DROP NOT NULL;
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'renewal';
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS landed_alias TEXT;
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS candidate_claims JSONB NOT NULL DEFAULT '{}'::jsonb;
CREATE UNIQUE INDEX IF NOT EXISTS central_login_active_alias
    ON central_login_operations(user_id, lower(alias))
    WHERE phase NOT IN ('completed','failed','canceled','rejected','replica_lost');
CREATE TABLE IF NOT EXISTS central_alias_tombstones (
    user_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    target TEXT NOT NULL,
    PRIMARY KEY(user_id,alias)
);
CREATE TABLE IF NOT EXISTS central_account_claims (
    workspace TEXT NOT NULL,
    namespace TEXT NOT NULL CHECK(namespace IN ('uid','sub')),
    claim TEXT NOT NULL,
    account_id TEXT NOT NULL,
    PRIMARY KEY(workspace,namespace,claim)
);
CREATE INDEX IF NOT EXISTS central_account_claims_owner ON central_account_claims(account_id);
CREATE TABLE IF NOT EXISTS central_login_identity_reservations (
    workspace TEXT NOT NULL,
    namespace TEXT NOT NULL CHECK(namespace IN ('uid','sub')),
    claim TEXT NOT NULL,
    user_id TEXT NOT NULL,
    id TEXT NOT NULL,
    PRIMARY KEY(workspace,namespace,claim),
    FOREIGN KEY(user_id,id) REFERENCES central_login_operations(user_id,id)
);
CREATE INDEX IF NOT EXISTS central_login_identity_owner ON central_login_identity_reservations(user_id,id);
ALTER TABLE central_alias_tombstones ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
ALTER TABLE central_account_claims ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
ALTER TABLE central_login_identity_reservations ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
CREATE TABLE IF NOT EXISTS central_login_identity_reservation_history (
    workspace TEXT NOT NULL,
    namespace TEXT NOT NULL CHECK(namespace IN ('uid','sub')),
    claim TEXT NOT NULL,
    user_id TEXT NOT NULL,
    id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY(workspace,namespace,claim,user_id,id)
);
CREATE TABLE IF NOT EXISTS central_account_identity_claims (
    workspace TEXT NOT NULL,
    namespace TEXT NOT NULL CHECK(namespace IN ('uid','sub')),
    claim TEXT NOT NULL,
    account_id TEXT NOT NULL,
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY(workspace,namespace,claim)
);
CREATE INDEX IF NOT EXISTS central_account_identity_claims_owner ON central_account_identity_claims(account_id);
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS candidate_uid TEXT;
ALTER TABLE central_login_operations ADD COLUMN IF NOT EXISTS candidate_sub TEXT;
CREATE OR REPLACE FUNCTION central_login_identity_agrees(p_workspace TEXT, p_uid TEXT, p_sub TEXT, p_account TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    WITH owned AS (SELECT account_id,workspace,login FROM central_accounts WHERE account_id=p_account AND deleted_at IS NULL),
    known AS (
        SELECT namespace,claim FROM central_account_identity_claims JOIN owned USING(account_id)
            WHERE central_account_identity_claims.workspace=owned.workspace AND central_account_identity_claims.deleted_at IS NULL
        UNION SELECT 'sub',login FROM owned WHERE login IS NOT NULL
    )
    SELECT EXISTS(SELECT 1 FROM owned WHERE workspace=p_workspace) AND CASE
        WHEN p_uid IS NOT NULL AND EXISTS(SELECT 1 FROM known WHERE namespace='uid')
        THEN EXISTS(SELECT 1 FROM known WHERE namespace='uid' AND claim=p_uid)
        ELSE EXISTS(SELECT 1 FROM known WHERE namespace='sub' AND claim=p_sub)
    END
$$;
CREATE OR REPLACE FUNCTION central_login_identity_matches(p_workspace TEXT, p_uid TEXT, p_sub TEXT, p_account TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    WITH owned AS (SELECT account_id,workspace,login FROM central_accounts WHERE account_id=p_account AND deleted_at IS NULL),
    known AS (
        SELECT namespace,claim FROM central_account_identity_claims JOIN owned USING(account_id)
            WHERE central_account_identity_claims.workspace=owned.workspace AND central_account_identity_claims.deleted_at IS NULL
        UNION SELECT 'sub',login FROM owned WHERE login IS NOT NULL
    )
    SELECT EXISTS(SELECT 1 FROM owned WHERE workspace=p_workspace) AND (
        EXISTS(SELECT 1 FROM known WHERE (namespace='uid' AND claim=p_uid) OR (namespace='sub' AND claim=p_sub))
        OR NOT EXISTS(SELECT 1 FROM known WHERE (namespace='uid' AND p_uid IS NOT NULL) OR (namespace='sub' AND p_sub IS NOT NULL))
    )
$$;
CREATE OR REPLACE FUNCTION central_login_identity_agrees(p_workspace TEXT, p_claims JSONB, p_account TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT central_login_identity_agrees(p_workspace,p_claims->>'uid',p_claims->>'sub',p_account)
$$;
CREATE OR REPLACE FUNCTION central_login_identity_matches(p_workspace TEXT, p_claims JSONB, p_account TEXT)
RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
    SELECT central_login_identity_matches(p_workspace,p_claims->>'uid',p_claims->>'sub',p_account)
$$;
"#;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::central) enum LoginKind {
    Renewal,
    Add,
}
pub(in crate::central) enum StartLookup<'a> {
    Id(&'a str),
    Alias(&'a str),
}
impl LoginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Renewal => "renewal",
            Self::Add => "add",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::central) enum LoginPhase {
    Starting,
    Pending,
    Candidate,
    Verifying,
    Completed,
    Failed,
    Canceled,
    Unresolved,
    Rejected,
    ReplicaLost,
}
impl LoginPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Pending => "pending",
            Self::Candidate => "candidate",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Unresolved => "unresolved",
            Self::Rejected => "rejected",
            Self::ReplicaLost => "replica_lost",
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(in crate::central) struct LoginPayload {
    #[serde(default)]
    pub dashboard_actor: Option<String>,
    pub code: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub landed: Option<String>,
    pub candidate: Option<Value>,
    pub error: Option<String>,
}
pub(in crate::central) enum AddAdmission {
    Ready,
    Refused(&'static str),
}

/// Durable authority for one login. Browser sessions never impersonate machines.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::central) enum LoginAuthority {
    Machine(String),
    Browser(String),
}

#[derive(Debug)]
pub(in crate::central) struct LoginAuthorityEnded;
impl std::fmt::Display for LoginAuthorityEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("login authority ended")
    }
}
impl std::error::Error for LoginAuthorityEnded {}

impl LoginAuthority {
    fn parts(&self) -> (&str, Option<&str>) {
        match self {
            Self::Machine(id) => (id, None),
            Self::Browser(digest) => ("", Some(digest)),
        }
    }
}

async fn audit_login<C: tokio_postgres::GenericClient + Sync>(
    client: &C,
    op: &LoginOperation,
    stage: &str,
    result: &str,
) -> Result<bool> {
    if !matches!(op.authority, LoginAuthority::Browser(_)) {
        return Ok(false);
    }
    let actor = op
        .payload
        .dashboard_actor
        .as_deref()
        .context("browser login audit actor missing")?;
    let operation = match op.kind {
        LoginKind::Add => DashboardOperation::AccountAdd.as_str(),
        LoginKind::Renewal => DashboardOperation::AccountRenew.as_str(),
    };
    let attempt = format!("login:{}:{}:{stage}", op.user, op.id);
    let inserted = client.execute(
        "INSERT INTO dashboard_audit(at,actor,operation,target,result,coalesce_key) VALUES(extract(epoch FROM clock_timestamp())::bigint,$1,$2,$3,$4,$5) ON CONFLICT(coalesce_key) DO NOTHING",
        &[&actor,&operation,&op.alias,&result,&attempt],
    ).await?;
    Ok(inserted == 1)
}

fn terminal_result(phase: LoginPhase) -> Option<&'static str> {
    match phase {
        LoginPhase::Completed => Some("ok:completed"),
        LoginPhase::Canceled => Some("ok:canceled"),
        LoginPhase::Failed => Some("failed:login_failed"),
        LoginPhase::Rejected => Some("failed:rejected"),
        LoginPhase::Unresolved => Some("failed:unresolved"),
        LoginPhase::ReplicaLost => Some("failed:expired"),
        _ => None,
    }
}
fn log_login_audit(op: &LoginOperation, stage: &str, result: &str) {
    if let (LoginAuthority::Browser(_), Some(actor)) = (&op.authority, &op.payload.dashboard_actor)
    {
        eprintln!(
            "{}",
            serde_json::json!({
                "operation":match op.kind { LoginKind::Add => "account_add", LoginKind::Renewal => "account_renew" },
                "stage":stage,"actor_hash":vault::digest(actor.as_bytes()),"target":op.alias,"result":result,
            })
        );
    }
}

/// Lock authority through a publication transaction, including a racing sign-out.
pub(super) async fn authority_live<C: tokio_postgres::GenericClient + Sync>(
    client: &C,
    user: &str,
    authority: &LoginAuthority,
) -> Result<bool> {
    let row = match authority {
        LoginAuthority::Machine(id) => client.query_opt(
            "SELECT d.id FROM central_devices d JOIN central_users u ON u.id=d.user_id WHERE d.id=$1 AND d.user_id=$2 AND d.tenant='sawmills' AND NOT d.revoked AND d.deleted_at IS NULL AND u.enabled AND u.deleted_at IS NULL FOR SHARE OF d,u", &[id,&user]).await?,
        LoginAuthority::Browser(digest) => client.query_opt(
            "SELECT s.token_hash FROM browser_sessions s JOIN central_users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.user_id=$2 AND s.expires_at>clock_timestamp() AND u.enabled AND u.deleted_at IS NULL FOR SHARE OF s,u", &[digest,&user]).await?,
    };
    Ok(row.is_some())
}

#[derive(Clone)]
pub(in crate::central) struct LoginOperation {
    pub kind: LoginKind,
    pub user: String,
    pub id: String,
    pub account_id: Option<String>,
    pub alias: String,
    pub authority: LoginAuthority,
    pub phase: LoginPhase,
    pub sequence: i64,
    pub holder: String,
    pub epoch: i64,
    pub payload: LoginPayload,
    pub polling_clear: bool,
    pub lease_expired: bool,
    pub failure_reported: bool,
}
impl LoginOperation {
    pub fn account(&self) -> Result<&str> {
        self.account_id
            .as_deref()
            .context("login has not reserved an account")
    }
}
impl PostgresStore {
    /// Only device polling can expire into a terminal receipt. A candidate or
    /// verification marker stays reserved, even after its operation lease expires.
    async fn expire_polling_login(
        &self,
        user: &str,
        id: Option<&str>,
        alias: Option<&str>,
    ) -> Result<()> {
        self.client().await?.execute(
            "UPDATE central_login_operations SET phase='replica_lost',sequence=sequence+1 WHERE user_id=$1 AND (id=$2 OR lower(alias)=lower($3)) AND phase IN ('starting','pending') AND expires_at<=clock_timestamp()",
            &[&user,&id,&alias],
        ).await?;
        Ok(())
    }
    fn login_row(&self, row: tokio_postgres::Row) -> Result<LoginOperation> {
        let phase: String = row.get("phase");
        let encrypted: Vec<u8> = row.get("encrypted_payload");
        Ok(LoginOperation {
            kind: serde_json::from_value(Value::String(row.get("kind")))?,
            user: row.get("user_id"),
            id: row.get("id"),
            account_id: row.get("account_id"),
            alias: row.get("alias"),
            authority: match row.get::<_, Option<String>>("browser_session_hash") {
                Some(digest) => LoginAuthority::Browser(digest),
                None => LoginAuthority::Machine(row.get("device_id")),
            },
            phase: serde_json::from_value(Value::String(phase))?,
            sequence: row.get("sequence"),
            holder: row.get("holder_id"),
            epoch: row.get("epoch"),
            payload: {
                let mut payload: LoginPayload =
                    serde_json::from_slice(&vault::decrypt_bytes(&self.key, &encrypted)?)?;
                payload.landed = row.get("landed_alias");
                payload
            },
            polling_clear: row.get("polling_clear"),
            lease_expired: row.get("lease_expired"),
            failure_reported: row.get("failure_reported"),
        })
    }
}
impl CentralStore {
    pub(in crate::central) async fn login_register_holder(&self, holder: &str) -> Result<()> {
        let db = self.login_db()?;
        bounded_db(async {
            let changed = db.client().await?.execute(
                "INSERT INTO central_login_holders(holder_id,expires_at,polling_bound) VALUES($1,clock_timestamp()+interval '30 seconds',$2) ON CONFLICT DO NOTHING",
                &[&holder, &cfg!(target_os = "linux")],
            ).await?;
            if changed != 1 { bail!("login holder incarnation already registered"); }
            Ok(())
        }).await
    }

    pub(in crate::central) async fn login_renew_holder(&self, holder: &str) -> Result<bool> {
        let db = self.login_db()?;
        bounded_db(async {
            Ok(db.client().await?.execute(
                "UPDATE central_login_holders SET expires_at=clock_timestamp()+interval '30 seconds' WHERE holder_id=$1 AND deleted_at IS NULL AND expires_at>clock_timestamp()",
                &[&holder],
            ).await? == 1)
        }).await
    }

    pub(in crate::central) async fn login_release_holder(&self, holder: &str) -> Result<()> {
        let db = self.login_db()?;
        bounded_db(async {
            // Expiry is the liveness bit. Keep the parent-bound provenance
            // immutable so a durable candidate can still be taken over after
            // graceful shutdown.
            db.client().await?.execute("UPDATE central_login_holders SET expires_at=clock_timestamp() WHERE holder_id=$1 AND deleted_at IS NULL", &[&holder]).await?;
            Ok(())
        }).await
    }

    /// Only a registered, parent-bound polling incarnation proves automatic exit.
    /// Unregistered legacy operations and verification evidence remain fenced.
    pub(in crate::central) async fn login_recover_polling(
        &self,
        op: &mut LoginOperation,
        holder: &str,
    ) -> Result<bool> {
        if op.phase != LoginPhase::ReplicaLost {
            return Ok(false);
        }
        let db = self.login_db()?;
        let recovered = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let _timing = super::identity::lock_admission(&tx).await?;
            let row = tx.query_opt(
                "UPDATE central_login_operations o SET polling_clear=true,selected_reserved=false,holder_id=$6,epoch=epoch+1,sequence=sequence+1 FROM central_login_holders h WHERE o.user_id=$1 AND o.id=$2 AND o.holder_id=$3 AND o.epoch=$4 AND o.sequence=$5 AND o.phase='replica_lost' AND o.polling_clear AND o.selected_reserved AND o.candidate_workspace IS NULL AND o.expires_at<=clock_timestamp() AND h.holder_id=o.holder_id AND h.deleted_at IS NULL AND h.polling_bound AND h.expires_at<=clock_timestamp() AND EXISTS(SELECT 1 FROM central_login_holders current_holder WHERE current_holder.holder_id=$6 AND current_holder.deleted_at IS NULL AND current_holder.expires_at>clock_timestamp()) RETURNING o.*,o.expires_at<=clock_timestamp() AS lease_expired",
                &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&holder],
            ).await?;
            if row.is_some() {
                tx.execute("UPDATE central_login_identity_reservations SET deleted_at=clock_timestamp() WHERE user_id=$1 AND id=$2 AND deleted_at IS NULL", &[&op.user,&op.id]).await?;
                tx.execute("UPDATE central_login_identity_reservation_history SET deleted_at=clock_timestamp() WHERE user_id=$1 AND id=$2 AND deleted_at IS NULL", &[&op.user,&op.id]).await?;
            }
            tx.commit().await?;
            Ok(row)
        }).await?;
        if let Some(row) = recovered {
            *op = db.login_row(row)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Claim only a durable candidate that has not started verification.
    /// Foreign refresh leases remain untouched until their own settlement.
    /// A live holder reclaims its own candidate even after the operation lease
    /// expires, because no other replica may claim it while the holder lives.
    pub(in crate::central) async fn login_takeover_candidate(
        &self,
        op: &mut LoginOperation,
        holder: &str,
    ) -> Result<bool> {
        if op.phase != LoginPhase::Candidate {
            return Ok(false);
        }
        let db = self.login_db()?;
        let row = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let _timing = super::identity::lock_admission(&tx).await?;
            let row = tx.query_opt(
                "UPDATE central_login_operations o SET holder_id=$6,epoch=epoch+1,sequence=sequence+1,expires_at=clock_timestamp()+interval '30 seconds' FROM central_login_holders h WHERE o.user_id=$1 AND o.id=$2 AND o.holder_id=$3 AND o.epoch=$4 AND o.sequence=$5 AND o.phase='candidate' AND h.holder_id=o.holder_id AND h.deleted_at IS NULL AND h.polling_bound AND ((o.holder_id=$6 AND h.expires_at>clock_timestamp()) OR (o.expires_at<=clock_timestamp() AND h.expires_at<=clock_timestamp())) AND NOT EXISTS(SELECT 1 FROM account_refresh_leases l WHERE l.account_id=o.account_id AND l.holder_id=o.holder_id||':'||o.id AND NOT l.released) AND EXISTS(SELECT 1 FROM central_login_holders current_holder WHERE current_holder.holder_id=$6 AND current_holder.deleted_at IS NULL AND current_holder.expires_at>clock_timestamp()) RETURNING o.*,o.expires_at<=clock_timestamp() AS lease_expired",
                &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&holder],
            ).await?;
            tx.commit().await?;
            Ok(row)
        }).await?;
        if let Some(row) = row {
            *op = db.login_row(row)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Expired verification, or unsettled pre-verification refresh ownership,
    /// keeps all credential and reservation evidence without replaying work.
    /// The caller settles its own holder's receipt only after its worker exits.
    pub(in crate::central) async fn login_recover_unresolved(
        &self,
        op: &mut LoginOperation,
        holder: &str,
    ) -> Result<bool> {
        if !matches!(op.phase, LoginPhase::Candidate | LoginPhase::Verifying) {
            return Ok(false);
        }
        let db = self.login_db()?;
        let mut payload = op.payload.clone();
        payload.error = Some(
            if op.phase == LoginPhase::Verifying {
                "relogin_verification_unresolved"
            } else {
                "relogin_settlement_unresolved"
            }
            .into(),
        );
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&payload)?)?;
        let (row, audited) = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let _timing = super::identity::lock_admission(&tx).await?;
            let row = tx.query_opt(
                "UPDATE central_login_operations o SET phase='unresolved',encrypted_payload=$7,holder_id=$6,epoch=epoch+1,sequence=sequence+1 FROM central_login_holders h WHERE o.user_id=$1 AND o.id=$2 AND o.holder_id=$3 AND o.epoch=$4 AND o.sequence=$5 AND o.phase=$8 AND o.expires_at<=clock_timestamp() AND (o.phase='verifying' OR (o.phase='candidate' AND EXISTS(SELECT 1 FROM account_refresh_leases l WHERE l.account_id=o.account_id AND l.holder_id=o.holder_id||':'||o.id AND NOT l.released))) AND h.holder_id=o.holder_id AND h.deleted_at IS NULL AND (h.expires_at<=clock_timestamp() OR o.holder_id=$6) AND EXISTS(SELECT 1 FROM central_login_holders current_holder WHERE current_holder.holder_id=$6 AND current_holder.deleted_at IS NULL AND current_holder.expires_at>clock_timestamp()) RETURNING o.*,o.expires_at<=clock_timestamp() AS lease_expired",
                &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&holder,&encrypted,&op.phase.as_str()],
            ).await?;
            let audited = if row.is_some() { audit_login(&tx, op, "terminal", "failed:unresolved").await? } else { false };
            tx.commit().await?;
            Ok((row, audited))
        }).await?;
        if let Some(row) = row {
            *op = db.login_row(row)?;
            if audited {
                log_login_audit(op, "terminal", "failed:unresolved");
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// A supervisor's awaited native exit and private home prove grant absence.
    #[cfg(target_os = "linux")]
    pub(in crate::central) async fn login_record_polling_absence(
        &self,
        op: &mut LoginOperation,
        canceled: bool,
    ) -> Result<()> {
        let db = self.login_db()?;
        if let LoginAuthority::Browser(session) = &op.authority {
            let phase = if canceled {
                LoginPhase::Canceled
            } else {
                LoginPhase::Failed
            };
            let result = terminal_result(phase).expect("polling stop is terminal");
            let mut payload = op.payload.clone();
            payload.code = None;
            if !canceled {
                payload.error = Some("relogin_interrupted_retry".into());
            }
            let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&payload)?)?;
            let (settled, audited) = bounded_db(async {
                let mut connection = db.admission_client().await?;
                let tx = connection.transaction().await?;
                // Awaited native exit and grant absence permit settlement even
                // after lease expiry. Never restore authority or touch a grant.
                let row = tx.query_opt(
                    "UPDATE central_login_operations SET phase=$5,polling_clear=true,sequence=sequence+1,encrypted_payload=$6,selected_reserved=CASE WHEN kind='add' AND account_id IS NULL THEN false ELSE selected_reserved END WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND browser_session_hash=$7 AND phase IN ('starting','pending','replica_lost') AND candidate_workspace IS NULL RETURNING *,expires_at<=clock_timestamp() AS lease_expired",
                    &[&op.user,&op.id,&op.holder,&op.epoch,&phase.as_str(),&encrypted,&session],
                ).await?.context("browser polling settlement fenced")?;
                let settled = db.login_row(row)?;
                let audited = audit_login(&tx,&settled,"terminal",result).await?;
                tx.commit().await?;
                Ok((settled,audited))
            }).await?;
            *op = settled;
            if audited {
                log_login_audit(op, "terminal", result);
            }
            return Ok(());
        }
        bounded_db(async {
            let changed = db.client().await?.execute(
                "UPDATE central_login_operations SET polling_clear=true WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND phase IN ('starting','pending','replica_lost') AND candidate_workspace IS NULL",
                &[&op.user,&op.id,&op.holder,&op.epoch],
            ).await?;
            if changed != 1 {bail!("polling settlement fenced");}
            Ok(())
        }).await
    }

    /// Late capture retains evidence without restoring authority or verifying.
    #[cfg(target_os = "linux")]
    pub(in crate::central) async fn login_record_unpublished_grant(
        &self,
        op: &LoginOperation,
    ) -> Result<()> {
        let db = self.login_db()?;
        let mut payload = op.payload.clone();
        payload.code = None;
        payload.error = Some("relogin_grant_unpublished".into());
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&payload)?)?;
        let audited = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let changed = tx.execute(
                "UPDATE central_login_operations SET phase='unresolved',sequence=sequence+1,encrypted_payload=$6 WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase IN ('starting','pending','replica_lost') AND candidate_workspace IS NULL",
                &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&encrypted],
            ).await?;
            if changed != 1 {bail!("late grant settlement fenced");}
            let audited = audit_login(&tx, op, "terminal", "failed:unresolved").await?;
            tx.commit().await?;
            Ok(audited)
        }).await?;
        if audited {
            log_login_audit(op, "terminal", "failed:unresolved");
        }
        Ok(())
    }

    /// One serving replica reports each durable supervisor/unknown-exit failure.
    /// A holder with no row predates registration and reports its own failures.
    /// A worker reports after its durable save, so a `failed` receipt counts.
    pub(in crate::central) async fn login_take_failure(
        &self,
        op: &mut LoginOperation,
        reporter: &str,
    ) -> Result<bool> {
        self.take_failure(op, reporter, false).await
    }

    /// Claims only when the durable receipt proves no grant can still complete:
    /// never a candidate or a verification, and live or lost polling only after
    /// proven absence.
    pub(in crate::central) async fn login_take_grantless_failure(
        &self,
        op: &mut LoginOperation,
        reporter: &str,
    ) -> Result<bool> {
        self.take_failure(op, reporter, true).await
    }

    async fn take_failure(
        &self,
        op: &mut LoginOperation,
        reporter: &str,
        grantless: bool,
    ) -> Result<bool> {
        let db = self.login_db()?;
        let changed = bounded_db(async {
            Ok(db.client().await?.execute(
                "UPDATE central_login_operations SET failure_reported=true WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND phase NOT IN ('completed','canceled') AND NOT failure_reported AND (holder_id=$5 OR (EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$3) AND NOT EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$3 AND deleted_at IS NULL AND expires_at>clock_timestamp()))) AND (NOT $6::boolean OR (phase NOT IN ('candidate','verifying') AND (phase NOT IN ('starting','pending','replica_lost') OR polling_clear)))",
                &[&op.user,&op.id,&op.holder,&op.epoch,&reporter,&grantless],
            ).await? == 1)
        }).await?;
        if changed {
            op.failure_reported = true;
        }
        Ok(changed)
    }

    /// The supervisor reads durable cancellation without extending parent authority.
    #[cfg(target_os = "linux")]
    pub(in crate::central) async fn login_polling_authority(
        &self,
        op: &LoginOperation,
    ) -> Result<(bool, bool)> {
        let db = self.login_db()?;
        bounded_db(async {
            let row=db.client().await?.query_opt(
                "SELECT cancel_requested,phase IN ('starting','pending') AND expires_at>clock_timestamp() AND deadline>clock_timestamp() AND EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$3 AND deleted_at IS NULL AND expires_at>clock_timestamp()) FROM central_login_operations WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4",
                &[&op.user,&op.id,&op.holder,&op.epoch],
            ).await?.context("polling incarnation fenced")?;
            Ok((row.get(0),row.get(1)))
        }).await
    }

    pub(super) fn login_db(&self) -> Result<&PostgresStore> {
        match self {
            Self::Postgres(db) => Ok(db),
            _ => bail!("shared login requires PostgreSQL"),
        }
    }
    /// The caller confirms exit and durable settlement of all pre-migration
    /// refresh owners. New lease acquisitions consume the legacy marker, so
    /// repeating this handoff cannot release a current owner's lease.
    pub async fn confirm_legacy_owners_settled(&self) -> Result<u64> {
        let db = match self {
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => db,
            Self::File(_) => bail!("legacy lease handoff requires PostgreSQL"),
        };
        bounded_db(async {
            Ok(db.client().await?.execute(
                "UPDATE account_refresh_leases SET released=true,legacy_handoff=false,expires_at=clock_timestamp() WHERE legacy_handoff AND NOT released",
                &[],
            ).await?)
        }).await
    }
    /// A late settlement receipt proves exit and absence of a saved grant, not
    /// permission to promote credentials. Bind it to the original incarnation.
    pub(in crate::central) async fn login_polling_clear(&self, op: &LoginOperation) -> Result<()> {
        if op.payload.candidate.is_some()
            || !matches!(op.phase, LoginPhase::Starting | LoginPhase::Pending)
        {
            bail!("login polling clearance requires confirmed grant absence");
        }
        let db = self.login_db()?;
        bounded_db(async {
            let changed = db.client().await?.execute(
                "UPDATE central_login_operations SET polling_clear=true WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND (sequence=$5 OR (sequence=$5+1 AND phase='replica_lost')) AND phase IN ('starting','pending','replica_lost') AND candidate_workspace IS NULL",
                &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence],
            ).await?;
            if changed!=1 {bail!("login polling clearance fenced");}
            Ok(())
        }).await
    }
    /// A browser GET cannot expire a journal or recover a reservation. Scope the
    /// snapshot to the live session in the same query that reads the receipt.
    pub(in crate::central) async fn login_browser_get(
        &self,
        user: &str,
        session: &str,
        id: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client().await?.query_opt(
                "SELECT o.*,o.expires_at<=clock_timestamp() OR (o.phase IN ('starting','pending') AND o.deadline<=clock_timestamp()) AS lease_expired FROM central_login_operations o JOIN browser_sessions s ON s.token_hash=o.browser_session_hash AND s.user_id=o.user_id JOIN central_users u ON u.id=s.user_id WHERE o.user_id=$1 AND o.id=$2 AND s.token_hash=$3 AND s.expires_at>clock_timestamp() AND u.enabled AND u.deleted_at IS NULL",
                &[&user,&id,&session],
            ).await?.map(|row|db.login_row(row)).transpose()
        }).await
    }

    /// Start conflict discovery must not expire another authority's receipt.
    pub(in crate::central) async fn login_start_lookup(
        &self,
        user: &str,
        lookup: StartLookup<'_>,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        let (query, value) = match lookup {
            StartLookup::Id(id) => (
                "SELECT *,expires_at<=clock_timestamp() AS lease_expired FROM central_login_operations WHERE user_id=$1 AND id=$2",
                id,
            ),
            StartLookup::Alias(alias) => (
                "SELECT *,expires_at<=clock_timestamp() AS lease_expired FROM central_login_operations WHERE user_id=$1 AND lower(alias)=lower($2) AND (phase NOT IN ('completed','failed','canceled','rejected','replica_lost') OR (phase='replica_lost' AND selected_reserved)) ORDER BY (phase='replica_lost'), id LIMIT 1",
                alias,
            ),
        };
        bounded_db(async {
            db.client()
                .await?
                .query_opt(query, &[&user, &value])
                .await?
                .map(|row| db.login_row(row))
                .transpose()
        })
        .await
    }

    pub(in crate::central) async fn login_get(
        &self,
        user: &str,
        id: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.expire_polling_login(user, Some(id), None).await?;
            db.client()
                .await?
                .query_opt(
                    "SELECT *,expires_at<=clock_timestamp() AS lease_expired FROM central_login_operations WHERE user_id=$1 AND id=$2",
                    &[&user, &id],
                )
                .await?
                .map(|row| db.login_row(row))
                .transpose()
        })
        .await
    }
    pub(in crate::central) async fn login_completed_after(
        &self,
        account: &str,
        previous: i64,
        current: i64,
    ) -> Result<bool> {
        let db = self.login_db()?;
        bounded_db(async {
            let row = db.client().await?.query_one(
                "SELECT EXISTS (SELECT 1 FROM central_login_operations WHERE account_id=$1 AND phase='completed' AND completed_revision>$2 AND completed_revision<=$3)",
                &[&account,&previous,&current],
            ).await?;
            Ok(row.get(0))
        }).await
    }
    pub(in crate::central) async fn login_active_alias(
        &self,
        user: &str,
        alias: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.expire_polling_login(user,None,Some(alias)).await?;
            db.client().await?.query_opt("SELECT *,expires_at<=clock_timestamp() AS lease_expired FROM central_login_operations WHERE user_id=$1 AND lower(alias)=lower($2) AND (phase NOT IN ('completed','failed','canceled','rejected','replica_lost') OR (phase='replica_lost' AND selected_reserved)) ORDER BY (phase='replica_lost'), id LIMIT 1", &[&user,&alias]).await?
                .map(|row| db.login_row(row)).transpose()
        }).await
    }
    pub(in crate::central) async fn login_active_account(
        &self,
        account: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client().await?.query_opt("SELECT *,expires_at<=clock_timestamp() AS lease_expired FROM central_login_operations WHERE account_id=$1 AND phase NOT IN ('completed','failed','canceled','rejected','replica_lost')",&[&account]).await?.map(|row|db.login_row(row)).transpose()
        }).await
    }
    pub(in crate::central) async fn login_audit_terminal(&self, op: &LoginOperation) -> Result<()> {
        if !matches!(op.authority, LoginAuthority::Browser(_)) {
            return Ok(());
        }
        let Some(result) = terminal_result(op.phase) else {
            return Ok(());
        };
        let db = self.login_db()?;
        let inserted = bounded_db(async {
            let client = db.client().await?;
            audit_login(&*client, op, "terminal", result).await
        })
        .await?;
        if inserted {
            log_login_audit(op, "terminal", result);
        }
        Ok(())
    }
    pub(in crate::central) async fn login_authority_live(
        &self,
        op: &LoginOperation,
    ) -> Result<bool> {
        let db = self.login_db()?;
        bounded_db(async {
            let client = db.client().await?;
            authority_live(&*client, &op.user, &op.authority).await
        })
        .await
    }
    pub(in crate::central) async fn login_create(&self, op: &LoginOperation) -> Result<bool> {
        let db = self.login_db()?;
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&op.payload)?)?;
        let created = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let tx = connection.transaction().await?;
            let _timing = identity::lock_admission(&tx).await?;
            if !authority_live(&tx, &op.user, &op.authority).await? { bail!(LoginAuthorityEnded); }
            // Renewal admission must not start native polling for a removed
            // account. Add intentionally has no selected account yet.
            if let Some(account) = op.account_id.as_ref() {
                if !super::resets::mutation_allowed(&tx, account).await? {
                    return Err(crate::central::resets::Refusal("reset_in_progress").into());
                }
                let row = tx.query_opt(
                    "SELECT deleted_at IS NOT NULL FROM central_accounts WHERE account_id=$1 AND user_id=$2 FOR SHARE",
                    &[account, &op.user],
                ).await?;
                match row {
                    Some(row) if row.get::<_, bool>(0) => bail!(super::AccountRemoved),
                    Some(_) => {}
                    None => bail!("renewal account missing"),
                }
            }
            let created = tx.execute("INSERT INTO central_login_operations(user_id,id,account_id,alias,device_id,phase,holder_id,expires_at,encrypted_payload,kind,browser_session_hash) SELECT $1,$2,$3,$4,$5,'starting',$6,clock_timestamp()+interval '30 seconds',$7,$8,$9 WHERE EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$6 AND deleted_at IS NULL AND expires_at>clock_timestamp()) ON CONFLICT DO NOTHING",
                &[&op.user,&op.id,&op.account_id,&op.alias,&op.authority.parts().0,&op.holder,&encrypted,&op.kind.as_str(),&op.authority.parts().1]).await? == 1;
            if created { audit_login(&tx, op, "start", "ok:started").await?; }
            tx.commit().await?;
            Ok(created)
        }).await?;
        if created {
            log_login_audit(op, "start", "ok:started");
        }
        Ok(created)
    }
    pub(in crate::central) async fn login_cancel(&self, op: &LoginOperation) -> Result<()> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client().await?.execute("UPDATE central_login_operations SET cancel_requested=true WHERE user_id=$1 AND id=$2 AND device_id=$3 AND browser_session_hash IS NOT DISTINCT FROM $4 AND phase IN ('starting','pending','candidate','verifying')", &[&op.user,&op.id,&op.authority.parts().0,&op.authority.parts().1]).await?;
            Ok(())
        }).await
    }
    /// A failed read or renewal is loss of authority. The child must stop.
    pub(in crate::central) async fn login_heartbeat(&self, op: &LoginOperation) -> Result<bool> {
        let db = self.login_db()?;
        bounded_db(async {
            let client = db.client().await?;
            let row = client.query_opt("UPDATE central_login_operations SET expires_at=clock_timestamp()+interval '30 seconds' WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND phase IN ('starting','pending','candidate','verifying') AND expires_at>clock_timestamp() AND (deadline>clock_timestamp() OR phase IN ('candidate','verifying')) AND EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$3 AND deleted_at IS NULL AND expires_at>clock_timestamp()) AND (account_id IS NULL OR EXISTS(SELECT 1 FROM central_accounts a WHERE a.account_id=central_login_operations.account_id AND a.user_id=$1 AND a.deleted_at IS NULL)) RETURNING cancel_requested", &[&op.user,&op.id,&op.holder,&op.epoch]).await?;
            if row.is_none() && let Some(account) = op.account_id.as_ref() {
                let state = client.query_opt(
                    "SELECT deleted_at IS NOT NULL FROM central_accounts WHERE account_id=$1 AND user_id=$2",
                    &[account, &op.user],
                ).await?;
                if state.is_some_and(|row| row.get::<_,bool>(0)) { bail!(super::AccountRemoved); }
            }
            let canceled: bool = row.context("login lease lost")?.get(0);
            let session_ended = matches!(op.authority, LoginAuthority::Browser(_))
                && !authority_live(&*client, &op.user, &op.authority).await?;
            Ok(canceled || session_ended)
        }).await
    }
    pub(in crate::central) async fn login_save(
        &self,
        op: &mut LoginOperation,
        phase: LoginPhase,
    ) -> Result<()> {
        if matches!(
            op.phase,
            LoginPhase::Completed
                | LoginPhase::Failed
                | LoginPhase::Canceled
                | LoginPhase::Rejected
                | LoginPhase::ReplicaLost
        ) {
            bail!("terminal login receipt is immutable");
        }
        // Losing publication of an unresolved marker must not make the worker
        // classify the same attempt as a stopped, repairable candidate.
        if phase == LoginPhase::Unresolved {
            op.phase = phase;
        }
        let db = self.login_db()?;
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&op.payload)?)?;
        let workspace = op
            .payload
            .candidate
            .as_ref()
            .map(vault::account)
            .transpose()?;
        let login = op
            .payload
            .candidate
            .as_ref()
            .and_then(|auth| vault::token(auth).ok())
            .and_then(crate::api::token_subject);
        let candidate_logins = op
            .payload
            .candidate
            .as_ref()
            .and_then(|auth| vault::token(auth).ok())
            .map(crate::api::token_logins)
            .unwrap_or_default();
        let claims = op
            .payload
            .candidate
            .as_ref()
            .map(identity::Claims::from_auth)
            .transpose()?
            .map(|c| c.json())
            .unwrap_or_else(|| serde_json::json!({}))
            .to_string();
        bounded_db(async {
            let mut connection=db.admission_client().await?;
            let client=connection.transaction().await?;
            let _admission_timing = identity::lock_admission(&client).await?;
            let changed = client.execute("UPDATE central_login_operations SET phase=$6, encrypted_payload=$7, sequence=sequence+1, candidate_workspace=$8, candidate_login=$9,candidate_claims=$10::text::jsonb,candidate_uid=$11,candidate_sub=$12 WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase NOT IN ('completed','failed','canceled','rejected','replica_lost') AND expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$3 AND deleted_at IS NULL AND expires_at>clock_timestamp())", &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&phase.as_str(),&encrypted,&workspace,&login,&claims,&candidate_logins.uid,&candidate_logins.sub]).await?;
            if changed != 1 { bail!("login transition fenced"); }
            if let Some(result) = terminal_result(phase) {
                audit_login(&client, op, "terminal", result).await?;
            }
            client.commit().await?;
            Ok(())
        }).await?;
        op.sequence += 1;
        op.phase = phase;
        if let Some(result) = terminal_result(phase) {
            log_login_audit(op, "terminal", result);
        }
        Ok(())
    }
}

impl CentralStore {
    /// `refresh_holder` is this replica's refresh identity. The worker settles
    /// that replica's own refresh child, so it may replace its lease.
    pub(in crate::central) async fn login_account_lease(
        &self,
        op: &LoginOperation,
        refresh_holder: &str,
    ) -> Result<Option<Lease>> {
        let db = self.login_db()?;
        // Different from the replica's normal refresh holder: no reentrant lease.
        let holder = format!("{}:{}", op.holder, op.id);
        bounded_db(db.acquire_login_lease(
            op.account()?,
            &holder,
            Duration::from_secs(120),
            Some((op, refresh_holder)),
        ))
        .await
    }
    /// Bind repair to the rejected evidence visible before refresh-capable work.
    /// A later candidate is a new quarantine even when its identity matches.
    pub(in crate::central) async fn login_begin_verification(
        &self,
        op: &mut LoginOperation,
        lease: &Lease,
    ) -> Result<()> {
        let db = self.login_db()?;
        bounded_db(async {
            let changed = db.client().await?.execute(
                "WITH fence AS MATERIALIZED (SELECT account_id FROM account_refresh_leases WHERE account_id=$1 AND holder_id=$2 AND epoch=$3 AND expires_at>clock_timestamp() FOR UPDATE) UPDATE central_login_operations SET phase='verifying',sequence=sequence+1,repair_evidence=(SELECT COALESCE(jsonb_agg(jsonb_build_array(r.user_id,r.id,r.sequence)), '[]'::jsonb) FROM central_login_operations r JOIN central_accounts a ON a.account_id=$1 WHERE r.phase='rejected' AND (r.account_id=$1 OR central_login_identity_agrees(r.candidate_workspace,r.candidate_uid,r.candidate_sub,$1))) WHERE user_id=$4 AND id=$5 AND holder_id=$6 AND epoch=$7 AND sequence=$8 AND phase='candidate' AND NOT cancel_requested AND expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$6 AND deleted_at IS NULL AND expires_at>clock_timestamp()) AND account_id IN (SELECT account_id FROM fence)",
                &[&lease.account_id,&lease.holder_id,&lease.epoch,&op.user,&op.id,&op.holder,&op.epoch,&op.sequence]).await?;
            if changed != 1 { bail!("login verification fenced"); }
            Ok(())
        }).await?;
        op.phase = LoginPhase::Verifying;
        op.sequence += 1;
        Ok(())
    }
    /// Publish the verified vault and terminal receipt in one statement, under both fences.
    pub(in crate::central) async fn login_complete(
        &self,
        op: &mut LoginOperation,
        lease: &Lease,
        record: &CredentialRecord,
        previous: i64,
    ) -> Result<()> {
        if record.account_id.as_str() != op.account()?
            || lease.account_id.as_str() != op.account()?
            || record.revision <= previous
        {
            bail!("login credential revision mismatch");
        }
        let db = self.login_db()?;
        let vault = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&record.vault)?)?;
        let payload = vault::encrypt_bytes(
            &db.key,
            &serde_json::to_vec(&LoginPayload {
                dashboard_actor: op.payload.dashboard_actor.clone(),
                ..Default::default()
            })?,
        )?;
        let claims = identity::Claims::from_record(record)?;
        let audited = bounded_db(async {
            let mut connection = db.admission_client().await?;
            let client = connection.transaction().await?;
            let _admission_timing = identity::lock_admission(&client).await?;
            if !authority_live(&client, &op.user, &op.authority).await? { bail!("login authority ended"); }
            // Match refresh claims: lock the account before its lease.
            let row = client.query_one(
                "WITH active_account AS MATERIALIZED (SELECT account_id FROM central_accounts WHERE account_id=$1 AND deleted_at IS NULL FOR UPDATE), fence AS MATERIALIZED (SELECT account_id FROM account_refresh_leases WHERE account_id IN (SELECT account_id FROM active_account) AND holder_id=$2 AND epoch=$3 AND expires_at>clock_timestamp() FOR UPDATE), operation AS MATERIALIZED (SELECT user_id,id,repair_evidence FROM central_login_operations WHERE user_id=$4 AND id=$5 AND holder_id=$6 AND epoch=$7 AND sequence=$8 AND phase='verifying' AND NOT cancel_requested AND expires_at>clock_timestamp() AND EXISTS(SELECT 1 FROM central_login_holders WHERE holder_id=$6 AND deleted_at IS NULL AND expires_at>clock_timestamp()) AND account_id IN (SELECT account_id FROM fence) FOR UPDATE), credential AS (UPDATE central_accounts SET encrypted_vault=$9,revision=$10,workspace=$13,login=$14,updated_at=clock_timestamp() WHERE account_id=$1 AND revision=$11 AND deleted_at IS NULL AND EXISTS (SELECT 1 FROM operation) RETURNING account_id), receipt AS (UPDATE central_login_operations SET phase='completed',encrypted_payload=$12,sequence=sequence+1,completed_revision=$10 WHERE user_id=$4 AND id=$5 AND EXISTS (SELECT 1 FROM credential) RETURNING id), repair AS (UPDATE central_login_operations SET selected_reserved=CASE WHEN account_id=$1 THEN false ELSE selected_reserved END, candidate_workspace=CASE WHEN central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1) THEN NULL ELSE candidate_workspace END, candidate_login=CASE WHEN central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1) THEN NULL ELSE candidate_login END,candidate_claims=CASE WHEN central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1) THEN '{}'::jsonb ELSE candidate_claims END,candidate_uid=CASE WHEN central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1) THEN NULL ELSE candidate_uid END,candidate_sub=CASE WHEN central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1) THEN NULL ELSE candidate_sub END WHERE phase='rejected' AND EXISTS (SELECT 1 FROM receipt) AND (account_id=$1 OR (central_login_identity_agrees(candidate_workspace,candidate_uid,candidate_sub,$1))) AND EXISTS (SELECT 1 FROM operation WHERE operation.repair_evidence @> jsonb_build_array(jsonb_build_array(central_login_operations.user_id,central_login_operations.id,central_login_operations.sequence))) RETURNING user_id,id), released_claims AS (UPDATE central_login_identity_reservations SET deleted_at=clock_timestamp() WHERE deleted_at IS NULL AND EXISTS (SELECT 1 FROM receipt) AND ((user_id=$4 AND id=$5) OR (user_id,id) IN (SELECT user_id,id FROM repair))), released_history AS (UPDATE central_login_identity_reservation_history SET deleted_at=clock_timestamp() WHERE deleted_at IS NULL AND EXISTS (SELECT 1 FROM receipt) AND ((user_id=$4 AND id=$5) OR (user_id,id) IN (SELECT user_id,id FROM repair))) SELECT count(*) FROM receipt",
                &[&lease.account_id,&lease.holder_id,&lease.epoch,&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&vault,&record.revision,&previous,&payload,&record.workspace,&record.login]).await?;
            if row.get::<_,i64>(0)!=1 { bail!("login completion fenced"); }
            let audited = audit_login(&client, op, "terminal", "ok:completed").await?;
            if let Some(claims)=claims.as_ref() {identity::record_claims(&client,op.account()?,claims).await?;}
            // Row locks serialize sign-out and disablement, but time can pass
            // while credential publication waits on another database lock.
            if matches!(op.authority, LoginAuthority::Browser(_))
                && !authority_live(&client, &op.user, &op.authority).await?
            { bail!("login authority expired during publication"); }
            client.commit().await?;
            Ok(audited)
        }).await?;
        if audited {
            log_login_audit(op, "terminal", "ok:completed");
        }
        op.phase = LoginPhase::Completed;
        op.sequence += 1;
        op.payload = LoginPayload {
            dashboard_actor: op.payload.dashboard_actor.take(),
            landed: op.payload.landed.take(),
            ..Default::default()
        };
        Ok(())
    }
}

#[cfg(all(test, feature = "central-real-db-tests"))]
mod tests {
    use super::*;
    use crate::central::storage::test_support::bootstrap;

    #[tokio::test]
    async fn failed_unresolved_transition_preserves_its_intended_phase() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[9; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (shared, control, schema) = shared.isolated_test_schema().await.unwrap();
        bootstrap(&shared).await.unwrap();
        control.batch_execute("INSERT INTO central_users(id,email,enabled) VALUES('user','synthetic@example.invalid',true); INSERT INTO central_devices(id,tenant,user_id,token_hash,revoked) VALUES('machine','sawmills','user','synthetic-machine-digest',false)").await.unwrap();
        shared
            .save_account(&CredentialRecord {
                account_id: "seat".into(),
                user_id: Some("user".into()),
                alias: "seat".into(),
                workspace: Some("workspace".into()),
                login: None,
                vault: serde_json::json!({}),
                revision: 1,
            })
            .await
            .unwrap();
        let mut op = LoginOperation {
            kind: LoginKind::Renewal,
            user: "user".into(),
            id: "a".repeat(64),
            account_id: Some("seat".into()),
            alias: "seat".into(),
            authority: LoginAuthority::Machine("machine".into()),
            phase: LoginPhase::Starting,
            sequence: 0,
            holder: "replica".into(),
            epoch: 1,
            payload: LoginPayload::default(),
            polling_clear: false,
            lease_expired: false,
            failure_reported: false,
        };
        shared.login_register_holder(&op.holder).await.unwrap();
        assert!(shared.login_create(&op).await.unwrap());
        op.payload.candidate = Some(
            serde_json::json!({"tokens":{"account_id":"workspace","access_token":"synthetic","refresh_token":"synthetic"}}),
        );
        shared
            .login_save(&mut op, LoginPhase::Candidate)
            .await
            .unwrap();
        control.batch_execute("CREATE FUNCTION reject_unresolved() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='unresolved' THEN RAISE EXCEPTION 'synthetic journal failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_unresolved BEFORE UPDATE ON central_login_operations FOR EACH ROW EXECUTE FUNCTION reject_unresolved();").await.unwrap();
        assert!(
            shared
                .login_save(&mut op, LoginPhase::Unresolved)
                .await
                .is_err()
        );
        assert_eq!(
            op.phase,
            LoginPhase::Unresolved,
            "failure classification must preserve unresolved intent"
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_holder_cannot_publish_a_candidate_with_a_live_operation_lease() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[9; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (shared, control, schema) = shared.isolated_test_schema().await.unwrap();
        bootstrap(&shared).await.unwrap();
        control.batch_execute("INSERT INTO central_users(id,email,enabled) VALUES('user','synthetic@example.invalid',true); INSERT INTO central_devices(id,tenant,user_id,token_hash,revoked) VALUES('machine','sawmills','user','synthetic-machine-digest',false)").await.unwrap();
        shared
            .save_account(&CredentialRecord {
                account_id: "seat".into(),
                user_id: Some("user".into()),
                alias: "seat".into(),
                workspace: Some("workspace".into()),
                login: None,
                vault: serde_json::json!({}),
                revision: 1,
            })
            .await
            .unwrap();
        let mut op = LoginOperation {
            kind: LoginKind::Renewal,
            user: "user".into(),
            id: "a".repeat(64),
            account_id: Some("seat".into()),
            alias: "seat".into(),
            authority: LoginAuthority::Machine("machine".into()),
            phase: LoginPhase::Starting,
            sequence: 0,
            holder: "replica".into(),
            epoch: 1,
            payload: LoginPayload::default(),
            polling_clear: false,
            lease_expired: false,
            failure_reported: false,
        };
        shared.login_register_holder(&op.holder).await.unwrap();
        assert!(shared.login_create(&op).await.unwrap());
        shared.login_release_holder(&op.holder).await.unwrap();
        op.payload.candidate = Some(
            serde_json::json!({"tokens":{"account_id":"workspace","access_token":"synthetic","refresh_token":"synthetic"}}),
        );
        assert!(
            shared
                .login_save(&mut op, LoginPhase::Candidate)
                .await
                .is_err(),
            "holder expiry must fence publication before the operation lease expires"
        );
        assert!(shared.login_heartbeat(&op).await.is_err());
        assert!(
            !shared.login_renew_holder(&op.holder).await.unwrap(),
            "an expired holder cannot resurrect"
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn candidate_takeover_fences_the_previous_holder_epoch() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[9; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (shared, control, schema) = shared.isolated_test_schema().await.unwrap();
        bootstrap(&shared).await.unwrap();
        control.batch_execute("INSERT INTO central_users(id,email,enabled) VALUES('user','synthetic@example.invalid',true); INSERT INTO central_devices(id,tenant,user_id,token_hash,revoked) VALUES('machine','sawmills','user','synthetic-machine-digest',false)").await.unwrap();
        shared
            .save_account(&CredentialRecord {
                account_id: "seat".into(),
                user_id: Some("user".into()),
                alias: "seat".into(),
                workspace: Some("workspace".into()),
                login: None,
                vault: serde_json::json!({}),
                revision: 1,
            })
            .await
            .unwrap();
        let mut op = LoginOperation {
            kind: LoginKind::Renewal,
            user: "user".into(),
            id: "a".repeat(64),
            account_id: Some("seat".into()),
            alias: "seat".into(),
            authority: LoginAuthority::Machine("machine".into()),
            phase: LoginPhase::Starting,
            sequence: 0,
            holder: "replica".into(),
            epoch: 1,
            payload: LoginPayload::default(),
            polling_clear: false,
            lease_expired: false,
            failure_reported: false,
        };
        shared.login_register_holder(&op.holder).await.unwrap();
        assert!(shared.login_create(&op).await.unwrap());
        op.payload.candidate = Some(
            serde_json::json!({"tokens":{"account_id":"workspace","access_token":"synthetic","refresh_token":"synthetic"}}),
        );
        shared
            .login_save(&mut op, LoginPhase::Candidate)
            .await
            .unwrap();
        let mut stale = op.clone();
        shared.login_register_holder("replacement").await.unwrap();
        shared.login_release_holder(&op.holder).await.unwrap();
        control.execute("UPDATE central_login_operations SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", &[&op.id]).await.unwrap();
        assert!(
            shared
                .login_takeover_candidate(&mut op, "replacement")
                .await
                .unwrap()
        );
        assert_eq!(op.epoch, stale.epoch + 1);
        assert!(
            shared
                .login_save(&mut stale, LoginPhase::Rejected)
                .await
                .is_err()
        );
        assert!(shared.login_heartbeat(&stale).await.is_err());
        assert!(
            shared
                .login_account_lease(&stale, &stale.holder)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!shared.login_heartbeat(&op).await.unwrap());
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn grantless_failure_claim_requires_durable_grant_absence() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[9; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (shared, control, schema) = shared.isolated_test_schema().await.unwrap();
        bootstrap(&shared).await.unwrap();
        control.batch_execute("INSERT INTO central_users(id,email,enabled) VALUES('user','synthetic@example.invalid',true); INSERT INTO central_devices(id,tenant,user_id,token_hash,revoked) VALUES('machine','sawmills','user','synthetic-machine-digest',false)").await.unwrap();
        shared
            .save_account(&CredentialRecord {
                account_id: "seat".into(),
                user_id: Some("user".into()),
                alias: "seat".into(),
                workspace: Some("workspace".into()),
                login: None,
                vault: serde_json::json!({}),
                revision: 1,
            })
            .await
            .unwrap();
        let new_op = |id: &str, alias: &str| LoginOperation {
            kind: LoginKind::Add,
            user: "user".into(),
            id: id.repeat(64),
            account_id: None,
            alias: alias.into(),
            authority: LoginAuthority::Machine("machine".into()),
            phase: LoginPhase::Starting,
            sequence: 0,
            holder: "replica".into(),
            epoch: 1,
            payload: LoginPayload::default(),
            polling_clear: false,
            lease_expired: false,
            failure_reported: false,
        };
        shared.login_register_holder("replica").await.unwrap();
        let mut polling = new_op("a", "first");
        assert!(shared.login_create(&polling).await.unwrap());
        assert!(
            !shared
                .login_take_grantless_failure(&mut polling, "replica")
                .await
                .unwrap(),
            "polling without proven absence may still publish a grant"
        );
        control
            .execute(
                &format!(
                    "UPDATE {schema}.central_login_operations SET polling_clear=true WHERE id=$1"
                ),
                &[&polling.id],
            )
            .await
            .unwrap();
        assert!(
            shared
                .login_take_grantless_failure(&mut polling, "replica")
                .await
                .unwrap()
        );
        // An expired polling receipt may still settle a late grant.
        let mut lost = new_op("c", "third");
        assert!(shared.login_create(&lost).await.unwrap());
        control
            .execute(
                &format!(
                    "UPDATE {schema}.central_login_operations SET phase='replica_lost' WHERE id=$1"
                ),
                &[&lost.id],
            )
            .await
            .unwrap();
        assert!(
            !shared
                .login_take_grantless_failure(&mut lost, "replica")
                .await
                .unwrap(),
            "lost polling without proven absence may still settle a grant"
        );
        let mut candidate = new_op("b", "second");
        assert!(shared.login_create(&candidate).await.unwrap());
        candidate.payload.candidate = Some(
            serde_json::json!({"tokens":{"account_id":"workspace","access_token":"synthetic","refresh_token":"synthetic"}}),
        );
        shared
            .login_save(&mut candidate, LoginPhase::Candidate)
            .await
            .unwrap();
        // The worker may not have seen the committed grant.
        candidate.payload.candidate = None;
        assert!(
            !shared
                .login_take_grantless_failure(&mut candidate, "replica")
                .await
                .unwrap(),
            "a committed candidate may still complete"
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn legacy_handoff_cannot_release_a_new_refresh_owner() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[10; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (shared, control, schema) = shared.isolated_test_schema().await.unwrap();
        bootstrap(&shared).await.unwrap();
        shared
            .save_account(&CredentialRecord {
                account_id: "seat".into(),
                user_id: Some("user".into()),
                alias: "seat".into(),
                workspace: Some("workspace".into()),
                login: None,
                vault: serde_json::json!({}),
                revision: 1,
            })
            .await
            .unwrap();
        control.batch_execute("ALTER TABLE account_refresh_leases DROP COLUMN released, DROP COLUMN legacy_handoff; INSERT INTO account_refresh_leases(account_id,holder_id,epoch,expires_at) VALUES('seat','legacy',41,clock_timestamp()-interval '1 second');").await.unwrap();
        bootstrap(&shared).await.unwrap();
        bootstrap(&shared).await.unwrap();
        let lease = shared
            .acquire_lease("seat", "new-owner", Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(shared.confirm_legacy_owners_settled().await.unwrap(), 0);
        assert!(shared.renew(&lease, Duration::from_secs(60)).await.unwrap());
        assert!(
            shared
                .acquire_lease("seat", "foreign", Duration::from_secs(60))
                .await
                .is_err()
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
