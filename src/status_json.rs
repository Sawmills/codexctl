//! Versioned machine-readable account status. No credential data belongs here.
use crate::{api, profile};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Local,
    Server,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Local,
    Server,
    Active,
    Unavailable,
}

#[derive(Serialize, Deserialize)]
pub struct AccountStatus {
    pub alias: String,
    pub label: Option<String>,
    pub plan: Option<String>,
    pub source: Source,
    pub state: State,
    pub primary_used_percent: Option<f64>,
    pub secondary_used_percent: Option<f64>,
    pub primary_window_seconds: Option<u64>,
    pub secondary_window_seconds: Option<u64>,
    pub primary_resets_at: Option<String>,
    pub secondary_resets_at: Option<String>,
    pub resets_at: Option<String>,
    pub billing_class: api::BillingClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<api::Credits>,
    pub error: Option<String>,
    pub usage_age_seconds: Option<u64>,
    pub usage_stale: Option<bool>,
    pub resets_banked: Option<i64>,
    pub resets_redeemable: Option<i64>,
    pub resets_next_expiry: Option<String>,
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
            primary_window_seconds: None,
            secondary_window_seconds: None,
            primary_resets_at: None,
            secondary_resets_at: None,
            resets_at: None,
            billing_class: api::BillingClass::Unknown,
            credits: None,
            error: None,
            usage_age_seconds: None,
            usage_stale: None,
            resets_banked: None,
            resets_redeemable: None,
            resets_next_expiry: None,
        }
    }

    pub fn set_usage(&mut self, usage: &api::RateLimitResponse) {
        self.plan = usage.plan_type.clone().or_else(|| self.plan.take());
        self.billing_class = usage.billing_class();
        self.credits = usage.credits.clone();
        let limits = usage.rate_limit.as_ref();
        self.primary_used_percent = limits
            .and_then(api::RateLimit::short_window)
            .map(|w| w.used_percent);
        self.secondary_used_percent = limits
            .and_then(api::RateLimit::long_window)
            .map(|w| w.used_percent);
        self.primary_window_seconds = limits
            .and_then(api::RateLimit::short_window)
            .and_then(api::RateLimitWindow::duration_seconds);
        self.secondary_window_seconds = limits
            .and_then(api::RateLimit::long_window)
            .and_then(api::RateLimitWindow::duration_seconds);
        self.primary_resets_at = timestamp(
            limits
                .and_then(api::RateLimit::short_window)
                .and_then(api::RateLimitWindow::reset_timestamp),
        );
        self.secondary_resets_at = timestamp(
            limits
                .and_then(api::RateLimit::long_window)
                .and_then(api::RateLimitWindow::reset_timestamp),
        );
        self.resets_at = self.secondary_resets_at.clone();
    }
}

pub fn timestamp(seconds: Option<i64>) -> Option<String> {
    chrono::DateTime::from_timestamp(seconds?, 0)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub fn format_credits(credits: Option<&api::Credits>) -> String {
    let Some(credits) = credits else {
        return "-".into();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(balance) = credits.balance.as_deref() {
        parts.push(crate::status_format::format_credit_balance(balance));
    }
    if credits.has_credits {
        parts.push("available".into());
    }
    if credits.unlimited {
        parts.push("unlimited".into());
    }
    if credits.overage_limit_reached {
        parts.push("overage".into());
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" ")
    }
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
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["primary_window_seconds"], serde_json::Value::Null);
        assert_eq!(json["secondary_window_seconds"], 604800);
        assert_eq!(json["primary_resets_at"], serde_json::Value::Null);
        assert_eq!(json["secondary_resets_at"], "2100-01-01T00:00:00Z");
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
        assert_eq!(row.primary_window_seconds, Some(18000));
        assert_eq!(row.secondary_window_seconds, None);
        assert_eq!(
            row.primary_resets_at.as_deref(),
            Some("2100-01-01T00:00:00Z")
        );
        assert_eq!(row.secondary_resets_at, None);
        assert_eq!(row.resets_at, None);
    }

    #[test]
    fn usage_json_exposes_all_credit_fields_when_the_server_returns_them() {
        let usage = serde_json::from_value(json!({
            "plan_type": "pro",
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "12.50",
                "overage_limit_reached": true
            }
        }))
        .unwrap();
        let mut row = AccountStatus::local(&profile::Meta::default(), false);

        row.set_usage(&usage);

        assert_eq!(
            serde_json::to_value(row).unwrap()["credits"],
            json!({
                "has_credits": true,
                "unlimited": false,
                "balance": "12.50",
                "overage_limit_reached": true
            })
        );
    }

    #[test]
    fn usage_json_omits_credits_when_the_server_omits_or_nulls_them() {
        for raw in [
            json!({"plan_type":"pro"}),
            json!({"plan_type":"pro", "credits":null}),
        ] {
            let usage = serde_json::from_value(raw).unwrap();
            let mut row = AccountStatus::local(&profile::Meta::default(), false);
            row.set_usage(&usage);
            assert!(
                !serde_json::to_value(row)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("credits")
            );
        }
    }

    #[test]
    fn profile_json_keeps_declared_durations_and_distinct_resets() {
        let usage = serde_json::from_value(json!({"rate_limit":{
            "primary_window":{"used_percent":12.5,"window_minutes":60,"reset_at":4102444800_i64},
            "secondary_window":{"used_percent":37.0,"limit_window_seconds":86400,"reset_at":4102531200_i64}
        }})).unwrap();
        let mut row = AccountStatus::local(&profile::Meta::default(), false);
        row.set_usage(&usage);
        let row = serde_json::to_value(row).unwrap();
        assert_eq!(row["primary_window_seconds"], 3600);
        assert_eq!(row["secondary_window_seconds"], 86400);
        assert_eq!(row["primary_resets_at"], "2100-01-01T00:00:00Z");
        assert_eq!(row["secondary_resets_at"], "2100-01-02T00:00:00Z");
        assert_eq!(row["resets_at"], row["secondary_resets_at"]);
    }

    #[test]
    fn profile_json_does_not_guess_missing_durations() {
        let usage = serde_json::from_value(json!({"rate_limit":{
            "primary_window":{"used_percent":12.5},
            "secondary_window":{"used_percent":37.0,"reset_at":4102444800_i64}
        }}))
        .unwrap();
        let mut row = AccountStatus::local(&profile::Meta::default(), false);
        row.set_usage(&usage);
        let row = serde_json::to_value(row).unwrap();
        assert_eq!(row["primary_used_percent"], 12.5);
        assert_eq!(row["secondary_used_percent"], 37.0);
        assert_eq!(
            row.get("primary_window_seconds"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            row.get("secondary_window_seconds"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(row["primary_resets_at"], serde_json::Value::Null);
        assert_eq!(row["secondary_resets_at"], "2100-01-01T00:00:00Z");
    }

    #[test]
    fn reset_fields_round_trip_and_default_to_null() {
        let mut row = AccountStatus::local(&profile::Meta::default(), false);
        let empty = serde_json::to_value(&row).unwrap();
        assert!(empty["resets_banked"].is_null());
        assert!(empty["resets_redeemable"].is_null());
        assert!(empty["resets_next_expiry"].is_null());
        row.resets_banked = Some(3);
        row.resets_redeemable = Some(1);
        row.resets_next_expiry = Some("2100-01-01T00:00:00Z".into());
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["resets_banked"], 3);
        assert_eq!(value["resets_redeemable"], 1);
        assert_eq!(value["resets_next_expiry"], "2100-01-01T00:00:00Z");
    }
}
