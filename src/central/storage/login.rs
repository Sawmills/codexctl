//! Shared login journal. Device polling has its own lease, separate from refresh.
use super::*;

pub(in crate::central) const LOGIN_SCHEMA: &str = r#"
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
CREATE UNIQUE INDEX IF NOT EXISTS central_login_active_account
    ON central_login_operations(account_id)
    WHERE phase NOT IN ('completed','failed','canceled','rejected','replica_lost');
CREATE INDEX IF NOT EXISTS central_login_completed_revision
    ON central_login_operations(account_id, completed_revision) WHERE phase='completed';
INSERT INTO central_schema_migrations(version) VALUES (2) ON CONFLICT DO NOTHING;
"#;

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
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(in crate::central) struct LoginPayload {
    pub code: Option<String>,
    pub candidate: Option<Value>,
    pub error: Option<String>,
}
#[derive(Clone)]
pub(in crate::central) struct LoginOperation {
    pub user: String,
    pub id: String,
    pub account_id: String,
    pub alias: String,
    pub device: String,
    pub phase: LoginPhase,
    pub sequence: i64,
    pub holder: String,
    pub epoch: i64,
    pub payload: LoginPayload,
}
impl PostgresStore {
    fn login_row(&self, row: tokio_postgres::Row) -> Result<LoginOperation> {
        let phase: String = row.get("phase");
        let encrypted: Vec<u8> = row.get("encrypted_payload");
        Ok(LoginOperation {
            user: row.get("user_id"),
            id: row.get("id"),
            account_id: row.get("account_id"),
            alias: row.get("alias"),
            device: row.get("device_id"),
            phase: serde_json::from_value(Value::String(phase))?,
            sequence: row.get("sequence"),
            holder: row.get("holder_id"),
            epoch: row.get("epoch"),
            payload: serde_json::from_slice(&vault::decrypt_bytes(&self.key, &encrypted)?)?,
        })
    }
}
impl CentralStore {
    fn login_db(&self) -> Result<&PostgresStore> {
        match self {
            Self::Postgres(db) => Ok(db),
            _ => bail!("shared login requires PostgreSQL"),
        }
    }
    pub(in crate::central) async fn login_get(
        &self,
        user: &str,
        id: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client()
                .await?
                .query_opt(
                    "SELECT * FROM central_login_operations WHERE user_id=$1 AND id=$2",
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
    pub(in crate::central) async fn login_active(
        &self,
        account: &str,
    ) -> Result<Option<LoginOperation>> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client().await?.query_opt("SELECT * FROM central_login_operations WHERE account_id=$1 AND phase NOT IN ('completed','failed','canceled','rejected','replica_lost')", &[&account]).await?
                .map(|row| db.login_row(row)).transpose()
        }).await
    }
    pub(in crate::central) async fn login_create(&self, op: &LoginOperation) -> Result<bool> {
        let db = self.login_db()?;
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&op.payload)?)?;
        bounded_db(async {
            Ok(db.client().await?.execute("INSERT INTO central_login_operations(user_id,id,account_id,alias,device_id,phase,holder_id,expires_at,encrypted_payload) VALUES($1,$2,$3,$4,$5,'starting',$6,clock_timestamp()+interval '30 seconds',$7) ON CONFLICT DO NOTHING",
                &[&op.user,&op.id,&op.account_id,&op.alias,&op.device,&op.holder,&encrypted]).await? == 1)
        }).await
    }
    pub(in crate::central) async fn login_cancel(&self, op: &LoginOperation) -> Result<()> {
        let db = self.login_db()?;
        bounded_db(async {
            db.client().await?.execute("UPDATE central_login_operations SET cancel_requested=true WHERE user_id=$1 AND id=$2 AND device_id=$3 AND phase IN ('starting','pending','candidate','verifying')", &[&op.user,&op.id,&op.device]).await?;
            Ok(())
        }).await
    }
    /// A failed read or renewal is loss of authority. The child must stop.
    pub(in crate::central) async fn login_heartbeat(&self, op: &LoginOperation) -> Result<bool> {
        let db = self.login_db()?;
        bounded_db(async {
            let row = db.client().await?.query_opt("UPDATE central_login_operations SET expires_at=clock_timestamp()+interval '30 seconds' WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND expires_at>clock_timestamp() AND deadline>clock_timestamp() RETURNING cancel_requested", &[&op.user,&op.id,&op.holder,&op.epoch]).await?;
            Ok(row.context("login lease lost")?.get(0))
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
        ) {
            bail!("terminal login receipt is immutable");
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
        bounded_db(async {
            let changed = db.client().await?.execute("UPDATE central_login_operations SET phase=$6, encrypted_payload=$7, sequence=sequence+1, candidate_workspace=$8, candidate_login=$9 WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase NOT IN ('completed','failed','canceled','rejected') AND expires_at>clock_timestamp()", &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&phase.as_str(),&encrypted,&workspace,&login]).await?;
            if changed != 1 { bail!("login transition fenced"); }
            Ok(())
        }).await?;
        op.sequence += 1;
        op.phase = phase;
        Ok(())
    }
}

