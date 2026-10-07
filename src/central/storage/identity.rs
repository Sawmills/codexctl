//! Namespaced identity claims and short, serialized credential admission.
use super::login::{AddAdmission, LoginOperation};
use super::*;
use crate::api;

pub(super) struct Claims {
    workspace: String,
    logins: api::Logins,
}
impl Claims {
    pub(super) fn from_record(record: &CredentialRecord) -> Result<Option<Self>> {
        if let Some(auth) = record.vault.get("auth") {
            return Self::from_auth(auth).map(Some);
        }
        Ok(record.workspace.as_ref().map(|workspace| Self {
            workspace: workspace.clone(),
            logins: api::Logins {
                uid: None,
                sub: record.login.clone(),
            },
        }))
    }
    pub(super) fn from_auth(auth: &Value) -> Result<Self> {
        Ok(Self {
            workspace: vault::account(auth)?,
            logins: api::token_logins(vault::token(auth)?),
        })
    }
    fn keys(&self) -> Vec<(&str, &str)> {
        let mut keys = Vec::new();
        if let Some(uid) = self.logins.uid.as_deref().filter(|v| !v.is_empty()) {
            keys.push(("uid", uid));
        }
        if let Some(sub) = self.logins.sub.as_deref().filter(|v| !v.is_empty()) {
            keys.push(("sub", sub));
        }
        keys
    }
    pub(super) fn json(&self) -> Value {
        Value::Object(
            self.keys()
                .into_iter()
                .map(|(namespace, claim)| (namespace.into(), Value::String(claim.into())))
                .collect(),
        )
    }
    fn tags(&self) -> Vec<String> {
        self.keys()
            .into_iter()
            .map(|(namespace, claim)| format!("{namespace}:{claim}"))
            .collect()
    }
}

#[derive(Debug)]
pub(in crate::central) struct CandidateIdentityMismatch;
impl std::fmt::Display for CandidateIdentityMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("login candidate identity changed")
    }
}
impl std::error::Error for CandidateIdentityMismatch {}

#[derive(Debug)]
pub(in crate::central) enum IdentityDenied {
    Reserved,
    Owned,
    Conflict,
    Unknown,
}
impl IdentityDenied {
    pub(in crate::central) fn reason(&self) -> &'static str {
        match self {
            Self::Reserved => "relogin_reserved",
            Self::Owned => "account_already_owned",
            Self::Conflict => "alias_identity_conflict",
            Self::Unknown => "account_identity_unresolved",
        }
    }
}
impl std::fmt::Display for IdentityDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}
impl std::error::Error for IdentityDenied {}

pub(super) async fn check_claims(
    tx: &tokio_postgres::Transaction<'_>,
    account: &str,
    claims: &Claims,
) -> Result<()> {
    let tags = claims.tags();
    let reserved: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM central_login_identity_reservations r JOIN central_login_operations o USING(user_id,id) WHERE r.deleted_at IS NULL AND r.workspace=$1 AND r.namespace||':'||r.claim=ANY($2) AND o.account_id IS DISTINCT FROM $3)", &[&claims.workspace,&tags,&account]).await?.get(0);
    if reserved {
        return Err(IdentityDenied::Reserved.into());
    }
    let owned: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM central_account_identity_claims WHERE deleted_at IS NULL AND workspace=$1 AND namespace||':'||claim=ANY($2) AND account_id<>$3)", &[&claims.workspace,&tags,&account]).await?.get(0);
    if owned {
        return Err(IdentityDenied::Owned.into());
    }
    let unknown:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_accounts a WHERE a.workspace=$1 AND a.account_id<>$2 AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM central_account_identity_claims c WHERE c.deleted_at IS NULL AND c.account_id=a.account_id AND c.workspace=$1 AND ((c.namespace='uid' AND $3::text IS NOT NULL) OR (c.namespace='sub' AND $4::text IS NOT NULL))))",&[&claims.workspace,&account,&claims.logins.uid,&claims.logins.sub]).await?.get(0);
    if unknown {
        return Err(IdentityDenied::Unknown.into());
    }
    let empty = Claims {
        workspace: claims.workspace.clone(),
        logins: Default::default(),
    };
    let known = known_claims(tx, account, &empty).await?;
    if (!known.uid.is_empty() || !known.sub.is_empty())
        && !matches!(known.compare(&claims.logins), IdentityDecision::Same)
    {
        return Err(IdentityDenied::Conflict.into());
    }
    Ok(())
}

