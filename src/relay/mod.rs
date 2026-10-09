//! Loopback relay for Codex lanes on the central provider.
//!
//! Codex retries a 429 or a "model at capacity" stop only when the server sends
//! retry advice. Without it the turn ends and the lane waits for a person. The
//! relay adds that advice and nothing else: it never retries by itself, never
//! changes the model or account, and never resumes a goal. When a failure
//! streak spends its budget, the relay passes the failure through unchanged
//! (and removes any upstream advice) so Codex stops and the board takes over.

pub mod cli;
mod policy;

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{Stream, StreamExt};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use policy::{Advice, Http429, Kind, Streaks};

/// The only remote upstream the relay forwards to.
const CHATGPT: &str = "https://chatgpt.com";
/// Only this path prefix is forwarded; the relay is not a general proxy.
const FORWARDED_PREFIX: &str = "/backend-api/codex/";
/// Lane-only header that labels metrics; it never leaves the host.
const ACCOUNT_CLASS_HEADER: &str = "x-codexctl-account-class";
/// A 429 body larger than this is not classified; it passes through without advice.
const MAX_429_BODY: usize = 64 * 1024;
/// Request bytes kept to read the model label. Codex writes `model` first.
const MODEL_PREFIX: usize = 4 * 1024;
/// Default listen address used by `codexctl relay` and the lane override.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:47631";
/// Rate limits stop after this so the board can move the account (M5).
pub const DEFAULT_RATE_BUDGET: Duration = Duration::from_secs(180);
/// Model capacity is not per account, so it waits longer (M5).
pub const DEFAULT_OVERLOADED_BUDGET: Duration = Duration::from_secs(600);

type Log = Arc<dyn Fn(&str) + Send + Sync>;
/// kind, model, account class, outcome.
type MetricKey = (&'static str, String, &'static str, &'static str);
/// kind, stage, model, account class.
type StreamMetricKey = (&'static str, &'static str, String, &'static str);
const CENTRAL_EVENT_QUEUE_CAPACITY: usize = 256;
const CENTRAL_EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_FAILURE_LOG_LIMIT: u64 = 30;
const STREAM_FAILURE_LOG_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURE_BODY: usize = 64 * 1024;
const MAX_PENDING_FRAME_BYTES: usize = 64 * 1024;
const MAX_PARTIAL_FAILURE_INSPECTION: usize = 256;
const MAX_TYPE_COUNTS: usize = 64;
const MAX_ERROR_KEYS: usize = 32;
const HTTP_ERROR_PEEK_TIMEOUT: Duration = Duration::from_millis(50);

/// Relay settings. Build with [`RelayConfig::new`].
pub struct RelayConfig {
    upstream: String,
    rate_budget: Duration,
    overloaded_budget: Duration,
    log: Log,
    central: Option<CentralReporter>,
}

impl RelayConfig {
    /// Accepts `https://chatgpt.com` or a loopback `http://` origin (tests and
    /// local drills). Anything else is refused, so tokens reach no other host.
    pub fn new(upstream: &str) -> Result<Self> {
        let upstream = upstream.trim_end_matches('/');
        let allowed = upstream == CHATGPT
            || upstream
                .strip_prefix("http://127.0.0.1:")
                .is_some_and(|port| port.parse::<u16>().is_ok());
        if !allowed {
            bail!("relay upstream must be {CHATGPT} or http://127.0.0.1:<port>");
        }
        Ok(Self {
            upstream: upstream.to_owned(),
            rate_budget: DEFAULT_RATE_BUDGET,
            overloaded_budget: DEFAULT_OVERLOADED_BUDGET,
            log: Arc::new(|line| eprintln!("{line}")),
            central: None,
        })
    }

    /// Sets how long a 429 streak and a capacity streak may keep advising.
    pub fn with_budgets(mut self, rate: Duration, overloaded: Duration) -> Self {
        self.rate_budget = rate;
        self.overloaded_budget = overloaded;
        self
    }

    /// Receives one JSON line per capacity event. The default is stderr.
    pub fn with_log(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Arc::new(log);
        self
    }

    /// Reports bounded capacity events to an authenticated account server.
    pub fn with_central_reporter(mut self, server: &str, token: &str) -> Result<Self> {
        self.central = Some(CentralReporter::new(server, token)?);
        Ok(self)
    }

    pub(crate) fn with_active_central_reporter(mut self) -> Self {
        let connection = crate::central::remote::connection().ok().flatten();
        if let Some(connection) = connection {
            match crate::central::remote::secret(&connection)
                .and_then(|token| CentralReporter::new(&connection.server, &token))
            {
                Ok(reporter) => self.central = Some(reporter),
                Err(error) => eprintln!("relay central reporter disabled: {error:#}"),
            }
        }
        self
    }
}

#[derive(Clone)]
struct CentralReporter {
    endpoint: String,
    token: String,
    client: reqwest::Client,
}

enum CentralSendFailure {
    RateLimited,
    Failed,
}

impl CentralReporter {
    fn new(server: &str, token: &str) -> Result<Self> {
        let mut url = reqwest::Url::parse(server.trim_end_matches('/'))?;
        let host = url.host_str().context("central reporter needs a host")?;
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !matches!(url.scheme(), "http" | "https") {
            bail!("central reporter needs an HTTP(S) server URL");
        }
        if url.scheme() == "http" && !loopback {
            bail!("central reporter requires HTTPS except for loopback tests");
        }
        url.set_path(&format!(
            "{}/v1/relay/capacity-events",
            url.path().trim_end_matches('/')
        ));
        Ok(Self {
            endpoint: url.to_string(),
            token: token.to_owned(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(100))
                .build()
                .context("could not build central reporter client")?,
        })
    }

    async fn send(&self, event: &CapacityEvent) -> std::result::Result<(), CentralSendFailure> {
        let response = tokio::time::timeout(
            CENTRAL_EVENT_SEND_TIMEOUT,
            self.client
                .post(&self.endpoint)
                .bearer_auth(&self.token)
                .json(event)
                .send(),
        )
        .await
        .map_err(|_| CentralSendFailure::Failed)?
        .map_err(|_| CentralSendFailure::Failed)?;
        if response.status().is_success() {
            Ok(())
        } else if response.status() == StatusCode::TOO_MANY_REQUESTS {
            Err(CentralSendFailure::RateLimited)
        } else {
            Err(CentralSendFailure::Failed)
        }
    }
}

#[derive(Serialize)]
struct CapacityEvent {
    kind: String,
    model: String,
    account_class: String,
    outcome: String,
}

#[derive(Clone)]
struct Relay {
    upstream: Arc<str>,
    client: reqwest::Client,
    streaks: Arc<Mutex<Streaks>>,
    counts: Arc<Mutex<BTreeMap<MetricKey, u64>>>,
    stream_failures: Arc<Mutex<BTreeMap<StreamMetricKey, u64>>>,
    stream_log: Arc<Mutex<StreamFailureLogState>>,
    log: Log,
    central_events: Option<mpsc::Sender<CapacityEvent>>,
    central_dropped: Arc<Mutex<BTreeMap<&'static str, u64>>>,
}

struct StreamFailureLogState {
    window_start: Instant,
    emitted: u64,
    suppressed: u64,
}

struct StreamFailureContext<'a> {
    advised: bool,
    event: Option<&'a Value>,
    request_id: Option<&'a str>,
    http_status: Option<u16>,
    body: Option<&'a Value>,
}

impl StreamFailureLogState {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            emitted: 0,
            suppressed: 0,
        }
    }
}

