#![cfg(feature = "central-prototype")]
//! The relay sits between a Codex lane and the Responses endpoint. It never
//! retries by itself: it only adds retry advice that Codex's own turn loop
//! honors, and passes everything else through byte for byte.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use codexctl::relay::{self, RelayConfig};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

const RATE_429: &str = r#"{"error":{"type":"rate_limit_exceeded","code":"rate_limit_exceeded","message":"Rate limit reached"}}"#;
const CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n";
const OVERLOADED: &str = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"r1\",\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"Selected model is at capacity. Please try a different model.\"}}}\n\n";
const DELTA: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
const REASONING_DELTA: &str = "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"synthetic reasoning\"}\n\n";
const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n";

enum Scripted {
    Status(u16, Vec<(&'static str, String)>, String),
    Sse(Vec<String>),
    Chunked(u16, Vec<String>),
    /// Sends the first frame, then waits for the test before each later one and
    /// reports when the relay stopped reading.
    Gated(Vec<String>, mpsc::Receiver<()>, oneshot::Sender<()>),
    Delayed(Duration, String),
}

#[derive(Default)]
struct Seen {
    headers: Vec<HeaderMap>,
    bodies: Vec<Bytes>,
}

#[derive(Clone)]
struct Upstream {
    script: Arc<Mutex<VecDeque<Scripted>>>,
    seen: Arc<Mutex<Seen>>,
}

async fn upstream_responses(
    State(upstream): State<Upstream>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    {
        let mut seen = upstream.seen.lock().unwrap();
        seen.headers.push(headers);
        seen.bodies.push(body);
    }
    let next = upstream
        .script
        .lock()
        .unwrap()
        .pop_front()
        .expect("unscripted upstream request");
    match next {
        Scripted::Status(status, headers, body) => {
            let mut response = Response::builder().status(status);
            for (name, value) in headers {
                response = response.header(name, value);
            }
            response.body(Body::from(body)).unwrap()
        }
        Scripted::Sse(frames) => sse(Body::from(frames.concat())),
        Scripted::Chunked(status, frames) => {
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
            tokio::spawn(async move {
                for frame in frames {
                    if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                        break;
                    }
                }
            });
            Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(tokio_stream(rx)))
                .unwrap()
        }
        Scripted::Gated(frames, mut gate, closed) => {
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
            tokio::spawn(async move {
                let mut frames = frames.into_iter();
                if let Some(first) = frames.next() {
                    let _ = tx.send(Ok(Bytes::from(first))).await;
                }
                for frame in frames {
                    tokio::select! {
                        () = tx.closed() => break,
                        opened = gate.recv() => if opened.is_none() { break },
                    }
                    if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                        break;
                    }
                }
                tx.closed().await;
                let _ = closed.send(());
            });
            sse(Body::from_stream(tokio_stream(rx)))
        }
        Scripted::Delayed(delay, frame) => {
            tokio::time::sleep(delay).await;
            sse(Body::from(frame))
        }
    }
}

fn tokio_stream<T: Send + 'static>(
    mut rx: mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> + Send + 'static {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

fn sse(body: Body) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

