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
    CapacityUnconfirmed,
    LoginRequired,
    TokenRefused,
}

impl StopReason {
    const ALL: [Self; 5] = [
        Self::RateLimited,
        Self::Capacity,
        Self::CapacityUnconfirmed,
        Self::LoginRequired,
        Self::TokenRefused,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Capacity => "capacity",
            Self::CapacityUnconfirmed => "capacity_unconfirmed",
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

/// Diagnostic context never becomes a metric label. Normalize before sending
/// and again before logging an authenticated client's report.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StopReport {
    pub(crate) lane_stop: StopReason,
    http_status: Option<u16>,
    error_code: Option<String>,
    account_alias: Option<String>,
    route: Option<String>,
    request_id: Option<String>,
}

impl StopReport {
    pub(crate) fn new(reason: StopReason, status: u16, code: Option<&str>) -> Self {
        Self {
            lane_stop: reason,
            http_status: Some(status),
            error_code: code.map(error_class).map(str::to_owned),
            account_alias: None,
            route: None,
            request_id: None,
        }
    }

    pub(crate) fn with_request(
        mut self,
        account_alias: Option<&str>,
        route: &str,
        upstream_request_id: Option<&str>,
    ) -> Self {
        self.account_alias = account_alias.and_then(alias);
        self.route = stop_route(route).map(str::to_owned);
        self.request_id = upstream_request_id.and_then(request_id);
        self
    }

    fn log(&self) -> String {
        serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(), "level": "WARN",
            "outcome": "lane_stop", "reason": self.lane_stop,
            "http_status": self.http_status.filter(|s| (100..600).contains(s)),
            "error_code": self.error_code.as_deref().map(error_class),
            "account_alias": self.account_alias.as_deref().and_then(alias),
            "route": self.route.as_deref().and_then(stop_route),
            "request_id": self.request_id.as_deref().and_then(request_id),
        })
        .to_string()
    }
}

fn stop_route(value: &str) -> Option<&'static str> {
    match value {
        "/backend-api/codex/responses" => Some("/backend-api/codex/responses"),
        "/backend-api/codex/*" => Some("/backend-api/codex/*"),
        _ => None,
    }
}

pub(crate) fn alias(value: &str) -> Option<String> {
    crate::store::validate_alias(value)
        .ok()
        .filter(|value| {
            !value.starts_with("sk-") && !value.starts_with("rt-") && !value.starts_with("eyJ")
        })
        .map(str::to_owned)
}

pub(crate) fn request_id(value: &str) -> Option<String> {
    let uuid = value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let named =
        (value.starts_with("req-") || value.starts_with("req_") || value.starts_with("request-"))
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    (uuid || named).then(|| value.to_owned())
}

pub(crate) fn error_class(value: &str) -> &'static str {
    match value {
        "server_is_overloaded" => "server_is_overloaded",
        "capacity_unconfirmed" => "capacity_unconfirmed",
        "rate_limit_exceeded" => "rate_limit_exceeded",
        "slow_down" => "slow_down",
        "usage_limit_reached" => "usage_limit_reached",
        "usage_not_included" => "usage_not_included",
        "insufficient_quota" => "insufficient_quota",
        "credit_balance_exhausted" => "credit_balance_exhausted",
        "organization_spend_limit_exceeded" => "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded" => "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded" => "organization_usage_limit_exceeded",
        "flex_unavailable" => "flex_unavailable",
        "login_required" => "login_required",
        "session_expired" => "session_expired",
        "invalid_grant" => "invalid_grant",
        "refresh_token_invalidated" => "refresh_token_invalidated",
        "refresh_token_expired" => "refresh_token_expired",
        "refresh_token_reused" => "refresh_token_reused",
        "invalid_api_key" => "invalid_api_key",
        "invalid_token" => "invalid_token",
        "token_expired" => "token_expired",
        "token_refused" => "token_refused",
        _ => "other",
    }
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

type StopLog = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct Outcomes {
    stops: Arc<[AtomicU64; 5]>,
    billed: Arc<[AtomicU64; 2]>,
    report_root: Option<Arc<PathBuf>>,
    stop_log: Option<StopLog>,
}

