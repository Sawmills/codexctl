//! Bounded telemetry for refresh decisions and request-driven recovery.
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

static RECOVERY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static FORCED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

pub(super) enum Recovery {
    Recovered,
    Failed,
    Backoff,
}

pub(super) fn recovery(result: Recovery) {
    RECOVERY[result as usize].fetch_add(1, Ordering::Relaxed);
}

pub(super) enum Forced {
    Refreshed,
    ServedRecent,
    TokenValid,
    UnknownExpiry { recent: bool },
}

pub(super) fn forced(outcome: Forced, alias: &str, user: &str, device: &str, age_ms: Option<u64>) {
    let (index, reason) = match outcome {
        Forced::Refreshed => (0, "previous_revision_current"),
        Forced::ServedRecent => (1, "previous_revision_current"),
        Forced::TokenValid => (1, "token_valid"),
        Forced::UnknownExpiry { recent } => (usize::from(recent), "exp_unknown"),
    };
    FORCED[index].fetch_add(1, Ordering::Relaxed);
    let outcome = ["refreshed", "served_recent"][index];
    eprintln!(
        "{}",
        json!({
            "operation": "forced_refresh",
            "outcome": outcome,
            "account": alias.to_ascii_lowercase(),
            "account_key": super::managed::account_key(user, alias),
            "device": device,
            "reason": reason,
            "last_rotation_age_s": age_ms.map(|age| age / 1000),
        })
    );
}

pub(super) fn metrics() -> String {
    let mut text = String::from("# TYPE codexctl_central_owner_recovery_total counter\n");
    for (index, result) in ["recovered", "failed", "backoff"].into_iter().enumerate() {
        text.push_str(&format!(
            "codexctl_central_owner_recovery_total{{result=\"{result}\"}} {}\n",
            RECOVERY[index].load(Ordering::Relaxed)
        ));
    }
    text.push_str("# TYPE codexctl_central_forced_refresh_total counter\n");
    for (index, outcome) in ["refreshed", "served_recent"].into_iter().enumerate() {
        text.push_str(&format!(
            "codexctl_central_forced_refresh_total{{outcome=\"{outcome}\"}} {}\n",
            FORCED[index].load(Ordering::Relaxed)
        ));
    }
    text
}