struct Harness {
    relay_url: String,
    upstream: Upstream,
    logs: Arc<Mutex<Vec<String>>>,
    client: reqwest::Client,
    shutdown: Option<oneshot::Sender<()>>,
    serve_task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct CentralSink {
    events: mpsc::Sender<(HeaderMap, Bytes)>,
}

async fn central_capacity_event(
    State(sink): State<CentralSink>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    sink.events.send((headers, body)).await.unwrap();
    StatusCode::NO_CONTENT
}

async fn slow_central_capacity_event(
    State(started): State<Arc<Mutex<Option<oneshot::Sender<()>>>>>,
    _headers: HeaderMap,
    _body: Bytes,
) -> StatusCode {
    if let Some(sender) = started.lock().unwrap().take() {
        let _ = sender.send(());
    }
    tokio::time::sleep(Duration::from_secs(30)).await;
    StatusCode::NO_CONTENT
}

async fn harness(script: Vec<Scripted>) -> Harness {
    harness_with(script, |config| config).await
}

async fn harness_with(
    script: Vec<Scripted>,
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> Harness {
    let upstream = Upstream {
        script: Arc::new(Mutex::new(script.into())),
        seen: Arc::default(),
    };
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(upstream.clone());
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(upstream_listener, app).await.unwrap() });

    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let config = configure(
        RelayConfig::new(&upstream_url)
            .unwrap()
            .with_log(move |line| sink.lock().unwrap().push(line.to_string())),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, shutdown_rx) = oneshot::channel();
    let serve_task = tokio::spawn(async move {
        relay::serve(listener, config, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap()
    });
    Harness {
        relay_url,
        upstream,
        logs,
        client: reqwest::Client::new(),
        shutdown: Some(shutdown),
        serve_task,
    }
}

#[tokio::test]
async fn central_report_contains_only_the_four_metric_labels() {
    let (events, mut received) = mpsc::channel(1);
    let sink = CentralSink { events };
    let app = Router::new()
        .route("/v1/relay/capacity-events", post(central_capacity_event))
        .with_state(sink);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let central_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let h = harness_with(
        vec![Scripted::Status(429, vec![], RATE_429.into())],
        |config| {
            config
                .with_budgets(Duration::ZERO, Duration::ZERO)
                .with_central_reporter(&central_url, "central-test-token")
                .unwrap()
        },
    )
    .await;
    assert_eq!(
        h.post_model("thread-hidden-from-central", "gpt-future-private")
            .await
            .status(),
        429
    );

    let (headers, body) = tokio::time::timeout(Duration::from_secs(1), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer central-test-token")
    );
    let event: Value = serde_json::from_slice(&body).unwrap();
    let object = event.as_object().unwrap();
    assert_eq!(object.len(), 4, "central payload: {event}");
    for label in ["kind", "model", "account_class", "outcome"] {
        assert!(object.contains_key(label), "central payload: {event}");
    }
    assert_eq!(event["model"], "other");
    assert!(!object.contains_key("thread_id"));
    assert!(!object.contains_key("request_id"));
}

#[tokio::test]
async fn central_rate_limit_has_its_own_drop_reason() {
    let app = Router::new().route(
        "/v1/relay/capacity-events",
        post(|| async { StatusCode::TOO_MANY_REQUESTS }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let central_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let h = harness_with(
        vec![Scripted::Status(429, vec![], RATE_429.into())],
        |config| {
            config
                .with_budgets(Duration::ZERO, Duration::ZERO)
                .with_central_reporter(&central_url, "central-test-token")
                .unwrap()
        },
    )
    .await;
    assert_eq!(h.post("central-limited").await.status(), 429);
    let metrics = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let metrics = h.metrics().await;
            if metrics.contains("codexctl_relay_central_dropped_total{reason=\"rate_limited\"} 1") {
                break metrics;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("central 429 must be counted as rate limited");
    assert!(!metrics.contains("reason=\"send_failed\""), "{metrics}");
}

#[tokio::test]
async fn advised_capacity_events_do_not_consume_central_event_budget() {
    let (events, mut received) = mpsc::channel(4);
    let sink = CentralSink { events };
    let app = Router::new()
        .route("/v1/relay/capacity-events", post(central_capacity_event))
        .with_state(sink);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let central_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let h = harness_with(
        vec![
            Scripted::Status(429, vec![], RATE_429.into()),
            Scripted::Status(429, vec![], RATE_429.into()),
        ],
        |config| {
            config
                .with_central_reporter(&central_url, "central-test-token")
                .unwrap()
        },
    )
    .await;
    assert_eq!(h.post("advised-1").await.status(), 429);
    assert_eq!(h.post("advised-2").await.status(), 429);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "advised events must not enter the central reporter queue"
    );
}

#[tokio::test]
async fn unreachable_central_does_not_add_request_latency() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let central_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let h = harness_with(
        vec![Scripted::Status(429, vec![], RATE_429.into())],
        |config| {
            config
                .with_budgets(Duration::ZERO, Duration::ZERO)
                .with_central_reporter(&central_url, "central-test-token")
                .unwrap()
        },
    )
    .await;

    let started = std::time::Instant::now();
    let response = tokio::time::timeout(Duration::from_millis(500), h.post("latency"))
        .await
        .expect("central reporting must not block the relay");
    assert_eq!(response.status(), 429);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "request waited for unreachable central: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn relay_shutdown_drops_in_flight_central_reports() {
    let (started, started_rx) = oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started)));
    let app = Router::new()
        .route(
            "/v1/relay/capacity-events",
            post(slow_central_capacity_event),
        )
        .with_state(started);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let central_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let h = harness_with(
        vec![Scripted::Status(429, vec![], RATE_429.into())],
        |config| {
            config
                .with_budgets(Duration::ZERO, Duration::ZERO)
                .with_central_reporter(&central_url, "central-test-token")
                .unwrap()
        },
    )
    .await;
    assert_eq!(h.post("shutdown").await.status(), 429);
    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("central reporter must start the request")
        .unwrap();
    let started = std::time::Instant::now();
    h.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "shutdown waited for the central request: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn unknown_models_use_the_other_metric_label() {
    let h = harness(vec![Scripted::Status(429, vec![], RATE_429.into())]).await;
    assert_eq!(
        h.post_model("unknown-model", "gpt-future-private")
            .await
            .status(),
        429
    );
    assert!(
        h.metrics()
            .await
            .contains("model=\"other\",account_class=\"included\""),
        "unknown model must be normalized in relay metrics"
    );
}

#[tokio::test]
async fn gpt_5_5_keeps_its_central_metric_label() {
    let h = harness(vec![Scripted::Status(429, vec![], RATE_429.into())]).await;
    assert_eq!(h.post_model("gpt-5-5", "gpt-5.5").await.status(), 429);
    assert!(
        h.metrics()
            .await
            .contains("model=\"gpt-5.5\",account_class=\"included\""),
        "supported gpt-5.5 must not be normalized to other"
    );
}

impl Harness {
    async fn shutdown(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        self.serve_task.await.unwrap();
    }

    async fn post(&self, thread: &str) -> reqwest::Response {
        self.post_model(thread, "gpt-6.1-sol").await
    }

    async fn post_model(&self, thread: &str, model: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/backend-api/codex/responses", self.relay_url))
            .header("authorization", "Bearer synthetic-secret-token")
            .header("thread-id", thread)
            .header("x-codexctl-account-class", "included")
            .json(&json!({"model": model, "input": "synthetic-body-marker"}))
            .send()
            .await
            .unwrap()
    }