/// Serves until `shutdown` completes, then drains open responses.
/// Refuses a listener that is not on a loopback address.
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: RelayConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        bail!("relay binds only to loopback, not {address}");
    }
    // No overall timeout: a Responses stream can run for many minutes, and
    // Codex owns its own idle timeout.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("could not build relay HTTP client")?;
    let central_dropped = Arc::new(Mutex::new(BTreeMap::new()));
    let (central_events, central_worker) = if let Some(reporter) = config.central {
        let (sender, mut receiver) = mpsc::channel(CENTRAL_EVENT_QUEUE_CAPACITY);
        let dropped = central_dropped.clone();
        let worker = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let reason = match reporter.send(&event).await {
                    Ok(()) => continue,
                    Err(CentralSendFailure::RateLimited) => "rate_limited",
                    Err(CentralSendFailure::Failed) => "send_failed",
                };
                {
                    *dropped
                        .lock()
                        .expect("central drop metric lock")
                        .entry(reason)
                        .or_default() += 1;
                }
            }
        });
        (Some(sender), Some(worker))
    } else {
        (None, None)
    };
    let relay = Relay {
        upstream: config.upstream.into(),
        client,
        streaks: Arc::new(Mutex::new(Streaks::new(
            config.rate_budget,
            config.overloaded_budget,
        ))),
        counts: Arc::default(),
        stream_failures: Arc::default(),
        stream_log: Arc::new(Mutex::new(StreamFailureLogState::new())),
        log: config.log,
        central_events,
        central_dropped,
    };
    let suppression_relay = relay.clone();
    let suppression_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            suppression_relay.flush_suppression_summary();
        }
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .fallback(forward)
        .layer(DefaultBodyLimit::disable())
        .with_state(relay);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("relay stopped")?;
    suppression_task.abort();
    let _ = suppression_task.await;
    if let Some(worker) = central_worker {
        // Capacity reporting is best effort. Do not hold relay shutdown on a
        // slow or unreachable central server while draining the queue.
        worker.abort();
        let _ = worker.await;
    }
    Ok(())
}

struct Labels {
    thread: String,
    model: String,
    account_class: &'static str,
}

