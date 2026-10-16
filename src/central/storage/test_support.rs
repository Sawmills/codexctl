//! Preparation of synthetic account-server storage fixtures.

use super::{CentralStore, StoreMode};
use anyhow::{Context, Result};
use tokio::sync::Semaphore;

static MIGRATIONS: Semaphore = Semaphore::const_new(1);

pub(in crate::central) async fn bootstrap(store: &CentralStore) -> Result<()> {
    // Wait outside migrate's operation budget; file fixtures never join this queue.
    let _slot = if store.mode() == StoreMode::File {
        None
    } else {
        Some(
            MIGRATIONS
                .acquire()
                .await
                .context("fixture migration queue closed")?,
        )
    };
    store.migrate().await
}

#[cfg(feature = "central-real-db-tests")]
mod tests {
    use super::*;
    use crate::central::{storage::StoreMode, vault};
    use std::time::Duration;

    #[tokio::test]
    async fn bootstrap_queues_before_budget_and_cancellation_keeps_the_next_fixture_live() {
        if std::env::var("DATABASE_URL").is_err() {
            assert_ne!(std::env::var("CI").as_deref(), Ok("true"));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        vault::create_secret(&key, &[31; 32]).unwrap();
        let shared = CentralStore::from_mode(StoreMode::Postgres, root.path(), &key)
            .await
            .unwrap();
        let (mut first, control, first_schema) = shared.isolated_test_schema().await.unwrap();
        let (mut next, observer, next_schema) = shared.isolated_test_schema().await.unwrap();
        let first_name = format!("bootstrap_{first_schema}");
        let next_name = format!("bootstrap_{next_schema}");
        for (store, name) in [(&mut first, &first_name), (&mut next, &next_name)] {
            let CentralStore::Postgres(db) = store else {
                unreachable!();
            };
            let mut url = reqwest::Url::parse(&db.url).unwrap();
            url.query_pairs_mut().append_pair("application_name", name);
            db.url = url.to_string();
        }
        bootstrap(&first).await.unwrap();
        control.batch_execute("CREATE TABLE fixture_bootstrap_delays(elapsed INTERVAL NOT NULL); CREATE FUNCTION slow_fixture_bootstrap() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE started TIMESTAMPTZ; BEGIN IF NEW.version=1 THEN started=clock_timestamp(); PERFORM pg_sleep(1); INSERT INTO fixture_bootstrap_delays VALUES(clock_timestamp()-started); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_fixture_bootstrap BEFORE INSERT ON central_schema_migrations FOR EACH ROW EXECUTE FUNCTION slow_fixture_bootstrap()").await.unwrap();

        // Fault injection belongs to this owner: keep admission closed while
        // checking callers, without racing a database timer or changing migrate.
        let gate = MIGRATIONS.acquire().await.unwrap();
        let mut first_call = Box::pin(bootstrap(&first));
        assert!(futures::poll!(first_call.as_mut()).is_pending());

        let file = CentralStore::file(&root.path().join("file"), &key);
        tokio::time::timeout(Duration::from_secs(5), bootstrap(&file))
            .await
            .expect("file preparation must not queue behind PostgreSQL")
            .unwrap();

        let mut cancelled = Box::pin(bootstrap(&next));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let connections: i64 = observer
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1",
                &[&next_name],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            connections, 0,
            "a queued bootstrap must not open a migration connection"
        );
        drop(cancelled);

        let mut following = Box::pin(bootstrap(&next));
        assert!(futures::poll!(following.as_mut()).is_pending());
        drop(gate);
        // This includes fixture admission, which is outside the production budget.
        tokio::time::timeout(Duration::from_secs(30), async {
            let (first_result, next_result) = tokio::join!(first_call, following);
            first_result.unwrap();
            next_result.unwrap();
        })
        .await
        .expect("cancelling a waiter must not retain fixture admission");
        first.require_identity_ready().await.unwrap();
        next.require_identity_ready().await.unwrap();
        let slow: bool = control
            .query_one(
                "SELECT elapsed > interval '100 milliseconds' FROM fixture_bootstrap_delays",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            slow,
            "the real migration must complete despite the controlled slow DDL"
        );
        for (client, schema) in [(control, first_schema), (observer, next_schema)] {
            client
                .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
                .await
                .unwrap();
        }
    }
}
