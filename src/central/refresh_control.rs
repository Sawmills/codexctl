//! Bounded telemetry for refresh decisions and owner recovery.
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

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

type AccountFailureKey = (String, String);

fn account_failures() -> &'static Mutex<BTreeMap<AccountFailureKey, u64>> {
    static FAILURES: OnceLock<Mutex<BTreeMap<AccountFailureKey, u64>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn settlement_kills() -> &'static Mutex<BTreeMap<AccountFailureKey, u64>> {
    static KILLS: OnceLock<Mutex<BTreeMap<AccountFailureKey, u64>>> = OnceLock::new();
    KILLS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn settlement_killed(alias: &str, user: &str) {
    let key = super::managed::account_key(user, alias);
    let mut kills = settlement_kills().lock().expect("settlement metric lock");
    let count = kills
        .entry((alias.to_ascii_lowercase(), key[..12].to_owned()))
        .or_default();
    *count = count.saturating_add(1);
}

/// Counts failed probes, launches and settlement retries without holding an
/// owner lock during a scrape. Labels match the provider-rejection metric.
pub(super) fn recovery_failed(alias: &str, user: &str) {
    let key = super::managed::account_key(user, alias);
    let mut failures = account_failures().lock().expect("recovery metric lock");
    let count = failures
        .entry((alias.to_ascii_lowercase(), key[..12].to_owned()))
        .or_default();
    *count = count.saturating_add(1);
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
    for (name, counts) in [
        (
            "codexctl_central_owner_recovery_failed_total",
            account_failures(),
        ),
        (
            "codexctl_central_owner_settlement_killed_total",
            settlement_kills(),
        ),
    ] {
        text.push_str(&format!("# TYPE {name} counter\n"));
        for ((alias, key), count) in counts.lock().expect("recovery metric lock").iter() {
            let alias = alias
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            text.push_str(&format!(
                "{name}{{account=\"{alias}\",account_key=\"{key}\"}} {count}\n"
            ));
        }
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
