//! Process CPU diagnostics independent of Tokio's ability to poll tasks.
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::json;

use super::Log;

const WINDOW: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(60 * 60);
#[cfg(any(target_os = "macos", test))]
const MAX_SAMPLE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(any(target_os = "macos", test))]
const MAX_SAMPLES: usize = 8;

#[cfg(any(target_os = "macos", test))]
fn prune_samples(
    directory: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
) -> io::Result<()> {
    let mut retained = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".txt")) else {
            continue;
        };
        let Ok(at) = chrono::NaiveDateTime::parse_from_str(stem, "%Y%m%dT%H%M%S%.3fZ") else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        if at.and_utc() < now - chrono::Duration::days(7)
            || entry.metadata()?.len() > MAX_SAMPLE_BYTES
        {
            std::fs::remove_file(entry.path())?;
        } else {
            retained.push((at, entry.path()));
        }
    }
    retained.sort_unstable_by_key(|(at, _)| *at);
    let remove = retained.len().saturating_sub(MAX_SAMPLES - 1);
    for (_, path) in retained.into_iter().take(remove) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn limit_sample_output(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: after fork this closure only calls setrlimit and reads errno.
    // Limits affect the sampler child only; core dumps are disabled there.
    unsafe {
        command.pre_exec(|| {
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let size = libc::rlimit {
                rlim_cur: MAX_SAMPLE_BYTES as libc::rlim_t,
                rlim_max: MAX_SAMPLE_BYTES as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0
                || libc::setrlimit(libc::RLIMIT_FSIZE, &size) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(super) struct Watchdog {
    // Dropping the sender wakes the OS thread and releases its runtime handle.
    _stop: mpsc::Sender<()>,
}

impl Watchdog {
    pub(super) fn start(directory: PathBuf, log: Log, streams: Arc<AtomicU64>) -> Result<Self> {
        let metrics = tokio::runtime::Handle::current().metrics();
        Self::spawn(WINDOW, cpu_time, move |observation| match observation {
            Ok(percent) => {
                log(&json!({
                    "outcome": "cpu_watchdog", "cpu_percent": percent,
                    "workers": metrics.num_workers(), "alive_tasks": metrics.num_alive_tasks(),
                    "global_queue_depth": metrics.global_queue_depth(),
                    "open_streams": streams.load(Ordering::Relaxed),
                })
                .to_string());
                let result = capture(&directory);
                log(&json!({"outcome": "cpu_watchdog_sample", "reason": result}).to_string());
            }
            Err(_) => {
                log(&json!({"outcome": "cpu_watchdog_failed", "reason": "cpu_read"}).to_string())
            }
        })
    }

    fn spawn(
        interval: Duration,
        mut usage: impl FnMut() -> io::Result<Duration> + Send + 'static,
        mut report: impl FnMut(io::Result<f64>) + Send + 'static,
    ) -> Result<Self> {
        let (stop, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name("codexctl-relay-watchdog".into())
            .spawn(move || {
                let first = match usage() {
                    Ok(cpu) => cpu,
                    Err(error) => {
                        report(Err(error));
                        return;
                    }
                };
                let mut windows = CpuWindows::new(first, Instant::now());
                while matches!(
                    stopped.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let cpu = match usage() {
                        Ok(cpu) => cpu,
                        Err(error) => {
                            report(Err(error));
                            break;
                        }
                    };
                    if let Some(percent) = windows.observe(cpu, Instant::now()) {
                        report(Ok(percent));
                    }
                }
            })
            .context("could not start relay CPU watchdog")?;
        Ok(Self { _stop: stop })
    }
}

struct CpuWindows {
    last_cpu: Duration,
    last_at: Instant,
    hot: u8,
    last_sample: Option<Instant>,
}

impl CpuWindows {
    fn new(cpu: Duration, now: Instant) -> Self {
        Self {
            last_cpu: cpu,
            last_at: now,
            hot: 0,
            last_sample: None,
        }
    }

    fn observe(&mut self, cpu: Duration, now: Instant) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.last_at).as_secs_f64();
        let consumed = cpu.saturating_sub(self.last_cpu).as_secs_f64();
        self.last_cpu = cpu;
        self.last_at = now;
        let percent = 100.0 * consumed / elapsed;
        self.hot = if percent > 150.0 {
            self.hot.saturating_add(1)
        } else {
            0
        };
        if self.hot >= 3
            && self
                .last_sample
                .is_none_or(|last| now.saturating_duration_since(last) >= COOLDOWN)
        {
            self.hot = 0;
            self.last_sample = Some(now);
            Some(percent)
        } else {
            None
        }
    }
}

fn cpu_time() -> io::Result<Duration> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage receives a valid writable rusage pointer.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful getrusage initializes usage.
    let usage = unsafe { usage.assume_init() };
    let duration = |value: libc::timeval| {
        Duration::from_secs(value.tv_sec as u64) + Duration::from_micros(value.tv_usec as u64)
    };
    Ok(duration(usage.ru_utime) + duration(usage.ru_stime))
}

#[cfg(target_os = "macos")]
fn capture(directory: &std::path::Path) -> &'static str {
    use std::{
        fs::OpenOptions,
        os::unix::fs::OpenOptionsExt,
        process::{Command, Stdio},
    };
    if crate::store::ensure_private_dir(directory).is_err() {
        return "directory";
    }
    if prune_samples(directory, chrono::Utc::now()).is_err() {
        return "retention";
    }
    let path = directory.join(format!(
        "{}.txt",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ));
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .is_err()
    {
        return "file";
    }
    let mut command = Command::new("/usr/bin/sample");
    limit_sample_output(&mut command);
    let mut sample = match command
        .arg(std::process::id().to_string())
        .arg("3")
        .arg("-file")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(sample) => sample,
        Err(_) => return "spawn",
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match sample.try_wait() {
            Ok(Some(status)) => {
                return if std::fs::metadata(&path).is_ok_and(|file| file.len() >= MAX_SAMPLE_BYTES)
                {
                    "size_limit"
                } else if status.success() {
                    "captured"
                } else {
                    "exit"
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            result => {
                let _ = sample.kill();
                let _ = sample.wait();
                return if result.is_err() { "wait" } else { "timeout" };
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn capture(_directory: &std::path::Path) -> &'static str {
    "unsupported_platform"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_retention_bounds_count_age_and_bytes_without_touching_other_files() {
        use chrono::TimeZone;
        let root = tempfile::tempdir().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 17, 0, 0, 0).unwrap();
        let name =
            |at: chrono::DateTime<chrono::Utc>| format!("{}.txt", at.format("%Y%m%dT%H%M%S%.3fZ"));
        let expired = root.path().join(name(now - chrono::Duration::days(8)));
        std::fs::write(&expired, b"expired").unwrap();
        let oversized = root.path().join(name(now - chrono::Duration::minutes(1)));
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_SAMPLE_BYTES + 1)
            .unwrap();
        for hour in 1..=24 {
            std::fs::write(
                root.path().join(name(now - chrono::Duration::hours(hour))),
                b"sample",
            )
            .unwrap();
        }
        let unrelated = root.path().join("keep-me.txt");
        std::fs::write(&unrelated, b"unrelated").unwrap();
        prune_samples(root.path(), now).unwrap();
        assert!(!expired.exists(), "expired diagnostic remains");
        assert!(!oversized.exists(), "oversized diagnostic remains");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), MAX_SAMPLES);
        // Seven latest samples plus the unrelated file; one slot is reserved
        // for the new capture, which brings retained diagnostics to eight.
        for hour in 1..MAX_SAMPLES {
            assert!(
                root.path()
                    .join(name(now - chrono::Duration::hours(hour as i64)))
                    .exists()
            );
        }
        assert_eq!(std::fs::read(unrelated).unwrap(), b"unrelated");
    }

    #[test]
    fn child_sample_cannot_write_more_than_four_mib() {
        use std::process::{Command, Stdio};
        let parent_limits = || {
            [libc::RLIMIT_CORE, libc::RLIMIT_FSIZE].map(|kind| {
                let mut limits = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // SAFETY: getrlimit receives a valid writable rlimit.
                assert_eq!(unsafe { libc::getrlimit(kind, &mut limits) }, 0);
                (limits.rlim_cur, limits.rlim_max)
            })
        };
        let before = parent_limits();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("sample.txt");
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import sys; open(sys.argv[1], 'wb').write(b'x' * (8 * 1024 * 1024))",
            ])
            .arg(&output)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        limit_sample_output(&mut command);
        let status = command.status().unwrap();
        assert_eq!(parent_limits(), before, "sampler changed parent limits");
        assert!(!status.success(), "unbounded sample write succeeded");
        assert!(std::fs::metadata(output).unwrap().len() <= MAX_SAMPLE_BYTES);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_self_sample_is_saved_in_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("diag");
        std::fs::create_dir(&directory).unwrap();
        let expired = directory.join(format!(
            "{}.txt",
            (chrono::Utc::now() - chrono::Duration::days(8)).format("%Y%m%dT%H%M%S%.3fZ")
        ));
        std::fs::write(expired, b"expired synthetic sample").unwrap();
        assert_eq!(capture(&directory), "captured");
        let files = std::fs::read_dir(directory)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let metadata = files[0].metadata().unwrap();
        assert!(metadata.len() > 0);
        assert!(metadata.len() <= MAX_SAMPLE_BYTES);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unavailable_cpu_accounting_reports_once_and_stops() {
        let (reported, received) = mpsc::channel();
        let watchdog = Watchdog::spawn(
            Duration::from_millis(10),
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            move |result| {
                reported.send(result.unwrap_err().kind()).unwrap();
            },
        )
        .unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        drop(watchdog);
    }

    #[test]
    fn needs_three_hot_windows_and_rate_limits_samples_for_an_hour() {
        let start = Instant::now();
        let mut windows = CpuWindows::new(Duration::ZERO, start);
        for index in 1..=3 {
            let sample = windows.observe(
                Duration::from_secs(index * 16),
                start + Duration::from_secs(index * 10),
            );
            assert_eq!(sample.is_some(), index == 3);
        }
        assert!(
            windows
                .observe(Duration::from_secs(64), start + Duration::from_secs(40))
                .is_none()
        );
        assert!(
            windows
                .observe(Duration::from_secs(80), start + Duration::from_secs(50))
                .is_none()
        );
        assert!(
            windows
                .observe(Duration::from_secs(96), start + Duration::from_secs(60))
                .is_none()
        );
        // Consecutive hot observations continue through the cooldown.
        assert!(
            windows
                .observe(Duration::from_secs(5808), start + Duration::from_secs(3630))
                .is_some()
        );
    }

    #[test]
    fn a_cool_window_resets_the_streak_and_150_percent_is_not_hot() {
        let start = Instant::now();
        let mut windows = CpuWindows::new(Duration::ZERO, start);
        for (seconds, cpu) in [(10, 16), (20, 32), (30, 47), (40, 63), (50, 79)] {
            assert!(
                windows
                    .observe(
                        Duration::from_secs(cpu),
                        start + Duration::from_secs(seconds)
                    )
                    .is_none()
            );
        }
        assert!(
            windows
                .observe(Duration::from_secs(95), start + Duration::from_secs(60))
                .is_some()
        );
    }

    #[test]
    fn watchdog_runs_even_when_the_tokio_runtime_is_not_polled() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let (sampled, received) = std::sync::mpsc::channel();
        let mut cpu = Duration::ZERO;
        let watchdog = Watchdog::spawn(
            Duration::from_millis(10),
            move || {
                cpu += Duration::from_millis(100);
                Ok(cpu)
            },
            move |percent| {
                sampled.send(percent.unwrap()).unwrap();
            },
        )
        .unwrap();
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap() > 150.0);
        drop(watchdog);
        // Disconnection proves the independent thread observes shutdown.
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