async fn forward(State(relay): State<Relay>, request: Request) -> Response {
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let is_responses = path.split_once('?').map_or(path.as_str(), |(path, _)| path)
        == "/backend-api/codex/responses";
    if !path.starts_with(FORWARDED_PREFIX) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (parts, body) = request.into_parts();
    // Stream the request; keep only a bounded prefix for the model label.
    let mut body = body.into_data_stream();
    let mut head: Vec<Bytes> = Vec::new();
    let mut head_len = 0;
    while head_len < MODEL_PREFIX {
        match body.next().await {
            Some(Ok(chunk)) => {
                head_len += chunk.len();
                head.push(chunk);
            }
            Some(Err(_)) => return StatusCode::BAD_REQUEST.into_response(),
            None => break,
        }
    }
    let prefix = head.concat();
    let labels = Labels {
        thread: header_str(&parts.headers, "thread-id")
            .or_else(|| header_str(&parts.headers, "session-id"))
            .unwrap_or("unknown")
            .to_owned(),
        model: relay.model_label(&parts.headers, &prefix[..prefix.len().min(MODEL_PREFIX)]),
        account_class: match header_str(&parts.headers, ACCOUNT_CLASS_HEADER) {
            Some("included") => "included",
            Some("credit") => "credit",
            _ => "unknown",
        },
    };
    let mut headers = parts.headers;
    // Content-Length stays: the streamed body is byte for byte the same.
    for name in [
        "host",
        "connection",
        "transfer-encoding",
        ACCOUNT_CLASS_HEADER,
    ] {
        headers.remove(name);
    }
    // The relay reads event frames, so ask for an uncompressed stream.
    headers.remove("accept-encoding");
    let upstream = relay
        .client
        .request(parts.method.clone(), format!("{}{path}", relay.upstream))
        .headers(headers)
        .body(reqwest::Body::wrap_stream(
            futures::stream::iter(head.into_iter().map(Ok)).chain(body),
        ))
        .send()
        .await;
    let upstream = match upstream {
        Ok(upstream) => upstream,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = upstream.status();
    let headers_at = Instant::now();
    let mut headers = upstream.headers().clone();
    for name in ["connection", "transfer-encoding"] {
        headers.remove(name);
    }
    let request_id = header_str(&headers, "x-request-id").map(str::to_owned);
    if status == StatusCode::TOO_MANY_REQUESTS && parts.method == Method::POST {
        return relay
            .rate_limited(upstream, headers, labels, request_id)
            .await;
    }
    if (status.is_client_error() || status.is_server_error()) && is_responses {
        return relay
            .http_error(upstream, headers, labels, request_id, status, headers_at)
            .await;
    }
    let is_stream = status.is_success()
        && header_str(&headers, "content-type").is_some_and(|t| t.starts_with("text/event-stream"))
        && headers.get("content-encoding").is_none();
    let mut builder = Response::builder().status(status);
    if is_stream {
        headers.remove("content-length");
        let frame_headers = headers.clone();
        *builder.headers_mut().expect("fresh builder") = headers;
        let frames = FrameRewriter::new(
            relay,
            labels,
            request_id,
            frame_headers,
            headers_at,
            is_responses,
        );
        builder
            .body(Body::from_stream(frames.wrap(upstream.bytes_stream())))
            .expect("valid response parts")
    } else {
        *builder.headers_mut().expect("fresh builder") = headers;
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .expect("valid response parts")
    }
}

impl Relay {
    async fn http_error(
        &self,
        upstream: reqwest::Response,
        mut headers: HeaderMap,
        labels: Labels,
        request_id: Option<String>,
        status: StatusCode,
        headers_at: Instant,
    ) -> Response {
        let mut rest = upstream.bytes_stream();
        let peek_until = Instant::now() + HTTP_ERROR_PEEK_TIMEOUT;
        let mut head = Vec::new();
        let mut prefix = Vec::new();
        let mut read_error = None;
        let mut error = None;
        while prefix.len() <= MAX_FAILURE_BODY {
            let remaining = peek_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rest.next()).await {
                Ok(Some(Ok(chunk))) => {
                    if prefix.len() <= MAX_FAILURE_BODY {
                        let take = (MAX_FAILURE_BODY + 1 - prefix.len()).min(chunk.len());
                        prefix.extend_from_slice(&chunk[..take]);
                    }
                    head.push(chunk);
                    if prefix.len() <= MAX_FAILURE_BODY {
                        error = serde_json::from_slice::<Value>(&prefix).ok();
                    }
                    if error.is_some() || prefix.len() > MAX_FAILURE_BODY {
                        break;
                    }
                }
                Ok(Some(Err(error_value))) => {
                    read_error = Some(error_value.to_string());
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        let trace = StreamTrace::new(headers_at);
        self.stream_failure(
            &trace,
            &labels,
            "http_error",
            &headers,
            StreamFailureContext {
                advised: false,
                event: None,
                request_id: request_id.as_deref(),
                http_status: Some(status.as_u16()),
                body: error.as_ref(),
            },
        );
        if read_error.is_some() {
            headers.remove("content-length");
        }
        let mut response = Response::builder().status(status);
        *response.headers_mut().expect("fresh builder") = headers;
        let body: std::pin::Pin<
            Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>,
        > = if let Some(message) = read_error {
            Box::pin(
                futures::stream::iter(head.into_iter().map(Ok)).chain(futures::stream::once(
                    async move { Err(std::io::Error::other(message)) },
                )),
            )
        } else {
            Box::pin(futures::stream::iter(head.into_iter().map(Ok)).chain(
                rest.map(|result| result.map_err(|error| std::io::Error::other(error.to_string()))),
            ))
        };
        response
            .body(Body::from_stream(body))
            .expect("valid response parts")
    }

    fn stream_failure(
        &self,
        trace: &StreamTrace,
        labels: &Labels,
        kind: &'static str,
        headers: &HeaderMap,
        context: StreamFailureContext<'_>,
    ) {
        let stage = if trace.first_output.is_some() {
            "after_output"
        } else {
            "pre_output"
        };
        *self
            .stream_failures
            .lock()
            .expect("stream failure metric lock")
            .entry((kind, stage, labels.model.clone(), labels.account_class))
            .or_default() += 1;
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "component": "codexctl-relay",
            "kind": kind,
            "outcome": "stream_failure",
            "stage": stage,
            "advised": context.advised,
            "model": labels.model,
            "account_class": labels.account_class,
            "thread_id": labels.thread,
            "request_id": context.request_id,
            "cf_ray": header_str(headers, "cf-ray"),
            "x_oai_request_id": header_str(headers, "x-oai-request-id"),
            "ms_to_failure": trace.started.elapsed().as_millis() as u64,
            "ms_to_first_output": trace.first_output.map(|time| time.duration_since(trace.started).as_millis() as u64),
            "types": trace.types_json(),
            "truncated": trace.truncated,
            "failure": context.event.map(failure_detail).or_else(|| context.body.map(http_failure_detail)),
            "http_status": context.http_status,
        });
        self.log_stream_failure(line);
    }

    fn log_stream_failure(&self, line: Value) {
        self.flush_suppression_summary();
        let mut limiter = self.stream_log.lock().expect("stream log lock");
        if limiter.emitted < STREAM_FAILURE_LOG_LIMIT {
            limiter.emitted += 1;
            (self.log)(&line.to_string());
        } else {
            limiter.suppressed += 1;
        }
    }

    fn flush_suppression_summary(&self) {
        let suppressed = {
            let mut limiter = self.stream_log.lock().expect("stream log lock");
            if limiter.window_start.elapsed() < STREAM_FAILURE_LOG_WINDOW {
                return;
            }
            let suppressed = limiter.suppressed;
            limiter.window_start = Instant::now();
            limiter.emitted = 0;
            limiter.suppressed = 0;
            suppressed
        };
        if suppressed > 0 {
            (self.log)(
                &json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "component": "codexctl-relay",
                    "outcome": "stream_failure",
                    "suppressed": suppressed,
                })
                .to_string(),
            );
        }
    }

    async fn rate_limited(
        &self,
        upstream: reqwest::Response,
        mut headers: HeaderMap,
        labels: Labels,
        request_id: Option<String>,
    ) -> Response {
        // Read at most MAX_429_BODY; a longer body streams on unread.
        let mut rest = upstream.bytes_stream();
        let mut head: Vec<Bytes> = Vec::new();
        let mut head_len = 0;
        let mut oversized = false;
        while let Some(chunk) = rest.next().await {
            let Ok(chunk) = chunk else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            head_len += chunk.len();
            head.push(chunk);
            if head_len > MAX_429_BODY {
                oversized = true;
                break;
            }
        }
        // An unclassified body may be a usage or billing stop, so it gets no
        // advice and reaches Codex unchanged.
        let class = if oversized {
            Http429::Terminal
        } else {
            policy::classify_429(&head.concat())
        };
        match class {
            Http429::Terminal => {
                // Codex stops on this; the next request is a resume.
                self.streaks
                    .lock()
                    .expect("streak lock")
                    .end(&labels.thread);
                self.event(
                    Kind::Rate429,
                    &labels,
                    "terminal_passthrough",
                    None,
                    request_id.as_deref(),
                );
            }
            Http429::Rate => {
                // Numeric upstream advice is the delay Codex will use, so it
                // counts against the budget. Other forms are replaced.
                let upstream_secs = header_str(&headers, "retry-after")
                    .and_then(|value| value.trim().parse::<u64>().ok());
                match self.failure(Kind::Rate429, &labels, request_id.as_deref(), upstream_secs) {
                    Some(secs) => {
                        headers.insert("retry-after", HeaderValue::from(secs));
                    }
                    None => {
                        headers.remove("retry-after");
                    }
                }
            }
        }
        let mut response = Response::builder().status(StatusCode::TOO_MANY_REQUESTS);
        *response.headers_mut().expect("fresh builder") = headers;
        let body = futures::stream::iter(head.into_iter().map(Ok)).chain(rest);
        response
            .body(Body::from_stream(body))
            .expect("valid response parts")
    }

    /// Records one failure. Returns the advised delay, or `None` when the
    /// streak is exhausted and Codex must stop.
    fn failure(
        &self,
        kind: Kind,
        labels: &Labels,
        request_id: Option<&str>,
        upstream_secs: Option<u64>,
    ) -> Option<u64> {
        let advice = self.streaks.lock().expect("streak lock").failure(
            &labels.thread,
            kind,
            Instant::now(),
            upstream_secs,
        );
        match advice {
            Advice::RetryAfter { secs, attempt } => {
                self.event(
                    kind,
                    labels,
                    "advised",
                    Some((attempt, Some(secs))),
                    request_id,
                );
                Some(secs)
            }
            Advice::Exhausted { attempt } => {
                self.event(kind, labels, "exhausted", Some((attempt, None)), request_id);
                None
            }
        }
    }

    fn success(&self, labels: &Labels) {
        let recovered = self
            .streaks
            .lock()
            .expect("streak lock")
            .success(&labels.thread);
        if let Some(recovered) = recovered {
            self.event(
                recovered.kind,
                labels,
                "recovered",
                Some((recovered.attempts, None)),
                None,
            );
        }
    }

    fn event(
        &self,
        kind: Kind,
        labels: &Labels,
        outcome: &'static str,
        attempt: Option<(u32, Option<u64>)>,
        request_id: Option<&str>,
    ) {
        *self
            .counts
            .lock()
            .expect("metric lock")
            .entry((
                kind.label(),
                labels.model.clone(),
                labels.account_class,
                outcome,
            ))
            .or_default() += 1;
        if outcome != "advised" {
            let report = CapacityEvent {
                kind: kind.label().into(),
                model: labels.model.clone(),
                account_class: labels.account_class.into(),
                outcome: outcome.into(),
            };
            match &self.central_events {
                Some(sender) => {
                    if let Err(error) = sender.try_send(report) {
                        let reason = match error {
                            TrySendError::Full(_) => "queue_full",
                            TrySendError::Closed(_) => "connection_closed",
                        };
                        *self
                            .central_dropped
                            .lock()
                            .expect("central drop metric lock")
                            .entry(reason)
                            .or_default() += 1;
                    }
                }
                None => {
                    *self
                        .central_dropped
                        .lock()
                        .expect("central drop metric lock")
                        .entry("no_connection")
                        .or_default() += 1;
                }
            }
        }
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "component": "codexctl-relay",
            "kind": kind.label(),
            "outcome": outcome,
            "model": labels.model,
            "account_class": labels.account_class,
            "thread_id": labels.thread,
            "attempt": attempt.map(|(attempt, _)| attempt),
            "retry_after_s": attempt.and_then(|(_, secs)| secs),
            "request_id": request_id,
        });
        (self.log)(&line.to_string());
    }

    /// A bounded model label from the fixed central allowlist, else `other`.
    fn model_label(&self, headers: &HeaderMap, prefix: &[u8]) -> String {
        static MODEL: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
        let pattern = MODEL.get_or_init(|| {
            regex::bytes::Regex::new(r#""model"\s*:\s*"([^"]{1,40})""#).expect("valid regex")
        });
        let model = (headers.get("content-encoding").is_none())
            .then(|| pattern.captures(prefix))
            .flatten()
            .and_then(|captures| String::from_utf8(captures[1].to_vec()).ok())
            .filter(|model| {
                model.starts_with("gpt-")
                    && model.len() <= 40
                    && model.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'
                    })
            });
        model
            .filter(|model| crate::RELAY_KNOWN_MODELS.contains(&model.as_str()))
            .unwrap_or_else(|| "other".into())
    }
}