    async fn metrics(&self) -> String {
        self.client
            .get(format!("{}/metrics", self.relay_url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }
}

fn retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .map(|value| value.to_str().unwrap().parse().unwrap())
}

fn failed_event(body: &str) -> Value {
    let data = body
        .split("\n\n")
        .find(|frame| frame.contains("response.failed"))
        .expect("response.failed frame")
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

fn metric(metrics: &str, needle: &str) -> u64 {
    metrics
        .lines()
        .find(|line| {
            line.starts_with("codexctl_relay_capacity_events_total") && line.contains(needle)
        })
        .and_then(|line| line.rsplit(' ').next())
        .map(|value| value.parse().unwrap())
        .unwrap_or(0)
}

fn metric_stream(metrics: &str, needle: &str) -> u64 {
    metrics
        .lines()
        .find(|line| {
            line.starts_with("codexctl_relay_stream_failures_total") && line.contains(needle)
        })
        .and_then(|line| line.rsplit(' ').next())
        .map(|value| value.parse().unwrap())
        .unwrap_or(0)
}

#[tokio::test]
async fn rate_limited_429_gets_retry_advice_and_keeps_its_body() {
    let h = harness(vec![Scripted::Status(429, vec![], RATE_429.into())]).await;
    let response = h.post("t1").await;
    assert_eq!(response.status(), 429);
    let delay = retry_after(&response).expect("relay adds Retry-After");
    assert!((1..=60).contains(&delay), "delay {delay}");
    assert_eq!(response.text().await.unwrap(), RATE_429);
}

#[tokio::test]
async fn upstream_retry_advice_is_kept_as_sent() {
    let h = harness(vec![Scripted::Status(
        429,
        vec![("retry-after", "7".into())],
        RATE_429.into(),
    )])
    .await;
    assert_eq!(retry_after(&h.post("t1").await), Some(7));
}

#[tokio::test]
async fn usage_and_billing_429s_stop_codex_without_retry_advice() {
    let terminal = [
        json!({"error": {"type": "usage_limit_reached", "message": "x"}}),
        json!({"error": {"type": "usage_not_included"}}),
        json!({"error": {"type": "insufficient_quota"}}),
        json!({"error": {"code": "insufficient_quota"}}),
        json!({"error": {"code": "credit_balance_exhausted"}}),
        json!({"error": {"code": "organization_spend_limit_exceeded"}}),
        json!({"error": {"code": "project_spend_limit_exceeded"}}),
        json!({"error": {"code": "organization_usage_limit_exceeded"}}),
        json!({"error": {"code": "flex_unavailable"}}),
        json!({"error": {"code": "usage_limit_reached"}}),
        json!({"error": {"type": "credit_balance_exhausted"}}),
    ];
    for body in terminal {
        let body = body.to_string();
        let h = harness(vec![Scripted::Status(429, vec![], body.clone())]).await;
        let response = h.post("t1").await;
        assert_eq!(retry_after(&response), None, "{body}");
        assert_eq!(response.text().await.unwrap(), body);
        assert_eq!(
            metric(&h.metrics().await, "outcome=\"terminal_passthrough\""),
            1,
            "{body}"
        );
    }
}

#[tokio::test]
async fn other_statuses_pass_through_unchanged() {
    for status in [401u16, 403, 500, 503] {
        let h = harness(vec![Scripted::Status(status, vec![], "{}".into())]).await;
        let response = h.post("t1").await;
        assert_eq!(response.status(), status);
        assert_eq!(retry_after(&response), None, "{status}");
    }
}

#[tokio::test]
async fn responses_http_error_is_recorded_without_changing_body() {
    let body =
        r#"{"error":{"code":"server_error","type":"upstream","message":"temporary failure"}}"#;
    let h = harness(vec![Scripted::Status(
        500,
        vec![
            ("cf-ray", "ray-test".into()),
            ("x-oai-request-id", "oai-test".into()),
        ],
        body.into(),
    )])
    .await;
    let response = h.post("http-error").await;
    assert_eq!(response.status(), 500);
    assert_eq!(response.text().await.unwrap(), body);
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .unwrap();
    assert_eq!(failure["kind"], "http_error");
    assert_eq!(failure["http_status"], 500);
    assert_eq!(failure["cf_ray"], "ray-test");
    assert_eq!(failure["x_oai_request_id"], "oai-test");
    assert_eq!(failure["failure"]["error"]["code"], "server_error");
    assert_eq!(metric_stream(&h.metrics().await, "kind=\"http_error\""), 1);
}

#[tokio::test]
async fn split_responses_http_error_keeps_bounded_details() {
    let chunks = vec![
        r#"{"error":{"code":"server_"#.to_owned(),
        r#"error","type":"upstream","message":"temporary failure"}}"#.to_owned(),
    ];
    let expected = chunks.concat();
    let h = harness(vec![Scripted::Chunked(500, chunks)]).await;
    let response = h.post("split-http-error").await;
    assert_eq!(response.status(), 500);
    assert_eq!(response.text().await.unwrap(), expected);
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .unwrap();
    assert_eq!(failure["failure"]["error"]["code"], "server_error");
}

#[tokio::test]
async fn stream_failure_redacts_messages_and_never_logs_output_text() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.signature-value";
    let token = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    let bearer = "Bearer synthetic-secret-token";
    let short_key = "0123456789abcdef0123456789abcdef";
    let basic = "Basic dXNlcjpwYXNz";
    let cookie = "Cookie: sid=abc123";
    let failure = format!(
        "event: response.failed\ndata: {}\n\n",
        json!({
            "type": "response.failed",
            "response": {"error": {"code": "model_unavailable", "message": format!("\u{1b}[2J\u{1b}[K\u{1b}]0;evil\u{7}jwt {jwt} token {token} {bearer} {basic} {cookie} key {short_key}")}}
        })
    );
    let output = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"NEVER_LOG_THIS_OUTPUT\"}\n\n";
    let h = harness(vec![Scripted::Sse(vec![output.into(), failure])]).await;
    h.post("redaction").await.text().await.unwrap();
    let logs = h.logs.lock().unwrap().join("\n");
    assert!(!logs.contains('\u{1b}'), "ANSI escape leaked: {logs}");
    assert!(!logs.contains("evil"), "OSC payload leaked: {logs}");
    assert!(!logs.contains(jwt), "JWT leaked: {logs}");
    assert!(!logs.contains(token), "token leaked: {logs}");
    assert!(!logs.contains(bearer), "bearer credential leaked: {logs}");
    assert!(!logs.contains(basic), "basic credential leaked: {logs}");
    assert!(!logs.contains(cookie), "cookie credential leaked: {logs}");
    assert!(!logs.contains(short_key), "short key leaked: {logs}");
    assert!(
        !logs.contains("NEVER_LOG_THIS_OUTPUT"),
        "output leaked: {logs}"
    );
    assert!(
        logs.contains("[redacted]"),
        "redaction marker missing: {logs}"
    );
}

#[tokio::test]
async fn stream_failure_type_trace_is_bounded() {
    let mut frames = vec![CREATED.to_string(), DELTA.to_string()];
    for index in 0..40 {
        frames.push(format!(
            "event: response.custom_{index}\ndata: {{\"type\":\"response.custom_{index}\"}}\n\n"
        ));
    }
    frames.push(OVERLOADED.into());
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(
        h.post("bounded").await.text().await.unwrap(),
        frames.concat()
    );
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .unwrap();
    assert!(failure["types"].as_array().unwrap().len() <= 32);
    assert_eq!(failure["truncated"], true);
}

#[tokio::test]
async fn stream_failure_logs_are_rate_limited() {
    let script = (0..40)
        .map(|_| Scripted::Sse(vec![OVERLOADED.into()]))
        .collect();
    let h = harness_with(script, |config| {
        config.with_budgets(Duration::ZERO, Duration::ZERO)
    })
    .await;
    for index in 0..40 {
        assert_eq!(h.post(&format!("limited-{index}")).await.status(), 200);
    }
    let logs = h.logs.lock().unwrap();
    let stream_lines: Vec<Value> = logs
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["outcome"] == "stream_failure")
        .collect();
    assert_eq!(stream_lines.len(), 30);
    assert!(stream_lines.iter().all(|line| line["suppressed"].is_null()));
}

