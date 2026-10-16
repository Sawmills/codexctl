//! Admission and child lifetime for ordinary HA storage preparation.

use super::command;
use std::path::Path;
use tokio::process::Command;
use tokio::sync::Semaphore;

static ADMISSION: Semaphore = Semaphore::const_new(1);

pub(super) async fn prepare(database: &str, state: &Path, key: &Path) {
    execute(vec![
        ("migrate", command(database, state, key, "migrate")),
        ("backfill", command(database, state, key, "backfill")),
    ])
    .await;
}

pub(super) async fn migrate(database: &str, state: &Path, key: &Path) {
    execute(vec![("migrate", command(database, state, key, "migrate"))]).await;
}

pub(super) async fn confirm_legacy_owners_settled(
    database: &str,
    state: &Path,
    key: &Path,
) -> serde_json::Value {
    let mut migration = command(database, state, key, "migrate");
    migration.arg("--confirm-legacy-owners-settled");
    let outputs = execute(vec![("migrate", migration)]).await;
    serde_json::from_slice(&outputs[0].stdout).unwrap()
}

async fn execute(commands: Vec<(&'static str, Command)>) -> Vec<std::process::Output> {
    // Admission does not consume the real CLI's operation budget.
    let _slot = ADMISSION.acquire().await.unwrap();
    let mut outputs = Vec::with_capacity(commands.len());
    for (operation, mut command) in commands {
        let output = command.kill_on_drop(true).output().await.unwrap();
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(output);
    }
    outputs
}

mod tests {
    use super::*;
    use std::{path::PathBuf, time::Duration};
    use tokio::time::timeout;

    struct Case {
        _root: tempfile::TempDir,
        state: PathBuf,
        key: PathBuf,
        database: String,
        schema: String,
        control: tokio_postgres::Client,
        connection: tokio::task::JoinHandle<()>,
    }

    impl Case {
        async fn new(database: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let state = root.path().join("state");
            let key = root.path().join("key");
            codexctl::central::managed::setup(&state, &key).unwrap();
            let (control, connection) = tokio_postgres::connect(database, tokio_postgres::NoTls)
                .await
                .unwrap();
            let connection = tokio::spawn(async move { connection.await.unwrap() });
            let schema = format!(
                "fixture_admission_{}",
                codexctl::central::enrollment::random_bytes()[..8]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            control
                .batch_execute(&format!(
                    "CREATE SCHEMA {schema}; SET search_path TO {schema}"
                ))
                .await
                .unwrap();
            let mut database = reqwest::Url::parse(database).unwrap();
            database
                .query_pairs_mut()
                .append_pair("options", &format!("-csearch_path={schema}"))
                .append_pair("application_name", &schema);
            Self {
                _root: root,
                state,
                key,
                database: database.to_string(),
                schema,
                control,
                connection,
            }
        }

        async fn finish(self) {
            self.control
                .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
                .await
                .unwrap();
            self.connection.abort();
        }
    }

    fn observed_migrate(case: &Case, pid_file: &Path) -> Command {
        let real = command(&case.database, &case.state, &case.key, "migrate");
        let mut observed = Command::new("sh");
        observed
            .args([
                "-c",
                "printf '%s\\n' \"$$\" > \"$1\"; shift; exec \"$@\"",
                "fixture-cli",
            ])
            .arg(pid_file)
            .arg(real.as_std().get_program())
            .args(real.as_std().get_args());
        for (name, value) in real.as_std().get_envs() {
            if let Some(value) = value {
                observed.env(name, value);
            } else {
                observed.env_remove(name);
            }
        }
        observed
    }

    #[tokio::test]
    async fn cancelled_admission_never_starts_a_cli_and_the_next_fixture_progresses() {
        let database = std::env::var("DATABASE_URL").expect("owned DATABASE_URL required");
        let database = super::super::backfill_test_database_named(&database, true).await;
        let case = Case::new(&database).await;
        let cancelled_pid = case._root.path().join("cancelled.pid");
        let following_pid = case._root.path().join("following.pid");
        let gate = ADMISSION.acquire().await.unwrap();
        let mut cancelled = Box::pin(execute(vec![(
            "migrate",
            observed_migrate(&case, &cancelled_pid),
        )]));
        assert!(
            timeout(Duration::from_millis(2100), cancelled.as_mut())
                .await
                .is_err(),
            "queued preparation started inside the production operation budget"
        );
        assert!(
            !cancelled_pid.exists(),
            "admission must precede spawning the CLI"
        );
        drop(cancelled);
        let mut following = Box::pin(execute(vec![(
            "migrate",
            observed_migrate(&case, &following_pid),
        )]));
        assert!(futures::poll!(following.as_mut()).is_pending());
        assert!(!following_pid.exists());
        drop(gate);
        following.await;
        assert!(following_pid.exists());
        case.finish().await;
    }

    #[tokio::test]
    async fn ordinary_ha_preparations_wait_before_the_cli_migration_budget() {
        let database = std::env::var("DATABASE_URL").expect("owned DATABASE_URL required");
        let database = super::super::backfill_test_database_named(&database, true).await;
        let first = Case::new(&database).await;
        let next = Case::new(&database).await;
        prepare(&first.database, &first.state, &first.key).await;
        first.control.batch_execute("CREATE TABLE fixture_delays(elapsed INTERVAL NOT NULL); CREATE FUNCTION slow_fixture_migration() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE started TIMESTAMPTZ; BEGIN IF NEW.version=1 THEN started=clock_timestamp(); PERFORM pg_sleep(1); INSERT INTO fixture_delays VALUES(clock_timestamp()-started); END IF; RETURN NEW; END $$; CREATE TRIGGER slow_fixture_migration BEFORE INSERT ON central_schema_migrations FOR EACH ROW EXECUTE FUNCTION slow_fixture_migration()").await.unwrap();
        tokio::join!(
            prepare(&first.database, &first.state, &first.key),
            prepare(&next.database, &next.state, &next.key)
        );
        let slow: bool = first
            .control
            .query_one(
                "SELECT max(elapsed)>interval '100 milliseconds' FROM fixture_delays",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            slow,
            "actual DDL exceeded the unchanged production lock timeout"
        );
        first.finish().await;
        next.finish().await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn cancelling_running_preparation_terminates_its_cli_and_releases_admission() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // A stable handle also cleans up failed negative controls without PID reuse.
        struct Cleanup(OwnedFd);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }

        let database = std::env::var("DATABASE_URL").expect("owned DATABASE_URL required");
        let database = super::super::backfill_test_database_named(&database, true).await;
        let case = Case::new(&database).await;
        prepare(&case.database, &case.state, &case.key).await;
        case.control
            .batch_execute(
                "BEGIN; SELECT pg_advisory_xact_lock(hashtextextended(current_schema(),12484))",
            )
            .await
            .unwrap();
        let pid_file = case._root.path().join("running.pid");
        let mut running = Box::pin(execute(vec![(
            "migrate",
            observed_migrate(&case, &pid_file),
        )]));
        assert!(futures::poll!(running.as_mut()).is_pending());
        let observation = async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let pid = tokio::select! {
            _ = running.as_mut() => panic!("CLI completed before the controlled lock wait"),
            observed = observation => observed,
        };
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(fd >= 0, "actual CLI must have a stable process handle");
        let cleanup = Cleanup(unsafe { OwnedFd::from_raw_fd(fd) });
        timeout(Duration::from_secs(5), async {
            loop {
                case.control.batch_execute("SELECT pg_stat_clear_snapshot()").await.unwrap();
                let waiting: bool = case.control.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock')",
                    &[&case.schema],
                ).await.unwrap().get(0);
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("actual CLI reaches controlled admission lock");
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    cleanup.0.as_raw_fd(),
                    libc::SIGSTOP,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
        let mut process = libc::pollfd {
            fd: cleanup.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut process, 1, 0) },
            0,
            "controlled CLI must still be alive before cancellation"
        );
        case.control
            .batch_execute("SELECT pg_stat_clear_snapshot()")
            .await
            .unwrap();
        let connected: bool = case
            .control
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1)",
                &[&case.schema],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            connected,
            "controlled CLI must own database work before cancellation"
        );
        drop(running);
        let terminated =
            timeout(Duration::from_secs(5), async {
                loop {
                    case.control
                        .batch_execute("SELECT pg_stat_clear_snapshot()")
                        .await
                        .unwrap();
                    let mut process = libc::pollfd {
                        fd: cleanup.0.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let exited = unsafe { libc::poll(&mut process, 1, 0) } == 1;
                    let connected: bool = case.control.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1)",
                    &[&case.schema],
                ).await.unwrap().get(0);
                    if exited && !connected {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok();
        drop(cleanup);
        case.control.batch_execute("ROLLBACK").await.unwrap();
        assert!(
            terminated,
            "cancelled preparation left its stopped CLI or database work alive"
        );
        prepare(&case.database, &case.state, &case.key).await;
        case.finish().await;
    }
}
