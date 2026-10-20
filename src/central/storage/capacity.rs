//! Transactional storage adapter for bounded capacity evidence and budgets.
use super::*;
use crate::central::capacity::State;
const MAX_DOCUMENT: usize = 4 * 1024 * 1024;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS central_capacity_policy (
    scope_id TEXT NOT NULL,
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    schema_version SMALLINT NOT NULL DEFAULT 1 CHECK (schema_version = 1),
    expires_at BIGINT NOT NULL,
    deleted_at BIGINT,
    PRIMARY KEY (scope_id, generation)
);
CREATE UNIQUE INDEX IF NOT EXISTS central_capacity_policy_active
    ON central_capacity_policy(scope_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS central_capacity_policy_expiry
    ON central_capacity_policy(expires_at) WHERE deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS central_capacity_histories (
    scope_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    identity_key TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    workspace TEXT NOT NULL CHECK (workspace <> ''),
    login_uid TEXT,
    login_sub TEXT,
    model TEXT NOT NULL CHECK (scope_id = 'model:' || model),
    anchor_until BIGINT NOT NULL,
    completed_at BIGINT,
    cooling_until BIGINT NOT NULL,
    seen_ids TEXT[] NOT NULL,
    seen_times BIGINT[] NOT NULL,
    failure_times BIGINT[] NOT NULL,
    completed_ids TEXT[] NOT NULL,
    completed_times BIGINT[] NOT NULL,
    terminal_sessions TEXT[] NOT NULL,
    terminal_ids TEXT[] NOT NULL,
    terminal_times BIGINT[] NOT NULL,
    deleted_at BIGINT,
    PRIMARY KEY (scope_id, generation, identity_key, revision),
    FOREIGN KEY (scope_id, generation) REFERENCES central_capacity_policy(scope_id, generation),
    CHECK (login_uid IS NOT NULL OR login_sub IS NOT NULL),
    CHECK (cardinality(seen_ids) = cardinality(seen_times) AND cardinality(seen_ids) <= 256),
    CHECK (cardinality(failure_times) <= 256),
    CHECK (cardinality(completed_ids) = cardinality(completed_times) AND cardinality(completed_ids) <= 256),
    CHECK (cardinality(terminal_sessions) = cardinality(terminal_ids) AND cardinality(terminal_ids) = cardinality(terminal_times) AND cardinality(terminal_ids) <= 16),
    CHECK (array_position(seen_ids,NULL) IS NULL AND array_position(seen_times,NULL) IS NULL AND array_position(failure_times,NULL) IS NULL),
    CHECK (array_position(completed_ids,NULL) IS NULL AND array_position(completed_times,NULL) IS NULL),
    CHECK (array_position(terminal_sessions,NULL) IS NULL AND array_position(terminal_ids,NULL) IS NULL AND array_position(terminal_times,NULL) IS NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS central_capacity_histories_active
    ON central_capacity_histories(scope_id,generation,identity_key) WHERE deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS central_capacity_moves (
    scope_id TEXT NOT NULL CHECK (scope_id LIKE 'sessions:%'),
    generation BIGINT NOT NULL,
    session_key TEXT NOT NULL,
    event_key TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    observed_at BIGINT NOT NULL,
    failed_identity TEXT NOT NULL,
    selected_identity TEXT NOT NULL,
    deleted_at BIGINT,
    PRIMARY KEY (scope_id, generation, session_key, event_key, revision),
    FOREIGN KEY (scope_id, generation) REFERENCES central_capacity_policy(scope_id, generation)
);
CREATE UNIQUE INDEX IF NOT EXISTS central_capacity_moves_active
    ON central_capacity_moves(scope_id,generation,session_key,event_key) WHERE deleted_at IS NULL;
"#;

// Only expire the scope generation. Its typed histories and move receipts stay
// attached to this tombstone and cannot become active when the scope is reused.
const EXPIRE_SQL: &str = r#"UPDATE central_capacity_policy SET deleted_at=$1
WHERE (scope_id,generation) IN (
    SELECT scope_id,generation FROM central_capacity_policy
    WHERE expires_at < $1 AND deleted_at IS NULL
    ORDER BY expires_at LIMIT 64 FOR UPDATE SKIP LOCKED
)"#;

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
                // The bounded expiry pass commits separately from policy locks.
                db.client().await?.execute(EXPIRE_SQL, &[&now]).await?;
                let mut connection = db.admission_client().await?;
                let tx = connection.transaction().await?;
                let mut states = BTreeMap::new();
                let mut originals = BTreeMap::new();
                let mut generations = BTreeMap::new();
                for scope in &scopes {
                    // Serializes absent as well as existing scopes across replicas.
                    // Hash collisions only serialize otherwise unrelated work.
                    tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))", &[scope]).await?;
                    tx.execute("UPDATE central_capacity_policy SET deleted_at=$2 WHERE scope_id=$1 AND expires_at < $2 AND deleted_at IS NULL", &[scope,&now]).await?;
                    let active = tx.query_opt("SELECT generation,schema_version FROM central_capacity_policy WHERE scope_id=$1 AND deleted_at IS NULL FOR UPDATE", &[scope]).await?;
                    let generation = if let Some(row) = active {
                        anyhow::ensure!(row.get::<_,i16>(1) == 1, "unsupported capacity schema version");
                        row.get::<_,i64>(0)
                    } else {
                        tx.query_one("INSERT INTO central_capacity_policy(scope_id,generation,expires_at) SELECT $1,COALESCE(MAX(generation),0)+1,$2 FROM central_capacity_policy WHERE scope_id=$1 RETURNING generation", &[scope,&(now+3600)]).await?.get(0)
                    };
                    let state = load_state(&tx,scope,generation).await?;
                    originals.insert(scope.clone(),state.clone());
                    states.insert(scope.clone(),state);
                    generations.insert(scope.clone(),generation);
                }
                let result = operation(&mut states)?;
                for (scope,state) in states {
                    let before = originals.remove(&scope).context("capacity scope absent")?;
                    if state != before {
                        let generation = generations[&scope];
                        save_state(&tx,&scope,generation,&before,&state,now).await?;
                        tx.execute("UPDATE central_capacity_policy SET expires_at=$3 WHERE scope_id=$1 AND generation=$2 AND deleted_at IS NULL", &[&scope,&generation,&(now+3600)]).await?;
                    }
                }
                tx.commit().await?;
                Ok(result)
            }).await,
        }
    }
}

