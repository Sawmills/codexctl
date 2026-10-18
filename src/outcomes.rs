//! Final account outcomes, shared by the relay and its account-server reports.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, bail};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    RateLimited,
    Capacity,
    LoginRequired,
    TokenRefused,
}

impl StopReason {
    const ALL: [Self; 4] = [
        Self::RateLimited,
        Self::Capacity,
        Self::LoginRequired,
        Self::TokenRefused,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Capacity => "capacity",
            Self::LoginRequired => "login_required",
            Self::TokenRefused => "token_refused",
        }
    }

    fn classify(status: u16, code: Option<&str>) -> Option<Self> {
        match code {
            Some("server_is_overloaded") => Some(Self::Capacity),
            Some(
                "login_required"
                | "session_expired"
                | "invalid_grant"
                | "refresh_token_invalidated"
                | "refresh_token_expired"
                | "refresh_token_reused",
            ) => Some(Self::LoginRequired),
            Some("invalid_api_key" | "invalid_token" | "token_expired" | "token_refused") => {
                Some(Self::TokenRefused)
            }
            _ => match status {
                429 => Some(Self::RateLimited),
                401 | 403 => Some(Self::TokenRefused),
                // Codex retries streamed rate-limit codes itself. A 200 SSE
                // failure is not proof that its own retry budget was spent.
                _ => None,
            },
        }
    }
}

/// The wire schema cannot carry an alias, credential, or arbitrary reason label.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StopReport {
    pub(crate) lane_stop: StopReason,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectionPath {
    Auto,
    Explicit,
}

impl SelectionPath {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectionReport {
    pub(crate) billed_selection: SelectionPath,
}

#[derive(Clone, Default)]
pub(crate) struct Outcomes {
    stops: Arc<[AtomicU64; 4]>,
    billed: Arc<[AtomicU64; 2]>,
    report_root: Option<Arc<PathBuf>>,
}

impl Outcomes {
    pub(crate) fn with_report_root(root: PathBuf) -> Self {
        Self {
            report_root: Some(Arc::new(root)),
            ..Self::default()
        }
    }

    pub(crate) fn request(&self) -> RequestOutcome {
        RequestOutcome {
            outcomes: self.clone(),
            recorded: false,
        }
    }

    pub(crate) fn stopped(&self, report: StopReport) {
        self.stops[report.lane_stop as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn selected(&self, report: SelectionReport) {
        self.billed[report.billed_selection as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metrics(&self, prefix: &str) -> String {
        let mut text = format!("# TYPE {prefix}_lane_stops_total counter\n");
        for reason in StopReason::ALL {
            text.push_str(&format!(
                "{prefix}_lane_stops_total{{reason=\"{}\"}} {}\n",
                reason.label(),
                self.stops[reason as usize].load(Ordering::Relaxed)
            ));
        }
        if prefix == "codexctl_central" {
            text.push_str("# TYPE codexctl_central_billed_selections_total counter\n");
            for path in [SelectionPath::Auto, SelectionPath::Explicit] {
                text.push_str(&format!(
                    "codexctl_central_billed_selections_total{{path=\"{}\"}} {}\n",
                    path.label(),
                    self.billed[path as usize].load(Ordering::Relaxed)
                ));
            }
        }
        if let Some(root) = self.report_root.as_deref() {
            match report_failure_count(root) {
                Ok(count) => text.push_str(&format!("# TYPE codexctl_client_outcome_reports_failed_total counter\ncodexctl_client_outcome_reports_failed_total {count}\n")),
                Err(_) => eprintln!("outcome_report_counter_unavailable operation=read"),
            }
        }
        text
    }
}

/// Declare before mutation locks, arm only after the selection commits. Drop
/// sends once after those locks release, including a later session-repair error.
#[derive(Default)]
pub(crate) struct SelectionReceipt(Option<PendingSelection>);

struct PendingSelection {
    server: String,
    credential: Option<String>,
    report: SelectionReport,
}

impl SelectionReceipt {
    pub(crate) fn commit(
        &mut self,
        server: &str,
        credential: impl FnOnce() -> Result<String>,
        billing: Option<crate::api::BillingClass>,
        explicit: bool,
    ) {
        if self.0.is_some() || billing != Some(crate::api::BillingClass::UsageBased) {
            return;
        }
        self.0 = Some(PendingSelection {
            server: server.to_owned(),
            credential: credential().ok(),
            report: SelectionReport {
                billed_selection: if explicit {
                    SelectionPath::Explicit
                } else {
                    SelectionPath::Auto
                },
            },
        });
    }
}

impl Drop for SelectionReceipt {
    fn drop(&mut self) {
        let Some(pending) = self.0.take() else { return };
        if send_selection(&pending).is_err() {
            let count = increment_report_failures().ok();
            eprintln!(
                "{}",
                serde_json::json!({
                    "operation":"billed_selection_report_failed", "reason":"delivery_failed",
                    "path":pending.report.billed_selection, "failed_total":count,
                    "counter_available":count.is_some(),
                })
            );
        }
    }
}

fn send_selection(pending: &PendingSelection) -> Result<()> {
    let origin = crate::central::transport::origin(&pending.server)?;
    let Some(credential) = pending.credential.as_deref() else {
        bail!("credential unavailable")
    };
    // No retry, proxy or redirect; bound the independent send to 250 ms.
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_millis(100))
        .timeout(Duration::from_millis(250))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?
        .post(origin.join("v1/relay/capacity-events")?)
        .bearer_auth(credential.trim())
        .json(&pending.report)
        .send()?;
    if !response.status().is_success() {
        bail!("receipt not accepted")
    }
    Ok(())
}

fn report_failure_count(root: &Path) -> Result<u64> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("outcome-metrics/report-failures"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > 32 {
        bail!("invalid outcome counter")
    }
    let mut value = String::new();
    file.take(33).read_to_string(&mut value)?;
    if value.len() > 32 {
        bail!("invalid outcome counter")
    }
    Ok(value.trim().parse()?)
}

fn increment_report_failures() -> Result<u64> {
    let root = crate::config::default_paths()?.codexctl_dir();
    let directory = root.join("outcome-metrics");
    crate::store::ensure_private_dir(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".lock"))?;
    if !lock.metadata()?.is_file() {
        bail!("invalid outcome counter lock")
    }
    lock.try_lock()?;
    let count = report_failure_count(&root)?.saturating_add(1);
    crate::store::atomic_write(
        &directory.join("report-failures"),
        format!("{count}\n").as_bytes(),
    )?;
    Ok(count)
}

/// One HTTP request or SSE response, including repeated terminal frames.
pub(crate) struct RequestOutcome {
    outcomes: Outcomes,
    recorded: bool,
}

impl RequestOutcome {
    pub(crate) fn terminal(
        &mut self,
        status: u16,
        code: Option<&str>,
        retrying: bool,
    ) -> Option<StopReport> {
        if self.recorded || retrying {
            return None;
        }
        let report = StopReport {
            lane_stop: StopReason::classify(status, code)?,
        };
        self.recorded = true;
        self.outcomes.stopped(report);
        Some(report)
    }
}