struct StreamTrace {
    started: Instant,
    first_output: Option<Instant>,
    types: Vec<(String, u64)>,
    counts: BTreeMap<String, u64>,
    truncated: bool,
    terminal: bool,
    failure_recorded: bool,
}

impl StreamTrace {
    fn new(started: Instant) -> Self {
        Self {
            started,
            first_output: None,
            types: Vec::new(),
            counts: BTreeMap::new(),
            truncated: false,
            terminal: false,
            failure_recorded: false,
        }
    }

    fn record_type(&mut self, kind: &str, now: Instant) {
        let kind = normalized_type(kind);
        if let Some(count) = self.counts.get_mut(&kind) {
            *count += 1;
        } else if self.counts.len() < MAX_TYPE_COUNTS {
            self.counts.insert(kind.clone(), 1);
        }
        if self.types.last().is_some_and(|(last, _)| last == &kind) {
            self.types.last_mut().expect("last type exists").1 += 1;
        } else if self.types.len() < 32 {
            self.types.push((kind.clone(), 1));
        } else {
            self.truncated = true;
        }
        if is_output(kind.as_str()) && self.first_output.is_none() {
            self.first_output = Some(now);
        }
        if matches!(
            kind.as_str(),
            "response.completed" | "response.failed" | "response.incomplete" | "error"
        ) {
            self.terminal = true;
        }
    }

    fn types_json(&self) -> Value {
        Value::Array(
            self.types
                .iter()
                .map(|(kind, count)| json!([kind, count]))
                .collect(),
        )
    }
}

/// Passes a Responses event stream through and, before any output, adds retry
/// advice to a capacity `response.failed` event. It keeps scanning event types
/// after output so failures are observable without changing forwarded bytes.
struct FrameRewriter {
    relay: Relay,
    labels: Labels,
    request_id: Option<String>,
    headers: HeaderMap,
    pending: Vec<u8>,
    forwarded: usize,
    partial_scanner: PartialTypeScanner,
    partial_scanned: usize,
    is_responses: bool,
    trace: StreamTrace,
}

#[derive(Default)]
struct PartialTypeScanner {
    depth: usize,
    started: bool,
    expecting_key: bool,
    key_is_type: bool,
    expecting_type_value: bool,
    capture_value: Option<bool>,
    capture: Vec<u8>,
    hint: Option<String>,
    in_string: bool,
    string_escape: bool,
}