async fn load_state(
    tx: &tokio_postgres::Transaction<'_>,
    scope: &str,
    generation: i64,
) -> Result<State> {
    use crate::central::capacity::History;
    let mut state = State::default();
    for row in tx.query("SELECT identity_key,workspace,login_uid,login_sub,model,anchor_until,completed_at,cooling_until,seen_ids,seen_times,failure_times,completed_ids,completed_times,terminal_sessions,terminal_ids,terminal_times FROM central_capacity_histories WHERE scope_id=$1 AND generation=$2 AND deleted_at IS NULL LIMIT 129", &[&scope,&generation]).await? {
        let pairs = |keys: usize, values: usize| -> BTreeMap<String, i64> { row.get::<_,Vec<String>>(keys).into_iter().zip(row.get::<_,Vec<i64>>(values)).collect() };
        let terminal = row.get::<_,Vec<String>>(13).into_iter().zip(row.get::<_,Vec<String>>(14)).zip(row.get::<_,Vec<i64>>(15)).map(|((session,id),at)| (session,(id,at))).collect();
        state.histories.insert(row.get(0),History {
            identity: crate::central::loans::CredentialSubject { workspace:row.get(1), uid:row.get(2), sub:row.get(3) },
            model:row.get(4), anchor_until:row.get(5), completed_at:row.get(6), cooling_until:row.get(7),
            seen:pairs(8,9), failures:row.get(10), completed_ids:pairs(11,12), terminal,
        });
    }
    anyhow::ensure!(state.histories.len() <= 128, "capacity history limit");
    let rows = tx.query("SELECT session_key,observed_at,failed_identity,selected_identity,event_key FROM central_capacity_moves WHERE scope_id=$1 AND generation=$2 AND deleted_at IS NULL ORDER BY observed_at,event_key LIMIT 385", &[&scope,&generation]).await?;
    anyhow::ensure!(rows.len() <= 384, "capacity move limit");
    for row in rows {
        let session = state.sessions.entry(row.get(0)).or_default();
        session
            .moves
            .push((row.get(1), row.get(2), row.get(3), row.get(4)));
        anyhow::ensure!(session.moves.len() <= 3, "capacity session move limit");
    }
    anyhow::ensure!(state.sessions.len() <= 128, "capacity session limit");
    Ok(state)
}

