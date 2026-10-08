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
    http::HeaderMap,
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
const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n";

enum Scripted {
    Status(u16, Vec<(&'static str, String)>, String),
    Sse(Vec<String>),
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
    tokio::spawn(async move {
        relay::serve(listener, config, std::future::pending())
            .await
            .unwrap()
    });
    Harness {
        relay_url,
        upstream,
        logs,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    async fn post(&self, thread: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/backend-api/codex/responses", self.relay_url))
            .header("authorization", "Bearer synthetic-secret-token")
            .header("thread-id", thread)
            .header("x-codexctl-account-class", "included")
            .json(&json!({"model": "gpt-6.1-sol", "input": "synthetic-body-marker"}))
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
}

#[tokio::test]
async fn overloaded_after_output_passes_through_byte_for_byte() {
    let frames = vec![CREATED.to_string(), DELTA.into(), OVERLOADED.into()];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(h.post("t1").await.text().await.unwrap(), frames.concat());
}

#[tokio::test]
async fn streamed_rate_limit_events_are_left_to_codex() {
    let rate = OVERLOADED.replace("server_is_overloaded", "rate_limit_exceeded");
    let frames = vec![CREATED.to_string(), rate];
    let h = harness(vec![Scripted::Sse(frames.clone())]).await;
    assert_eq!(h.post("t1").await.text().await.unwrap(), frames.concat());
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