#[tokio::test]
async fn multiple_failure_frames_emit_one_stream_failure_observation() {
    let second = OVERLOADED.replace("server_is_overloaded", "model_unavailable");
    let h = harness(vec![Scripted::Sse(vec![OVERLOADED.into(), second])]).await;
    h.post("one-observation").await.text().await.unwrap();
    let logs = h.logs.lock().unwrap();
    let count = logs
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["outcome"] == "stream_failure")
        .count();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn overloaded_before_output_gets_retry_advice_in_the_event() {
    let h = harness(vec![Scripted::Sse(vec![CREATED.into(), OVERLOADED.into()])]).await;
    let body = h.post("t1").await.text().await.unwrap();
    assert!(body.starts_with(CREATED), "head frames pass unchanged");
    let event = failed_event(&body);
    let advice = event["response"]["error"]["headers"]["retry-after"]
        .as_str()
        .expect("retry-after in error.headers");
    let secs: u64 = advice.parse().unwrap();
    assert!((1..=60).contains(&secs));
    // Codex 0.161.0 retries only this code, with the delay from the message.
    assert_eq!(event["response"]["error"]["code"], "rate_limit_exceeded");
    assert_eq!(
        event["response"]["error"]["message"],
        format!(
            "Selected model is at capacity. Please try a different model. Please try again in {secs}s."
        )
    );
    assert!(body.ends_with("\n\n"), "frame stays a complete event");
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("pre-output failure is recorded");
    assert_eq!(failure["kind"], "overloaded");
    assert_eq!(failure["stage"], "pre_output");
    assert_eq!(failure["advised"], true);
}

#[tokio::test]
async fn multiline_sse_failure_is_classified_and_advised() {
    let multiline = "event: response.failed\ndata: {\ndata: \"type\":\"response.failed\",\ndata: \"response\":{\"error\":{\"code\":\"server_is_overloaded\"}}}\n\n";
    let h = harness(vec![Scripted::Sse(vec![multiline.into()])]).await;
    let body = h.post("multiline").await.text().await.unwrap();
    let event = failed_event(&body);
    assert!(event["response"]["error"]["headers"]["retry-after"].is_string());
    assert!(
        h.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("\"outcome\":\"stream_failure\""))
    );
}

#[tokio::test]
async fn large_sse_failure_payload_still_gets_advice() {
    let padding = "x".repeat(400);
    let multiline = format!(
        "event: response.failed\ndata: {{\"padding\":\"{padding}\",\ndata: \"type\":\"response.failed\",\ndata: \"response\":{{\"error\":{{\"code\":\"server_is_overloaded\"}}}}}}\n\n"
    );
    let h = harness(vec![Scripted::Sse(vec![multiline])]).await;
    let body = h.post("large-failure").await.text().await.unwrap();
    assert!(!body.contains("server_is_overloaded"));
    assert!(body.contains("retry-after"));
}

#[tokio::test]
async fn oversized_post_output_failure_is_recorded_before_partial_flush() {
    let prefix = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"";
    let suffix = "\"}}}\n\n";
    let padding = "x".repeat(70 * 1024);
    let frames = vec![
        CREATED.to_string(),
        REASONING_DELTA.to_string(),
        prefix.to_owned(),
        padding,
        suffix.to_owned(),
    ];
    let expected = frames.concat();
    let h = harness(vec![Scripted::Chunked(200, frames)]).await;
    assert_eq!(
        h.post("oversized-after-output").await.text().await.unwrap(),
        expected
    );
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("oversized failure is recorded");
    assert_eq!(failure["kind"], "overloaded");
    assert_eq!(failure["stage"], "after_output");
}

