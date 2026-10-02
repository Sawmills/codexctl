//! Read-only reset inventory. OpenAI credentials remain with the refresh owner.
use super::{managed::Broker, server::TokenRequest};
use crate::api;
use anyhow::Result;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub user_id: String,
    pub accounts: Vec<Account>,
}

#[derive(Serialize, Deserialize)]
pub struct Account {
    pub alias: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Outcome {
    Read {
        available: i64,
        applicable: i64,
        credits: Vec<api::ResetCredit>,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone)]
pub(super) struct Reader {
    client: reqwest::Client,
    usage_url: String,
    credits_url: String,
}
impl Reader {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            usage_url: api::USAGE_URL.into(),
            credits_url: api::RESET_CREDITS_URL.into(),
        })
    }

    #[cfg(test)]
    pub(super) fn with_base(base: &str) -> Self {
        let mut reader = Self::new().unwrap();
        reader.usage_url = format!("{base}/usage");
        reader.credits_url = format!("{base}/credits");
        reader
    }

    async fn read(&self, token: &str, account: &str) -> Result<Outcome> {
        let usage =
            api::fetch_usage_at(&self.client, &self.usage_url, token, Some(account)).await?;
        let details =
            api::fetch_reset_credits_at(&self.client, &self.credits_url, token, Some(account))
                .await?;
        Ok(Outcome::Read {
            available: usage.reset_credits_available().max(details.available_count),
            applicable: usage.reset_credits_applicable(),
            credits: details.credits,
        })
    }
}

pub(super) async fn list(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Response, super::managed::HttpError> {
    let device = broker.authorize(&headers)?;
    let owners: Vec<_> = broker
        .owners
        .read()
        .await
        .values()
        .filter(|(identity, _)| identity.user == device.user)
        .map(|(identity, owner)| (identity.alias.clone(), owner.clone()))
        .collect();
    if owners.is_empty()
        && broker
            .ownership_unresolved
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "recovery_failed"));
    }
    let tasks = owners.into_iter().map(|(alias, owner)| {
        let broker = broker.clone();
        let user = device.user.clone();
        tokio::spawn(async move {
            // A canceled HTTP request must not interrupt credential persistence.
            let result = async {
                let _permit = broker.work.acquire().await?;
                let mut request = TokenRequest::default();
                let mut retried = false;
                loop {
                    let token = {
                        let mut owner = owner.lock().await;
                        anyhow::ensure!(owner.vault.user == user, "account user mismatch");
                        owner
                            .tokens(request)
                            .await
                            .map_err(|_| anyhow::anyhow!("owner unavailable"))?
                    };
                    anyhow::ensure!(
                        token.native_routing_supported,
                        "unsupported workspace routing"
                    );
                    let result = broker
                        .reset_reader
                        .read(&token.access_token, &token.chatgpt_account_id)
                        .await;
                    if !retried
                        && result
                            .as_ref()
                            .is_err_and(|error| error.is::<api::AuthExpired>())
                    {
                        broker.record_failure(
                            "reset_auth_rejected",
                            "resets",
                            StatusCode::UNAUTHORIZED,
                        );
                        retried = true;
                        request = TokenRequest {
                            previous_revision: Some(token.revision),
                            account_id: Some(token.chatgpt_account_id),
                            ..Default::default()
                        };
                        continue;
                    }
                    return result;
                }
            }
            .await;
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(_) => {
                    broker.record_failure("reset_read_failed", "resets", StatusCode::BAD_GATEWAY);
                    Outcome::Failed {
                        error: "reset_read_failed".into(),
                    }
                }
            };
            Account { alias, outcome }
        })
    });
    let mut accounts = Vec::new();
    for task in futures::future::join_all(tasks).await {
        accounts.push(
            task.map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_read_failed"))?,
        );
    }
    accounts.sort_by(|a, b| a.alias.cmp(&b.alias));
    broker.authorize(&headers)?;
    Ok((
        [("cache-control", "no-store")],
        Json(Inventory {
            user_id: device.user,
            accounts,
        }),
    )
        .into_response())
}
