//! Why the provider rejected an owner's stored refresh grant.
//!
//! Codex logs the token endpoint's status and error code to stderr when a
//! refresh fails (`Failed to refresh token status=… detail=TokenErrorDetail {
//! error_code: Some("…"), … }`). The owner child's stderr is the only place
//! that cause appears, so it is read here, reduced to a bounded reason, and
//! counted per account. Forwarded lines are capped, rate limited and redacted.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use regex::Regex;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Bytes of one stderr line held while reading; the rest is dropped.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// Longest forwarded stderr line, in characters.
const MAX_LINE_CHARS: usize = 300;
/// Forwarded stderr lines per window; the rest is drained and counted.
const MAX_LINES_PER_WINDOW: usize = 30;
const WINDOW: Duration = Duration::from_secs(60);
/// The line Codex logs when the token endpoint rejects a refresh.
const REFRESH_FAILED: &str = "Failed to refresh token";

/// The latest provider refresh reason seen on one owner child's stderr.
#[derive(Clone, Default)]
pub(super) struct ReasonSlot(Arc<Mutex<Option<&'static str>>>);

impl ReasonSlot {
    pub(super) fn take(&self) -> Option<&'static str> {
        self.0.lock().expect("refresh reason lock").take()
    }

    fn set(&self, reason: &'static str) {
        *self.0.lock().expect("refresh reason lock") = Some(reason);
    }
}

/// Codex's fixed messages for a permanent refresh rejection, after
/// `Failed to refresh token: ` (codex-rs/login/src/auth/manager.rs). Codex
/// logs the same unknown message for 400 `invalid_grant` and for a 401 with
/// another code, so those two share `other`.
const REJECTIONS: &[(&str, &str)] = &[
    (
        "Your access token could not be refreshed because your refresh token was already used.",
        "refresh_token_reused",
    ),
    (
        "Your access token could not be refreshed because your refresh token has expired.",
        "refresh_token_expired",
    ),
    (
        "Your access token could not be refreshed because your refresh token was revoked.",
        "refresh_token_invalidated",
    ),
    (
        "Your access token could not be refreshed because you have since logged out or signed in to another account.",
        "account_mismatch",
    ),
    (
        "Your access token could not be refreshed. Please log out and sign in again.",
        "other",
    ),
];

/// Removes terminal color codes; Codex's stderr logger emits them.
fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"))
        .replace_all(line, "")
}

/// Reduces a Codex refresh log line to a bounded reason. Only Codex's fixed
/// permanent-rejection messages count; a transient failure (Codex logs it
/// with `status=` and retries) and any other line return `None`.
pub(super) fn classify(line: &str) -> Option<&'static str> {
    let line = strip_ansi(line);
    let message = line.split_once(REFRESH_FAILED)?.1.strip_prefix(": ")?;
    REJECTIONS
        .iter()
        .find(|(text, _)| message.starts_with(text))
        .map(|(_, reason)| *reason)
}

/// Removes token-shaped values and caps the length. Codex already keeps
/// tokens out of these logs; this is a second guard before they leave the pod.
pub(super) fn redact(line: &str) -> String {
    static SECRET: OnceLock<Regex> = OnceLock::new();
    let secret = SECRET.get_or_init(|| {
        Regex::new(concat!(
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
            r"|\b(?:rt|sk|sess|at)[-_][A-Za-z0-9_-]{16,}",
            r"|[A-Za-z0-9_-]{40,}",
        ))
        .expect("valid regex")
    });
    let plain = strip_ansi(line);
    let cleaned = secret.replace_all(&plain, "[redacted]");
    let mut short: String = cleaned.chars().take(MAX_LINE_CHARS).collect();
    if cleaned.chars().count() > MAX_LINE_CHARS {
        short.push('…');
    }
    short
}

/// Drains an owner child's stderr. Every byte is read, so the child never
/// blocks on a full pipe, and at most `MAX_LINE_BYTES` of one line are held;
/// the rest of an overlong line is dropped. At most `MAX_LINES_PER_WINDOW`
/// lines per minute are forwarded to `sink`, redacted, as structured JSON. A
/// refresh failure line always sets `slot`, even when forwarding is suppressed.
pub(super) async fn forward(stderr: impl AsyncRead + Unpin, slot: ReasonSlot, sink: impl Fn(&str)) {
    forward_with(stderr, slot, sink, |_| {}).await;
}