#[tokio::test]
async fn oversized_post_output_completed_frame_is_not_truncated() {
    let prefix = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"padding\":\"";
    let suffix = "\"}}}\n\n";
    let padding = "x".repeat(70 * 1024);
    let frames = vec![
        CREATED.to_string(),
        REASONING_DELTA.to_string(),
        prefix.to_owned(),
        padding,
        suffix.to_owned(),
    ];
    let expected = frames.concat();
    let h = harness(vec![Scripted::Chunked(200, frames)]).await;
    assert_eq!(
        h.post("oversized-completed").await.text().await.unwrap(),
        expected
    );
    assert!(!h.logs.lock().unwrap().iter().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| value["kind"] == "truncated")
    }));
}

#[tokio::test]
async fn oversized_post_output_failure_with_late_type_is_not_truncated() {
    let prefix = "event: response.failed\ndata: {\"response\":{\"error\":{\"code\":\"server_is_overloaded\"}},\"padding\":\"";
    let suffix = "\",\"type\":\"response.failed\"}\n\n";
    let padding = "x".repeat(70 * 1024);
    let frames = vec![
        CREATED.to_string(),
        REASONING_DELTA.to_string(),
        prefix.to_owned(),
        padding,
        suffix.to_owned(),
    ];
    let h = harness(vec![Scripted::Chunked(200, frames)]).await;
    h.post("oversized-late-type").await.text().await.unwrap();
    assert!(!h.logs.lock().unwrap().iter().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| value["kind"] == "truncated")
    }));
}

#[tokio::test]
async fn oversized_failure_uses_top_level_type_after_nested_error_type() {
    let prefix = r#"event: response.failed
data: {"response":{"error":{"type":"server_error","code":"server_is_overloaded"}},"padding":""#;
    let suffix = r#"","type":"response.failed"}

"#;
    let padding = "x".repeat(70 * 1024);
    let mut chunks = vec![prefix.to_owned()];
    chunks.extend(
        padding
            .as_bytes()
            .chunks(200)
            .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap()),
    );
    chunks.push(suffix.to_owned());
    let h = harness(vec![Scripted::Chunked(200, chunks)]).await;
    h.post("nested-type").await.text().await.unwrap();
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("failure is recorded");
    assert_eq!(failure["kind"], "overloaded");
}

#[tokio::test]
async fn oversized_complete_failure_is_recorded_without_parsing_the_body() {
    let padding = "x".repeat(70 * 1024);
    let event = json!({
        "type": "response.failed",
        "response": {"error": {"code": "server_is_overloaded", "message": padding}}
    });
    let failure = format!("event: response.failed\ndata: {event}\n\n");
    let h = harness(vec![Scripted::Sse(vec![
        CREATED.into(),
        REASONING_DELTA.into(),
        failure.clone(),
    ])])
    .await;
    assert_eq!(
        h.post("oversized-complete").await.text().await.unwrap(),
        format!("{CREATED}{REASONING_DELTA}{failure}")
    );
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("oversized failure is recorded");
    assert_eq!(failure["kind"], "overloaded");
    assert_eq!(failure["stage"], "after_output");
    assert_eq!(failure["failure"]["type"], "response.failed");
    assert!(
        failure.to_string().len() < 10_000,
        "failure log must stay bounded"
    );
}

#[tokio::test]
async fn oversized_pre_output_overload_gets_retry_advice() {
    let padding = "x".repeat(70 * 1024);
    let event = json!({
        "type": "response.failed",
        "response": {"error": {"code": "server_is_overloaded", "message": padding}}
    });
    let failure = format!("event: response.failed\ndata: {event}\n\n");
    let h = harness(vec![Scripted::Sse(vec![failure])]).await;
    let body = h
        .post("oversized-before-output")
        .await
        .text()
        .await
        .unwrap();
    assert!(!body.contains("server_is_overloaded"));
    assert!(body.contains("retry-after"));
}

#[tokio::test]
async fn late_terminal_type_does_not_create_a_false_truncation() {
    let padding = "x".repeat(400);
    let completed = format!(
        "event: response.completed\ndata: {{\"response\":{{\"padding\":\"{padding}\"}},\"type\":\"response.completed\"}}\n\n"
    );
    let h = harness(vec![Scripted::Sse(vec![
        CREATED.into(),
        REASONING_DELTA.into(),
        completed.clone(),
    ])])
    .await;
    assert_eq!(
        h.post("late-completed").await.text().await.unwrap(),
        format!("{CREATED}{REASONING_DELTA}{completed}")
    );
    assert!(!h.logs.lock().unwrap().iter().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| value["kind"] == "truncated")
    }));
}

#[tokio::test]
async fn data_only_late_completed_type_does_not_create_a_false_truncation() {
    let padding = "x".repeat(400);
    let completed = format!(
        "data: {{\"response\":{{\"padding\":\"{padding}\"}},\"type\":\"response.completed\"}}\n\n"
    );
    let h = harness(vec![Scripted::Sse(vec![
        CREATED.into(),
        REASONING_DELTA.into(),
        completed.clone(),
    ])])
    .await;
    assert_eq!(
        h.post("data-only-late-completed")
            .await
            .text()
            .await
            .unwrap(),
        format!("{CREATED}{REASONING_DELTA}{completed}")
    );
    assert!(!h.logs.lock().unwrap().iter().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| value["kind"] == "truncated")
    }));
}

