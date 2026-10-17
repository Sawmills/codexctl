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
    let mut sample = match Command::new("/usr/bin/sample")
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
            Ok(Some(status)) => return if status.success() { "captured" } else { "exit" },
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

    #[cfg(target_os = "macos")]
    #[test]
    fn native_self_sample_is_saved_in_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("diag");
        assert_eq!(capture(&directory), "captured");
        let files = std::fs::read_dir(directory)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let metadata = files[0].metadata().unwrap();
        assert!(metadata.len() > 0);
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