impl PartialTypeScanner {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if let Some(is_type_value) = self.capture_value {
                if byte == b'\\' && self.capture.last() != Some(&b'\\') {
                    self.capture.push(byte);
                    continue;
                }
                if byte == b'"' && self.capture.last() != Some(&b'\\') {
                    if is_type_value {
                        if let Ok(value) = String::from_utf8(self.capture.clone()) {
                            self.hint = Some(value);
                        }
                        self.expecting_type_value = false;
                        self.key_is_type = false;
                    } else {
                        self.key_is_type = self.capture == b"type";
                        self.expecting_key = false;
                    }
                    self.capture_value = None;
                    self.capture.clear();
                } else if self.capture.len() < 64 {
                    self.capture.push(byte);
                }
                continue;
            }
            if self.in_string {
                if self.string_escape {
                    self.string_escape = false;
                } else if byte == b'\\' {
                    self.string_escape = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
                continue;
            }
            if !self.started {
                if byte == b'{' {
                    self.started = true;
                    self.depth = 1;
                    self.expecting_key = true;
                }
                continue;
            }
            match byte {
                b'"' if self.depth == 1 && self.expecting_type_value => {
                    self.capture_value = Some(true);
                    self.capture.clear();
                }
                b'"' if self.depth == 1 && self.expecting_key => {
                    self.capture_value = Some(false);
                    self.capture.clear();
                }
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    self.depth += 1;
                }
                b'}' | b']' => {
                    self.depth = self.depth.saturating_sub(1);
                }
                b':' if self.depth == 1 => {
                    self.expecting_type_value = self.key_is_type;
                    self.expecting_key = false;
                }
                b',' if self.depth == 1 => {
                    self.expecting_key = true;
                    self.key_is_type = false;
                    self.expecting_type_value = false;
                }
                _ => {}
            }
        }
    }

    fn take_hint(&mut self) -> Option<String> {
        self.hint.take()
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

impl FrameRewriter {
    fn new(
        relay: Relay,
        labels: Labels,
        request_id: Option<String>,
        headers: HeaderMap,
        headers_at: Instant,
        is_responses: bool,
    ) -> Self {
        Self {
            relay,
            labels,
            request_id,
            headers,
            pending: Vec::new(),
            forwarded: 0,
            partial_scanner: PartialTypeScanner::default(),
            partial_scanned: 0,
            is_responses,
            trace: StreamTrace::new(headers_at),
        }
    }

    /// Dropping the returned stream (the client left) drops the upstream
    /// response, which closes the upstream connection.
    fn wrap(
        self,
        upstream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    ) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
        futures::stream::unfold(
            (Box::pin(upstream), self, false),
            |(mut upstream, mut frames, done)| async move {
                if done {
                    return None;
                }
                loop {
                    match upstream.next().await {
                        Some(Ok(chunk)) => {
                            let out = frames.push(&chunk);
                            if !out.is_empty() {
                                return Some((Ok(Bytes::from(out)), (upstream, frames, false)));
                            }
                        }
                        Some(Err(error)) => {
                            frames.finish_truncated();
                            return Some((
                                Err(std::io::Error::other(error)),
                                (upstream, frames, true),
                            ));
                        }
                        None => {
                            frames.finish_truncated();
                            let rest = if frames.forwarded < frames.pending.len() {
                                frames.pending[frames.forwarded..].to_vec()
                            } else {
                                Vec::new()
                            };
                            frames.pending.clear();
                            frames.forwarded = 0;
                            return (!rest.is_empty())
                                .then(|| (Ok(Bytes::from(rest)), (upstream, frames, true)));
                        }
                    }
                }
            },
        )
    }

    /// Returns the bytes ready to send. Whole frames are inspected throughout
    /// the stream; only trigger frames are parsed after output starts.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        if self.trace.first_output.is_some() {
            out.extend_from_slice(&self.pending[self.forwarded..]);
            self.forwarded = self.pending.len();
        }
        loop {
            let Some(end) = find_frame_end(&self.pending) else {
                self.scan_partial_pending();
                break;
            };
            self.scan_partial_pending_until(end);
            let frame: Vec<u8> = self.pending.drain(..end).collect();
            self.partial_scanned = self.partial_scanned.saturating_sub(end);
            let hinted_kind = self.partial_scanner.take_hint();
            self.partial_scanner.reset();
            let was_forwarded = self.forwarded >= end;
            if was_forwarded {
                self.forwarded -= end;
                self.frame(frame, hinted_kind);
            } else {
                out.extend_from_slice(&self.frame(frame, hinted_kind));
            }
            if self.trace.first_output.is_some() && self.forwarded < self.pending.len() {
                out.extend_from_slice(&self.pending[self.forwarded..]);
                self.forwarded = self.pending.len();
            }
        }
        if self.trace.first_output.is_some() && self.pending.len() > MAX_PENDING_FRAME_BYTES {
            self.scan_partial_pending();
            let flush = self.pending.len() - MAX_PENDING_FRAME_BYTES;
            let prefix = self.pending[..flush.min(MAX_PARTIAL_FAILURE_INSPECTION)].to_vec();
            self.observe_partial_failure(&prefix);
            self.pending.drain(..flush);
            self.forwarded = self.forwarded.saturating_sub(flush);
            self.partial_scanned = self.partial_scanned.saturating_sub(flush);
        }
        out
    }

    fn scan_partial_pending(&mut self) {
        self.scan_partial_pending_until(self.pending.len());
    }

    fn scan_partial_pending_until(&mut self, end: usize) {
        if self.partial_scanned < end {
            self.partial_scanner
                .feed(&self.pending[self.partial_scanned..end]);
            self.partial_scanned = end;
        }
    }

    fn observe_partial_failure(&mut self, prefix: &[u8]) {
        if self.trace.failure_recorded {
            return;
        }
        let Some(kind) = scan_event_type(prefix).or_else(|| scan_sse_event_name(prefix)) else {
            return;
        };
        if kind == "response.completed" {
            return;
        }
        self.trace.record_type(&kind, Instant::now());
        if !matches!(
            kind.as_str(),
            "response.failed" | "error" | "response.incomplete"
        ) {
            return;
        }
        let event = partial_failure_event(&kind, scan_failure_code(prefix));
        self.trace.failure_recorded = true;
        self.relay.stream_failure(
            &self.trace,
            &self.labels,
            failure_kind(&kind, Some(&event)),
            &self.headers,
            StreamFailureContext {
                advised: false,
                event: Some(&event),
                request_id: self.request_id.as_deref(),
                http_status: None,
                body: None,
            },
        );
    }

    fn finish_truncated(&mut self) {
        if !self.is_responses {
            return;
        }
        if !self.trace.terminal && !self.trace.failure_recorded {
            self.trace.failure_recorded = true;
            self.trace.terminal = true;
            self.relay.stream_failure(
                &self.trace,
                &self.labels,
                "truncated",
                &self.headers,
                StreamFailureContext {
                    advised: false,
                    event: None,
                    request_id: self.request_id.as_deref(),
                    http_status: None,
                    body: None,
                },
            );
        }
    }

    fn frame(&mut self, frame: Vec<u8>, hinted_kind: Option<String>) -> Vec<u8> {
        let Some(kind) = scan_event_type(&frame)
            .or_else(|| scan_sse_event_name(&frame))
            .or(hinted_kind)
            .or_else(|| {
                if self.trace.first_output.is_none() || contains_failure_marker(&frame) {
                    frame_data(&frame).and_then(|event| {
                        event.get("type").and_then(Value::as_str).map(str::to_owned)
                    })
                } else {
                    None
                }
            })
        else {
            return frame;
        };
        let now = Instant::now();
        self.trace.record_type(&kind, now);
        if is_output(&kind) {
            self.relay.success(&self.labels);
            return frame;
        }
        if !matches!(
            kind.as_str(),
            "response.failed" | "error" | "response.incomplete"
        ) {
            return frame;
        }
        let parsed_event = if self.trace.first_output.is_none() || frame.len() <= MAX_FAILURE_BODY {
            frame_data(&frame)
        } else {
            None
        };
        let event = parsed_event.as_ref().cloned().or_else(|| {
            Some(partial_failure_event(
                &kind,
                scan_failure_code(&frame[..frame.len().min(MAX_PARTIAL_FAILURE_INSPECTION)]),
            ))
        });
        let mut advised = false;
        let mut output = frame;
        if kind == "response.failed"
            && self.trace.first_output.is_none()
            && parsed_event
                .as_ref()
                .is_some_and(policy::is_overloaded_failure)
        {
            let event_value = event.as_ref().expect("overload event exists");
            output = match self.relay.failure(
                Kind::Overloaded,
                &self.labels,
                self.request_id.as_deref(),
                upstream_event_secs(event_value),
            ) {
                Some(secs) => {
                    advised = true;
                    with_retry_advice(&output, event_value.clone(), secs)
                }
                None => without_retry_advice(output, event_value.clone()),
            };
        }
        let failure_kind = failure_kind(&kind, event.as_ref());
        if !self.trace.failure_recorded {
            self.trace.failure_recorded = true;
            self.relay.stream_failure(
                &self.trace,
                &self.labels,
                failure_kind,
                &self.headers,
                StreamFailureContext {
                    advised,
                    event: event.as_ref(),
                    request_id: self.request_id.as_deref(),
                    http_status: None,
                    body: None,
                },
            );
        }
        output
    }
}