#[tokio::test]
async fn oversized_data_only_late_completed_type_does_not_create_a_false_truncation() {
    let padding = "{".repeat(70 * 1024);
    let completed = format!(
        "data: {{\"response\":{{\"padding\":\"{padding}\"}},\"type\":\"response.completed\"}}\n\n"
    );
    let mut chunks = vec![CREATED.into(), REASONING_DELTA.into()];
    chunks.extend(
        completed
            .as_bytes()
            .chunks(200)
            .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap()),
    );
    let h = harness(vec![Scripted::Chunked(200, chunks)]).await;
    assert_eq!(
        h.post("oversized-data-only-late-completed")
            .await
            .text()
            .await
            .unwrap(),
        format!("{CREATED}{REASONING_DELTA}{completed}")
    );
    assert!(!h.logs.lock().unwrap().iter().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|value| value["kind"] == "truncated")
    }));
}

#[tokio::test]
async fn overloaded_after_output_passes_through_byte_for_byte() {
    let frames = vec![
        CREATED.to_string(),
        REASONING_DELTA.into(),
        REASONING_DELTA.into(),
        REASONING_DELTA.into(),
        OVERLOADED.into(),
    ];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(h.post("t1").await.text().await.unwrap(), frames.concat());
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("post-output failure is recorded");
    assert_eq!(failure["kind"], "overloaded");
    assert_eq!(failure["stage"], "after_output");
    assert_eq!(failure["advised"], false);
    assert_eq!(failure["types"][0], json!(["response.created", 1]));
    assert_eq!(
        failure["types"][1],
        json!(["response.reasoning_summary_text.delta", 3])
    );
    assert_eq!(failure["types"][2], json!(["response.failed", 1]));
    assert!(failure["ms_to_first_output"].is_number());
}

#[tokio::test]
async fn audio_output_before_failure_is_not_rewritten() {
    let audio = r#"event: response.audio.delta
data: {"type":"response.audio.delta","delta":"abc"}

"#;
    let frames = vec![CREATED.to_string(), audio.to_owned(), OVERLOADED.to_owned()];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(
        h.post("audio-output").await.text().await.unwrap(),
        frames.concat()
    );
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .expect("post-output failure is recorded");
    assert_eq!(failure["stage"], "after_output");
    assert_eq!(failure["advised"], false);
}

#[tokio::test]
async fn streamed_rate_limit_events_are_left_to_codex() {
    let rate = OVERLOADED.replace("server_is_overloaded", "rate_limit_exceeded");
    let frames = vec![CREATED.to_string(), rate];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(h.post("t1").await.text().await.unwrap(), frames.concat());
}

#[tokio::test]
async fn unknown_failed_code_is_recorded_without_rewriting() {
    let unknown = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":500,\"error\":{\"code\":\"model_unavailable\",\"type\":\"server_error\",\"message\":\"try later\"}}}\n\n";
    let h = harness(vec![Scripted::Sse(vec![CREATED.into(), unknown.into()])]).await;
    let body = h.post("unknown-code").await.text().await.unwrap();
    assert!(body.contains(unknown));
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .unwrap();
    assert_eq!(failure["kind"], "other_code");
    assert_eq!(failure["advised"], false);
    assert_eq!(
        failure["failure"]["response"]["error"]["code"],
        "model_unavailable"
    );
}

#[tokio::test]
async fn error_and_incomplete_events_are_recorded() {
    let error = "event: error\ndata: {\"type\":\"error\",\"code\":\"bad_gateway\",\"message\":\"provider failed\",\"param\":null}\n\n";
    let incomplete = "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
    for (frame, expected_kind) in [(error, "error_event"), (incomplete, "incomplete")] {
        let h = harness(vec![Scripted::Sse(vec![frame.into()])]).await;
        assert_eq!(h.post("failure-event").await.text().await.unwrap(), frame);
        let failure = h
            .logs
            .lock()
            .unwrap()
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|line| line["outcome"] == "stream_failure")
            .unwrap();
        assert_eq!(failure["kind"], expected_kind);
        assert_eq!(failure["advised"], false);
        if expected_kind == "error_event" {
            assert_eq!(failure["failure"]["error"]["code"], "bad_gateway");
            assert_eq!(failure["failure"]["error"]["message"], "provider failed");
            assert!(failure["failure"]["error"]["param"].is_null());
        }
    }
}

#[tokio::test]
async fn stream_without_terminal_event_is_recorded_as_truncated() {
    let frames = vec![CREATED.to_string(), REASONING_DELTA.into()];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(
        h.post("truncated").await.text().await.unwrap(),
        frames.concat()
    );
    let failure = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["outcome"] == "stream_failure")
        .unwrap();
    assert_eq!(failure["kind"], "truncated");
    assert_eq!(failure["stage"], "after_output");
    assert!(failure["failure"].is_null());
}

#[tokio::test]
async fn exhausted_budget_lets_codex_stop_and_counts_each_streak_once() {
    let h = harness_with(
        vec![
            Scripted::Status(429, vec![], RATE_429.into()),
            Scripted::Status(429, vec![("retry-after", "3".into())], RATE_429.into()),
            Scripted::Sse(vec![OVERLOADED.into()]),
        ],
        |config| config.with_budgets(Duration::ZERO, Duration::ZERO),
    )
    .await;
    assert_eq!(retry_after(&h.post("t1").await), None);
    assert_eq!(
        retry_after(&h.post("t1").await),
        None,
        "upstream advice is removed so Codex stops"
    );
    assert_eq!(h.post("t2").await.text().await.unwrap(), OVERLOADED);
    let metrics = h.metrics().await;
    assert_eq!(
        metric(
            &metrics,
            "kind=\"rate_429\",model=\"gpt-6.1-sol\",account_class=\"included\",outcome=\"exhausted\""
        ),
        2,
        "each stop ends its streak; the next request is a resume"
    );
    assert_eq!(
        metric(
            &metrics,
            "kind=\"overloaded\",model=\"gpt-6.1-sol\",account_class=\"included\",outcome=\"exhausted\""
        ),
        1
    );
}

