//! Exercise the migration command that HA initializers will run.
use super::*;

async fn old_pods_serve(f: &BrowserFixture, session: &str) {
    for pod in [&f.first, &f.second] {
        assert_eq!(
            f.http
                .get(format!("{}/ready", pod.url))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            f.http
                .get(format!("{}/accounts/data", pod.url))
                .header("cookie", session)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
}

fn assert_bounded_lock_timeout(stderr: &[u8]) {
    let stderr = String::from_utf8_lossy(stderr);
    let logs: Vec<_> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["operation"] == "schema_migration")
        .collect();
    assert_eq!(
        logs,
        [
            json!({"operation":"schema_migration","stage":"ddl","failure_class":"lock_timeout","sqlstate":"55P03"})
        ]
    );
    assert!(!stderr.contains("dashboard@example.invalid"));
}

#[tokio::test]
async fn postgres_schema_migration_contended_table_does_not_stall_old_pods() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.control
        .batch_execute(&format!(
            "BEGIN; SELECT 1 FROM {}.browser_sessions",
            f.schema
        ))
        .await
        .unwrap();
    let child = command(
        &f.database(),
        &f._root.path().join("state"),
        &f._root.path().join("key"),
        "migrate",
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = f.control.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation=$1::text::regclass AND mode='AccessExclusiveLock' AND NOT granted)",
                &[&format!("{}.browser_sessions", f.schema)]
            ).await.unwrap().get(0);
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("migration reached the serving table lock");
    let started = std::time::Instant::now();
    old_pods_serve(&f, &session).await;
    let elapsed = started.elapsed();
    let output = child.wait_with_output().await.unwrap();
    f.control.batch_execute("ROLLBACK").await.unwrap();
    f.finish().await;
    assert!(
        !output.status.success(),
        "contended migration must fail closed"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "failed migration stalled old serving pods for {elapsed:?}"
    );
    assert_bounded_lock_timeout(&output.stderr);
    eprintln!("old-pod dashboard traffic during contended migration: {elapsed:?}");
}

#[tokio::test]
async fn postgres_schema_migration_three_racing_initializers_recover_with_one_retry() {
    let f = BrowserFixture::start().await;
    let (session, _) = f.sign_in().await;
    f.control
        .batch_execute(&format!(
            "BEGIN; SELECT 1 FROM {0}.browser_sessions",
            f.schema
        ))
        .await
        .unwrap();
    let finished = std::cell::Cell::new(false);
    let retries = async {
        let mut attempts = [0; 3];
        for wave in 0..2 {
            let results = futures::future::join_all((0..3).map(|_| async {
                let started = std::time::Instant::now();
                let output = command(
                    &f.database(),
                    &f._root.path().join("state"),
                    &f._root.path().join("key"),
                    "migrate",
                )
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
                (output, started.elapsed())
            }))
            .await;
            for (index, (output, elapsed)) in results.into_iter().enumerate() {
                attempts[index] += 1;
                if wave == 0 {
                    assert!(
                        !output.status.success(),
                        "first attempt encounters the serving read lock"
                    );
                    assert_bounded_lock_timeout(&output.stderr);
                } else {
                    assert!(
                        output.status.success(),
                        "retry: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                assert!(
                    elapsed < Duration::from_secs(2),
                    "migration attempt exceeded the DB budget: {elapsed:?}"
                );
                eprintln!(
                    "initializer {index}, attempt {}: {elapsed:?}",
                    attempts[index]
                );
            }
            if wave == 0 {
                let present: bool = f
                    .control
                    .query_one(
                        "SELECT to_regclass($1) IS NOT NULL",
                        &[&format!("{}.dashboard_audit", f.schema)],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert!(present, "failed migration preserves the current schema");
                // Model K8s starting a new init process after contention clears.
                // No retry is hidden inside the production migration command.
                f.control.batch_execute("ROLLBACK").await.unwrap();
            }
        }
        finished.set(true);
        attempts
    };
    let traffic = async {
        let mut reads = 0;
        let mut longest = Duration::ZERO;
        loop {
            let started = std::time::Instant::now();
            old_pods_serve(&f, &session).await;
            longest = longest.max(started.elapsed());
            reads += 1;
            if finished.get() {
                return (reads, longest);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let (attempts, (reads, longest)) = tokio::join!(retries, traffic);
    assert_eq!(
        attempts,
        [2, 2, 2],
        "three initializers each complete after one retry"
    );
    assert!(reads > 1, "dashboard traffic remains active across retries");
    assert!(
        longest < Duration::from_millis(500),
        "traffic stalled for {longest:?}"
    );
    let row = f.control.query_one(&format!(
        "SELECT (SELECT count(*) FROM {0}.browser_sessions), \
         (SELECT completed FROM {0}.central_identity_migration_progress WHERE stage='source_fence'), \
         to_regclass('{0}.dashboard_audit') IS NOT NULL", f.schema
    ), &[]).await.unwrap();
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "migration preserves the live session"
    );
    assert!(
        row.get::<_, bool>(1),
        "completed identity fence stays complete"
    );
    assert!(
        row.get::<_, bool>(2),
        "successful retries preserve the current schema"
    );
    for pod in [&f.first, &f.second] {
        assert_eq!(pod.launches(), 0);
        assert_eq!(
            std::fs::read_to_string(pod.root.path().join("count")).unwrap(),
            "0"
        );
    }
    eprintln!(
        "all initializers completed in {attempts:?} attempts; {reads} traffic rounds, longest {longest:?}"
    );
    f.finish().await;
}
