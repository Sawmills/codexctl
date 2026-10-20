//! Confirm native HTTP retry exhaustion at the client, then consume its pending failure.
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::http::{HeaderMap, StatusCode};
use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::{Labels, Relay, identity};
use crate::outcomes::{StopReason, StopReport};

const MESSAGE: &str = "Selected model is at capacity. Please try a different model.";
const MAX_PENDING: usize = 1024;
const MAX_LINE: usize = 64 * 1024;
const TTL: Duration = Duration::from_secs(600);
const UNAVAILABLE_TTL: Duration = Duration::from_secs(3600);
mod completion;
mod diagnostics;

#[derive(Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    #[default]
    Confirm,
    Inspect,
    Recovered,
    NativeStateUnavailable,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    thread_id: Option<String>,
    launch_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    action: Action,
    #[serde(default)]
    closing: bool,
}

#[derive(Serialize, Deserialize)]
struct Turn {
    thread_id: String,
    turn_id: String,
}

pub(super) fn valid_id(value: &str) -> bool {
    (value.len() == 32 || value.len() == 36)
        && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn key(thread: Option<&str>, launch: Option<&str>, turn: Option<&str>) -> Option<String> {
    thread.filter(|v| valid_id(v)).map(|thread| {
        format!(
            "{}:{thread}:{}",
            launch.filter(|v| valid_id(v)).unwrap_or("local"),
            turn.filter(|v| valid_id(v)).unwrap_or("unknown")
        )
    })
}

struct Failure {
    labels: Labels,
    request_id: Option<String>,
    status: u16,
    observed: Instant,
    unavailable_reported: bool,
}

struct UnavailableObservation {
    newly_reported: bool,
    report: Option<(StopReport, Labels, Option<String>, u16)>,
}

impl Failure {
    fn ttl(&self) -> Duration {
        if self.unavailable_reported {
            UNAVAILABLE_TTL
        } else {
            TTL
        }
    }

    fn live(&self) -> bool {
        self.observed.elapsed() < self.ttl()
    }
}

#[derive(Default)]
pub(super) struct Pending {
    failures: BTreeMap<String, Failure>,
    unavailable: u64,
}

impl Pending {
    pub(super) fn counted(&mut self, labels: &Labels) {
        if labels.terminal_request
            && let Some(key) = key(
                Some(&labels.thread),
                labels.launch_id.as_deref(),
                labels.turn_id.as_deref(),
            )
        {
            self.failures.remove(&key);
        }
    }

    pub(super) fn clear(&mut self, labels: &Labels) {
        // Without a turn id, a response cannot prove which turn recovered.
        if labels.turn_id.is_none() {
            return;
        }
        if let Some(key) = key(
            Some(&labels.thread),
            labels.launch_id.as_deref(),
            labels.turn_id.as_deref(),
        ) {
            self.failures.remove(&key);
        }
    }

    pub(super) fn observe(
        &mut self,
        labels: &Labels,
        status: u16,
        code: Option<&str>,
        request_id: Option<&str>,
    ) {
        if !labels.terminal_request {
            return;
        }
        self.failures.retain(|_, f| f.live());
        if !(500..600).contains(&status) || code != Some("server_is_overloaded") {
            self.clear(labels);
            return;
        }
        if let Some(key) = key(
            Some(&labels.thread),
            labels.launch_id.as_deref(),
            labels.turn_id.as_deref(),
        ) {
            if self.failures.len() >= MAX_PENDING && !self.failures.contains_key(&key) {
                eprintln!("terminal_capacity_tracking_failed reason=pending_limit");
                return;
            }
            let unavailable_reported = self
                .failures
                .get(&key)
                .is_some_and(|f| f.unavailable_reported);
            self.failures.insert(
                key,
                Failure {
                    labels: labels.clone(),
                    request_id: request_id.map(str::to_owned),
                    status,
                    observed: Instant::now(),
                    unavailable_reported,
                },
            );
        }
    }

    pub(super) fn metrics(&self) -> String {
        format!(
            "# TYPE codexctl_relay_capacity_confirm_unavailable_total counter\ncodexctl_relay_capacity_confirm_unavailable_total {}\n",
            self.unavailable
        )
    }

    fn mark_unavailable(&mut self, key: &str, closing: bool) -> Option<UnavailableObservation> {
        let (newly_reported, report) = {
            let failure = self.failures.get_mut(key)?;
            if !failure.live() {
                return None;
            }
            let newly_reported = !failure.unavailable_reported;
            failure.unavailable_reported = true;
            if !closing {
                // The watcher polls every 200 ms while the native state is
                // unavailable. Keep the bounded evidence alive while those
                // polls continue, but let a silent watcher expire it.
                failure.observed = Instant::now();
                (newly_reported, None)
            } else {
                (
                    newly_reported,
                    Some((
                        StopReport::new(
                            StopReason::CapacityUnconfirmed,
                            failure.status,
                            Some("capacity_unconfirmed"),
                        ),
                        failure.labels.clone(),
                        failure.request_id.clone(),
                        failure.status,
                    )),
                )
            }
        };
        if report.is_some() {
            self.failures.remove(key);
        }
        Some(UnavailableObservation {
            newly_reported,
            report,
        })
    }
}

pub(super) fn accept(
    relay: &Relay,
    identity: &identity::Identity,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let signature = headers
        .get("x-codexctl-receipt")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !identity.verify_receipt(body, signature) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(receipt) = serde_json::from_slice::<Receipt>(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut pending = relay.terminal.lock().expect("terminal receipt lock");
    if receipt.action == Action::Inspect {
        let Some(launch) = receipt.launch_id.as_deref().filter(|v| valid_id(v)) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let turns: Vec<_> = pending
            .failures
            .values()
            .filter(|f| f.live() && f.labels.launch_id.as_deref() == Some(launch))
            .filter_map(|f| {
                Some(Turn {
                    thread_id: f.labels.thread.clone(),
                    turn_id: f.labels.turn_id.clone()?,
                })
            })
            .take(8)
            .collect();
        return Json(turns).into_response();
    }
    let Some(mut key) = key(
        receipt.thread_id.as_deref(),
        receipt.launch_id.as_deref(),
        receipt.turn_id.as_deref(),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if receipt.action == Action::Recovered {
        if receipt.turn_id.as_deref().is_some_and(valid_id) {
            pending.failures.remove(&key);
        }
        return StatusCode::NO_CONTENT.into_response();
    }
    if receipt.action == Action::NativeStateUnavailable {
        if receipt.turn_id.is_none() || receipt.launch_id.is_none() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let mut report = None;
        let mut diagnostic = false;
        if let Some(observation) = pending.mark_unavailable(&key, receipt.closing) {
            diagnostic = observation.newly_reported;
            report = observation.report;
        }
        if diagnostic {
            pending.unavailable = pending.unavailable.saturating_add(1);
        }
        drop(pending);
        if diagnostic {
            (relay.log)(
                "{\"component\":\"codexctl-relay\",\"level\":\"warning\",\"reason\":\"native_state_unavailable\"}",
            );
        }
        if let Some((report, labels, request_id, status)) = report {
            relay.outcomes.stopped(&report);
            relay.lane_stop(report, &labels, request_id.as_deref(), status);
        }
        return StatusCode::NO_CONTENT.into_response();
    }
    if receipt.turn_id.is_none() {
        let matching: Vec<_> = pending
            .failures
            .iter()
            .filter(|(_, f)| {
                Some(f.labels.thread.as_str()) == receipt.thread_id.as_deref()
                    && f.labels.launch_id == receipt.launch_id
            })
            .map(|(key, _)| key.clone())
            .take(2)
            .collect();
        // JSON exec has no native turn id. Ambiguous turns are left to the
        // launch watcher, which checks each persisted completion exactly.
        if matching.len() != 1 {
            return StatusCode::NO_CONTENT.into_response();
        }
        key = matching[0].clone();
    }
    if receipt.turn_id.is_some()
        && pending
            .failures
            .get(&key)
            .is_some_and(|f| f.labels.turn_id != receipt.turn_id)
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    let failure = pending.failures.remove(&key);
    drop(pending);
    let Some(failure) = failure.filter(Failure::live) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let report = StopReport::new(
        StopReason::Capacity,
        failure.status,
        Some("server_is_overloaded"),
    );
    relay.outcomes.stopped(&report);
    relay.lane_stop(
        report,
        &failure.labels,
        failure.request_id.as_deref(),
        failure.status,
    );
    StatusCode::NO_CONTENT.into_response()
}

/// A launch's output observer. Optional reporting never changes launch/retry behavior.
pub struct TerminalObserver {
    target: Option<(PathBuf, SocketAddr)>,
    thread: Option<String>,
    launch: Option<String>,
    line: Vec<u8>,
    oversized: bool,
    reported: bool,
    _watch: Option<completion::Watch>,
}

impl TerminalObserver {
    pub(crate) fn for_launch(
        paths: &crate::config::Paths,
        listen: Option<SocketAddr>,
        launch: String,
    ) -> Self {
        let watch = listen.and_then(|listen| {
            match completion::Watch::start(paths, listen, launch.clone()) {
                Ok(watch) => Some(watch),
                Err(_) => {
                    diagnostics::record(
                        &paths.codexctl_dir(),
                        diagnostics::Reason::WatcherStartFailed,
                    );
                    None
                }
            }
        });
        let mut observer = Self::new(
            listen.map(|listen| (paths.codexctl_dir(), listen)),
            Some(launch),
        );
        observer._watch = watch;
        observer
    }

    pub fn from_env() -> Self {
        let target = crate::config::default_paths().ok().and_then(|paths| {
            super::routing::receipt_listener(&paths)
                .ok()
                .flatten()
                .map(|listen| (paths.codexctl_dir(), listen))
        });
        Self::new(target, None)
    }

    fn new(target: Option<(PathBuf, SocketAddr)>, launch: Option<String>) -> Self {
        Self {
            target,
            thread: None,
            launch,
            line: Vec::new(),
            oversized: false,
            reported: false,
            _watch: None,
        }
    }

    /// JSON exec supplies a terminal event directly. The TUI launch watcher
    /// uses native task_complete records, never a redraw of a historical error.
    pub fn observe(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                if !self.oversized {
                    let line = std::mem::take(&mut self.line);
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                        match value.get("type").and_then(|v| v.as_str()) {
                            Some("thread.started") => {
                                self.thread = value
                                    .get("thread_id")
                                    .and_then(|v| v.as_str())
                                    .filter(|v| valid_id(v))
                                    .map(str::to_owned)
                            }
                            Some("turn.started") => self.reported = false,
                            Some("turn.failed")
                                if value.pointer("/error/message").and_then(|v| v.as_str())
                                    == Some(MESSAGE) =>
                            {
                                self.report(Action::Confirm)
                            }
                            Some("turn.completed") => self.report(Action::Recovered),
                            _ => {}
                        }
                    }
                }
                self.line.clear();
                self.oversized = false;
            } else if !self.oversized {
                if self.line.len() == MAX_LINE {
                    self.line.clear();
                    self.oversized = true;
                } else {
                    self.line.push(byte);
                }
            }
        }
    }

    /// A marked native error row is confirmed only against this launch's
    /// unique pending upstream overload. Text alone never creates a report.
    pub fn record_capacity_policy_stop() {
        if let Ok(paths) = crate::config::default_paths() {
            diagnostics::record(
                &paths.codexctl_dir(),
                diagnostics::Reason::CapacityPolicyStopped,
            );
        }
    }

    pub fn confirm_capacity_for_session(&mut self, session: &str) {
        if !valid_id(session) || self.thread.as_deref().is_some_and(|id| id != session) {
            return;
        }
        self.thread = Some(session.into());
        self.report(Action::Confirm);
    }

    fn report(&mut self, action: Action) {
        if action == Action::Confirm && self.reported {
            return;
        }
        if action == Action::Confirm {
            self.reported = true;
        }
        let Some((root, listen)) = &self.target else {
            return;
        };
        if key(self.thread.as_deref(), self.launch.as_deref(), None).is_none() {
            return;
        }
        let send = || -> Result<()> {
            let body = serde_json::to_vec(&Receipt {
                thread_id: self.thread.clone(),
                launch_id: self.launch.clone(),
                turn_id: None,
                action,
                closing: false,
            })?;
            let signature = identity::receipt_signature(root, *listen, &body)?;
            let response = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(100))
                .timeout(Duration::from_millis(250))
                .build()?
                .post(format!("http://{listen}/v1/terminal-capacity"))
                .header("content-type", "application/json")
                .header("x-codexctl-receipt", signature)
                .body(body)
                .send()?;
            anyhow::ensure!(response.status().is_success(), "terminal receipt refused");
            Ok(())
        };
        if send().is_err() {
            diagnostics::record(root, diagnostics::Reason::DeliveryFailed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(observed: Instant, unavailable_reported: bool) -> Failure {
        Failure {
            labels: Labels {
                capacity_id: "synthetic-request".into(),
                capacity_observed: std::sync::Arc::new(std::sync::OnceLock::new()),
                thread: "11111111-2222-4333-8444-555555555555".into(),
                route: "/backend-api/codex/responses",
                model: "gpt-6.1-sol".into(),
                account_class: "subscription",
                account_alias: Some("lane".into()),
                launch_id: Some("11111111-2222-4333-8444-666666666666".into()),
                turn_id: Some("11111111-2222-4333-8444-777777777777".into()),
                terminal_request: true,
            },
            request_id: None,
            status: 503,
            observed,
            unavailable_reported,
        }
    }

    #[test]
    fn unavailable_receipts_refresh_then_close_within_bounded_window() {
        let now = Instant::now();
        let normal = failure(now - TTL - Duration::from_secs(1), false);
        assert!(!normal.live());

        let thread = "11111111-2222-4333-8444-555555555555";
        let launch = "11111111-2222-4333-8444-666666666666";
        let turn = "11111111-2222-4333-8444-777777777777";
        let receipt_key = key(Some(thread), Some(launch), Some(turn)).unwrap();
        let mut pending = Pending::default();
        pending
            .failures
            .insert(receipt_key.clone(), failure(now, false));

        let first = pending.mark_unavailable(&receipt_key, false).unwrap();
        assert!(first.newly_reported);
        assert!(first.report.is_none());
        {
            let failure = pending.failures.get_mut(&receipt_key).unwrap();
            failure.observed = Instant::now() - TTL - Duration::from_secs(1);
        }
        let refreshed = pending.mark_unavailable(&receipt_key, false).unwrap();
        assert!(!refreshed.newly_reported);
        assert!(refreshed.report.is_none());
        assert!(pending.failures.get(&receipt_key).unwrap().live());

        {
            let failure = pending.failures.get_mut(&receipt_key).unwrap();
            failure.observed = Instant::now() - UNAVAILABLE_TTL + Duration::from_secs(1);
        }
        let closing = pending.mark_unavailable(&receipt_key, true).unwrap();
        assert!(!closing.newly_reported);
        assert!(closing.report.is_some());
        assert!(!pending.failures.contains_key(&receipt_key));

        let mut silent = Pending::default();
        silent.failures.insert(
            receipt_key.clone(),
            failure(
                Instant::now() - UNAVAILABLE_TTL - Duration::from_secs(1),
                true,
            ),
        );
        assert!(silent.mark_unavailable(&receipt_key, true).is_none());
        let stale_labels = silent.failures.get(&receipt_key).unwrap().labels.clone();
        silent.observe(&stale_labels, 200, None, None);
        assert!(!silent.failures.contains_key(&receipt_key));
    }
}