/// Events that carry model output, or a completed response. Once one passes,
/// retry advice would repeat output, so failures are observed without rewriting.
fn is_output(kind: &str) -> bool {
    kind == "response.completed"
        || [
            "response.output_item.",
            "response.output_text.",
            "response.reasoning",
            "response.content_part.",
            "response.audio.",
            "response.audio_transcript.",
            "response.function_call_arguments.",
            "response.refusal.",
        ]
        .iter()
        .any(|prefix| kind.starts_with(prefix))
}

/// Numeric `error.headers["retry-after"]` sent by upstream on a streamed error.
fn upstream_event_secs(event: &Value) -> Option<u64> {
    event
        .pointer("/response/error/headers")?
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| match value {
            Value::String(text) => text.trim().parse().ok(),
            Value::Number(number) => number.as_u64(),
            _ => None,
        })
}

/// Index just past the blank line that ends the first frame (LF or CRLF).
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4);
    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(lf.min(crlf)),
        (lf, crlf) => lf.or(crlf),
    }
}

fn frame_data(frame: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(frame).ok()?;
    let data: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data.join("\n")).ok()
}

/// Reads only the first data line and at most 256 bytes to identify an event.
/// Trigger frames are parsed fully only after this bounded scan succeeds.
fn scan_event_type(frame: &[u8]) -> Option<String> {
    let mut payload = Vec::with_capacity(256);
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let remaining = 256usize.saturating_sub(payload.len());
        payload.extend_from_slice(&data[..data.len().min(remaining)]);
        if payload.len() >= 256 {
            break;
        }
        payload.push(b'\n');
    }
    scan_top_level_type(&payload)
}

fn scan_sse_event_name(frame: &[u8]) -> Option<String> {
    frame.split(|byte| *byte == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let name = line.strip_prefix(b"event:")?.trim_ascii();
        (!name.is_empty())
            .then(|| std::str::from_utf8(name).ok().map(str::to_owned))
            .flatten()
    })
}

fn scan_top_level_type(data: &[u8]) -> Option<String> {
    let mut cursor = 0;
    skip_json_whitespace(data, &mut cursor);
    if data.get(cursor) != Some(&b'{') {
        return None;
    }
    cursor += 1;
    loop {
        skip_json_whitespace(data, &mut cursor);
        if data.get(cursor) == Some(&b'}') {
            return None;
        }
        let key = parse_json_string(data, &mut cursor)?;
        skip_json_whitespace(data, &mut cursor);
        if data.get(cursor) != Some(&b':') {
            return None;
        }
        cursor += 1;
        skip_json_whitespace(data, &mut cursor);
        if key == "type" {
            return parse_json_string(data, &mut cursor);
        }
        cursor = skip_json_value(data, cursor)?;
        skip_json_whitespace(data, &mut cursor);
        match data.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => return None,
            _ => return None,
        }
    }
}

fn scan_failure_code(data: &[u8]) -> Option<String> {
    let mut payload = Vec::with_capacity(MAX_PARTIAL_FAILURE_INSPECTION);
    for line in data.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let remaining = MAX_PARTIAL_FAILURE_INSPECTION.saturating_sub(payload.len());
        payload.extend_from_slice(&data[..data.len().min(remaining)]);
        if payload.len() >= MAX_PARTIAL_FAILURE_INSPECTION {
            break;
        }
        payload.push(b'\n');
    }
    let mut cursor = 0;
    skip_json_whitespace(&payload, &mut cursor);
    scan_object_path(&payload, &mut cursor, &["response", "error", "code"])
}

fn scan_object_path(data: &[u8], cursor: &mut usize, path: &[&str]) -> Option<String> {
    if data.get(*cursor) != Some(&b'{') || path.is_empty() {
        return None;
    }
    *cursor += 1;
    loop {
        skip_json_whitespace(data, cursor);
        if data.get(*cursor) == Some(&b'}') {
            return None;
        }
        let key = parse_json_string(data, cursor)?;
        skip_json_whitespace(data, cursor);
        if data.get(*cursor) != Some(&b':') {
            return None;
        }
        *cursor += 1;
        skip_json_whitespace(data, cursor);
        if key == path[0] {
            if path.len() == 1 {
                return parse_json_string(data, cursor);
            }
            return scan_object_path(data, cursor, &path[1..]);
        }
        *cursor = skip_json_value(data, *cursor)?;
        skip_json_whitespace(data, cursor);
        match data.get(*cursor) {
            Some(b',') => *cursor += 1,
            Some(b'}') => return None,
            _ => return None,
        }
    }
}

fn skip_json_whitespace(data: &[u8], cursor: &mut usize) {
    while data.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
        *cursor += 1;
    }
}

fn parse_json_string(data: &[u8], cursor: &mut usize) -> Option<String> {
    let start = *cursor;
    if data.get(*cursor) != Some(&b'\"') {
        return None;
    }
    *cursor += 1;
    let mut escaped = false;
    while let Some(&byte) = data.get(*cursor) {
        *cursor += 1;
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'\"' {
            return serde_json::from_slice(&data[start..*cursor]).ok();
        }
    }
    None
}

fn skip_json_value(data: &[u8], mut cursor: usize) -> Option<usize> {
    let opening = *data.get(cursor)?;
    if opening == b'\"' {
        parse_json_string(data, &mut cursor)?;
        return Some(cursor);
    }
    if opening == b'{' || opening == b'[' {
        let mut stack = vec![opening];
        cursor += 1;
        let mut escaped = false;
        while let Some(&byte) = data.get(cursor) {
            cursor += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
            } else if byte == b'\"' {
                while let Some(&inner) = data.get(cursor) {
                    cursor += 1;
                    if inner == b'\\' {
                        cursor += 1;
                    } else if inner == b'\"' {
                        break;
                    }
                }
            } else if byte == b'{' || byte == b'[' {
                stack.push(byte);
            } else if byte == b'}' || byte == b']' {
                let expected = if byte == b'}' { b'{' } else { b'[' };
                if stack.pop()? != expected {
                    return None;
                }
                if stack.is_empty() {
                    return Some(cursor);
                }
            }
        }
        return None;
    }
    while data
        .get(cursor)
        .is_some_and(|byte| !matches!(byte, b',' | b'}' | b']'))
    {
        cursor += 1;
    }
    Some(cursor)
}

fn contains_failure_marker(frame: &[u8]) -> bool {
    [
        b"response.failed".as_slice(),
        b"response.incomplete",
        b"response.completed",
        b"\"error\"",
    ]
    .iter()
    .any(|marker| frame.windows(marker.len()).any(|window| window == *marker))
}

fn normalized_type(kind: &str) -> String {
    if (1..=64).contains(&kind.len())
        && kind.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'_'
        })
    {
        kind.to_owned()
    } else {
        "other".to_owned()
    }
}

fn failure_kind(kind: &str, event: Option<&Value>) -> &'static str {
    match kind {
        "error" => "error_event",
        "response.incomplete" => "incomplete",
        "response.failed" => match event
            .and_then(|event| event.pointer("/response/error/code"))
            .and_then(Value::as_str)
        {
            Some("server_is_overloaded") => "overloaded",
            Some("rate_limit_exceeded" | "slow_down") => "rate_limited",
            _ => "other_code",
        },
        _ => "other_code",
    }
}