async fn save_state(
    tx: &tokio_postgres::Transaction<'_>,
    scope: &str,
    generation: i64,
    before: &State,
    state: &State,
    now: i64,
) -> Result<()> {
    for key in before
        .histories
        .keys()
        .filter(|key| !state.histories.contains_key(*key))
    {
        tx.execute("UPDATE central_capacity_histories SET deleted_at=$4 WHERE scope_id=$1 AND generation=$2 AND identity_key=$3 AND deleted_at IS NULL", &[&scope,&generation,key,&now]).await?;
    }
    for (key, history) in &state.histories {
        if before.histories.get(key) == Some(history) {
            continue;
        }
        let seen_ids = history.seen.keys().cloned().collect::<Vec<_>>();
        let seen_times = history.seen.values().copied().collect::<Vec<_>>();
        let completed_ids = history.completed_ids.keys().cloned().collect::<Vec<_>>();
        let completed_times = history.completed_ids.values().copied().collect::<Vec<_>>();
        let terminal_sessions = history.terminal.keys().cloned().collect::<Vec<_>>();
        let terminal_ids = history
            .terminal
            .values()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let terminal_times = history
            .terminal
            .values()
            .map(|(_, at)| *at)
            .collect::<Vec<_>>();
        tx.execute("INSERT INTO central_capacity_histories(scope_id,generation,identity_key,workspace,login_uid,login_sub,model,anchor_until,completed_at,cooling_until,seen_ids,seen_times,failure_times,completed_ids,completed_times,terminal_sessions,terminal_ids,terminal_times,revision) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,COALESCE(MAX(revision),0)+1 FROM central_capacity_histories WHERE scope_id=$1 AND generation=$2 AND identity_key=$3 ON CONFLICT(scope_id,generation,identity_key) WHERE deleted_at IS NULL DO UPDATE SET workspace=$4,login_uid=$5,login_sub=$6,model=$7,anchor_until=$8,completed_at=$9,cooling_until=$10,seen_ids=$11,seen_times=$12,failure_times=$13,completed_ids=$14,completed_times=$15,terminal_sessions=$16,terminal_ids=$17,terminal_times=$18", &[&scope,&generation,key,&history.identity.workspace,&history.identity.uid,&history.identity.sub,&history.model,&history.anchor_until,&history.completed_at,&history.cooling_until,&seen_ids,&seen_times,&history.failures,&completed_ids,&completed_times,&terminal_sessions,&terminal_ids,&terminal_times]).await?;
    }
    for (session, previous) in &before.sessions {
        for (_, _, _, event) in &previous.moves {
            if state
                .sessions
                .get(session)
                .is_none_or(|s| !s.moves.iter().any(|(_, _, _, id)| id == event))
            {
                tx.execute("UPDATE central_capacity_moves SET deleted_at=$5 WHERE scope_id=$1 AND generation=$2 AND session_key=$3 AND event_key=$4 AND deleted_at IS NULL", &[&scope,&generation,session,event,&now]).await?;
            }
        }
    }
    for (session, moves) in &state.sessions {
        if before.sessions.get(session) == Some(moves) {
            continue;
        }
        for (at, failed, selected, event) in &moves.moves {
            tx.execute("INSERT INTO central_capacity_moves(scope_id,generation,session_key,event_key,observed_at,failed_identity,selected_identity,revision) SELECT $1,$2,$3,$4,$5,$6,$7,COALESCE(MAX(revision),0)+1 FROM central_capacity_moves WHERE scope_id=$1 AND generation=$2 AND session_key=$3 AND event_key=$4 ON CONFLICT(scope_id,generation,session_key,event_key) WHERE deleted_at IS NULL DO UPDATE SET observed_at=$5,failed_identity=$6,selected_identity=$7", &[&scope,&generation,session,event,at,failed,selected]).await?;
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "central-real-db-tests"))]
mod tests {
    use super::*;
    use crate::central::capacity::{
        CapacityAccount, CapacityRecovery, Evidence, Outcome, PlanRequest,
    };
    #[tokio::test]
    async fn capacity_rows_are_typed_versioned_and_expiry_retains_history() {
        if std::env::var("DATABASE_URL").is_err() {
            assert_ne!(std::env::var("CI").as_deref(), Ok("true"));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[22; 32]).unwrap();
        let base = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (store, control, schema) = base.isolated_test_schema().await.unwrap();
        super::super::test_support::bootstrap(&store).await.unwrap();
        let policy = CapacityRecovery::new(store.clone(), "test".into());
        let model = "gpt-6.1-sol";
        for n in 0..3 {
            policy
                .observe(
                    Evidence {
                        alias: "a".into(),
                        model: model.into(),
                        event_id: format!("failure-{n}"),
                        outcome: Outcome::Overloaded,
                        session_id: None,
                        observed_at: None,
                        identity: Some(crate::central::loans::CredentialSubject {
                            workspace: "workspace".into(),
                            uid: None,
                            sub: Some("a".into()),
                        }),
                    },
                    1000 + n,
                )
                .await
                .unwrap();
        }
        let session_id = "019e8489-aa28-7071-ab90-16b81c7cfd1d";
        policy
            .observe(
                Evidence {
                    alias: "a".into(),
                    model: model.into(),
                    event_id: "terminal".into(),
                    outcome: Outcome::Terminal,
                    session_id: Some(session_id.into()),
                    observed_at: None,
                    identity: Some(crate::central::loans::CredentialSubject {
                        workspace: "workspace".into(),
                        uid: None,
                        sub: Some("a".into()),
                    }),
                },
                1003,
            )
            .await
            .unwrap();
        let accounts = || {
            ["a","b"].into_iter().map(|alias| CapacityAccount {
            account: serde_json::from_value(serde_json::json!({"userId":"test","alias":alias,"accountId":alias,"plan":"pro","billingClass":"rate_limited","primaryUsed":1,"available":true,"usageScore":1,"liveSessions":0})).unwrap(),
            identity: crate::central::loans::CredentialSubject {workspace:"workspace".into(),uid:None,sub:Some(alias.into())},
        }).collect()
        };
        let request = || PlanRequest {
            model: model.into(),
            session_id: Some(session_id.into()),
            failed_alias: Some("a".into()),
            tried: vec!["a".into()],
            recent_429_rates: Default::default(),
        };
        assert_eq!(
            policy
                .plan(request(), accounts(), 1004)
                .await
                .unwrap()
                .alias
                .as_deref(),
            Some("b")
        );
        let mut replica = store.clone();
        if let CentralStore::Postgres(db) = &mut replica {
            db.admission = Arc::new(tokio::sync::Mutex::new(None));
        }
        let other = CapacityRecovery::new(replica.clone(), "test".into());
        assert_eq!(
            other
                .plan(request(), accounts(), 1005)
                .await
                .unwrap()
                .alias
                .as_deref(),
            Some("b")
        );
        assert_eq!(
            control
                .query_one(
                    "SELECT count(*) FROM central_capacity_moves WHERE deleted_at IS NULL",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        // This is the persistence contract: SQL can read and constrain policy
        // fields without decoding an application-owned document.
        let row = control.query_one("SELECT p.schema_version,h.cooling_until,cardinality(h.failure_times) FROM central_capacity_policy p JOIN central_capacity_histories h USING(scope_id,generation) WHERE p.deleted_at IS NULL AND h.deleted_at IS NULL AND h.login_sub='a'", &[]).await.unwrap();
        assert_eq!(row.get::<_, i16>(0), 1);
        assert_eq!(row.get::<_, i64>(1), 1302);
        assert_eq!(row.get::<_, i32>(2), 3);
        let version = control
            .execute("UPDATE central_capacity_policy SET schema_version=0", &[])
            .await
            .unwrap_err();
        assert_eq!(
            version.code(),
            Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION)
        );
        let malformed = control
            .execute(
                "UPDATE central_capacity_histories SET seen_times=ARRAY[]::bigint[]",
                &[],
            )
            .await
            .unwrap_err();
        assert_eq!(
            malformed.code(),
            Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION)
        );
        // Pruning and then observing the same account must retain its old
        // tombstone rather than overwrite it under the stable identity key.
        policy
            .plan(
                PlanRequest {
                    model: model.into(),
                    session_id: None,
                    failed_alias: None,
                    tried: vec![],
                    recent_429_rates: Default::default(),
                },
                accounts(),
                1303,
            )
            .await
            .unwrap();
        policy
            .observe(
                Evidence {
                    alias: "a".into(),
                    model: model.into(),
                    event_id: "after-pruning".into(),
                    outcome: Outcome::Completed,
                    session_id: None,
                    observed_at: None,
                    identity: Some(crate::central::loans::CredentialSubject {
                        workspace: "workspace".into(),
                        uid: None,
                        sub: Some("a".into()),
                    }),
                },
                1304,
            )
            .await
            .unwrap();
        let versions = control.query("SELECT revision,deleted_at FROM central_capacity_histories WHERE login_sub='a' ORDER BY revision",&[]).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].get::<_, Option<i64>>(1), Some(1303));
        assert_eq!(versions[1].get::<_, i64>(0), 2);
        assert_eq!(versions[1].get::<_, Option<i64>>(1), None);
        // An ordinary operation expires the old generation and opens a fresh
        // scope. The old typed rows and their expiry marker remain readable.
        let scope = format!("model:{model}");
        let (left, right) = tokio::join!(
            store.capacity_state(&scope, 5000, |_| Ok(())),
            replica.capacity_state(&scope, 5000, |_| Ok(())),
        );
        left.unwrap();
        right.unwrap();
        let old = control.query_one("SELECT p.deleted_at,h.cooling_until FROM central_capacity_policy p JOIN central_capacity_histories h USING(scope_id,generation) WHERE p.generation=1 AND h.login_sub='a' AND h.revision=1", &[]).await.unwrap();
        assert_eq!(old.get::<_, Option<i64>>(0), Some(5000));
        assert_eq!(old.get::<_, i64>(1), 1302);
        let live = control
            .query_one(
                "SELECT generation FROM central_capacity_policy WHERE deleted_at IS NULL",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(live.get::<_, i64>(0), 2);
        let active = control.query_one("SELECT count(*) FROM central_capacity_histories h JOIN central_capacity_policy p USING(scope_id,generation) WHERE p.deleted_at IS NULL AND h.deleted_at IS NULL", &[]).await.unwrap();
        assert_eq!(active.get::<_, i64>(0), 0);
        let retained_move = control.query_one("SELECT p.deleted_at,m.observed_at FROM central_capacity_moves m JOIN central_capacity_policy p USING(scope_id,generation)",&[]).await.unwrap();
        assert_eq!(retained_move.get::<_, Option<i64>>(0), Some(5000));
        assert_eq!(retained_move.get::<_, i64>(1), 1004);
        // The active generation survives a second independent connection.
        replica
            .capacity_state(&format!("model:{model}"), 5001, |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            control
                .query_one("SELECT count(*) FROM central_capacity_policy WHERE scope_id='model:gpt-6.1-sol'", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2
        );
        control
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

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
