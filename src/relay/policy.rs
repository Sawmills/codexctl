//! Which upstream failures get retry advice, and how long a failure streak may
//! keep advising before Codex is allowed to stop.

use std::{
    collections::HashMap,
    hash::{BuildHasher, Hasher},
    time::{Duration, Instant},
};

use serde_json::Value;

/// Base of the doubling delay schedule.
const BASE_DELAY_SECS: u64 = 2;
/// Longest single delay the relay advises.
const MAX_DELAY_SECS: u64 = 60;
/// A failure this long after the previous one starts a new streak: Codex
/// retries within `MAX_DELAY_SECS`, so a longer gap means a later resume.
const STREAK_IDLE: Duration = Duration::from_secs(120);
/// Streaks idle this long are dropped so the table stays bounded.
const STREAK_EVICT: Duration = Duration::from_secs(3600);

/// Codex treats these as final (`codex-api/src/api_bridge.rs` 429 branch and
/// `sse/responses_error.rs`). A retry would hide a usage, billing or plan stop
/// from the quota guard, so they never get advice. Matched in `type` and `code`.
const TERMINAL: &[&str] = &[
    "usage_limit_reached",
    "usage_not_included",
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "flex_unavailable",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Rate429,
    Overloaded,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Kind::Rate429 => "rate_429",
            Kind::Overloaded => "overloaded",
        }
    }
}

/// What an HTTP 429 body means for advice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Http429 {
    /// A rate limit Codex may retry once told when.
    Rate,
    /// A usage, billing or plan stop that must reach Codex unchanged.
    Terminal,
}

pub(crate) fn classify_429(body: &[u8]) -> Http429 {
    let error = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body.get("error").cloned());
    let named = |field: &str| {
        error
            .as_ref()
            .and_then(|error| error.get(field))
            .and_then(Value::as_str)
            .is_some_and(|value| TERMINAL.contains(&value))
    };
    if named("type") || named("code") {
        Http429::Terminal
    } else {
        Http429::Rate
    }
}

/// Returns true for a `response.failed` event whose error is a model capacity
/// stop. Codex already retries the streamed rate-limit codes by itself.
pub(crate) fn is_overloaded_failure(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("response.failed")
        && event
            .pointer("/response/error/code")
            .and_then(Value::as_str)
            == Some("server_is_overloaded")
}

/// The result of one advisable failure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Advice {
    /// Tell Codex to retry after this many seconds.
    RetryAfter { secs: u64, attempt: u32 },
    /// The streak spent its budget; Codex stops on this failure.
    Exhausted { attempt: u32 },
}

/// A recovery: an advised streak that ended in a successful response.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub(crate) kind: Kind,
    pub(crate) attempts: u32,
}

struct Streak {
    kind: Kind,
    started: Instant,
    last_failure: Instant,
    attempts: u32,
    exhausted: bool,
}

/// Failure streaks per Codex thread.
pub(crate) struct Streaks {
    rate_budget: Duration,
    overloaded_budget: Duration,
    by_thread: HashMap<String, Streak>,
    jitter: std::collections::hash_map::RandomState,
    draws: u64,
}

impl Streaks {
    pub(crate) fn new(rate_budget: Duration, overloaded_budget: Duration) -> Self {
        Self {
            rate_budget,
            overloaded_budget,
            by_thread: HashMap::new(),
            jitter: std::collections::hash_map::RandomState::new(),
            draws: 0,
        }
    }

    pub(crate) fn failure(&mut self, thread: &str, kind: Kind, now: Instant) -> Advice {
        self.by_thread
            .retain(|_, streak| now.duration_since(streak.last_failure) < STREAK_EVICT);
        let streak = self
            .by_thread
            .entry(thread.to_owned())
            .and_modify(|streak| {
                // A new streak when the kind changes (its own budget), after a
                // long gap, or after exhaustion: Codex stopped then, so this
                // failure belongs to a resumed turn.
                if streak.kind != kind
                    || streak.exhausted
                    || now.duration_since(streak.last_failure) > STREAK_IDLE
                {
                    *streak = Streak::new(kind, now);
                }
            })
            .or_insert_with(|| Streak::new(kind, now));
        streak.last_failure = now;
        streak.attempts += 1;
        let attempt = streak.attempts;
        let cap = BASE_DELAY_SECS
            .saturating_mul(1u64 << (attempt - 1).min(16))
            .min(MAX_DELAY_SECS);
        let budget = match kind {
            Kind::Rate429 => self.rate_budget,
            Kind::Overloaded => self.overloaded_budget,
        };
        let started = streak.started;
        let secs = self.jittered(cap);
        let streak = self
            .by_thread
            .get_mut(thread)
            .expect("streak inserted above");
        if now.duration_since(started) + Duration::from_secs(secs) > budget {
            streak.exhausted = true;
            return Advice::Exhausted { attempt };
        }
        Advice::RetryAfter { secs, attempt }
    }

    /// Ends the thread's streak. Returns a recovery when it had advised and was
    /// not exhausted.
    pub(crate) fn success(&mut self, thread: &str) -> Option<Recovered> {
        let streak = self.by_thread.remove(thread)?;
        (!streak.exhausted).then_some(Recovered {
            kind: streak.kind,
            attempts: streak.attempts,
        })
    }

    /// A delay in `[ceil(cap / 2), cap]`, at least one second.
    fn jittered(&mut self, cap: u64) -> u64 {
        self.draws += 1;
        let mut hasher = self.jitter.build_hasher();
        hasher.write_u64(self.draws);
        let low = cap.div_ceil(2).max(1);
        low + hasher.finish() % (cap - low + 1)
    }
}