fn partial_failure_event(kind: &str, code: Option<String>) -> Value {
    match code {
        Some(code) if kind == "response.failed" => {
            json!({"type": kind, "response": {"error": {"code": code}}})
        }
        _ => json!({"type": kind}),
    }
}

fn failure_detail(event: &Value) -> Value {
    let mut detail = serde_json::Map::new();
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("other");
    detail.insert("type".into(), Value::String(normalized_type(event_type)));
    let mut error_keys = Vec::new();
    if let Some(response) = event.get("response").and_then(Value::as_object) {
        let mut response_detail = serde_json::Map::new();
        if let Some(value) = response
            .get("status")
            .and_then(Value::as_u64)
            .filter(|status| *status <= 999)
        {
            response_detail.insert("status".into(), json!(value));
        }
        if let Some(error) = response.get("error").and_then(Value::as_object) {
            response_detail.insert("error".into(), error_fields(error));
            error_keys.extend(error.keys().take(MAX_ERROR_KEYS).map(|key| key.to_owned()));
        }
        if let Some(reason) = response
            .get("incomplete_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("reason"))
        {
            response_detail.insert(
                "incomplete_details".into(),
                json!({"reason": bounded_detail_value(reason)}),
            );
        }
        if !response_detail.is_empty() {
            detail.insert("response".into(), Value::Object(response_detail));
        }
    }
    if let Some(error) = event.get("error").and_then(Value::as_object) {
        detail.insert("error".into(), top_level_error_fields(error));
        error_keys.extend(error.keys().take(MAX_ERROR_KEYS).map(|key| key.to_owned()));
    } else if event_type == "error"
        && let Some(error) = event.as_object()
    {
        detail.insert("error".into(), top_level_error_fields(error));
        error_keys.extend(
            ["code", "type", "message", "param"]
                .into_iter()
                .filter(|key| error.contains_key(*key))
                .map(str::to_owned),
        );
    }
    if !error_keys.is_empty() {
        error_keys.sort_unstable();
        error_keys.dedup();
        error_keys.truncate(MAX_ERROR_KEYS);
        detail.insert(
            "error_keys".into(),
            Value::Array(
                error_keys
                    .into_iter()
                    .map(|key| Value::String(redact_message(&key)))
                    .collect(),
            ),
        );
    }
    Value::Object(detail)
}

fn http_failure_detail(body: &Value) -> Value {
    let mut detail = serde_json::Map::new();
    if let Some(error) = body.get("error").and_then(Value::as_object) {
        detail.insert("error".into(), error_fields(error));
    }
    Value::Object(detail)
}

fn error_fields(error: &serde_json::Map<String, Value>) -> Value {
    let mut fields = serde_json::Map::new();
    for key in ["code", "type"] {
        if let Some(value) = error.get(key) {
            fields.insert(key.into(), bounded_detail_value(value));
        }
    }
    insert_message_metadata(error, &mut fields);
    if let Some(reason) = error
        .get("incomplete_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("reason"))
    {
        fields.insert(
            "incomplete_details".into(),
            json!({"reason": bounded_detail_value(reason)}),
        );
    }
    Value::Object(fields)
}

fn top_level_error_fields(error: &serde_json::Map<String, Value>) -> Value {
    let mut fields = serde_json::Map::new();
    for key in ["code", "type"] {
        if let Some(value) = error.get(key) {
            fields.insert(key.into(), bounded_detail_value(value));
        }
    }
    insert_message_metadata(error, &mut fields);
    Value::Object(fields)
}

fn bounded_detail_value(value: &Value) -> Value {
    let Some(value) = value.as_str() else {
        return Value::String("other".into());
    };
    let value = redact_message(value);
    if (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Value::String(value)
    } else {
        Value::String("other".into())
    }
}

fn insert_message_metadata(
    error: &serde_json::Map<String, Value>,
    fields: &mut serde_json::Map<String, Value>,
) {
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return;
    };
    let stripped = strip_ansi(message);
    let guarded = redact_message(&stripped);
    fields.insert("message_len".into(), json!(stripped.chars().count()));
    fields.insert("message_class".into(), json!(message_class(&guarded)));
}

fn message_class(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("capacity") || message.contains("overloaded") {
        "capacity"
    } else if message.contains("rate limit") || message.contains("try again") {
        "rate_limit"
    } else {
        "other"
    }
}

fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut clean = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                index += 1;
                match bytes.get(index).copied() {
                    Some(b'[') => {
                        index += 1;
                        while let Some(byte) = bytes.get(index).copied() {
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    Some(b']') => {
                        index += 1;
                        while index < bytes.len() {
                            if bytes[index] == 0x07 {
                                index += 1;
                                break;
                            }
                            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                                index += 2;
                                break;
                            }
                            index += 1;
                        }
                    }
                    Some(_) => {
                        index += text[index..].chars().next().map_or(1, char::len_utf8);
                    }
                    None => {}
                }
            }
            byte if byte < 0x20 || byte == 0x7f || (0x80..=0x9f).contains(&byte) => {
                index += 1;
            }
            _ => {
                let character = text[index..].chars().next().expect("valid UTF-8");
                if !character.is_control() {
                    clean.push(character);
                }
                index += character.len_utf8();
            }
        }
    }
    clean
}

fn redact_message(text: &str) -> String {
    static SECRET: OnceLock<Regex> = OnceLock::new();
    let secret = SECRET.get_or_init(|| {
        Regex::new(concat!(
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
            r"|\b(?:rt|sk|sess|at)[-_][A-Za-z0-9_-]{16,}",
            r"|(?i:\b(?:basic|bearer)\s+[A-Za-z0-9._~+/=-]{8,})",
            r"|(?i:\b(?:api[_-]?key|token|secret|credential)\s*[:=]\s*[A-Za-z0-9._~+/=-]{8,})",
            r"|(?i:\b(?:cookie|set-cookie)\s*:\s*[A-Za-z0-9._~+/=-]+)",
            r"|[A-Za-z0-9_-]{32,}",
        ))
        .expect("valid regex")
    });
    let plain = strip_ansi(text);
    let cleaned = secret.replace_all(&plain, "[redacted]");
    let mut short: String = cleaned.chars().take(300).collect();
    if cleaned.chars().count() > 300 {
        short.push('…');
    }
    short
}

/// Rebuilds the frame so every Codex version retries it after `secs`.
///
/// Codex 0.162.0-alpha.8 and later read `error.headers["retry-after"]`. Codex
/// 0.161.0 ignores advice on `server_is_overloaded` but reads "try again in
/// N s" from a `rate_limit_exceeded` message, so the code and message change
/// too. Temporary: SAW-12575 removes this rewrite once lanes run Codex 0.162
/// stable. Newer Codex reads the header first for that code, so both agree. The
/// final stop after the budget is the original frame, unchanged. Other lines
/// keep their order and text.
fn with_retry_advice(frame: &[u8], mut event: Value, secs: u64) -> Vec<u8> {
    let Some(error) = event
        .pointer_mut("/response/error")
        .and_then(Value::as_object_mut)
    else {
        return frame.to_vec();
    };
    error.insert("code".into(), Value::String("rate_limit_exceeded".into()));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Selected model is at capacity.")
        .to_owned();
    error.insert(
        "message".into(),
        Value::String(format!("{message} Please try again in {secs}s.")),
    );
    let headers = error.entry("headers").or_insert_with(|| json!({}));
    let Some(headers) = headers.as_object_mut() else {
        return frame.to_vec();
    };
    headers.insert("retry-after".into(), Value::String(secs.to_string()));
    rebuild_frame(frame, &event)
}