/// `forward`, reporting the bytes held for each completed line to `held`.
async fn forward_with(
    mut stderr: impl AsyncRead + Unpin,
    slot: ReasonSlot,
    sink: impl Fn(&str),
    held: impl Fn(usize),
) {
    let mut window_start = Instant::now();
    let mut forwarded = 0usize;
    let mut suppressed = 0u64;
    let mut line = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let mut handle = |line: &[u8]| {
        held(line.len());
        let line = String::from_utf8_lossy(line);
        if let Some(reason) = classify(&line) {
            slot.set(reason);
        }
        if window_start.elapsed() >= WINDOW {
            if suppressed > 0 {
                sink(&suppressed_note(suppressed));
            }
            window_start = Instant::now();
            forwarded = 0;
            suppressed = 0;
        }
        if forwarded < MAX_LINES_PER_WINDOW {
            forwarded += 1;
            sink(
                &json!({"operation":"owner_child","stage":"stderr","line":redact(&line)})
                    .to_string(),
            );
        } else {
            suppressed += 1;
        }
    };
    loop {
        let read = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for &byte in &chunk[..read] {
            if byte == b'\n' {
                handle(&line);
                line.clear();
            } else if line.len() < MAX_LINE_BYTES {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        handle(&line);
    }
    drop(handle);
    if suppressed > 0 {
        sink(&suppressed_note(suppressed));
    }
}

fn suppressed_note(suppressed: u64) -> String {
    json!({"operation":"owner_child","stage":"stderr","suppressed":suppressed}).to_string()
}

/// Failures per (account, account key, reason) for this server process.
type FailureKey = (String, String, &'static str);

fn failures() -> &'static Mutex<BTreeMap<FailureKey, u64>> {
    static FAILURES: OnceLock<Mutex<BTreeMap<FailureKey, u64>>> = OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

/// Counts one rejected owner refresh. `account` is the readable alias; two
/// company users can share it, so the series also carries the first 12
/// characters of the same digest as `managed::account_key`. The set of server
/// accounts is small, so both labels stay bounded.
pub(super) fn record(account: &str, user: &str, reason: &'static str) {
    let key = super::vault::digest(format!("{user}\0{}", account.to_ascii_lowercase()).as_bytes());
    *failures()
        .lock()
        .expect("refresh failure lock")
        .entry((account.to_owned(), key[..12].to_owned(), reason))
        .or_default() += 1;
}

/// Prometheus text for `codexctl_central_owner_refresh_failed_total`.
pub(super) fn metrics() -> String {
    let escape = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    };
    failures()
        .lock()
        .expect("refresh failure lock")
        .iter()
        .map(|((account, key, reason), count)| {
            format!(
                "codexctl_central_owner_refresh_failed_total{{account=\"{}\",account_key=\"{key}\",reason=\"{reason}\"}} {count}\n",
                escape(account)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real lines from codex-cli 0.161.0 app-server stderr (ANSI included),
    // captured against a local token endpoint answering each provider code.
    const REUSED_TEXT: &str = "\x1b[2m2026-10-08T21:04:25.249445Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_login::auth::manager\x1b[0m\x1b[2m:\x1b[0m Failed to refresh token: Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";
    const EXPIRED_TEXT: &str = "\x1b[2m2026-10-08T21:04:44.322603Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_login::auth::manager\x1b[0m\x1b[2m:\x1b[0m Failed to refresh token: Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.";
    const REVOKED_TEXT: &str = "\x1b[2m2026-10-08T21:04:44.837697Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_login::auth::manager\x1b[0m\x1b[2m:\x1b[0m Failed to refresh token: Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.";
    /// What Codex logs for 400 invalid_grant and for a 401 with another code.
    const UNKNOWN_TEXT: &str = "\x1b[2m2026-10-08T21:04:45.953995Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_login::auth::manager\x1b[0m\x1b[2m:\x1b[0m Failed to refresh token: Your access token could not be refreshed. Please log out and sign in again.";
    /// A transient 503: Codex retries it, so it is not a rejection.
    const TRANSIENT_TEXT: &str = "\x1b[2m2026-10-08T21:04:46.574894Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_login::auth::manager\x1b[0m\x1b[2m:\x1b[0m Failed to refresh token \x1b[3mstatus\x1b[0m\x1b[2m=\x1b[0m503 Service Unavailable \x1b[3mdetail\x1b[0m\x1b[2m=\x1b[0mTokenErrorDetail { error_code: Some(\"server_error\"), error_message: Some(\"synthetic rejection\"), .. }";

    #[test]
    fn real_codex_refresh_lines_map_to_a_bounded_reason() {
        assert_eq!(classify(REUSED_TEXT), Some("refresh_token_reused"));
        assert_eq!(classify(EXPIRED_TEXT), Some("refresh_token_expired"));
        assert_eq!(classify(REVOKED_TEXT), Some("refresh_token_invalidated"));
        assert_eq!(classify(UNKNOWN_TEXT), Some("other"));
        assert_eq!(
            classify(
                "ERROR codex_login::auth::manager: Failed to refresh token: Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again."
            ),
            Some("account_mismatch")
        );
        assert_eq!(classify(TRANSIENT_TEXT), None, "transient, Codex retries");
        assert_eq!(
            classify("ERROR codex_login: Failed to refresh token: connection reset"),
            None,
            "not a provider rejection message"
        );
        assert_eq!(classify("ERROR codex_core: stream disconnected"), None);
    }

    #[test]
    fn forwarded_lines_drop_tokens_and_stay_short() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyLTEyMyJ9.c2lnbmF0dXJlLXZhbHVlLWhlcmU";
        let line = format!(
            "ERROR x: refresh with rt_{} and Bearer {jwt} failed {}",
            "A".repeat(40),
            "y".repeat(2000)
        );
        let safe = redact(&line);
        assert!(!safe.contains(jwt), "{safe}");
        assert!(!safe.contains(&"A".repeat(40)), "{safe}");
        assert!(safe.contains("[redacted]"));
        // Short prefixed tokens below the 40-character arm are caught too.
        for short in [
            "sk-abcdefghijklmnop12",
            "rt_0123456789abcdefXY",
            "at-ABCDEFGHIJKLMNOPQR",
        ] {
            let safe = redact(&format!("ERROR x: token {short} rejected"));
            assert!(!safe.contains(short), "{short} leaked: {safe}");
        }
        assert!(safe.chars().count() <= MAX_LINE_CHARS + 1, "{}", safe.len());
        let plain = redact(REUSED_TEXT);
        assert!(plain.contains("refresh token was already used"), "{plain}");
        assert!(!plain.contains('\x1b'), "color codes removed: {plain:?}");
    }

    #[tokio::test]
    async fn stderr_is_drained_rate_limited_and_the_reason_is_kept() {
        let mut input = String::new();
        for i in 0..(MAX_LINES_PER_WINDOW + 10) {
            input.push_str(&format!("WARN noise line {i}\n"));
        }
        input.push_str(REUSED_TEXT);
        input.push('\n');
        let slot = ReasonSlot::default();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        forward(input.as_bytes(), slot.clone(), move |line| {
            sink.lock().unwrap().push(line.to_owned())
        })
        .await;
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines.len(),
            MAX_LINES_PER_WINDOW + 1,
            "window cap plus one suppression note"
        );
        assert!(
            lines.last().unwrap().contains("\"suppressed\""),
            "{lines:?}"
        );
        assert_eq!(
            slot.take(),
            Some("refresh_token_reused"),
            "drained past the cap"
        );
    }

    #[test]
    fn metrics_count_each_failure_per_account_and_reason() {
        record("metrics-test", "metrics-user-a", "refresh_token_reused");
        record("metrics-test", "metrics-user-a", "refresh_token_reused");
        record("metrics-test", "metrics-user-b", "refresh_token_reused");
        record("other\"acct", "metrics-user-a", "invalid_grant");
        let text = metrics();
        let a = &super::super::vault::digest(b"metrics-user-a\0metrics-test")[..12];
        let b = &super::super::vault::digest(b"metrics-user-b\0metrics-test")[..12];
        assert!(
            text.contains(&format!(
                "codexctl_central_owner_refresh_failed_total{{account=\"metrics-test\",account_key=\"{a}\",reason=\"refresh_token_reused\"}} 2\n"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{{account=\"metrics-test\",account_key=\"{b}\",reason=\"refresh_token_reused\"}} 1\n"
            )),
            "same alias, other user, separate series: {text}"
        );
        assert!(text.contains("account=\"other\\\"acct\","), "{text}");
    }

    #[tokio::test]
    async fn an_endless_line_is_capped_while_it_is_read() {
        let mut input = vec![b'x'; 4 * 1024 * 1024];
        input.push(b'\n');
        input.extend_from_slice(REUSED_TEXT.as_bytes());
        input.push(b'\n');
        let slot = ReasonSlot::default();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let longest = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = longest.clone();
        forward_with(
            input.as_slice(),
            slot.clone(),
            move |line| sink.lock().unwrap().push(line.to_owned()),
            move |held| {
                seen.fetch_max(held, std::sync::atomic::Ordering::Relaxed);
            },
        )
        .await;
        assert!(
            longest.load(std::sync::atomic::Ordering::Relaxed) <= MAX_LINE_BYTES,
            "held {} bytes for one line",
            longest.load(std::sync::atomic::Ordering::Relaxed)
        );
        assert_eq!(lines.lock().unwrap().len(), 2);
        assert_eq!(
            slot.take(),
            Some("refresh_token_reused"),
            "the next line still parses"
        );
    }
}
