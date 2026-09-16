//! Quota forecasts use percentages within each seat, never pooled token counts.
use std::collections::HashSet;
use std::io::{Read, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{api, config::Paths, store};

mod render;
mod shared;

pub const WEEK: i64 = 7 * 24 * 3600;
const HOUR: i64 = 3600;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Window {
    pub seconds: i64,
    pub used: f64,
    pub reset: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Sample {
    pub seat: (String, String),
    /// Keep UID alongside the legacy subject so old history remains readable.
    #[serde(default)]
    pub login_uid: Option<String>,
    pub alias: String,
    pub plan: String,
    pub at: i64,
    pub windows: Vec<Window>,
}

impl Sample {
    fn same_seat(&self, other: &Self) -> bool {
        let claims = |s: &Self| api::Logins {
            uid: s.login_uid.clone(),
            sub: (!s.seat.1.is_empty()).then(|| s.seat.1.clone()),
        };
        self.seat.0 == other.seat.0 && claims(self).same(&claims(other))
    }

    pub fn from_usage(
        alias: &str,
        auth: &api::AuthJson,
        usage: &api::RateLimitResponse,
        at: i64,
    ) -> Option<Self> {
        if usage.billing_class() != api::BillingClass::RateLimited {
            return None;
        }
        let logins = api::token_logins(&auth.access_token);
        if logins.is_empty() {
            return None;
        }
        let seat = (
            auth.account_id
                .clone()
                .or_else(|| api::extract_account_id(&auth.access_token))?,
            logins.sub.unwrap_or_default(),
        );
        let mut windows = Vec::new();
        for (_, window) in usage.rate_limit.as_ref()?.windows() {
            let seconds = i64::try_from(window.duration_seconds()?).ok()?;
            let reset = window.reset_timestamp()?;
            if !(1..=WEEK).contains(&seconds)
                || reset <= at
                || reset > at + seconds + 120
                || !window.used_percent.is_finite()
                || !(0.0..=100.0).contains(&window.used_percent)
            {
                return None;
            }
            windows.push(Window {
                seconds,
                used: window.used_percent,
                reset,
            });
        }
        windows.sort_by_key(|w| w.seconds);
        if !windows.iter().any(|w| w.seconds == WEEK)
            || windows.windows(2).any(|w| w[0].seconds == w[1].seconds)
        {
            return None;
        }
        Some(Self {
            seat,
            login_uid: logins.uid,
            alias: alias.to_owned(),
            plan: usage.plan_type.clone()?,
            at,
            windows,
        })
    }
}

/// Hold the existing store lock only for the local read/modify/write, never HTTP.
/// Return old observations even if the newest fetch is too close to save.
pub fn record(paths: &Paths, samples: &[Sample], now: i64) -> Result<Vec<Sample>> {
    let _lock = store::lock(paths)?;
    let path = paths.codexctl_dir().join("usage-history.json.gz");
    let legacy = paths.codexctl_dir().join("usage-history.json");
    let (bytes, migrated) = match std::fs::read(&path) {
        Ok(bytes) => {
            let mut decoded = Vec::new();
            flate2::read::MultiGzDecoder::new(bytes.as_slice())
                .read_to_end(&mut decoded)
                .context("invalid compressed usage history; file left unchanged")?;
            (decoded, false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match std::fs::read(&legacy) {
            Ok(bytes) => (bytes, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (b"[]".to_vec(), false),
            Err(e) => return Err(e).context("cannot read legacy usage history"),
        },
        Err(e) => return Err(e).context("cannot read usage history"),
    };
    let mut history: Vec<Sample> =
        serde_json::from_slice(&bytes).context("invalid usage history; file left unchanged")?;
    // An older fetch can acquire the lock after a newer fetch has been saved.
    // Do not delete that newer observation; estimators ignore future samples.
    history.retain(|s| s.at >= now - 4 * WEEK);
    for sample in samples {
        // Repeated status calls do not grow history faster than one sample per 5 minutes.
        if !history
            .iter()
            .any(|s| s.same_seat(sample) && s.at >= sample.at - 300)
        {
            history.push(sample.clone());
        }
    }
    history.sort_by_key(|s| s.at);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&serde_json::to_vec(&history)?)?;
    store::atomic_write(&path, &encoder.finish()?)?;
    if migrated {
        std::fs::remove_file(&legacy)
            .context("compressed history saved, but legacy file could not be removed")?;
    }
    Ok(history)
}

#[derive(Clone, Debug)]
pub struct Projection {
    pub sample: Sample,
    /// Percentage points per second, one rate for each window.
    pub rates: Vec<f64>,
    pub provisional: bool,
    pub unknown_short_pace: bool,
    pub weekly_from_history: bool,
    pub reset_expiries: Vec<i64>,
}

impl Projection {
    pub fn new(sample: &Sample, history: &[Sample]) -> Option<Self> {
        let mut rates = Vec::new();
        let mut provisional = false;
        let mut unknown_short_pace = false;
        let mut weekly_from_history = false;
        for window in &sample.windows {
            // A sub-hour window must be observable before it resets.
            let minimum_span = HOUR.min(window.seconds / 2).max(1);
            let mut observations: Vec<_> = history
                .iter()
                .filter(|s| {
                    s.same_seat(sample)
                        && s.plan == sample.plan
                        && s.at >= sample.at - WEEK
                        && s.at < sample.at
                        && s.windows.len() == sample.windows.len()
                        && s.windows
                            .iter()
                            .zip(&sample.windows)
                            .all(|(a, b)| a.seconds == b.seconds)
                })
                .filter_map(|s| {
                    s.windows
                        .iter()
                        .find(|w| w.seconds == window.seconds)
                        .map(|w| (s.at, w))
                })
                .collect();
            observations.sort_by_key(|(at, _)| *at);
            observations.push((sample.at, window));
            let mut consumed = 0.0;
            let mut elapsed = 0;
            for pair in observations.windows(2) {
                let ((a, previous), (b, current)) = (pair[0], pair[1]);
                // Never interpret a reset, correction, or unobserved multi-day gap as burn.
                if b > a
                    && b - a <= 2 * 24 * HOUR
                    && (previous.reset - current.reset).abs() <= 120.min(window.seconds / 10)
                    && a < previous.reset
                    && b < current.reset
                    && previous.used.is_finite()
                    && (0.0..=100.0).contains(&previous.used)
                    && current.used >= previous.used
                {
                    consumed += current.used - previous.used;
                    elapsed += b - a;
                }
            }
            if elapsed >= minimum_span && consumed > 0.0 {
                rates.push(consumed / elapsed as f64);
                weekly_from_history |= window.seconds == WEEK;
            } else {
                // First run: observed consumption since the reported window began.
                let age = sample.at - (window.reset - window.seconds);
                if age < minimum_span || window.used <= 0.0 {
                    if window.seconds == WEEK && window.used < 100.0 {
                        return None;
                    }
                    // Retain the weekly forecast for an idle/fresh short window,
                    // and retain known exhaustion even without a measured pace.
                    rates.push(0.0);
                    provisional = true;
                    unknown_short_pace |= window.seconds != WEEK;
                    continue;
                }
                rates.push(window.used / age as f64);
                provisional = true;
            }
        }
        Some(Self {
            sample: sample.clone(),
            rates,
            provisional,
            unknown_short_pace,
            weekly_from_history,
            reset_expiries: Vec::new(),
        })
    }

    /// Seed a simulation from the observation, advancing only to its start time.
    fn used_at(&self, at: i64) -> Vec<f64> {
        self.sample
            .windows
            .iter()
            .zip(&self.rates)
            .map(|(w, rate)| {
                let used = if at < w.reset {
                    w.used + (at - self.sample.at).max(0) as f64 * rate
                } else {
                    ((at - w.reset) % w.seconds) as f64 * rate
                };
                used.clamp(0.0, 100.0)
            })
            .collect()
    }

    #[cfg(test)]
    fn remaining(&self, at: i64) -> f64 {
        self.used_at(at)
            .iter()
            .map(|used| 100.0 - used)
            .fold(100.0, f64::min)
    }
}

#[derive(Default)]
pub struct Report {
    pub samples: Vec<Sample>,
    pub projections: Vec<Projection>,
    pub excluded: usize,
    pub usage_based: usize,
    pub duplicates: usize,
    pub history_error: Option<String>,
    pub history_samples: usize,
    pub history_span: i64,
    pub sampling_status: Option<String>,
    pub reset_inventory_unknown: usize,
}

impl Report {
    pub fn build(samples: Vec<Sample>, history: &[Sample], excluded: usize) -> Self {
        let mut report = Self {
            excluded,
            ..Self::default()
        };
        for sample in samples {
            if report.samples.iter().any(|s| s.same_seat(&sample)) {
                report.duplicates += 1;
                continue;
            }
            if let Some(projection) = Projection::new(&sample, history) {
                report.projections.push(projection);
            }
            report.samples.push(sample);
        }
        let now = report.samples.iter().map(|s| s.at).max().unwrap_or(0);
        report.history_samples = history
            .iter()
            .filter(|s| {
                report.samples.iter().any(|current| current.same_seat(s))
                    && s.at >= now - WEEK
                    && s.at <= now
            })
            .count();
        report.history_span = history
            .iter()
            .filter(|s| {
                report.samples.iter().any(|current| current.same_seat(s))
                    && s.at >= now - WEEK
                    && s.at <= now
            })
            .map(|s| now - s.at)
            .max()
            .unwrap_or(0);
        report
    }

    /// Attach live inventory only; never persist credits in usage history.
    pub fn set_reset_inventory(
        &mut self,
        alias: &str,
        held: i64,
        details: Option<&api::ResetCreditsDetails>,
        now: i64,
    ) {
        let Some(p) = self
            .projections
            .iter_mut()
            .find(|p| p.sample.alias == alias)
        else {
            return;
        };
        let Some(details) = details else {
            self.reset_inventory_unknown += usize::from(held > 0);
            return;
        };
        let mut seen = HashSet::new();
        let available: Vec<_> = details
            .credits
            .iter()
            .filter(|c| c.is_available() && seen.insert(&c.id))
            .collect();
        let mut omitted = false;
        p.reset_expiries = available
            .iter()
            .filter_map(|c| {
                if c.reset_type.as_deref() != Some("codex_rate_limits") {
                    omitted = true;
                    return None;
                }
                match c.expires_at_timestamp() {
                    Some(expiry) if expiry > now => Some(expiry),
                    Some(_) => None,
                    None => {
                        omitted = true;
                        None
                    }
                }
            })
            .collect();
        p.reset_expiries.sort_unstable();
        let cap = held.min(details.available_count).max(0) as usize;
        omitted |= available.len() < cap || held != details.available_count;
        p.reset_expiries.truncate(cap);
        self.reset_inventory_unknown += usize::from(omitted);
    }

    pub fn gap_seconds_without_banked_resets(&self, now: i64) -> i64 {
        let mut projections = self.projections.clone();
        for p in &mut projections {
            p.reset_expiries.clear();
        }
        shared::availability(&projections, now)
            .iter()
            .filter(|(_, _, count)| *count == 0)
            .map(|(a, b, _)| b - a)
            .sum()
    }

    /// Piecewise availability between every exhaustion/reset boundary.
    /// Display summaries use these intervals so short outages cannot disappear.
    fn availability(&self, now: i64) -> Vec<(i64, i64, usize)> {
        shared::availability(&self.projections, now)
    }

    pub fn outages(&self, now: i64) -> Vec<(i64, i64)> {
        let mut outages: Vec<(i64, i64)> = Vec::new();
        for (start, end, count) in self.availability(now) {
            if count == 0 {
                if let Some(last) = outages.last_mut()
                    && last.1 == start
                {
                    last.1 = end;
                } else {
                    outages.push((start, end));
                }
            }
        }
        outages
    }

    pub fn render(&self, now: i64, width: usize) -> String {
        self.render_terminal(now, width, false)
    }

    pub fn render_forecast(&self, now: i64, width: usize, color: bool, details: bool) -> String {
        if details {
            render::render(self, now, width, color)
        } else {
            render::compact(self, now, width, color)
        }
    }

    pub fn render_terminal(&self, now: i64, width: usize, color: bool) -> String {
        render::render(self, now, width, color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn sample(alias: &str, used: f64, reset_in: i64) -> Sample {
        Sample {
            seat: ("workspace".into(), alias.into()),
            login_uid: None,
            alias: alias.into(),
            plan: "pro".into(),
            at: NOW,
            windows: vec![Window {
                seconds: WEEK,
                used,
                reset: NOW + reset_in,
            }],
        }
    }

    fn past(current: &Sample, ago: i64, used: f64) -> Sample {
        let mut older = current.clone();
        older.at -= ago;
        older.windows[0].used = used;
        older
    }

    #[test]
    fn login_claims_preserve_history_across_subject_rotation_without_merging_conflicting_uids() {
        let mut a = sample("a", 40.0, 86400);
        a.login_uid = Some("user-a".into());
        let mut rotated = past(&a, 86400, 20.0);
        rotated.seat.1 = "rotated-subject".into();
        assert!(
            Projection::new(&a, &[rotated.clone()])
                .unwrap()
                .weekly_from_history
        );
        let mut conflicting = a.clone();
        conflicting.login_uid = Some("user-b".into());
        assert_eq!(
            Report::build(vec![a.clone(), conflicting], &[], 0)
                .samples
                .len(),
            2
        );
        let mut legacy = a.clone();
        legacy.login_uid = None;
        assert!(a.same_seat(&legacy));
        rotated.seat.1.clear();
        assert!(a.same_seat(&rotated));
        legacy.seat.1 = "user-a".into();
        assert!(!legacy.same_seat(&rotated));
    }

    #[test]
    fn reset_inventory_is_verified_deduplicated_and_not_history() {
        let mut report = Report::build(vec![sample("a", 100.0, 86400)], &[], 0);
        let details: api::ResetCreditsDetails = serde_json::from_value(serde_json::json!({
            "available_count": 2,
            "credits": [
                {"id":"one","status":"available","reset_type":"codex_rate_limits","expires_at":"2030-01-01T00:00:00Z"},
                {"id":"one","status":"available","reset_type":"codex_rate_limits","expires_at":"2030-01-01T00:00:00Z"},
                {"id":"unknown","status":"available","reset_type":"codex_rate_limits"},
                {"id":"spent","status":"redeemed","reset_type":"codex_rate_limits"}
            ]
        })).unwrap();
        report.set_reset_inventory("a", 2, Some(&details), NOW);
        assert_eq!(report.projections[0].reset_expiries.len(), 1);
        assert_eq!(report.reset_inventory_unknown, 1);
        let output = report.render(NOW, 120);
        assert!(output.contains("1 verified banked credits modeled"));
        assert!(output.contains("without banked resets"));
        assert!(
            !serde_json::to_string(&report.samples)
                .unwrap()
                .contains("reset_expiries")
        );
        let mut missing = Report::build(vec![sample("a", 100.0, 86400)], &[], 0);
        missing.set_reset_inventory("a", 2, None, NOW);
        assert_eq!(missing.reset_inventory_unknown, 1);
        assert!(missing.projections[0].reset_expiries.is_empty());
    }

    #[test]
    fn saved_usage_predicts_weekly_exhaustion_and_reset() {
        let current = sample("a", 80.0, 2 * 86400);
        let history = vec![past(&current, 86400, 40.0)];
        let projection = Projection::new(&current, &history).unwrap();
        assert!(!projection.provisional);
        assert!((projection.rates[0] * 86400.0 - 40.0).abs() < 1e-6);
        assert_eq!(projection.remaining(NOW + 43200), 0.0);
        assert_eq!(projection.remaining(NOW + 2 * 86400), 100.0);
        let report = Report::build(vec![current], &history, 0);
        assert_eq!(report.outages(NOW)[0], (NOW + 43200, NOW + 2 * 86400));
    }

    #[test]
    fn reset_before_depletion_restores_allowance() {
        let current = sample("a", 10.0, 86400);
        let p = Projection::new(&current, &[past(&current, 86400, 5.0)]).unwrap();
        assert!(p.remaining(NOW + 86399) > 80.0);
        assert_eq!(p.remaining(NOW + 86400), 100.0);
        assert!(
            Report::build(vec![current.clone()], &[past(&current, 86400, 5.0)], 0)
                .outages(NOW)
                .is_empty()
        );
    }

    #[test]
    fn reset_and_correction_are_not_consumption_deltas() {
        let current = sample("a", 20.0, 3 * 86400);
        let mut before_reset = past(&current, 86400, 10.0);
        before_reset.windows[0].reset -= WEEK;
        assert!(
            Projection::new(&current, &[before_reset])
                .unwrap()
                .provisional
        );
        assert!(
            Projection::new(&current, &[past(&current, 86400, 70.0)])
                .unwrap()
                .provisional
        );
        let mut other_plan = past(&current, 86400, 10.0);
        other_plan.plan = "plus".into();
        assert!(
            Projection::new(&current, &[other_plan])
                .unwrap()
                .provisional
        );
    }

    #[test]
    fn no_usage_or_too_young_never_claims_unlimited_supply() {
        assert!(Projection::new(&sample("a", 0.0, WEEK - 86400), &[]).is_none());
        assert!(Projection::new(&sample("a", 10.0, WEEK - 60), &[]).is_none());
        let report = Report::build(vec![sample("a", 0.0, WEEK - 86400)], &[], 0);
        assert!(report.render(NOW, 80).contains("Collecting history"));
        assert!(!report.render(NOW, 80).contains("No total outage"));
    }

    #[test]
    fn both_windows_must_have_quota() {
        let mut current = sample("a", 10.0, 86400);
        current.windows.push(Window {
            seconds: 5 * HOUR,
            used: 100.0,
            reset: NOW + HOUR,
        });
        let projection = Projection::new(&current, &[]).unwrap();
        assert_eq!(projection.remaining(NOW), 0.0);
        assert!(projection.remaining(NOW + HOUR) > 0.0);
    }

    #[test]
    fn fleet_outage_requires_overlap_and_finds_sub_chart_gap() {
        let a = sample("a", 100.0, 3600);
        let b = sample("b", 99.0, 7200);
        let mut report = Report::build(vec![a, b], &[], 0);
        report.projections[0].rates[0] = 0.0;
        report.projections[1].rates[0] = 1.0 / 3500.0;
        assert_eq!(report.outages(NOW)[0], (NOW + 3500, NOW + 3600));
        assert!(report.render(NOW, 40).contains('!'));
    }

    #[test]
    fn aliases_dedupe_by_seat_but_workspace_seats_stay_separate() {
        let a = sample("a", 30.0, 86400);
        let mut alias = a.clone();
        alias.alias = "other-name".into();
        let b = sample("b", 30.0, 86400);
        let report = Report::build(vec![a, alias, b], &[], 1);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.duplicates, 1);
        assert!(report.render(NOW, 80).contains("Coverage incomplete"));
        assert!(!report.render(NOW, 80).contains("ON TRACK"));
    }

    #[test]
    fn primary_weekly_window_is_supported_and_unknown_duration_is_not() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"seat"}"#);
        let auth = api::AuthJson {
            access_token: format!("header.{payload}.sig"),
            refresh_token: None,
            account_id: Some("workspace".into()),
        };
        let mut json = serde_json::json!({"plan_type":"pro", "rate_limit": {"primary_window": {
            "used_percent":40, "limit_window_seconds":WEEK, "reset_at":NOW + 86400
        }}});
        let usage = serde_json::from_value(json.clone()).unwrap();
        assert!(Sample::from_usage("a", &auth, &usage, NOW).is_some());
        json["rate_limit"]["primary_window"]["limit_window_seconds"] = serde_json::Value::Null;
        let usage = serde_json::from_value(json.clone()).unwrap();
        assert!(Sample::from_usage("a", &auth, &usage, NOW).is_none());
        json["rate_limit"]["primary_window"]["limit_window_seconds"] = WEEK.into();
        json["rate_limit"]["primary_window"]["reset_at"] = (NOW - 1).into();
        let usage = serde_json::from_value(json).unwrap();
        assert!(Sample::from_usage("a", &auth, &usage, NOW).is_none());
    }

    #[test]
    fn fresh_weekly_exhaustion_is_visible_without_a_burn_rate() {
        let report = Report::build(vec![sample("a", 100.0, WEEK - 60)], &[], 0);
        assert_eq!(report.projections.len(), 1);
        assert_eq!(report.outages(NOW), vec![(NOW, NOW + WEEK - 60)]);
        let text = report.render(NOW, 80);
        assert!(text.contains("RISK"));
        assert!(text.contains("Empty now"));
        assert!(text.contains("Pace after reset: unknown"));
        assert!(text.contains("Coverage incomplete"));
        assert!(!text.contains("Collecting history"));
    }

    #[test]
    fn out_of_order_collectors_preserve_newer_history() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_owned());
        let older = sample("a", 40.0, 86400);
        let mut newer = older.clone();
        newer.at += 301;
        newer.windows[0].used = 41.0;
        record(&paths, std::slice::from_ref(&newer), newer.at).unwrap();
        let history = record(&paths, std::slice::from_ref(&older), older.at).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].at, newer.at);
        let report = Report::build(vec![older], &history, 0);
        assert_eq!(report.history_samples, 0);
        assert!(report.projections[0].provisional);
    }

    #[test]
    fn idle_short_window_keeps_weekly_projection_and_known_current_capacity() {
        let mut current = sample("a", 40.0, 3 * 86400);
        current.windows.push(Window {
            seconds: 5 * HOUR,
            used: 0.0,
            reset: NOW + HOUR,
        });
        let older = past(&current, 86400, 20.0);
        let report = Report::build(vec![current], &[older], 0);
        assert_eq!(report.projections.len(), 1);
        let p = &report.projections[0];
        assert!(p.unknown_short_pace);
        assert!(p.weekly_from_history);
        assert!((p.rates[0] * 86400.0 - 20.0).abs() < 1e-6);
        assert_eq!(p.remaining(NOW), 60.0);
        assert!(p.remaining(NOW + HOUR) > 59.0);
        assert!(report.outages(NOW).is_empty());
        assert!(report.render(NOW, 80).contains("Short-window pace unknown"));
    }

    #[test]
    fn fresh_exhausted_short_window_still_blocks_until_reset_with_zero_rate() {
        let mut current = sample("a", 40.0, 3 * 86400);
        current.windows.push(Window {
            seconds: 5 * HOUR,
            used: 100.0,
            reset: NOW + 5 * HOUR - 60,
        });
        let report = Report::build(vec![current], &[], 0);
        assert!(report.projections[0].unknown_short_pace);
        assert_eq!(report.outages(NOW)[0], (NOW, NOW + 5 * HOUR - 60));
    }

    #[test]
    fn paid_credit_exclusions_do_not_make_known_weekly_coverage_partial() {
        let mut report = Report::build(vec![sample("a", 10.0, 86400)], &[], 0);
        report.usage_based = 2;
        let output = report.render(NOW, 80);
        assert!(output.contains("Model minimum:"));
        assert!(output.contains("2 paid-credit seats excluded by design"));
        assert!(!output.contains("Coverage incomplete"));
    }

    #[test]
    fn sub_hour_window_keeps_weekly_history_and_blocks_until_reset() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"seat"}"#);
        let auth = api::AuthJson {
            access_token: format!("header.{payload}.sig"),
            refresh_token: None,
            account_id: Some("workspace".into()),
        };
        let usage = serde_json::from_value(serde_json::json!({
            "plan_type":"pro", "rate_limit": {
                "primary_window": {"used_percent":100, "limit_window_seconds":900, "reset_at":NOW + 300},
                "secondary_window": {"used_percent":40, "limit_window_seconds":WEEK, "reset_at":NOW + 86400}
            }
        })).unwrap();
        let current = Sample::from_usage("a", &auth, &usage, NOW).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_owned());
        let history = record(&paths, std::slice::from_ref(&current), NOW).unwrap();
        assert_eq!(history[0].windows.len(), 2);
        let report = Report::build(vec![current], &history, 0);
        assert_eq!(report.projections.len(), 1);
        assert_eq!(report.projections[0].remaining(NOW), 0.0);
        assert!(report.projections[0].remaining(NOW + 300) > 0.0);
        assert_eq!(report.outages(NOW)[0], (NOW, NOW + 300));
        assert!(report.render(NOW, 80).contains("60.0%"));
    }

    #[test]
    fn legacy_history_migrates_losslessly_and_corrupt_gzip_never_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_owned());
        let old = sample("a", 40.0, 86400);
        let legacy = paths.codexctl_dir().join("usage-history.json");
        store::atomic_write(&legacy, &serde_json::to_vec(&vec![old.clone()]).unwrap()).unwrap();
        let history = record(&paths, &[], NOW).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].at, old.at);
        assert!(!legacy.exists());
        assert_eq!(record(&paths, &[], NOW).unwrap()[0].windows[0].used, 40.0);
        let compressed = paths.codexctl_dir().join("usage-history.json.gz");
        let mut bytes = std::fs::read(&compressed).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&compressed, &bytes).unwrap();
        std::fs::write(&legacy, b"[]").unwrap();
        assert!(record(&paths, &[], NOW).is_err());
        assert_eq!(std::fs::read(&compressed).unwrap(), bytes);
        assert!(legacy.exists());
    }

    #[test]
    fn corrupt_legacy_is_not_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_owned());
        let legacy = paths.codexctl_dir().join("usage-history.json");
        store::atomic_write(&legacy, b"broken").unwrap();
        assert!(record(&paths, &[], NOW).is_err());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"broken");
        assert!(!paths.codexctl_dir().join("usage-history.json.gz").exists());
    }

    #[test]
    fn history_is_bounded_private_and_corruption_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_owned());
        let current = sample("a", 40.0, 86400);
        let history = record(&paths, std::slice::from_ref(&current), NOW).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(record(&paths, &[current], NOW + 60).unwrap().len(), 1);
        assert!(record(&paths, &[], NOW + 5 * WEEK).unwrap().is_empty());
        let path = paths.codexctl_dir().join("usage-history.json.gz");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&path, "broken").unwrap();
        assert!(record(&paths, &[], NOW).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
    }
}
