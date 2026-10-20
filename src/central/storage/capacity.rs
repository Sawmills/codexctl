//! Transactional storage adapter for bounded capacity evidence and budgets.
use super::*;
use crate::central::capacity::State;
const MAX_DOCUMENT: usize = 4 * 1024 * 1024;

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS central_capacity_policy (scope_id TEXT PRIMARY KEY, payload TEXT NOT NULL CHECK(octet_length(payload) <= 4194304), expires_at BIGINT NOT NULL); CREATE INDEX IF NOT EXISTS central_capacity_policy_expiry ON central_capacity_policy(expires_at);";

impl CentralStore {
    pub(in crate::central) async fn capacity_state<T: Send + 'static>(
        &self,
        scope: &str,
        now: i64,
        operation: impl FnOnce(&mut State) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let scope = scope.to_owned();
        self.capacity_states(vec![scope.clone()], now, move |states| {
            operation(states.get_mut(&scope).context("capacity scope absent")?)
        })
        .await
    }

    pub(in crate::central) async fn capacity_states<T: Send + 'static>(
        &self,
        mut scopes: Vec<String>,
        now: i64,
        operation: impl FnOnce(&mut BTreeMap<String, State>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        scopes.sort();
        scopes.dedup();
        anyhow::ensure!(
            !scopes.is_empty() && scopes.len() <= 2,
            "invalid capacity scopes"
        );
        match self {
            Self::File(file) => {
                let directory = file.state.clone();
                tokio::task::spawn_blocking(move || {
                    let _guard = vault::lock(&directory, "capacity.lock")?;
                    let mut states = BTreeMap::new();
                    let mut originals = BTreeMap::new();
                    for scope in &scopes {
                        let path = directory.join("capacity").join(format!("{}.json",vault::digest(scope.as_bytes())));
                        let bytes = if path.try_exists()? { vault::private_read(&path)? } else { serde_json::to_vec(&State::default())? };
                        anyhow::ensure!(bytes.len() <= MAX_DOCUMENT, "capacity state too large");
                        states.insert(scope.clone(), serde_json::from_slice::<State>(&bytes)?);
                        originals.insert(scope.clone(), (path,bytes));
                    }
                    let result = operation(&mut states)?;
                    // Publish the budget first. A crash may charge an unused
                    // reservation, but cannot erase a charged session move.
                    let mut writes = Vec::new();
                    for (scope,state) in states {
                        let bytes = serde_json::to_vec(&state)?;
                        anyhow::ensure!(bytes.len() <= MAX_DOCUMENT, "capacity state too large");
                        let (path,before) = originals.remove(&scope).context("capacity scope absent")?;
                        if bytes != before { writes.push((!scope.starts_with("sessions:"),path,bytes)); }
                    }
                    writes.sort_by_key(|(model,_,_)| *model);
                    for (_,path,bytes) in writes { crate::store::atomic_write(&path,&bytes)?; }
                    Ok(result)
                }).await?
            }
            Self::Postgres(db) | Self::Dual { postgres: db, .. } => bounded_db(async {
                // Expiry cleanup commits separately so unrelated expired rows
                // cannot add locks to the model/budget transaction's lock order.
                db.client().await?.execute("DELETE FROM central_capacity_policy WHERE scope_id IN (SELECT scope_id FROM central_capacity_policy WHERE expires_at < $1 LIMIT 64 FOR UPDATE SKIP LOCKED)", &[&now]).await?;
                let mut connection = db.admission_client().await?;
                let tx = connection.transaction().await?;
                let empty = serde_json::to_string(&State::default())?;
                let mut states = BTreeMap::new();
                let mut originals = BTreeMap::new();
                for scope in &scopes {
                    tx.execute("INSERT INTO central_capacity_policy(scope_id,payload,expires_at) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[scope,&empty,&(now+3600)]).await?;
                    let row = tx.query_one("SELECT payload FROM central_capacity_policy WHERE scope_id=$1 FOR UPDATE", &[scope]).await?;
                    let payload: String = row.get(0);
                    anyhow::ensure!(payload.len() <= MAX_DOCUMENT, "capacity state too large");
                    states.insert(scope.clone(),serde_json::from_str::<State>(&payload)?);
                    originals.insert(scope.clone(),payload);
                }
                let result = operation(&mut states)?;
                for (scope,state) in states {
                    let payload = serde_json::to_string(&state)?;
                    anyhow::ensure!(payload.len() <= MAX_DOCUMENT, "capacity state too large");
                    if originals.get(&scope) != Some(&payload) {
                        tx.execute("UPDATE central_capacity_policy SET payload=$2,expires_at=$3 WHERE scope_id=$1", &[&scope,&payload,&(now+3600)]).await?;
                    }
                }
                tx.commit().await?;
                Ok(result)
            }).await,
        }
    }
}

