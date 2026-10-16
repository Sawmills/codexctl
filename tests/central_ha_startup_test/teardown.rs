//! Teardown must release only this fixture's remaining database work.
use super::*;

#[tokio::test]
async fn postgres_fixture_teardown_closes_remaining_sessions_before_schema_drop() {
    let mut f = login_fixture().await;
    let schema = f.schema.clone();
    let mut database = reqwest::Url::parse(&f.database).unwrap();
    database
        .query_pairs_mut()
        .append_pair("application_name", &schema);
    let (remaining, connection) = tokio_postgres::connect(database.as_str(), tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let remaining_pid: i32 = remaining
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    remaining
        .batch_execute("BEGIN; LOCK account_live_sessions IN ACCESS SHARE MODE")
        .await
        .unwrap();

    // A similar name must not make an unrelated client a teardown target.
    database
        .query_pairs_mut()
        .append_pair("application_name", &format!("{schema}_other"));
    let (other, other_connection) =
        tokio_postgres::connect(database.as_str(), tokio_postgres::NoTls)
            .await
            .unwrap();
    let other_connection = tokio::spawn(other_connection);
    // Keep the assertion's budget specific to database cleanup, rather than
    // including both servers' separate graceful-shutdown deadlines.
    f.first.stop().await;
    f.second.stop().await;
    let mut teardown = Box::pin(stop_fixture(f));
    let completed = timeout(Duration::from_secs(8), teardown.as_mut())
        .await
        .is_ok();
    if !completed {
        // Clean up the injected blocker even when testing the unfixed path.
        other
            .query_one("SELECT pg_terminate_backend($1, 5000)", &[&remaining_pid])
            .await
            .unwrap();
        timeout(Duration::from_secs(5), teardown.as_mut())
            .await
            .expect("teardown must finish after the regression blocker is released");
    }
    assert!(
        completed,
        "fixture teardown must close remaining sessions before DROP SCHEMA"
    );
    let terminated = connection.await.unwrap().unwrap_err();
    assert_eq!(
        terminated.code(),
        Some(&tokio_postgres::error::SqlState::ADMIN_SHUTDOWN)
    );
    assert!(
        other
            .query_one("SELECT to_regnamespace($1) IS NULL", &[&schema])
            .await
            .unwrap()
            .get::<_, bool>(0),
        "the fixture schema must be gone and the unrelated client must remain usable"
    );
    drop(other);
    other_connection.await.unwrap().unwrap();
}