enum IdentityDecision {
    Same,
    Different,
    Unknown,
    Conflict,
}
struct KnownClaims {
    uid: std::collections::BTreeSet<String>,
    sub: std::collections::BTreeSet<String>,
}
impl KnownClaims {
    fn compare(&self, arriving: &api::Logins) -> IdentityDecision {
        if let Some(uid) = arriving.uid.as_ref().filter(|_| !self.uid.is_empty()) {
            if self.uid.contains(uid) {
                return IdentityDecision::Same;
            }
            return if arriving
                .sub
                .as_ref()
                .is_some_and(|sub| self.sub.contains(sub))
            {
                IdentityDecision::Conflict
            } else {
                IdentityDecision::Different
            };
        }
        if let Some(sub) = arriving.sub.as_ref().filter(|_| !self.sub.is_empty()) {
            return if self.sub.contains(sub) {
                IdentityDecision::Same
            } else {
                IdentityDecision::Different
            };
        }
        IdentityDecision::Unknown
    }
}
async fn known_claims(
    tx: &tokio_postgres::Transaction<'_>,
    account: &str,
    current: &Claims,
) -> Result<KnownClaims> {
    let mut known = KnownClaims {
        uid: Default::default(),
        sub: Default::default(),
    };
    for (namespace, claim) in current.keys() {
        if namespace == "uid" {
            known.uid.insert(claim.into());
        } else {
            known.sub.insert(claim.into());
        }
    }
    for row in tx.query("SELECT namespace,claim FROM central_account_identity_claims WHERE deleted_at IS NULL AND account_id=$1 AND workspace=$2", &[&account,&current.workspace]).await? {
        let namespace: String = row.get(0);
        let claim: String = row.get(1);
        if namespace == "uid" { known.uid.insert(claim); } else { known.sub.insert(claim); }
    }
    Ok(known)
}

impl PostgresStore {
    pub(super) async fn admission_client(
        &self,
    ) -> Result<tokio::sync::MappedMutexGuard<'_, tokio_postgres::Client>> {
        let mut slot = self.admission.lock().await;
        if slot.as_ref().is_none_or(tokio_postgres::Client::is_closed) {
            *slot = Some(
                Arc::try_unwrap(self.establish().await?)
                    .map_err(|_| anyhow::anyhow!("admission connection is shared"))?,
            );
        }
        tokio::sync::MutexGuard::try_map(slot, Option::as_mut)
            .map_err(|_| anyhow::anyhow!("admission connection is absent"))
    }
}
#[must_use]
pub(super) struct AdmissionTiming(std::time::Instant);
static ADMISSION_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ADMISSION_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
impl Drop for AdmissionTiming {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        ADMISSION_NANOS.fetch_add(
            self.0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Relaxed,
        );
        ADMISSION_COUNT.fetch_add(1, Relaxed);
    }
}
pub(in crate::central) fn admission_metrics() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    format!(
        "codexctl_central_admission_lock_seconds_sum {}\ncodexctl_central_admission_lock_seconds_count {}\n",
        ADMISSION_NANOS.load(Relaxed) as f64 / 1_000_000_000.0,
        ADMISSION_COUNT.load(Relaxed)
    )
}
pub(super) async fn lock_admission(
    tx: &tokio_postgres::Transaction<'_>,
) -> Result<AdmissionTiming> {
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended(current_schema(),12484))",
        &[],
    )
    .await?;
    Ok(AdmissionTiming(std::time::Instant::now()))
}
pub(super) async fn record_claims(
    tx: &tokio_postgres::Transaction<'_>,
    account: &str,
    claims: &Claims,
) -> Result<()> {
    let empty = Claims {
        workspace: claims.workspace.clone(),
        logins: Default::default(),
    };
    let known = known_claims(tx, account, &empty).await?;
    if (!known.uid.is_empty() || !known.sub.is_empty())
        && !matches!(known.compare(&claims.logins), IdentityDecision::Same)
    {
        return Err(IdentityDenied::Conflict.into());
    }
    for (namespace, claim) in claims.keys() {
        let row=tx.query_opt("INSERT INTO central_account_identity_claims(workspace,namespace,claim,account_id) VALUES($1,$2,$3,$4) ON CONFLICT(workspace,namespace,claim) DO UPDATE SET claim=EXCLUDED.claim WHERE central_account_identity_claims.account_id=EXCLUDED.account_id AND central_account_identity_claims.deleted_at IS NULL RETURNING account_id",&[&claims.workspace,&namespace,&claim,&account]).await?;
        if row.is_none() {
            return Err(IdentityDenied::Owned.into());
        }
        // Keep the legacy projection for readers of migration 4. The independent
        // ledger above remains authoritative even if its old FK cascades.
        let legacy=tx.query_opt("INSERT INTO central_account_claims(workspace,namespace,claim,account_id) VALUES($1,$2,$3,$4) ON CONFLICT(workspace,namespace,claim) DO UPDATE SET claim=EXCLUDED.claim WHERE central_account_claims.account_id=EXCLUDED.account_id AND central_account_claims.deleted_at IS NULL RETURNING account_id",&[&claims.workspace,&namespace,&claim,&account]).await?;
        if legacy.is_none() {
            return Err(IdentityDenied::Owned.into());
        }
    }
    Ok(())
}