impl Outcomes {
    pub(crate) fn with_stop_log(log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            stop_log: Some(Arc::new(log)),
            ..Self::default()
        }
    }

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

    pub(crate) fn stopped(&self, report: &StopReport) {
        self.stops[report.lane_stop as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(log) = &self.stop_log {
            log(&report.log());
        }
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
        // Codex owns HTTP 5xx retries. A relay response cannot prove their
        // client-side budget was exhausted, regardless of its error code.
        if self.recorded || retrying || status >= 500 {
            return None;
        }
        let report = StopReport::new(StopReason::classify(status, code)?, status, code);
        self.recorded = true;
        self.outcomes.stopped(&report);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    fn logged() -> (Outcomes, Arc<Mutex<Vec<Value>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let outcomes = Outcomes::with_stop_log(move |line| {
            sink.lock()
                .unwrap()
                .push(serde_json::from_str(line).unwrap());
        });
        (outcomes, lines)
    }

    #[test]
    fn terminal_stop_counts_and_logs_once_but_advised_retries_do_neither() {
        let (outcomes, lines) = logged();
        let mut request = outcomes.request();
        assert!(
            request
                .terminal(429, Some("rate_limit_exceeded"), true)
                .is_none()
        );
        assert!(lines.lock().unwrap().is_empty());
        assert!(
            request
                .terminal(429, Some("rate_limit_exceeded"), false)
                .is_some()
        );
        assert!(
            request
                .terminal(429, Some("rate_limit_exceeded"), false)
                .is_none()
        );
        let logs = lines.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["level"], "WARN");
        assert_eq!(logs[0]["reason"], "rate_limited");
        assert_eq!(logs[0]["http_status"], 429);
        assert_eq!(logs[0]["error_code"], "rate_limit_exceeded");
        drop(logs);
        assert!(
            outcomes
                .metrics("codexctl_central")
                .contains("codexctl_central_lane_stops_total{reason=\"rate_limited\"} 1\n")
        );
        assert!(
            outcomes
                .request()
                .terminal(503, Some("server_is_overloaded"), false)
                .is_none()
        );
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn accepted_stop_preserves_bounded_context_across_the_wire() {
        let (outcomes, lines) = logged();
        let report = StopReport::new(StopReason::RateLimited, 429, Some("rate_limit_exceeded"))
            .with_request(
                Some("synthetic-seat"),
                "/backend-api/codex/responses",
                Some("req-synthetic"),
            );
        let wire = serde_json::to_vec(&report).unwrap();
        outcomes.stopped(&serde_json::from_slice(&wire).unwrap());
        let logs = lines.lock().unwrap();
        assert_eq!(logs.len(), 1);
        for (key, value) in [
            ("reason", json!("rate_limited")),
            ("http_status", json!(429)),
            ("error_code", json!("rate_limit_exceeded")),
            ("account_alias", json!("synthetic-seat")),
            ("route", json!("/backend-api/codex/responses")),
            ("request_id", json!("req-synthetic")),
        ] {
            assert_eq!(logs[0][key], value, "{key}");
        }
        assert!(!outcomes.metrics("codexctl_central").contains("synthetic"));
    }

    #[test]
    fn stop_context_drops_secrets_before_sending_and_before_logging() {
        let context = |status| {
            json!({
                "lane_stop":"rate_limited", "http_status":status,
                "error_code":"sk-synthetic-secret", "account_alias":"sk-synthetic-secret",
                "route":"/backend-api/codex/responses?token=synthetic-secret",
                "request_id":"Bearer synthetic-secret"
            })
        };
        let (outcomes, lines) = logged();
        let hostile: StopReport = serde_json::from_value(context(999)).unwrap();
        outcomes.stopped(&hostile);
        let logs = lines.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["error_code"], "other");
        for key in ["http_status", "account_alias", "route", "request_id"] {
            assert!(logs[0][key].is_null(), "{key}: {}", logs[0]);
        }
        assert!(!logs[0].to_string().contains("synthetic-secret"));
        let outgoing = StopReport::new(StopReason::RateLimited, 429, Some("sk-synthetic-secret"))
            .with_request(
                Some("sk-synthetic-secret"),
                "/unknown?token=synthetic-secret",
                Some("Bearer synthetic-secret"),
            );
        assert!(
            !serde_json::to_string(&outgoing)
                .unwrap()
                .contains("synthetic-secret")
        );
    }
}
