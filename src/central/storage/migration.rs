//! Restartable identity migration. Writers stay drained until the final marker.
use super::*;
use tokio_postgres::Row;

pub(super) const READY_VERSION: i32 = 6;
const BATCH_ROWS: usize = 32;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const INVENTORY_ROWS: i64 = 5_000;
const MAINTENANCE_TIME: Duration = Duration::from_secs(300);

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS central_identity_migration_progress (
    stage TEXT PRIMARY KEY CHECK(stage IN ('source_fence','claims','accounts','operations','reservations')),
    cursor_account TEXT, cursor_user TEXT, cursor_id TEXT,
    cursor_workspace TEXT, cursor_namespace TEXT, cursor_claim TEXT,
    end_account TEXT, end_user TEXT, end_id TEXT,
    end_workspace TEXT, end_namespace TEXT, end_claim TEXT,
    revision BIGINT NOT NULL DEFAULT 0,
    records BIGINT NOT NULL DEFAULT 0,
    completed BOOLEAN NOT NULL DEFAULT false,
    deleted_at TIMESTAMPTZ
);
INSERT INTO central_identity_migration_progress(stage) VALUES('source_fence') ON CONFLICT DO NOTHING;
CREATE OR REPLACE FUNCTION central_identity_require_drained_writers() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF current_setting('codexctl.identity_migration',true) IS DISTINCT FROM '1'
       AND EXISTS(SELECT 1 FROM central_identity_migration_progress WHERE stage='source_fence' AND deleted_at IS NULL)
       AND EXISTS(SELECT 1 FROM central_identity_migration_progress WHERE stage='source_fence' AND NOT completed AND deleted_at IS NULL) THEN
        RAISE EXCEPTION 'identity migration requires drained writers';
    END IF;
    RETURN CASE WHEN TG_OP='DELETE' THEN OLD ELSE NEW END;
END $$;
DO $$ DECLARE relation TEXT; BEGIN
    FOREACH relation IN ARRAY ARRAY['central_accounts','account_refresh_leases','central_login_operations','central_account_claims','central_login_identity_reservations','central_account_identity_claims'] LOOP
        IF NOT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=relation::regclass AND tgname='central_identity_migration_fence') THEN
            EXECUTE format('CREATE TRIGGER central_identity_migration_fence BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION central_identity_require_drained_writers()',relation);
        END IF;
    END LOOP;
END $$;
"#;

#[derive(Clone, Copy)]
enum Stage {
    Claims,
    Accounts,
    Operations,
    Reservations,
}
impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Claims => "claims",
            Self::Accounts => "accounts",
            Self::Operations => "operations",
            Self::Reservations => "reservations",
        }
    }
    fn source(self) -> &'static str {
        match self {
            Self::Claims => "central_account_claims",
            Self::Accounts => "central_accounts",
            Self::Operations => "central_login_operations",
            Self::Reservations => "central_login_identity_reservations",
        }
    }
    fn order(self) -> &'static str {
        match self {
            Self::Accounts => "account_id",
            Self::Operations => "user_id,id",
            _ => "workspace,namespace,claim",
        }
    }
    fn live(self) -> &'static str {
        if matches!(self, Self::Accounts) {
            "deleted_at IS NULL AND "
        } else {
            ""
        }
    }
    fn page_sql(self, full: bool) -> String {
        let columns = match (self, full) {
            (Self::Accounts, false) => {
                "account_id,octet_length(encrypted_vault)::bigint AS payload_bytes"
            }
            (Self::Accounts, true) => {
                "account_id,user_id,alias,workspace,login,encrypted_vault,revision"
            }
            (Self::Operations, false) => {
                "user_id,id,(octet_length(encrypted_payload)+octet_length(candidate_claims::text)+octet_length(repair_evidence::text))::bigint AS payload_bytes"
            }
            (Self::Operations, true) => {
                "user_id,id,encrypted_payload,candidate_workspace,candidate_login,candidate_claims::text AS candidate_claims"
            }
            (Self::Claims, _) => {
                "workspace,namespace,claim,account_id,deleted_at,(octet_length(workspace)+octet_length(namespace)+octet_length(claim))::bigint AS payload_bytes"
            }
            (Self::Reservations, _) => {
                "workspace,namespace,claim,user_id,id,deleted_at,(octet_length(workspace)+octet_length(namespace)+octet_length(claim))::bigint AS payload_bytes"
            }
        };
        let range = match self {
            Self::Accounts => "(c1 IS NULL OR account_id>c1) AND account_id<=e1",
            Self::Operations => "(c1 IS NULL OR (user_id,id)>(c1,c2)) AND (user_id,id)<=(e1,e2)",
            _ => {
                "(c1 IS NULL OR (workspace,namespace,claim)>(c1,c2,c3)) AND (workspace,namespace,claim)<=(e1,e2,e3)"
            }
        };
        format!(
            "WITH bounds AS (SELECT $1::text c1,$2::text c2,$3::text c3,$4::text e1,$5::text e2,$6::text e3) SELECT {columns} FROM {},bounds WHERE {}{range} ORDER BY {} LIMIT {BATCH_ROWS}",
            self.source(),
            self.live(),
            self.order()
        )
    }
}