#[tokio::test]
async fn success_after_advice_counts_one_recovery_per_streak() {
    let h = harness(vec![
        Scripted::Status(429, vec![], RATE_429.into()),
        Scripted::Status(429, vec![], RATE_429.into()),
        Scripted::Sse(vec![CREATED.into(), DELTA.into(), COMPLETED.into()]),
        Scripted::Sse(vec![CREATED.into(), COMPLETED.into()]),
    ])
    .await;
    for _ in 0..4 {
        h.post("t1").await.text().await.unwrap();
    }
    let metrics = h.metrics().await;
    assert_eq!(metric(&metrics, "outcome=\"advised\""), 2);
    assert_eq!(
        metric(
            &metrics,
            "kind=\"rate_429\",model=\"gpt-6.1-sol\",account_class=\"included\",outcome=\"recovered\""
        ),
        1
    );
}

#[tokio::test]
async fn request_bytes_and_headers_reach_upstream_unchanged() {
    let h = harness(vec![Scripted::Sse(vec![COMPLETED.into()])]).await;
    let raw = vec![0x28, 0xb5, 0x2f, 0xfd, 0, 1, 2, 255];
    h.client
        .post(format!("{}/backend-api/codex/responses", h.relay_url))
        .header("authorization", "Bearer synthetic-secret-token")
        .header("content-encoding", "zstd")
        .header("chatgpt-account-id", "acct-1")
        .header("x-codexctl-account-class", "credit")
        .body(raw.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let seen = h.upstream.seen.lock().unwrap();
    assert_eq!(seen.bodies[0].as_ref(), raw.as_slice());
    let headers = &seen.headers[0];
    assert_eq!(headers["authorization"], "Bearer synthetic-secret-token");
    assert_eq!(headers["content-encoding"], "zstd");
    assert_eq!(headers["chatgpt-account-id"], "acct-1");
    assert!(
        headers.get("x-codexctl-account-class").is_none(),
        "relay-only header never leaves the host"
    );
}

#[tokio::test]
async fn responses_stream_and_client_disconnect_cancels_upstream() {
    let (gate, gate_rx) = mpsc::channel(1);
    let (closed_tx, closed_rx) = oneshot::channel();
    let h = harness(vec![Scripted::Gated(
        vec![CREATED.into(), DELTA.into(), DELTA.into(), COMPLETED.into()],
        gate_rx,
        closed_tx,
    )])
    .await;
    let response = h.post("t1").await;
    let mut stream = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("first frame streams before the rest exists")
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), CREATED.as_bytes());
    gate.send(()).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second.as_ref(), DELTA.as_bytes());
    drop(stream);
    let _ = gate.send(()).await;
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("upstream stream closes after the client leaves")
        .unwrap();
}

#[tokio::test]
async fn post_output_partial_frames_forward_without_waiting_for_the_delimiter() {
    let (gate, gate_rx) = mpsc::channel(1);
    let (closed_tx, closed_rx) = oneshot::channel();
    let partial = "event: response.failed\ndata: {\"type\":\"response.failed\"";
    let h = harness(vec![Scripted::Gated(
        vec![CREATED.into(), DELTA.into(), partial.into()],
        gate_rx,
        closed_tx,
    )])
    .await;
    let response = h.post("partial-forward").await;
    let mut stream = response.bytes_stream();
    assert_eq!(
        stream.next().await.unwrap().unwrap().as_ref(),
        CREATED.as_bytes()
    );
    gate.send(()).await.unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap().as_ref(),
        DELTA.as_bytes()
    );
    gate.send(()).await.unwrap();
    let forwarded = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("partial post-output frame is forwarded promptly")
        .unwrap()
        .unwrap();
    assert_eq!(forwarded.as_ref(), partial.as_bytes());
    drop(stream);
    let _ = gate.send(()).await;
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("upstream stream closes after the client leaves")
        .unwrap();
}

#[tokio::test]
async fn post_output_partial_frame_is_not_replayed_at_eof() {
    let partial = "event: response.failed\ndata: {\"type\":\"response.failed\"";
    let frames = vec![CREATED.to_string(), DELTA.to_string(), partial.to_owned()];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(
        h.post("partial-eof").await.text().await.unwrap(),
        frames.concat()
    );
}

#[tokio::test]
async fn slow_upstream_headers_do_not_time_out() {
    let h = harness(vec![Scripted::Delayed(
        Duration::from_secs(3),
        COMPLETED.into(),
    )])
    .await;
    let response = h.post("t1").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), COMPLETED);
}

/// The full 120-second wait runs on the devbox: `cargo test -- --ignored`.
#[tokio::test]
#[ignore]
async fn upstream_headers_after_120_seconds_still_arrive() {
    let h = harness(vec![Scripted::Delayed(
        Duration::from_secs(125),
        COMPLETED.into(),
    )])
    .await;
    assert_eq!(h.post("t1").await.text().await.unwrap(), COMPLETED);
}

#[tokio::test]
async fn logs_never_carry_tokens_or_bodies() {
    let h = harness(vec![
        Scripted::Status(429, vec![], RATE_429.into()),
        Scripted::Sse(vec![CREATED.into(), OVERLOADED.into()]),
    ])
    .await;
    h.post("t1").await.text().await.unwrap();
    h.post("t1").await.text().await.unwrap();
    let logs = h.logs.lock().unwrap().join("\n");
    assert!(!logs.is_empty(), "each event writes one log line");
    for secret in [
        "synthetic-secret-token",
        "synthetic-body-marker",
        "Rate limit reached",
    ] {
        assert!(!logs.contains(secret), "log leaked {secret}: {logs}");
    }
    for line in logs.lines() {
        serde_json::from_str::<Value>(line).expect("structured JSON log line");
    }
}