/// The exhausted stop: the original frame, minus any upstream retry advice,
/// so Codex stops and the board takes over.
fn without_retry_advice(frame: Vec<u8>, mut event: Value) -> Vec<u8> {
    let removed = event
        .pointer_mut("/response/error/headers")
        .and_then(Value::as_object_mut)
        .and_then(|headers| {
            let names: Vec<String> = headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("retry-after"))
                .cloned()
                .collect();
            names.iter().for_each(|name| {
                headers.remove(name);
            });
            (!names.is_empty()).then_some(())
        });
    match removed {
        Some(()) => rebuild_frame(&frame, &event),
        None => frame,
    }
}

/// Writes `event` as the frame's data line; other lines keep their order.
fn rebuild_frame(frame: &[u8], event: &Value) -> Vec<u8> {
    let text = String::from_utf8_lossy(frame);
    let mut out = String::with_capacity(frame.len() + 32);
    let mut wrote_data = false;
    for line in text.lines() {
        if line.starts_with("data:") {
            if !wrote_data {
                out.push_str("data: ");
                out.push_str(&event.to_string());
                out.push('\n');
                wrote_data = true;
            }
        } else if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    out.into_bytes()
}

async fn metrics(State(relay): State<Relay>) -> Response {
    let counts = relay.counts.lock().expect("metric lock");
    let mut out = String::from("# TYPE codexctl_relay_capacity_events_total counter\n");
    for ((kind, model, account_class, outcome), count) in counts.iter() {
        out.push_str(&format!(
            "codexctl_relay_capacity_events_total{{kind=\"{kind}\",model=\"{model}\",account_class=\"{account_class}\",outcome=\"{outcome}\"}} {count}\n"
        ));
    }
    out.push_str("# TYPE codexctl_relay_stream_failures_total counter\n");
    for ((kind, stage, model, account_class), count) in relay
        .stream_failures
        .lock()
        .expect("stream failure metric lock")
        .iter()
    {
        out.push_str(&format!(
            "codexctl_relay_stream_failures_total{{kind=\"{kind}\",stage=\"{stage}\",model=\"{model}\",account_class=\"{account_class}\"}} {count}\n"
        ));
    }
    out.push_str("# TYPE codexctl_relay_central_dropped_total counter\n");
    for (reason, count) in relay
        .central_dropped
        .lock()
        .expect("central drop metric lock")
        .iter()
    {
        out.push_str(&format!(
            "codexctl_relay_central_dropped_total{{reason=\"{reason}\"}} {count}\n"
        ));
    }
    (
        [(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        out,
    )
        .into_response()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_queue_full_is_counted_without_blocking_the_relay() {
        let (sender, _receiver) = mpsc::channel(CENTRAL_EVENT_QUEUE_CAPACITY);
        let dropped = Arc::new(Mutex::new(BTreeMap::new()));
        let relay = Relay {
            upstream: Arc::from("http://127.0.0.1:1"),
            client: reqwest::Client::new(),
            streaks: Arc::new(Mutex::new(Streaks::new(
                DEFAULT_RATE_BUDGET,
                DEFAULT_OVERLOADED_BUDGET,
            ))),
            counts: Arc::default(),
            stream_failures: Arc::default(),
            stream_log: Arc::new(Mutex::new(StreamFailureLogState::new())),
            log: Arc::new(|_| {}),
            central_events: Some(sender),
            central_dropped: dropped.clone(),
        };
        let labels = Labels {
            thread: "thread".into(),
            model: "gpt-6.1-sol".into(),
            account_class: "included",
        };

        for _ in 0..=CENTRAL_EVENT_QUEUE_CAPACITY {
            relay.event(Kind::Rate429, &labels, "exhausted", None, None);
        }

        assert_eq!(
            dropped
                .lock()
                .expect("central drop metric lock")
                .get("queue_full"),
            Some(&1)
        );
    }

    #[test]
    fn central_reporter_requires_tls_for_non_loopback_servers() {
        assert!(CentralReporter::new("http://central.example", "token").is_err());
        assert!(CentralReporter::new("https://central.example", "token").is_ok());
        assert!(CentralReporter::new("http://127.0.0.1:8787", "token").is_ok());
    }

    #[test]
    fn suppression_summary_reports_the_complete_previous_window() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let relay = Relay {
            upstream: Arc::from("http://127.0.0.1:1"),
            client: reqwest::Client::new(),
            streaks: Arc::new(Mutex::new(Streaks::new(
                DEFAULT_RATE_BUDGET,
                DEFAULT_OVERLOADED_BUDGET,
            ))),
            counts: Arc::default(),
            stream_failures: Arc::default(),
            stream_log: Arc::new(Mutex::new(StreamFailureLogState::new())),
            log: Arc::new(move |line| sink.lock().expect("test log lock").push(line.to_owned())),
            central_events: None,
            central_dropped: Arc::default(),
        };
        relay
            .stream_log
            .lock()
            .expect("stream log lock")
            .window_start = Instant::now() - STREAM_FAILURE_LOG_WINDOW - Duration::from_secs(1);
        relay.stream_log.lock().expect("stream log lock").suppressed = 10;
        relay.flush_suppression_summary();
        let lines = lines.lock().expect("test log lock");
        let summary: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(summary["suppressed"], 10);
    }

    #[test]
    fn post_output_partial_frame_buffer_is_bounded_and_forwarded() {
        let relay = Relay {
            upstream: Arc::from("http://127.0.0.1:1"),
            client: reqwest::Client::new(),
            streaks: Arc::new(Mutex::new(Streaks::new(
                DEFAULT_RATE_BUDGET,
                DEFAULT_OVERLOADED_BUDGET,
            ))),
            counts: Arc::default(),
            stream_failures: Arc::default(),
            stream_log: Arc::new(Mutex::new(StreamFailureLogState::new())),
            log: Arc::new(|_| {}),
            central_events: None,
            central_dropped: Arc::default(),
        };
        let labels = Labels {
            thread: "thread".into(),
            model: "gpt-6.1-sol".into(),
            account_class: "included",
        };
        let mut frames =
            FrameRewriter::new(relay, labels, None, HeaderMap::new(), Instant::now(), true);
        frames.push(b"data: {\"type\":\"response.created\"}\n\n");
        frames.push(b"data: {\"type\":\"response.output_text.delta\"}\n\n");
        let partial = vec![b'x'; MAX_PENDING_FRAME_BYTES * 2];
        let forwarded = frames.push(&partial);
        assert!(frames.pending.len() <= MAX_PENDING_FRAME_BYTES);
        assert_eq!(forwarded.len(), partial.len());
    }
}