#[derive(Clone)]
enum Key {
    Account(String),
    Operation(String, String),
    Claim(String, String, String),
}
impl Key {
    fn source(stage: Stage, row: &Row) -> Self {
        match stage {
            Stage::Accounts => Self::Account(row.get("account_id")),
            Stage::Operations => Self::Operation(row.get("user_id"), row.get("id")),
            _ => Self::Claim(row.get("workspace"), row.get("namespace"), row.get("claim")),
        }
    }
    fn progress(stage: Stage, row: &Row, end: bool) -> Option<Self> {
        let prefix = if end { "end" } else { "cursor" };
        let get = |field: &str| row.get::<_, Option<String>>(format!("{prefix}_{field}").as_str());
        match stage {
            Stage::Accounts => get("account").map(Self::Account),
            Stage::Operations => Some(Self::Operation(get("user")?, get("id")?)),
            _ => Some(Self::Claim(
                get("workspace")?,
                get("namespace")?,
                get("claim")?,
            )),
        }
    }
    fn range(key: Option<&Self>) -> [Option<&str>; 3] {
        match key {
            Some(Self::Account(account)) => [Some(account), None, None],
            Some(Self::Operation(user, id)) => [Some(user), Some(id), None],
            Some(Self::Claim(workspace, namespace, claim)) => {
                [Some(workspace), Some(namespace), Some(claim)]
            }
            None => [None, None, None],
        }
    }
    fn columns(key: Option<&Self>) -> [Option<&str>; 6] {
        match key {
            Some(Self::Account(account)) => [Some(account), None, None, None, None, None],
            Some(Self::Operation(user, id)) => [None, Some(user), Some(id), None, None, None],
            Some(Self::Claim(workspace, namespace, claim)) => [
                None,
                None,
                None,
                Some(workspace),
                Some(namespace),
                Some(claim),
            ],
            None => [None; 6],
        }
    }
}
struct Progress {
    cursor: Option<Key>,
    end: Option<Key>,
    revision: i64,
    completed: bool,
}
enum Prepared {
    Claim(Row),
    Account(CredentialRecord),
    Operation {
        user: String,
        id: String,
        uid: Option<String>,
        sub: Option<String>,
        claims: String,
    },
    Reservation(Row),
}

