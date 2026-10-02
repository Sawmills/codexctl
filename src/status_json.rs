//! Versioned machine-readable account status. No credential data belongs here.
use crate::{api, profile};
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Local,
    Server,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Local,
    Server,
    Active,
    Unavailable,
}

#[derive(Serialize)]
pub struct AccountStatus {
    pub alias: String,
    pub label: Option<String>,
    pub plan: Option<String>,
    pub source: Source,
    pub state: State,
    pub primary_used_percent: Option<f64>,
    pub secondary_used_percent: Option<f64>,
    pub resets_at: Option<String>,
    pub billing_class: api::BillingClass,
    pub error: Option<String>,
}

impl AccountStatus {
    pub fn local(meta: &profile::Meta, active: bool) -> Self {
        Self {
            alias: meta.alias.clone(),
            label: meta.label.clone(),
            plan: meta.plan.clone(),
            source: Source::Local,
            state: if active { State::Active } else { State::Local },
            primary_used_percent: None,
            secondary_used_percent: None,
            resets_at: None,
            billing_class: api::BillingClass::Unknown,
            error: None,
        }
    }

    pub fn set_usage(&mut self, usage: &api::RateLimitResponse) {
        self.plan = usage.plan_type.clone().or_else(|| self.plan.take());
        self.billing_class = usage.billing_class();
        let limits = usage.rate_limit.as_ref();
        self.primary_used_percent = limits
            .and_then(api::RateLimit::short_window)
            .map(|w| w.used_percent);
        self.secondary_used_percent = limits
            .and_then(api::RateLimit::long_window)
            .map(|w| w.used_percent);
        self.resets_at = timestamp(
            limits
                .and_then(api::RateLimit::long_window)
                .and_then(api::RateLimitWindow::reset_timestamp),
        );
    }
}

pub fn timestamp(seconds: Option<i64>) -> Option<String> {
    chrono::DateTime::from_timestamp(seconds?, 0)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub fn print(accounts: &[AccountStatus]) -> Result<()> {
    #[derive(Serialize)]
    struct Document<'a> {
        version: u32,
        accounts: &'a [AccountStatus],
    }
    let document = serde_json::to_vec(&Document {
        version: 1,
        accounts,
    })?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&document)?;
    writeln!(stdout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weekly_only_primary_is_reported_as_secondary_with_absolute_reset() {
        let usage = serde_json::from_value(json!({"plan_type":"pro", "rate_limit":{
            "primary_window":{"used_percent":37.25,"limit_window_seconds":604800,"reset_at":4102444800_i64}
        }})).unwrap();
        let mut row = AccountStatus::local(&profile::Meta::default(), false);

        row.set_usage(&usage);

        assert_eq!(row.primary_used_percent, None);
        assert_eq!(row.secondary_used_percent, Some(37.25));
        assert_eq!(row.resets_at.as_deref(), Some("2100-01-01T00:00:00Z"));
    }

    #[test]
    fn short_only_usage_does_not_invent_a_weekly_window() {
        let usage = serde_json::from_value(json!({"plan_type":"pro", "rate_limit":{
            "primary_window":{"used_percent":12.5,"limit_window_seconds":18000,"reset_at":4102444800_i64}
        }})).unwrap();
        let mut row = AccountStatus::local(&profile::Meta::default(), false);

        row.set_usage(&usage);

        assert_eq!(row.primary_used_percent, Some(12.5));
        assert_eq!(row.secondary_used_percent, None);
        assert_eq!(row.resets_at, None);
    }
}