async fn reserve_claims(
    tx: &tokio_postgres::Transaction<'_>,
    op: &LoginOperation,
    account: &str,
    claims: &Claims,
) -> Result<bool> {
    for (namespace, claim) in claims.keys() {
        let held=tx.query_opt("SELECT r.user_id,r.id,o.phase,central_login_identity_agrees(o.candidate_workspace,o.candidate_uid,o.candidate_sub,$4) FROM central_login_identity_reservations r JOIN central_login_operations o USING(user_id,id) WHERE r.deleted_at IS NULL AND r.workspace=$1 AND r.namespace=$2 AND r.claim=$3",&[&claims.workspace,&namespace,&claim,&account]).await?;
        if let Some(held) = held {
            let user: String = held.get(0);
            let id: String = held.get(1);
            let phase: String = held.get(2);
            let repair: bool = held.get(3);
            if !((user == op.user && id == op.id) || (phase == "rejected" && repair)) {
                return Ok(false);
            }
        } else {
            let inserted=tx.execute("INSERT INTO central_login_identity_reservations(workspace,namespace,claim,user_id,id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(workspace,namespace,claim) DO UPDATE SET user_id=EXCLUDED.user_id,id=EXCLUDED.id,deleted_at=NULL WHERE central_login_identity_reservations.deleted_at IS NOT NULL",&[&claims.workspace,&namespace,&claim,&op.user,&op.id]).await?;
            if inserted != 1 {
                return Ok(false);
            }
        }
        tx.execute("INSERT INTO central_login_identity_reservation_history(workspace,namespace,claim,user_id,id) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",&[&claims.workspace,&namespace,&claim,&op.user,&op.id]).await?;
    }
    Ok(true)
}