impl Streak {
    fn new(kind: Kind, now: Instant) -> Self {
        Self {
            kind,
            started: now,
            last_failure: now,
            attempts: 0,
            exhausted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_with_jitter_and_stop_at_the_cap() {
        let mut streaks = Streaks::new(Duration::from_secs(86_400), Duration::from_secs(86_400));
        let now = Instant::now();
        let mut caps = vec![];
        for _ in 0..8 {
            let Advice::RetryAfter { secs, .. } = streaks.failure("t", Kind::Overloaded, now)
            else {
                panic!("advice expected");
            };
            caps.push(secs);
        }
        for (secs, cap) in caps.iter().zip([2u64, 4, 8, 16, 32, 60, 60, 60]) {
            assert!(
                (cap.div_ceil(2)..=cap).contains(secs),
                "{secs} not in cap {cap}"
            );
        }
    }

    #[test]
    fn budget_is_per_kind_and_exhaustion_ends_the_streak() {
        let mut streaks = Streaks::new(Duration::from_secs(180), Duration::from_secs(600));
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        for secs in [0, 100] {
            assert!(matches!(
                streaks.failure("t", Kind::Rate429, at(secs)),
                Advice::RetryAfter { .. }
            ));
        }
        // Attempt 3 waits 4 to 8 s: 170 + 8 still fits 180 s.
        assert!(matches!(
            streaks.failure("t", Kind::Rate429, at(170)),
            Advice::RetryAfter { .. }
        ));
        // Attempt 4 waits at least 8 s: 179 + 8 passes 180 s.
        assert_eq!(
            streaks.failure("t", Kind::Rate429, at(179)),
            Advice::Exhausted { attempt: 4 }
        );
        assert!(
            matches!(
                streaks.failure("t", Kind::Rate429, at(185)),
                Advice::RetryAfter { attempt: 1, .. }
            ),
            "a failure after exhaustion is a resume"
        );
        assert!(matches!(
            streaks.failure("u", Kind::Overloaded, at(0)),
            Advice::RetryAfter { .. }
        ));
        assert!(matches!(
            streaks.failure("u", Kind::Overloaded, at(100)),
            Advice::RetryAfter { .. }
        ));
        assert!(
            matches!(
                streaks.failure("u", Kind::Overloaded, at(200)),
                Advice::RetryAfter { .. }
            ),
            "overloaded keeps its longer budget past 180 s"
        );
    }

    #[test]
    fn a_late_failure_starts_a_new_streak() {
        let mut streaks = Streaks::new(Duration::ZERO, Duration::ZERO);
        let start = Instant::now();
        assert!(matches!(
            streaks.failure("t", Kind::Rate429, start),
            Advice::Exhausted { .. }
        ));
        assert!(matches!(
            streaks.failure(
                "t",
                Kind::Rate429,
                start + STREAK_IDLE + Duration::from_secs(1)
            ),
            Advice::Exhausted { attempt: 1 }
        ));
    }

    #[test]
    fn a_kind_change_starts_a_new_streak_with_its_own_budget() {
        let mut streaks = Streaks::new(Duration::from_secs(180), Duration::from_secs(600));
        let start = Instant::now();
        for secs in [0, 100, 200] {
            streaks.failure("t", Kind::Overloaded, start + Duration::from_secs(secs));
        }
        assert!(matches!(
            streaks.failure("t", Kind::Rate429, start + Duration::from_secs(260)),
            Advice::RetryAfter { attempt: 1, .. }
        ));
    }

    #[test]
    fn a_failure_after_exhaustion_is_a_resume_and_gets_advice() {
        let mut streaks = Streaks::new(Duration::from_secs(5), Duration::from_secs(600));
        let start = Instant::now();
        streaks.failure("t", Kind::Rate429, start);
        assert!(matches!(
            streaks.failure("t", Kind::Rate429, start + Duration::from_secs(4)),
            Advice::Exhausted { .. }
        ));
        // Codex stopped; the next request is a resume within 120 s.
        assert!(matches!(
            streaks.failure("t", Kind::Rate429, start + Duration::from_secs(30)),
            Advice::RetryAfter { attempt: 1, .. }
        ));
    }

    #[test]
    fn success_reports_recovery_only_for_live_streaks() {
        let mut streaks = Streaks::new(Duration::from_secs(600), Duration::ZERO);
        let now = Instant::now();
        streaks.failure("t", Kind::Rate429, now);
        streaks.failure("t", Kind::Rate429, now);
        assert_eq!(
            streaks.success("t"),
            Some(Recovered {
                kind: Kind::Rate429,
                attempts: 2
            })
        );
        assert_eq!(streaks.success("t"), None);
        streaks.failure("u", Kind::Overloaded, now);
        assert_eq!(
            streaks.success("u"),
            None,
            "exhausted streaks never recover"
        );
    }

    #[test]
    fn terminal_429s_match_type_and_code() {
        for name in TERMINAL {
            for field in ["type", "code"] {
                let body = serde_json::json!({"error": {field: name}}).to_string();
                assert_eq!(
                    classify_429(body.as_bytes()),
                    Http429::Terminal,
                    "{field}={name}"
                );
            }
        }
        assert_eq!(
            classify_429(br#"{"error":{"type":"rate_limit_exceeded"}}"#),
            Http429::Rate
        );
        assert_eq!(classify_429(b"not json"), Http429::Rate);
    }
}
