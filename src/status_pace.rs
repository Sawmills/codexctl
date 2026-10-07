//! Weekly usage compared with a straight line through the declared window.

/// A fresh weekly observation, in percentage points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    pub elapsed_percent: f64,
    pub points: f64,
}

impl Pace {
    /// Read the main Codex long window from a successful usage response.
    pub fn from_usage(usage: &crate::api::RateLimitResponse, now: i64) -> Option<Self> {
        let window = usage.rate_limit.as_ref()?.long_window()?;
        Self::weekly(
            Some(window.used_percent),
            window.duration_seconds(),
            window.reset_timestamp(),
            true,
            now,
        )
    }

    /// Calculate pace only for a usable, declared seven-day window.
    pub fn weekly(
        used_percent: Option<f64>,
        window_seconds: Option<u64>,
        resets_at: Option<i64>,
        usable: bool,
        now: i64,
    ) -> Option<Self> {
        if !usable || window_seconds != Some(604800) {
            return None;
        }
        let used = used_percent?;
        let reset = resets_at?;
        let remaining = reset.checked_sub(now)?;
        if remaining < 0 || !used.is_finite() || !(0.0..=100.0).contains(&used) {
            return None;
        }
        let elapsed_percent = (1.0 - remaining as f64 / 604800.0).clamp(0.0, 1.0) * 100.0;
        Some(Self {
            elapsed_percent,
            points: used - elapsed_percent,
        })
    }
}

/// Equal-weight mean over accounts with known pace; empty fleets stay unknown.
pub fn fleet_points(points: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let (sum, count) = points
        .flatten()
        .fold((0.0, 0_u64), |(sum, count), value| (sum + value, count + 1));
    (count > 0).then(|| sum / count as f64)
}