impl CentralStore {
    /// Publish precedes admission. Account creation, namespaced identity claims,
    /// and the operation's reservation transfer commit together.
    pub(in crate::central) async fn login_admit_add(
        &self,
        op: &mut LoginOperation,
    ) -> Result<AddAdmission> {
        let db = self.login_db()?;
        let auth = op
            .payload
            .candidate
            .as_ref()
            .context("missing login candidate")?;
        let claims = Claims::from_auth(auth)?;
        if claims.keys().is_empty() {
            return Ok(AddAdmission::Refused("account_identity_unresolved"));
        }
        let proposed = crate::central::managed::account_key(&op.user, &op.alias);
        let saved = vault::Vault {
            alias: op.alias.clone(),
            tenant: "sawmills".into(),
            user: op.user.clone(),
            auth: auth.clone(),
            label: op.payload.label.clone(),
            verified: false,
            import_rejected: false,
            revision: 1,
        };
        let encrypted = vault::encrypt_bytes(&db.key, &serde_json::to_vec(&saved)?)?;
        let cipher = vault::cipher(&db.key)?;
        let tags = claims.tags();
        let result=bounded_db(async {
            let mut client=db.admission_client().await?;
            let tx=client.transaction().await?;
            let _admission_timing = lock_admission(&tx).await?;
            let authorized=tx.query_opt("SELECT d.id FROM central_devices d JOIN central_users u ON u.id=d.user_id WHERE d.id=$1 AND d.user_id=$2 AND d.tenant='sawmills' AND NOT d.revoked AND d.deleted_at IS NULL AND u.enabled AND u.deleted_at IS NULL FOR SHARE OF d,u",&[&op.device,&op.user]).await?;
            if authorized.is_none() {return Ok(Err("login_canceled"));}
            tx.query_opt("SELECT id FROM central_login_operations WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase='candidate' AND kind='add' AND account_id IS NULL AND NOT cancel_requested AND expires_at>clock_timestamp() FOR UPDATE",&[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence]).await?.context("add admission fenced")?;
            if tx.query_opt("SELECT target FROM central_alias_tombstones WHERE deleted_at IS NULL AND user_id=$1 AND alias=lower($2)",&[&op.user,&op.alias]).await?.is_some() {return Ok(Err("alias_renamed"));}
            // An active login may have learned a claim that the saved account
            // does not yet declare. Refuse its reservation before comparing vaults.
            let active_reserved:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_login_identity_reservations r JOIN central_login_operations o USING(user_id,id) WHERE r.deleted_at IS NULL AND r.workspace=$1 AND r.namespace||':'||r.claim=ANY($2) AND (r.user_id<>$3 OR r.id<>$4) AND o.phase<>'rejected')",&[&claims.workspace,&tags,&op.user,&op.id]).await?.get(0);
            if active_reserved {return Ok(Err("relogin_reserved"));}
            let mut selected=None;
            for row in tx.query("SELECT account_id,user_id,alias,encrypted_vault FROM central_accounts WHERE workspace=$1 AND deleted_at IS NULL",&[&claims.workspace]).await? {
                let bytes:Vec<u8>=row.get(3);
                let stored: vault::Vault=serde_json::from_slice(&vault::decrypt_with_cipher(&cipher,&bytes)?)?;
                let existing=Claims::from_auth(&stored.auth)?;
                let account: String = row.get(0);
                match known_claims(&tx,&account,&existing).await?.compare(&claims.logins) {
                    IdentityDecision::Same => {},
                    IdentityDecision::Different => continue,
                    IdentityDecision::Unknown => return Ok(Err("account_identity_unresolved")),
                    IdentityDecision::Conflict => return Ok(Err("alias_identity_conflict")),
                }
                if row.get::<_,Option<String>>(1).as_deref()!=Some(op.user.as_str()) {return Ok(Err("account_already_owned"));}
                if selected.is_some() {bail!("ambiguous account identity");}
                selected=Some((row.get::<_,String>(0),row.get::<_,String>(2)));
            }
            let selected_account=selected.as_ref().map(|(account,_)|account.as_str());
            let reserved=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_login_identity_reservations r JOIN central_login_operations o USING(user_id,id) WHERE r.deleted_at IS NULL AND r.workspace=$1 AND r.namespace||':'||r.claim=ANY($2) AND (r.user_id<>$3 OR r.id<>$4) AND NOT (o.phase='rejected' AND $5::text IS NOT NULL AND central_login_identity_agrees(o.candidate_workspace,o.candidate_uid,o.candidate_sub,$5)))",&[&claims.workspace,&tags,&op.user,&op.id,&selected_account]).await?.get::<_,bool>(0);
            if reserved {return Ok(Err("relogin_reserved"));}
            let (account,landed)=match selected {
                Some((account,alias))=>{
                    let active=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_login_operations WHERE account_id=$1 AND phase NOT IN ('completed','failed','canceled','rejected','replica_lost'))",&[&account]).await?.get::<_,bool>(0);
                    if active {return Ok(Err("relogin_reserved"));}
                    (account,Some(alias))
                },
                None=>{
                    let inserted=tx.execute("INSERT INTO central_accounts(account_id,user_id,alias,workspace,login,encrypted_vault,revision) VALUES($1,$2,$3,$4,$5,$6,1) ON CONFLICT DO NOTHING",&[&proposed,&op.user,&op.alias,&claims.workspace,&claims.logins.sub,&encrypted]).await?;
                    if inserted!=1 {return Ok(Err("alias_exists"));}
                    (proposed.clone(),None)
                },
            };
            record_claims(&tx,&account,&claims).await?;
            if !reserve_claims(&tx,op,&account,&claims).await? {return Ok(Err("relogin_reserved"));}
            let changed=tx.execute("UPDATE central_login_operations SET account_id=$6,landed_alias=$7,sequence=sequence+1 WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase='candidate' AND NOT cancel_requested AND expires_at>clock_timestamp()",&[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&account,&landed]).await?;
            if changed!=1 {bail!("add admission fenced");}
            tx.commit().await?;
            Ok(Ok((account,landed)))
        }).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                // COMMIT can succeed after its response times out. Queue the read
                // on the same connection so it observes the settled transaction.
                let admitted = bounded_db(async {
                    let client = db.admission_client().await?;
                    Ok(client.query_opt("SELECT account_id,landed_alias FROM central_login_operations WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5::bigint+1 AND phase='candidate' AND kind='add' AND account_id IS NOT NULL AND expires_at>clock_timestamp()", &[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence]).await?)
                }).await?;
                if let Some(row) = admitted {
                    Ok((row.get::<_, String>(0), row.get::<_, Option<String>>(1)))
                } else if error
                    .downcast_ref::<tokio_postgres::Error>()
                    .is_some_and(|e| {
                        e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
                    })
                {
                    Err("relogin_reserved")
                } else {
                    return Err(error);
                }
            }
        };
        match result {
            Ok((account, landed)) => {
                op.account_id = Some(account);
                op.payload.landed = landed;
                op.sequence += 1;
                Ok(AddAdmission::Ready)
            }
            Err(reason) => Ok(AddAdmission::Refused(reason)),
        }
    }
}