#[test]
fn relay_refuses_non_loopback_binds_and_foreign_upstreams() {
    assert!(RelayConfig::new("https://chatgpt.com").is_ok());
    assert!(RelayConfig::new("http://127.0.0.1:9").is_ok());
    for bad in [
        "http://chatgpt.com",
        "https://example.com",
        "https://chatgpt.com.example.com",
        "http://10.0.0.1:80",
        "https://chatgpt.com/backend-api",
    ] {
        assert!(RelayConfig::new(bad).is_err(), "{bad}");
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let config = RelayConfig::new("https://chatgpt.com").unwrap();
        assert!(
            relay::serve(listener, config, std::future::pending())
                .await
                .is_err()
        );
    });
}

#[tokio::test]
async fn exhausted_capacity_event_loses_upstream_retry_advice() {
    let with_advice = OVERLOADED.replace(
        "\"code\":\"server_is_overloaded\"",
        "\"code\":\"server_is_overloaded\",\"headers\":{\"retry-after\":\"5\"}",
    );
    let h = harness_with(
        vec![Scripted::Sse(vec![CREATED.into(), with_advice])],
        |config| config.with_budgets(Duration::ZERO, Duration::ZERO),
    )
    .await;
    let body = h.post("t1").await.text().await.unwrap();
    let event = failed_event(&body);
    assert_eq!(event["response"]["error"]["code"], "server_is_overloaded");
    assert!(
        event["response"]["error"]["headers"]
            .get("retry-after")
            .is_none(),
        "Codex must stop once the budget is spent: {body}"
    );
}

#[tokio::test]
async fn crlf_framed_capacity_events_get_advice() {
    let crlf = |frame: &str| frame.replace('\n', "\r\n");
    let h = harness(vec![Scripted::Sse(vec![crlf(CREATED), crlf(OVERLOADED)])]).await;
    let body = h.post("t1").await.text().await.unwrap();
    let event: Value = serde_json::from_str(
        body.split("\r\n\r\n")
            .chain(body.split("\n\n"))
            .find(|frame| frame.contains("response.failed"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert!(
        event["response"]["error"]["headers"]["retry-after"].is_string(),
        "{body}"
    );
}

#[tokio::test]
async fn a_terminal_429_ends_the_streak() {
    let terminal = json!({"error": {"type": "usage_limit_reached"}}).to_string();
    let h = harness(vec![
        Scripted::Status(429, vec![], RATE_429.into()),
        Scripted::Status(429, vec![], RATE_429.into()),
        Scripted::Status(429, vec![], terminal),
        Scripted::Status(429, vec![], RATE_429.into()),
    ])
    .await;
    for _ in 0..4 {
        h.post("t1").await.text().await.unwrap();
    }
    let logs = h.logs.lock().unwrap().clone();
    let last: Value = serde_json::from_str(logs.last().unwrap()).unwrap();
    assert_eq!(last["outcome"], "advised");
    assert_eq!(
        last["attempt"], 1,
        "the resume after a stop starts at attempt 1"
    );
}

#[tokio::test]
async fn oversized_429_bodies_pass_through_without_advice() {
    let body = format!(
        r#"{{"error":{{"type":"usage_limit_reached","message":"{}"}}}}"#,
        "x".repeat(70 * 1024)
    );
    let h = harness(vec![Scripted::Status(429, vec![], body.clone())]).await;
    let response = h.post("t1").await;
    assert_eq!(retry_after(&response), None);
    assert_eq!(response.text().await.unwrap(), body);
}

#[tokio::test]
async fn metadata_before_output_does_not_end_the_head() {
    let metadata =
        "event: response.metadata\ndata: {\"type\":\"response.metadata\",\"metadata\":{}}\n\n";
    let limits = "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\"}\n\n";
    let h = harness(vec![Scripted::Sse(vec![
        CREATED.into(),
        metadata.into(),
        limits.into(),
        OVERLOADED.into(),
    ])])
    .await;
    let body = h.post("t1").await.text().await.unwrap();
    let event = failed_event(&body);
    assert!(
        event["response"]["error"]["headers"]["retry-after"].is_string(),
        "{body}"
    );
    let metrics = h.metrics().await;
    assert_eq!(metric(&metrics, "outcome=\"recovered\""), 0, "{metrics}");
}

#[tokio::test]
async fn upstream_advice_beyond_the_budget_lets_codex_stop() {
    let h = harness(vec![Scripted::Status(
        429,
        vec![("retry-after", "200".into())],
        RATE_429.into(),
    )])
    .await;
    assert_eq!(
        retry_after(&h.post("t1").await),
        None,
        "200 s does not fit the 180 s rate budget"
    );
}

#[tokio::test]
async fn large_requests_stream_through_and_keep_the_model_label() {
    let h = harness(vec![Scripted::Status(429, vec![], RATE_429.into())]).await;
    let big = format!(
        r#"{{"model":"gpt-6.1-sol","stream":true,"input":"{}"}}"#,
        "x".repeat(3 * 1024 * 1024)
    );
    h.client
        .post(format!("{}/backend-api/codex/responses", h.relay_url))
        .header("thread-id", "t1")
        .body(big.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        h.upstream.seen.lock().unwrap().bodies[0].as_ref(),
        big.as_bytes()
    );
    assert_eq!(
        metric(&h.metrics().await, "model=\"gpt-6.1-sol\""),
        1,
        "model read from the first bytes"
    );
}
