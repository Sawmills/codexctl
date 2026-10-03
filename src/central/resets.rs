//! Reset inventory and durable redemption. Credentials remain with the refresh owner.
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

pub(super) fn routes() -> axum::Router<Broker> {
    axum::Router::new()
        .route("/v1/resets", axum::routing::get(list))
        .route("/v1/resets/redeem", axum::routing::post(redeem))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Redemption {
    pub alias: String,
    pub redeem_request_id: String,
}

async fn redeem(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Redemption>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, super::managed::HttpError> {
    let device = broker.authorize(&headers)?;
    let Json(request) =
        body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if request.redeem_request_id.is_empty() || request.redeem_request_id.len() > 128 {
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let owner = broker.owner(&device, &request.alias).await?;
    if broker.read_only || broker.stopping.load(std::sync::atomic::Ordering::Acquire) {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_unavailable"));
    }
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let worker = broker.clone();
    // Complete persistence even if the requesting machine disconnects.
    let response = tokio::spawn(async move {
        let _permit = permit;
        let mut owner = owner.lock().await;
        worker.authorize(&headers)?;
        if owner.vault.user != device.user {
            return Err(worker.error(StatusCode::NOT_FOUND, "account_not_found"));
        }
        worker
            .reset_reader
            .redeem(&mut owner, &request)
            .await
            .map_err(|error| {
                if let Some(refusal) = error.downcast_ref::<Refusal>() {
                    worker.error(StatusCode::CONFLICT, refusal.0)
                } else {
                    worker.error(StatusCode::BAD_GATEWAY, "reset_redeem_failed")
                }
            })
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_redeem_failed"))??;
    Ok(([("cache-control", "no-store")], Json(response)).into_response())
}

#[derive(Debug)]
struct Refusal(&'static str);
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Refusal {}

#[derive(Serialize, Deserialize)]
struct Pending {
    request_id: String,
    credit_id: String,
    upstream_id: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal {
    pending: Option<Pending>,
    completed: std::collections::BTreeMap<String, api::ConsumeResetResponse>,
}
impl Reader {
    async fn redeem(
        &self,
        owner: &mut super::server::Owner,
        request: &Redemption,
    ) -> Result<api::ConsumeResetResponse> {
        let path = owner.state.join("reset-redemptions.json");
        let mut journal: Journal = if path.try_exists()? {
            serde_json::from_slice(&super::vault::private_read(&path)?)?
        } else {
            Journal::default()
        };
        if let Some(response) = journal.completed.get(&request.redeem_request_id) {
            let mut response = response.clone();
            if matches!(
                response.code,
                api::ConsumeResetCode::Reset | api::ConsumeResetCode::AlreadyRedeemed
            ) {
                response.code = api::ConsumeResetCode::AlreadyRedeemed;
            }
            return Ok(response);
        }
        if journal
            .pending
            .as_ref()
            .is_some_and(|p| p.request_id != request.redeem_request_id)
        {
            return Err(Refusal("reset_pending").into());
        }
        let mut token_request = TokenRequest::default();
        for attempt in 0..2 {
            let token = owner
                .tokens(token_request)
                .await
                .map_err(|_| anyhow::anyhow!("owner unavailable"))?;
            anyhow::ensure!(
                token.native_routing_supported,
                "unsupported workspace routing"
            );
            let result = async {
                if journal.pending.is_none() {
                    let usage = api::fetch_usage_at(
                        &self.client,
                        &self.usage_url,
                        &token.access_token,
                        Some(&token.chatgpt_account_id),
                    )
                    .await?;
                    if usage.reset_credits_applicable() <= 0 {
                        return Err(Refusal("nothing_to_reset").into());
                    }
                    let details = api::fetch_reset_credits_at(
                        &self.client,
                        &self.credits_url,
                        &token.access_token,
                        Some(&token.chatgpt_account_id),
                    )
                    .await?;
                    let credit = details
                        .credits
                        .iter()
                        .filter(|c| c.is_available())
                        .filter(|c| {
                            c.expires_at_timestamp()
                                .is_none_or(|t| t > chrono::Utc::now().timestamp())
                        })
                        .min_by_key(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
                        .ok_or(Refusal("no_reset_credit"))?;
                    journal.pending = Some(Pending {
                        request_id: request.redeem_request_id.clone(),
                        credit_id: credit.id.clone(),
                        upstream_id: super::vault::digest(&serde_json::to_vec(&(
                            &owner.vault.user,
                            &owner.vault.alias,
                            &request.redeem_request_id,
                        ))?),
                    });
                    // A crash or timeout after this point retries this exact credit and key.
                    crate::store::atomic_write(&path, &serde_json::to_vec(&journal)?)?;
                }
                let pending = journal
                    .pending
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("missing redemption journal"))?;
                let response = api::consume_reset_credit_at(
                    &self.client,
                    &self.credits_url,
                    &token.access_token,
                    Some(&token.chatgpt_account_id),
                    &pending.upstream_id,
                    Some(&pending.credit_id),
                )
                .await?;
                anyhow::ensure!(
                    response.code != api::ConsumeResetCode::Unknown,
                    "unknown redemption outcome"
                );
                journal
                    .completed
                    .insert(request.redeem_request_id.clone(), response.clone());
                journal.pending = None;
                crate::store::atomic_write(&path, &serde_json::to_vec(&journal)?)?;
                Ok(response)
            }
            .await;
            if attempt == 0
                && result
                    .as_ref()
                    .is_err_and(|e: &anyhow::Error| e.is::<api::AuthExpired>())
            {
                token_request = TokenRequest {
                    previous_revision: Some(token.revision),
                    account_id: Some(token.chatgpt_account_id),
                    ..Default::default()
                };
                continue;
            }
            return result;
        }
        unreachable!("bounded authentication retry returns on its second attempt")
    }
}