impl CentralStore {
    /// Retained claims prove landing even when the current token omits a claim.
    pub(in crate::central) async fn validate_login_candidate(
        &self,
        op: &LoginOperation,
    ) -> Result<()> {
        let db = self.login_db()?;
        let candidate = Claims::from_auth(
            op.payload
                .candidate
                .as_ref()
                .context("missing login candidate")?,
        )?;
        let cipher = vault::cipher(&db.key)?;
        bounded_db(async {
            let mut client = db.admission_client().await?;
            let tx = client.transaction().await?;
            let _admission_timing = lock_admission(&tx).await?;
            let row = tx.query_opt("SELECT encrypted_vault FROM central_accounts WHERE account_id=$1 AND user_id=$2 AND deleted_at IS NULL", &[&op.account()?,&op.user]).await?.context("login account missing")?;
            let bytes: Vec<u8> = row.get(0);
            let saved: vault::Vault = serde_json::from_slice(&vault::decrypt_with_cipher(&cipher,&bytes)?)?;
            let current = Claims::from_auth(&saved.auth)?;
            if current.workspace != candidate.workspace || !matches!(known_claims(&tx,op.account()?,&current).await?.compare(&candidate.logins),IdentityDecision::Same) { return Err(CandidateIdentityMismatch.into()); }
            Ok(())
        }).await
    }
}

impl CentralStore {
    pub(in crate::central) async fn check_import_identity(
        &self,
        account: &str,
        auth: &Value,
    ) -> Result<()> {
        let db = self.login_db()?;
        let claims = Claims::from_auth(auth)?;
        bounded_db(async {
            let mut client = db.admission_client().await?;
            let tx = client.transaction().await?;
            let _admission_timing = lock_admission(&tx).await?;
            check_claims(&tx, account, &claims).await
        })
        .await
    }
}

