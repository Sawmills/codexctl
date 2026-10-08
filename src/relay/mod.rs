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
use serde_json::{Value, json};

use policy::{Advice, Http429, Kind, Streaks};

/// The only remote upstream the relay forwards to.
const CHATGPT: &str = "https://chatgpt.com";
/// Only this path prefix is forwarded; the relay is not a general proxy.
const FORWARDED_PREFIX: &str = "/backend-api/codex/";
/// Lane-only header that labels metrics; it never leaves the host.
const ACCOUNT_CLASS_HEADER: &str = "x-codexctl-account-class";
/// A 429 body larger than this is not classified and passes through.
const MAX_429_BODY: usize = 64 * 1024;
/// Default listen address used by `codexctl relay` and the lane override.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:47631";
/// Rate limits stop after this so the board can move the account (M5).
pub const DEFAULT_RATE_BUDGET: Duration = Duration::from_secs(180);
/// Model capacity is not per account, so it waits longer (M5).
pub const DEFAULT_OVERLOADED_BUDGET: Duration = Duration::from_secs(600);

type Log = Arc<dyn Fn(&str) + Send + Sync>;
/// kind, model, account class, outcome.
type MetricKey = (&'static str, String, &'static str, &'static str);

/// Relay settings. Build with [`RelayConfig::new`].
pub struct RelayConfig {
    upstream: String,
    rate_budget: Duration,
    overloaded_budget: Duration,
    log: Log,
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
}

#[derive(Clone)]
struct Relay {
    upstream: Arc<str>,
    client: reqwest::Client,
    streaks: Arc<Mutex<Streaks>>,
    counts: Arc<Mutex<BTreeMap<MetricKey, u64>>>,
    model_labels: Arc<Mutex<Vec<String>>>,
    log: Log,
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
    let relay = Relay {
        upstream: config.upstream.into(),
        client,
        streaks: Arc::new(Mutex::new(Streaks::new(
            config.rate_budget,
            config.overloaded_budget,
        ))),
        counts: Arc::default(),
        model_labels: Arc::default(),
        log: config.log,
    };
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .fallback(forward)
        .layer(DefaultBodyLimit::disable())
        .with_state(relay);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("relay stopped")
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
    if !path.starts_with(FORWARDED_PREFIX) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let labels = Labels {
        thread: header_str(&parts.headers, "thread-id")
            .or_else(|| header_str(&parts.headers, "session-id"))
            .unwrap_or("unknown")
            .to_owned(),
        model: relay.model_label(&parts.headers, &body),
        account_class: match header_str(&parts.headers, ACCOUNT_CLASS_HEADER) {
            Some("included") => "included",
            Some("credit") => "credit",
            _ => "unknown",
        },
    };
    let mut headers = parts.headers;
    for name in [
        "host",
        "connection",
        "content-length",
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
        .body(body)
        .send()
        .await;
    let upstream = match upstream {
        Ok(upstream) => upstream,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = upstream.status();
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
    let is_stream = status.is_success()
        && header_str(&headers, "content-type").is_some_and(|t| t.starts_with("text/event-stream"))
        && headers.get("content-encoding").is_none();
    let mut builder = Response::builder().status(status);
    if is_stream {
        headers.remove("content-length");
        *builder.headers_mut().expect("fresh builder") = headers;
        let frames = FrameRewriter::new(relay, labels, request_id);
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
    async fn rate_limited(
        &self,
        upstream: reqwest::Response,
        mut headers: HeaderMap,
        labels: Labels,
        request_id: Option<String>,
    ) -> Response {
        let body = match upstream.bytes().await {
            Ok(body) => body,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        let class = if body.len() > MAX_429_BODY {
            Http429::Rate
        } else {
            policy::classify_429(&body)
        };
        match class {
            Http429::Terminal => {
                self.event(
                    Kind::Rate429,
                    &labels,
                    "terminal_passthrough",
                    None,
                    request_id.as_deref(),
                );
            }
            Http429::Rate => match self.failure(Kind::Rate429, &labels, request_id.as_deref()) {
                Some(secs) => {
                    if !headers.contains_key("retry-after") {
                        headers.insert("retry-after", HeaderValue::from(secs));
                    }
                }
                None => {
                    headers.remove("retry-after");
                }
            },
        }
        let mut response = Response::builder().status(StatusCode::TOO_MANY_REQUESTS);
        *response.headers_mut().expect("fresh builder") = headers;
        response
            .body(Body::from(body))
            .expect("valid response parts")
    }

    /// Records one failure. Returns the advised delay, or `None` when the
    /// streak is exhausted and Codex must stop.
    fn failure(&self, kind: Kind, labels: &Labels, request_id: Option<&str>) -> Option<u64> {
        let advice =
            self.streaks
                .lock()
                .expect("streak lock")
                .failure(&labels.thread, kind, Instant::now());
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
            Advice::StillExhausted => None,
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

    /// A bounded model label: one of at most 16 `gpt-` names seen, else `other`.
    fn model_label(&self, headers: &HeaderMap, body: &Bytes) -> String {
        #[derive(serde::Deserialize)]
        struct Model {
            model: Option<String>,
        }
        let model = (headers.get("content-encoding").is_none())
            .then(|| serde_json::from_slice::<Model>(body).ok())
            .flatten()
            .and_then(|body| body.model)
            .filter(|model| {
                model.starts_with("gpt-")
                    && model.len() <= 40
                    && model.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'
                    })
            });
        let Some(model) = model else {
            return "other".into();
        };
        let mut labels = self.model_labels.lock().expect("model label lock");
        if labels.contains(&model) {
            return model;
        }
        if labels.len() >= 16 {
            return "other".into();
        }
        labels.push(model.clone());
        model
    }
}

/// Passes a Responses event stream through and, before any output, adds retry
/// advice to a capacity `response.failed` event.
struct FrameRewriter {
    relay: Relay,
    labels: Labels,
    request_id: Option<String>,
    pending: Vec<u8>,
    output_started: bool,
}

impl FrameRewriter {
    fn new(relay: Relay, labels: Labels, request_id: Option<String>) -> Self {
        Self {
            relay,
            labels,
            request_id,
            pending: Vec::new(),
            output_started: false,
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
                            return Some((
                                Err(std::io::Error::other(error)),
                                (upstream, frames, true),
                            ));
                        }
                        None => {
                            let rest = std::mem::take(&mut frames.pending);
                            return (!rest.is_empty())
                                .then(|| (Ok(Bytes::from(rest)), (upstream, frames, true)));
                        }
                    }
                }
            },
        )
    }

    /// Returns the bytes ready to send. Whole frames are inspected until
    /// output starts; after that, bytes pass through untouched.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.output_started {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = find_frame_end(&self.pending) {
            let frame: Vec<u8> = self.pending.drain(..end).collect();
            if self.output_started {
                out.extend_from_slice(&frame);
                continue;
            }
            out.extend_from_slice(&self.frame(frame));
        }
        if self.output_started {
            out.append(&mut self.pending);
        }
        out
    }

    fn frame(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let Some(event) = frame_data(&frame) else {
            return frame;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.created" | "response.in_progress") => frame,
            Some("response.failed") if policy::is_overloaded_failure(&event) => {
                match self
                    .relay
                    .failure(Kind::Overloaded, &self.labels, self.request_id.as_deref())
                {
                    Some(secs) => with_retry_advice(&frame, event, secs),
                    None => frame,
                }
            }
            Some("response.failed") => frame,
            _ => {
                self.output_started = true;
                self.relay.success(&self.labels);
                frame
            }
        }
    }
}

/// Index just past the blank line that ends the first frame.
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
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
        } else {
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