impl CentralStore {
    pub(in crate::central) async fn login_account_lease(
        &self,
        op: &LoginOperation,
    ) -> Result<Option<Lease>> {
        let db = self.login_db()?;
        // Different from the replica's normal refresh holder: no reentrant lease.
        let holder = format!("{}:{}", op.holder, op.id);
        bounded_db(db.acquire_login_lease(
            &op.account_id,
            &holder,
            Duration::from_secs(120),
            Some(&op.id),
            Some(&op.holder),
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
                "WITH fence AS MATERIALIZED (SELECT account_id FROM account_refresh_leases WHERE account_id=$1 AND holder_id=$2 AND epoch=$3 AND expires_at>clock_timestamp() FOR UPDATE) UPDATE central_login_operations SET phase='verifying',sequence=sequence+1,repair_evidence=(SELECT COALESCE(jsonb_agg(jsonb_build_array(r.user_id,r.id,r.sequence)), '[]'::jsonb) FROM central_login_operations r JOIN central_accounts a ON a.account_id=$1 WHERE r.phase='rejected' AND (r.account_id=$1 OR (r.candidate_workspace=a.workspace AND r.candidate_login=a.login))) WHERE user_id=$4 AND id=$5 AND holder_id=$6 AND epoch=$7 AND sequence=$8 AND phase='candidate' AND NOT cancel_requested AND expires_at>clock_timestamp() AND account_id IN (SELECT account_id FROM fence)",
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
        if record.account_id != op.account_id
            || lease.account_id != op.account_id
            || record.revision <= previous
        {
            bail!("login credential revision mismatch");
        }
        let db = self.login_db()?;
        let vault = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&record.vault)?)?;
        let payload =
            vault::encrypt_bytes(&db.key, &serde_json::to_vec(&LoginPayload::default())?)?;
        bounded_db(async {
            let row = db.client().await?.query_one(
                "WITH fence AS MATERIALIZED (SELECT account_id FROM account_refresh_leases WHERE account_id=$1 AND holder_id=$2 AND epoch=$3 AND expires_at>clock_timestamp() FOR UPDATE), operation AS MATERIALIZED (SELECT user_id,id,repair_evidence FROM central_login_operations WHERE user_id=$4 AND id=$5 AND holder_id=$6 AND epoch=$7 AND sequence=$8 AND phase='verifying' AND NOT cancel_requested AND expires_at>clock_timestamp() AND account_id IN (SELECT account_id FROM fence) FOR UPDATE), credential AS (UPDATE central_accounts SET encrypted_vault=$9,revision=$10,updated_at=clock_timestamp() WHERE account_id=$1 AND revision=$11 AND deleted_at IS NULL AND EXISTS (SELECT 1 FROM operation) RETURNING account_id), receipt AS (UPDATE central_login_operations SET phase='completed',encrypted_payload=$12,sequence=sequence+1,completed_revision=$10 WHERE user_id=$4 AND id=$5 AND EXISTS (SELECT 1 FROM credential) RETURNING id), repair AS (UPDATE central_login_operations SET selected_reserved=CASE WHEN account_id=$1 THEN false ELSE selected_reserved END, candidate_workspace=CASE WHEN candidate_workspace=$13 AND candidate_login=$14 THEN NULL ELSE candidate_workspace END, candidate_login=CASE WHEN candidate_workspace=$13 AND candidate_login=$14 THEN NULL ELSE candidate_login END WHERE phase='rejected' AND EXISTS (SELECT 1 FROM receipt) AND (account_id=$1 OR (candidate_workspace=$13 AND candidate_login=$14)) AND EXISTS (SELECT 1 FROM operation WHERE operation.repair_evidence @> jsonb_build_array(jsonb_build_array(central_login_operations.user_id,central_login_operations.id,central_login_operations.sequence)))) SELECT count(*) FROM receipt",
                &[&lease.account_id,&lease.holder_id,&lease.epoch,&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&vault,&record.revision,&previous,&payload,&record.workspace,&record.login]).await?;
            if row.get::<_,i64>(0)!=1 { bail!("login completion fenced"); }
            Ok(())
        }).await?;
        op.phase = LoginPhase::Completed;
        op.sequence += 1;
        op.payload = LoginPayload::default();
        Ok(())
    }
}