impl PostgresStore {
    pub(super) async fn backfill_identity_claims(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        cipher: &aes_gcm::Aes256Gcm,
    ) -> Result<()> {
        for row in tx.query("SELECT account_id,user_id,alias,workspace,login,encrypted_vault,revision FROM central_accounts WHERE deleted_at IS NULL ORDER BY account_id",&[]).await? {
            let bytes: Vec<u8>=row.get(5);
            let record=CredentialRecord {
                account_id:row.get(0),user_id:row.get(1),alias:row.get(2),workspace:row.get(3),login:row.get(4),
                vault:serde_json::from_slice(&vault::decrypt_with_cipher(cipher,&bytes)?)?,revision:row.get(6),
            };
            if let Some(claims)=Claims::from_record(&record)? {record_claims(tx,&record.account_id,&claims).await?;}
        }
        for row in tx.query("SELECT user_id,id,encrypted_payload FROM central_login_operations WHERE candidate_workspace IS NOT NULL",&[]).await? {
            let user:String=row.get(0);
            let id:String=row.get(1);
            let bytes:Vec<u8>=row.get(2);
            let payload: super::login::LoginPayload=serde_json::from_slice(&vault::decrypt_with_cipher(cipher,&bytes)?)?;
            if let Some(auth)=payload.candidate {
                let claims=Claims::from_auth(&auth)?;
                let json=claims.json().to_string();
                tx.execute("UPDATE central_login_operations SET candidate_claims=$3::text::jsonb,candidate_uid=$4,candidate_sub=$5 WHERE user_id=$1 AND id=$2",&[&user,&id,&json,&claims.logins.uid,&claims.logins.sub]).await?;
            }
        }
        Ok(())
    }
}

impl CentralStore {
    /// A renewal reserves every candidate claim before its refresh-capable launch.
    /// Repair keeps the rejected holder until verified completion retires its receipt.
    pub(in crate::central) async fn login_reserve_renewal(
        &self,
        op: &LoginOperation,
    ) -> Result<bool> {
        let db = self.login_db()?;
        let claims = Claims::from_auth(
            op.payload
                .candidate
                .as_ref()
                .context("missing renewal candidate")?,
        )?;
        bounded_db(async {
            let mut connection=db.admission_client().await?;
            let tx=connection.transaction().await?;
            let _admission_timing = lock_admission(&tx).await?;
            tx.query_opt("SELECT id FROM central_login_operations WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase='candidate' AND kind='renewal' AND account_id=$6 AND NOT cancel_requested AND expires_at>clock_timestamp() FOR UPDATE",&[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence,&op.account()?]).await?.context("renewal reservation fenced")?;
            let tags=claims.tags();
            let claimed:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_account_identity_claims WHERE deleted_at IS NULL AND workspace=$1 AND namespace||':'||claim=ANY($2) AND account_id<>$3)",&[&claims.workspace,&tags,&op.account()?]).await?.get(0);
            if claimed {return Ok(false);}
            if !reserve_claims(&tx,op,op.account()?,&claims).await? {return Ok(false);}
            let fresh:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM central_login_operations WHERE user_id=$1 AND id=$2 AND holder_id=$3 AND epoch=$4 AND sequence=$5 AND phase='candidate' AND NOT cancel_requested AND expires_at>clock_timestamp())",&[&op.user,&op.id,&op.holder,&op.epoch,&op.sequence]).await?.get(0);
            if !fresh {bail!("renewal reservation fenced");}
            tx.commit().await?;
            Ok(true)
        }).await
    }
}