impl PostgresStore {
    pub(super) async fn check_schema_version(&self, supported: i32) -> Result<()> {
        bounded_db(async {
            let client = self.client().await?;
            let present: bool = client
                .query_one(
                    "SELECT to_regclass('central_schema_migrations') IS NOT NULL",
                    &[],
                )
                .await?
                .get(0);
            if present {
                let version: Option<i32> = client
                    .query_one("SELECT max(version) FROM central_schema_migrations", &[])
                    .await?
                    .get(0);
                if let Some(version) = version.filter(|version| *version > supported) {
                    bail!("schema v{version} is newer than this binary; upgrade codexctl-central");
                }
            }
            Ok(())
        })
        .await
    }
    pub(super) async fn identity_ready(&self) -> Result<bool> {
        bounded_db(async {
            Ok(self
                .client()
                .await?
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM central_identity_migration_progress WHERE stage='source_fence' AND completed AND deleted_at IS NULL) AND EXISTS(SELECT 1 FROM central_schema_migrations WHERE version=$1)",
                    &[&READY_VERSION],
                )
                .await?
                .get(0))
        })
        .await
    }
    pub(super) async fn migrate_identity(&self) -> Result<()> {
        if self.identity_ready().await? {
            return Ok(());
        }
        tokio::time::timeout(MAINTENANCE_TIME, self.identity_batches()).await
            .context("identity migration maintenance budget elapsed; keep writers drained and resume migration")?
    }
    async fn identity_batches(&self) -> Result<()> {
        bounded_db(self.check_inventory_bound()).await?;
        let cipher = vault::cipher(&self.key)?;
        for stage in [
            Stage::Claims,
            Stage::Accounts,
            Stage::Operations,
            Stage::Reservations,
        ] {
            loop {
                let progress = bounded_db(self.migration_progress(stage)).await?;
                if progress.completed {
                    break;
                }
                let rows = bounded_db(self.migration_page(stage, &progress)).await?;
                let next = rows.last().map(|row| Key::source(stage, row));
                let mut prepared = Vec::with_capacity(rows.len());
                let started = std::time::Instant::now();
                for row in rows {
                    prepared.push(match stage {
                        Stage::Claims => Prepared::Claim(row),
                        Stage::Reservations => Prepared::Reservation(row),
                        Stage::Accounts => {
                            let encrypted:Vec<u8>=row.get("encrypted_vault");
                            Prepared::Account(CredentialRecord { account_id:row.get("account_id"),user_id:row.get("user_id"),alias:row.get("alias"),workspace:row.get("workspace"),login:row.get("login"),vault:serde_json::from_slice(&vault::decrypt_with_cipher(&cipher,&encrypted)?)?,revision:row.get("revision") })
                        }
                        Stage::Operations => {
                            let encrypted:Vec<u8>=row.get("encrypted_payload");
                            let payload:login::LoginPayload=serde_json::from_slice(&vault::decrypt_with_cipher(&cipher,&encrypted)?)?;
                            let mut claims:Value=serde_json::from_str(&row.get::<_,String>("candidate_claims"))?;
                            let workspace:Option<String>=row.get("candidate_workspace");
                            if let (Some(workspace),Some(auth))=(workspace,payload.candidate) {
                                if vault::account(&auth)?!=workspace { bail!("legacy candidate identity metadata conflicts with its encrypted grant"); }
                                claims=identity::Claims::from_auth(&auth)?.json();
                            }
                            Prepared::Operation { user:row.get("user_id"),id:row.get("id"),uid:claims.get("uid").and_then(Value::as_str).map(str::to_owned),sub:claims.get("sub").and_then(Value::as_str).map(str::to_owned).or_else(||row.get("candidate_login")),claims:claims.to_string() }
                        }
                    });
                    if started.elapsed() > DB_TIMEOUT {
                        bail!("identity migration decode budget elapsed");
                    }
                }
                bounded_db(self.commit_migration_batch(stage, &progress, next.as_ref(), &prepared))
                    .await?;
            }
        }
        bounded_db(async {
            let mut connection=self.admission_client().await?;
            let tx=connection.transaction().await?;
            let _timing=identity::lock_admission(&tx).await?;
            let complete:i64=tx.query_one("SELECT count(*) FROM central_identity_migration_progress WHERE stage IN ('claims','accounts','operations','reservations') AND completed AND deleted_at IS NULL",&[]).await?.get(0);
            if complete!=4 { bail!("identity migration is incomplete"); }
            tx.execute("UPDATE central_identity_migration_progress SET completed=true WHERE stage='source_fence' AND deleted_at IS NULL",&[]).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }
    async fn check_inventory_bound(&self) -> Result<()> {
        let client = self.client().await?;
        let recorded:i64=client.query_one("SELECT revision FROM central_identity_migration_progress WHERE stage='source_fence' AND deleted_at IS NULL",&[]).await?.get(0);
        if recorded > 0 {
            return Ok(());
        }
        // The source fence is committed. Count at most one row beyond the
        // accepted bound, outside admission, before any identity backfill.
        let mut total = 0i64;
        for stage in [
            Stage::Claims,
            Stage::Accounts,
            Stage::Operations,
            Stage::Reservations,
        ] {
            let sql = format!(
                "SELECT count(*) FROM (SELECT 1 FROM {} WHERE {}true LIMIT {}) bounded_inventory",
                stage.source(),
                stage.live(),
                INVENTORY_ROWS - total + 1
            );
            total += client.query_one(&sql, &[]).await?.get::<_, i64>(0);
            if total > INVENTORY_ROWS {
                break;
            }
        }
        let mut connection = self.admission_client().await?;
        let tx = connection.transaction().await?;
        let _timing = identity::lock_admission(&tx).await?;
        let revision:i64=tx.query_one("SELECT revision FROM central_identity_migration_progress WHERE stage='source_fence' AND deleted_at IS NULL FOR UPDATE",&[]).await?.get(0);
        if revision == 0 {
            if total > INVENTORY_ROWS {
                bail!(
                    "identity migration inventory exceeds 5000 source rows; keep writers drained"
                );
            }
            tx.execute("UPDATE central_identity_migration_progress SET records=$1,revision=1 WHERE stage='source_fence' AND deleted_at IS NULL",&[&total]).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    async fn migration_progress(&self, stage: Stage) -> Result<Progress> {
        let client = self.client().await?;
        let exists=client.query_opt("SELECT * FROM central_identity_migration_progress WHERE stage=$1 AND deleted_at IS NULL",&[&stage.name()]).await?;
        let row = if let Some(row) = exists {
            row
        } else {
            // The source fence is already committed, so this indexed high-water
            // read is stable without holding admission during ciphertext work.
            let sql = format!(
                "SELECT {} FROM {} WHERE {}true ORDER BY {} DESC LIMIT 1",
                stage.order(),
                stage.source(),
                stage.live(),
                stage.order().replace(',', " DESC,")
            );
            let end = client
                .query_opt(&sql, &[])
                .await?
                .map(|row| Key::source(stage, &row));
            let fields = Key::columns(end.as_ref());
            client.query_one("INSERT INTO central_identity_migration_progress(stage,end_account,end_user,end_id,end_workspace,end_namespace,end_claim) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(stage) DO UPDATE SET stage=EXCLUDED.stage RETURNING *",&[&stage.name(),&fields[0],&fields[1],&fields[2],&fields[3],&fields[4],&fields[5]]).await?
        };
        Ok(Progress {
            cursor: Key::progress(stage, &row, false),
            end: Key::progress(stage, &row, true),
            revision: row.get("revision"),
            completed: row.get("completed"),
        })
    }
    async fn migration_page(&self, stage: Stage, progress: &Progress) -> Result<Vec<Row>> {
        let Some(end) = progress.end.as_ref() else {
            return Ok(Vec::new());
        };
        let cursor = Key::range(progress.cursor.as_ref());
        let upper = Key::range(Some(end));
        let client = self.client().await?;
        let rows = client
            .query(
                &stage.page_sql(false),
                &[
                    &cursor[0], &cursor[1], &cursor[2], &upper[0], &upper[1], &upper[2],
                ],
            )
            .await?;
        let mut bytes = 0usize;
        let mut accepted = 0;
        for row in &rows {
            let length: i64 = row.get("payload_bytes");
            let length = usize::try_from(length).context("invalid migration payload size")?;
            if length > BATCH_BYTES {
                bail!("identity migration payload exceeds batch byte budget");
            }
            if bytes + length > BATCH_BYTES {
                break;
            }
            bytes += length;
            accepted += 1;
        }
        if accepted == 0 {
            return Ok(Vec::new());
        }
        if matches!(stage, Stage::Claims | Stage::Reservations) {
            return Ok(rows.into_iter().take(accepted).collect());
        }
        let last = Key::source(stage, &rows[accepted - 1]);
        let upper = Key::range(Some(&last));
        let loaded = client
            .query(
                &stage.page_sql(true),
                &[
                    &cursor[0], &cursor[1], &cursor[2], &upper[0], &upper[1], &upper[2],
                ],
            )
            .await?;
        if loaded.len() != accepted {
            bail!("identity migration source inventory changed");
        }
        Ok(loaded)
    }
    async fn commit_migration_batch(
        &self,
        stage: Stage,
        progress: &Progress,
        next: Option<&Key>,
        rows: &[Prepared],
    ) -> Result<()> {
        let mut connection = self.admission_client().await?;
        let tx = connection.transaction().await?;
        let _timing = identity::lock_admission(&tx).await?;
        let revision:i64=tx.query_one("SELECT revision FROM central_identity_migration_progress WHERE stage=$1 AND deleted_at IS NULL FOR UPDATE",&[&stage.name()]).await?.get(0);
        if revision != progress.revision {
            return Ok(());
        }
        tx.batch_execute("SET LOCAL codexctl.identity_migration='1'")
            .await?;
        for row in rows {
            match row {
                Prepared::Claim(row) => {
                    let workspace: String = row.get("workspace");
                    let namespace: String = row.get("namespace");
                    let claim: String = row.get("claim");
                    let account: String = row.get("account_id");
                    let deleted: Option<SystemTime> = row.get("deleted_at");
                    let copied=tx.query_opt("INSERT INTO central_account_identity_claims(workspace,namespace,claim,account_id,deleted_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(workspace,namespace,claim) DO UPDATE SET claim=EXCLUDED.claim WHERE central_account_identity_claims.account_id=EXCLUDED.account_id AND central_account_identity_claims.deleted_at IS NOT DISTINCT FROM EXCLUDED.deleted_at RETURNING account_id",&[&workspace,&namespace,&claim,&account,&deleted]).await?;
                    if copied.is_none() {
                        bail!("retained identity migration conflict");
                    }
                }
                Prepared::Account(record) => {
                    if let Some(claims) = identity::Claims::from_record(record)? {
                        identity::record_claims(&tx, &record.account_id, &claims).await?;
                    }
                }
                Prepared::Operation {
                    user,
                    id,
                    uid,
                    sub,
                    claims,
                } => {
                    let changed=tx.execute("UPDATE central_login_operations SET candidate_claims=$3::text::jsonb,candidate_uid=$4,candidate_sub=$5 WHERE user_id=$1 AND id=$2",&[&user,&id,&claims,&uid,&sub]).await?;
                    if changed != 1 {
                        bail!("login receipt disappeared during migration");
                    }
                }
                Prepared::Reservation(row) => {
                    let workspace: String = row.get("workspace");
                    let namespace: String = row.get("namespace");
                    let claim: String = row.get("claim");
                    let user: String = row.get("user_id");
                    let id: String = row.get("id");
                    let deleted: Option<SystemTime> = row.get("deleted_at");
                    tx.execute("INSERT INTO central_login_identity_reservation_history(workspace,namespace,claim,user_id,id,deleted_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING",&[&workspace,&namespace,&claim,&user,&id,&deleted]).await?;
                }
            }
        }
        let cursor = Key::columns(next.or(progress.cursor.as_ref()));
        tx.execute("UPDATE central_identity_migration_progress SET cursor_account=$3,cursor_user=$4,cursor_id=$5,cursor_workspace=$6,cursor_namespace=$7,cursor_claim=$8,completed=$9,revision=revision+1,records=records+$10 WHERE stage=$1 AND revision=$2 AND deleted_at IS NULL",&[&stage.name(),&progress.revision,&cursor[0],&cursor[1],&cursor[2],&cursor[3],&cursor[4],&cursor[5],&rows.is_empty(),&(rows.len() as i64)]).await?;
        tx.commit().await?;
        eprintln!(
            "{}",
            serde_json::json!({"operation":"identity_migration","stage":stage.name(),"records":rows.len()})
        );
        Ok(())
    }
}