#[cfg(all(test, feature = "central-real-db-tests"))]
mod tests {
    use super::*;
    use crate::central::capacity::{
        CapacityAccount, CapacityRecovery, Evidence, Outcome, PlanRequest,
    };
    #[tokio::test]
    async fn capacity_policy_is_shared_and_deduplicated_across_postgres_connections() {
        if std::env::var("DATABASE_URL").is_err() {
            assert_ne!(std::env::var("CI").as_deref(), Ok("true"));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[21; 32]).unwrap();
        let base = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (first, control, schema) = base.isolated_test_schema().await.unwrap();
        super::super::test_support::bootstrap(&first).await.unwrap();
        let mut replica = first.clone();
        if let CentralStore::Postgres(db) = &mut replica {
            db.admission = Arc::new(tokio::sync::Mutex::new(None));
        }
        for _ in 0..40 {
            assert!(
                first
                    .capacity_event_allowed("separate-budget")
                    .await
                    .unwrap()
            );
        }
        assert!(first.relay_event_allowed("separate-budget").await.unwrap());
        first
            .retire_relay_event_limiter("separate-budget")
            .await
            .unwrap();
        assert!(
            !first
                .capacity_event_allowed("separate-budget")
                .await
                .unwrap()
        );
        let a = CapacityRecovery::new(first, "test".into());
        let b = CapacityRecovery::new(replica, "test".into());
        let event = |id: &str| Evidence {
            alias: "a".into(),
            model: "gpt-6.1-sol".into(),
            event_id: id.into(),
            outcome: Outcome::Overloaded,
            session_id: None,
            observed_at: None,
            identity: Some(crate::central::loans::CredentialSubject {
                workspace: "workspace".into(),
                uid: None,
                sub: Some("a".into()),
            }),
        };
        let (left, right) =
            tokio::join!(a.observe(event("one"), 1000), b.observe(event("one"), 1000));
        left.unwrap();
        right.unwrap();
        let account = |alias: &str| {
            CapacityAccount {
            account: serde_json::from_value(serde_json::json!({"userId":"test","alias":alias,"accountId":alias,"plan":"pro","billingClass":"rate_limited","primaryUsed":1,"available":true,"usageScore":1,"liveSessions":0})).unwrap(),
            identity: crate::central::loans::CredentialSubject {workspace:"workspace".into(), uid:None, sub:Some(alias.into())},
        }
        };
        let request = || PlanRequest {
            model: "gpt-6.1-sol".into(),
            session_id: None,
            failed_alias: None,
            tried: vec![],
            recent_429_rates: Default::default(),
        };
        a.observe(event("two"), 1001).await.unwrap();
        assert_eq!(
            b.plan(request(), vec![account("a"), account("b")], 1002)
                .await
                .unwrap()
                .alias
                .as_deref(),
            Some("a")
        );
        a.observe(event("three"), 1002).await.unwrap();
        assert_eq!(
            b.plan(request(), vec![account("a"), account("b")], 1003)
                .await
                .unwrap()
                .alias
                .as_deref(),
            Some("b")
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