/// Retained facts for a committed account, used beside local execution evidence.
pub(in crate::central) struct RetainedIdentity {
    workspace: String,
    logins: KnownClaims,
}
impl RetainedIdentity {
    pub(in crate::central) fn from_auth(auth: &Value) -> Result<Self> {
        let claims = Claims::from_auth(auth)?;
        Ok(Self {
            workspace: claims.workspace,
            logins: KnownClaims {
                uid: claims.logins.uid.into_iter().collect(),
                sub: claims.logins.sub.into_iter().collect(),
            },
        })
    }
    pub(in crate::central) fn validate(&self, auth: &Value) -> Result<()> {
        self.with_auth(auth).map(|_| ())
    }
    fn with_auth(&self, auth: &Value) -> Result<KnownClaims> {
        let current = Claims::from_auth(auth)?;
        if current.workspace != self.workspace {
            bail!("local account workspace differs from shared evidence");
        }
        let mut known = KnownClaims {
            uid: self.logins.uid.clone(),
            sub: self.logins.sub.clone(),
        };
        if (!known.uid.is_empty() || !known.sub.is_empty())
            && !matches!(known.compare(&current.logins), IdentityDecision::Same)
        {
            bail!("local account identity differs from shared evidence");
        }
        if let Some(uid) = current.logins.uid {
            known.uid.insert(uid);
        }
        if let Some(sub) = current.logins.sub {
            known.sub.insert(sub);
        }
        Ok(known)
    }
    pub(in crate::central) fn overlaps(
        &self,
        auth: &Value,
        other: &Self,
        arriving: &Value,
    ) -> Result<bool> {
        let local_workspace = vault::account(auth)?;
        let arriving_workspace = vault::account(arriving)?;
        // A conflicting stopped credential still names bounded workspace scopes.
        // Preserve both scopes, without fencing a workspace neither source names.
        if self.workspace != other.workspace
            && self.workspace != arriving_workspace
            && local_workspace != other.workspace
            && local_workspace != arriving_workspace
        {
            return Ok(false);
        }
        let left = self.with_auth(auth)?;
        let right = other.with_auth(arriving)?;
        if self.workspace != other.workspace {
            return Ok(false);
        }
        Ok(!left.uid.is_disjoint(&right.uid)
            || !left.sub.is_disjoint(&right.sub)
            || ((left.uid.is_empty() || right.uid.is_empty())
                && (left.sub.is_empty() || right.sub.is_empty())))
    }
}
impl CentralStore {
    pub(in crate::central) async fn retained_identities(
        &self,
    ) -> Result<BTreeMap<String, RetainedIdentity>> {
        let db = self.login_db()?;
        bounded_db(async {
            let rows=db.client().await?.query("SELECT a.account_id,a.workspace,c.namespace,c.claim FROM central_accounts a LEFT JOIN central_account_identity_claims c ON c.account_id=a.account_id AND c.workspace=a.workspace AND c.deleted_at IS NULL WHERE a.deleted_at IS NULL ORDER BY a.account_id",&[]).await?;
            let mut identities=BTreeMap::new();
            for row in rows {
                let Some(workspace)=row.get::<_,Option<String>>(1) else {continue;};
                let id:String=row.get(0);
                let identity=identities.entry(id).or_insert_with(||RetainedIdentity {workspace,logins:KnownClaims {uid:Default::default(),sub:Default::default()}});
                if let (Some(namespace),Some(claim))=(row.get::<_,Option<String>>(2),row.get::<_,Option<String>>(3)) {
                    if namespace=="uid" {identity.logins.uid.insert(claim);} else {identity.logins.sub.insert(claim);}
                }
            }
            Ok(identities)
        }).await
    }
}

impl CentralStore {
    pub(in crate::central) async fn renamed_alias(
        &self,
        user: &str,
        alias: &str,
    ) -> Result<Option<String>> {
        let db = self.login_db()?;
        bounded_db(async {
            Ok(db.client().await?.query_opt("SELECT target FROM central_alias_tombstones WHERE deleted_at IS NULL AND user_id=$1 AND alias=lower($2)",&[&user,&alias]).await?.map(|row|row.get(0)))
        }).await
    }
}
impl PostgresStore {
    pub(super) async fn save_alias_tombstones(
        &self,
        aliases: Vec<(String, String, String)>,
    ) -> Result<()> {
        bounded_db(async {
            let mut connection=self.admission_client().await?;
            let tx=connection.transaction().await?;
            let _admission_timing = lock_admission(&tx).await?;
            for (user,alias,target) in aliases {
                tx.execute("INSERT INTO central_alias_tombstones(user_id,alias,target) VALUES($1,$2,$3) ON CONFLICT(user_id,alias) DO UPDATE SET target=EXCLUDED.target",&[&user,&alias,&target]).await?;
            }
            tx.commit().await?;
            Ok(())
        }).await
    }
}
