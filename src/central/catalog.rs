//! Usage observations are independent of refresh ownership and token delivery.
use super::{managed::Account, server};
use crate::api;
use anyhow::Result;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(super) const TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Entry {
    revision: String,
    sample: Option<(api::RateLimitResponse, Instant)>,
    attempted: Option<Instant>,
    error: Option<&'static str>,
}

pub(super) struct Reader {
    client: reqwest::Client,
    endpoint: String,
    ttl: Duration,
    entries: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Entry>>>>,
}

impl Reader {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint: api::USAGE_URL.into(),
            ttl: TTL,
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    #[cfg(test)]
    pub(super) fn testing(endpoint: String, timeout: Duration) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(timeout)
                .build()
                .unwrap(),
            ..Self::new().unwrap()
        }
    }

    #[cfg(test)]
    pub(super) async fn expire(&self) {
        let entries: Vec<_> = self.entries.lock().unwrap().values().cloned().collect();
        for entry in entries {
            let mut entry = entry.lock().await;
            if let Some((_, at)) = &mut entry.sample {
                *at -= TTL;
            }
            if let Some(at) = &mut entry.attempted {
                *at -= TTL;
            }
        }
    }

    pub(super) async fn reset_counts(&self, key: &str) -> (Option<i64>, Option<i64>) {
        let entry = self
            .entries
            .lock()
            .expect("usage cache lock")
            .get(key)
            .cloned();
        let Some(entry) = entry else {
            return (None, None);
        };
        let entry = entry.lock().await;
        let summary = entry
            .sample
            .as_ref()
            .and_then(|(u, _)| u.rate_limit_reset_credits.as_ref());
        (
            summary.map(|s| s.available_count),
            summary.map(|s| s.applicable_available_count),
        )
    }

    pub(super) async fn fetch_direct(
        &self,
        access: &str,
        account_id: &str,
    ) -> Result<api::RateLimitResponse, &'static str> {
        api::fetch_usage_at(&self.client, &self.endpoint, access, Some(account_id))
            .await
            .map_err(|error| {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(reqwest::Error::is_timeout)
                }) {
                    "timeout"
                } else if error.is::<crate::api::AuthExpired>()
                    || error.to_string().contains("API returned")
                {
                    "http_status"
                } else {
                    "client"
                }
            })
    }

    pub(super) async fn read(
        &self,
        key: &str,
        revision: &str,
        access: Option<&str>,
        seed: Option<(api::RateLimitResponse, Instant)>,
        summary: &mut Account,
    ) -> Option<&'static str> {
        let entry = self
            .entries
            .lock()
            .expect("usage cache lock")
            .entry(key.into())
            .or_default()
            .clone();
        // This lock deduplicates usage requests only. Token delivery never takes it.
        let mut entry = entry.lock().await;
        if entry.revision != revision {
            *entry = Entry {
                revision: revision.into(),
                ..Default::default()
            };
        }
        if let Some(seed) = seed
            && entry.sample.as_ref().is_none_or(|(_, at)| seed.1 > *at)
        {
            entry.sample = Some(seed);
            entry.error = None;
            entry.attempted = None;
        }
        let fresh = entry
            .sample
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < self.ttl);
        let cooling_down = entry.attempted.is_some_and(|at| at.elapsed() < self.ttl);
        let mut failure = None;
        if access.is_some_and(api::is_token_expired) {
            // An idle account can outlive its access token. Only token delivery
            // may refresh it; this expected state is not an upstream failure.
            entry.error = Some("access_expired");
        } else if summary.available && !fresh && !cooling_down {
            let result = match access {
                Some(access) => {
                    api::fetch_usage_at(
                        &self.client,
                        &self.endpoint,
                        access,
                        Some(&summary.account_id),
                    )
                    .await
                }
                None => Err(anyhow::anyhow!("missing access credential")),
            };
            entry.attempted = Some(Instant::now());
            match result {
                Ok(usage) => {
                    entry.sample = Some((usage, Instant::now()));
                    entry.error = None;
                }
                Err(error) => {
                    let timeout = error.chain().any(|cause| {
                        cause
                            .downcast_ref::<reqwest::Error>()
                            .is_some_and(reqwest::Error::is_timeout)
                    });
                    let reason = if timeout {
                        "catalog_usage_timeout"
                    } else {
                        "catalog_usage_failed"
                    };
                    entry.error = Some(reason);
                    failure = Some(reason);
                }
            }
        }
        summary.usage_age_seconds = entry.sample.as_ref().map(|(_, at)| at.elapsed().as_secs());
        summary.usage_stale = entry.error.is_some()
            || entry
                .sample
                .as_ref()
                .is_none_or(|(_, at)| at.elapsed() >= self.ttl);
        summary.usage_error = entry.error.map(str::to_owned);
        summary.statusline_usage = None;
        summary.primary_used = None;
        summary.secondary_used = None;
        summary.primary_window_seconds = None;
        summary.secondary_window_seconds = None;
        summary.primary_resets_at = None;
        summary.resets_at = None;
        summary.usage_score = None;
        summary.billing_class = api::BillingClass::Unknown;
        if let Some((usage, _)) = &entry.sample {
            let limits = usage.rate_limit.as_ref();
            summary.plan = usage.plan_type.clone().or_else(|| summary.plan.clone());
            summary.primary_used = limits
                .and_then(|r| r.short_window())
                .map(|w| w.used_percent);
            summary.secondary_used = limits.and_then(|r| r.long_window()).map(|w| w.used_percent);
            summary.primary_window_seconds = limits
                .and_then(api::RateLimit::short_window)
                .and_then(api::RateLimitWindow::duration_seconds);
            summary.secondary_window_seconds = limits
                .and_then(api::RateLimit::long_window)
                .and_then(api::RateLimitWindow::duration_seconds);
            summary.primary_resets_at = limits
                .and_then(api::RateLimit::short_window)
                .and_then(api::RateLimitWindow::reset_timestamp);
            summary.resets_at = limits
                .and_then(|r| r.long_window())
                .and_then(|w| w.reset_timestamp());
            if !summary.usage_stale {
                let mut snapshot = crate::statusline::Usage::from_usage(usage);
                snapshot.age_seconds = summary.usage_age_seconds.unwrap_or_default();
                summary.statusline_usage = Some(snapshot);
                summary.billing_class = server::usage_billing_class(usage);
                summary.usage_score = limits.map(|r| r.availability_score());
            }
        }
        failure
    }
}
