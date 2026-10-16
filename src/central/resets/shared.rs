//! Shared reset operations use committed credentials, never a native refresh or billing probe.
use super::*;
use crate::central::{managed::HttpError, storage::CentralStore, vault};
use crate::central::{relogin::shared::Caller, storage::resets::ResetWrite};

pub(super) async fn list(broker: &Broker, headers: &HeaderMap) -> Result<Response, HttpError> {
    let device = broker.authorize(headers).await?;
    let central = broker
        .central
        .as_ref()
        .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_unavailable"))?;
    let aliases = central
        .list_account_aliases(&device.user)
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_read_failed"))?;
    let mut accounts = Vec::new();
    for alias in aliases {
        let outcome = match credential(central, &device.user, &alias).await {
            Ok(saved) => {
                broker
                    .reset_reader
                    .read(
                        vault::token(&saved.auth).unwrap_or_default(),
                        &vault::account(&saved.auth).unwrap_or_default(),
                    )
                    .await
            }
            Err(error) => Err(error),
        }
        .unwrap_or_else(|_| {
            broker.record_failure("reset_read_failed", "resets", StatusCode::BAD_GATEWAY);
            Outcome::Failed {
                error: "reset_read_failed".into(),
            }
        });
        accounts.push(Account { alias, outcome });
    }
    broker.authorize(headers).await?;
    Ok((
        [("cache-control", "no-store")],
        Json(Inventory {
            user_id: device.user,
            accounts,
        }),
    )
        .into_response())
}

async fn credential(central: &CentralStore, user: &str, alias: &str) -> Result<vault::Vault> {
    let record = central
        .load_account_by_alias(user, alias)
        .await?
        .ok_or_else(|| anyhow::anyhow!("account unavailable"))?;
    let saved: vault::Vault = serde_json::from_value(record.vault)?;
    anyhow::ensure!(
        saved.user == user && saved.verified,
        "account is not verified"
    );
    let token = vault::token(&saved.auth)?;
    anyhow::ensure!(
        !api::is_token_expired(token),
        "saved credential expired; renew login"
    );
    vault::account(&saved.auth)?;
    Ok(saved)
}

pub(in crate::central) struct ResetView {
    pub alias: String,
    pub count: i64,
    pub nearest_expiry: Option<i64>,
    pub applicable: bool,
}

pub(in crate::central) async fn view(
    broker: &Broker,
    caller: &Caller,
    alias: &str,
) -> Result<ResetView, HttpError> {
    let central = broker
        .central
        .as_ref()
        .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_unavailable"))?;
    let saved = credential(central, &caller.user, alias)
        .await
        .map_err(|_| broker.error(StatusCode::NOT_FOUND, "account_not_found"))?;
    let token = vault::token(&saved.auth)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_read_failed"))?;
    let account = vault::account(&saved.auth)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_read_failed"))?;
    let observed = broker
        .reset_reader
        .read(token, &account)
        .await
        .map_err(|_| broker.error(StatusCode::BAD_GATEWAY, "reset_read_failed"))?;
    let Outcome::Read {
        available,
        applicable,
        credits,
    } = observed
    else {
        return Err(broker.error(StatusCode::BAD_GATEWAY, "reset_read_failed"));
    };
    let now = chrono::Utc::now().timestamp();
    let qualifying: Vec<_> = credits
        .iter()
        .filter(|c| c.is_available() && c.expires_at_timestamp().is_none_or(|at| at > now))
        .collect();
    Ok(ResetView {
        alias: saved.alias,
        count: available,
        applicable: applicable > 0 && !qualifying.is_empty(),
        nearest_expiry: qualifying
            .iter()
            .filter_map(|c| c.expires_at_timestamp())
            .min(),
    })
}

pub(in crate::central) async fn apply(
    broker: &Broker,
    caller: Caller,
    request: Redemption,
) -> Result<api::ConsumeResetResponse, HttpError> {
    if request.redeem_request_id.is_empty() || request.redeem_request_id.len() > 128 {
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_request"));
    }
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
    tokio::spawn(async move {
        let _permit=permit;
        run(&worker,&caller,&request).await.map_err(|error| {
            if let Some(refusal)=error.downcast_ref::<Refusal>() {
                let status=match refusal.0 {"account_not_found"=>StatusCode::NOT_FOUND,"reset_authority_ended"=>StatusCode::UNAUTHORIZED,"refresh_in_progress"=>StatusCode::SERVICE_UNAVAILABLE,_=>StatusCode::CONFLICT};
                worker.error(status,refusal.0)
            } else if error.is::<TerminalRejection>() {
                worker.error(StatusCode::UNPROCESSABLE_ENTITY,"reset_rejected")
            } else {
                eprintln!("{}",serde_json::json!({"operation":"reset_redemption","stage":"shared","reason":"reset_redeem_failed"}));
                worker.error(StatusCode::BAD_GATEWAY,"reset_redeem_failed")
            }
        })
    }).await.map_err(|_|broker.error(StatusCode::SERVICE_UNAVAILABLE,"reset_redeem_failed"))?
}

fn receipt(
    response: &api::ConsumeResetResponse,
    replay: bool,
) -> Result<api::ConsumeResetResponse> {
    if response.code == api::ConsumeResetCode::Unknown {
        return Err(TerminalRejection("provider rejected this operation".into()).into());
    }
    let mut response = response.clone();
    if replay
        && matches!(
            response.code,
            api::ConsumeResetCode::Reset | api::ConsumeResetCode::AlreadyRedeemed
        )
    {
        response.code = api::ConsumeResetCode::AlreadyRedeemed;
    }
    Ok(response)
}

async fn run(
    broker: &Broker,
    caller: &Caller,
    request: &Redemption,
) -> Result<api::ConsumeResetResponse> {
    let central = broker
        .central
        .as_ref()
        .ok_or(Refusal("reset_unavailable"))?;
    central.authorize_reset(caller).await?;
    let record = central
        .load_account_by_alias(&caller.user, &request.alias)
        .await?
        .ok_or(Refusal("account_not_found"))?;
    let saved = credential(central, &caller.user, &request.alias).await?;
    let token = vault::token(&saved.auth)?;
    let workspace = vault::account(&saved.auth)?;
    let reader = &broker.reset_reader;
    let mut prepared = None;
    for _ in 0..8 {
        let (version, value) = central.reset_journal(&record.account_id).await?;
        let mut journal: Journal = serde_json::from_value(value)?;
        if let Some(completed) = journal.completed.get(&request.redeem_request_id) {
            return receipt(completed, true);
        }
        if let Some(pending) = journal.pending.as_mut() {
            pending
                .joined_requests
                .insert(request.redeem_request_id.clone());
            if pending.actor.is_none() {
                pending.actor = caller.dashboard_actor.clone();
            }
        } else {
            let usage =
                api::fetch_usage_at(&reader.client, &reader.usage_url, token, Some(&workspace))
                    .await?;
            if usage.reset_credits_applicable() <= 0 {
                return Err(Refusal("nothing_to_reset").into());
            }
            let details = api::fetch_reset_credits_at(
                &reader.client,
                &reader.credits_url,
                token,
                Some(&workspace),
            )
            .await?;
            let now = chrono::Utc::now().timestamp();
            let credit = details
                .credits
                .iter()
                .filter(|c| c.is_available() && c.expires_at_timestamp().is_none_or(|at| at > now))
                .min_by_key(|c| c.expires_at_timestamp().unwrap_or(i64::MAX))
                .ok_or(Refusal("no_reset_credit"))?;
            journal.pending = Some(Pending {
                request_id: request.redeem_request_id.clone(),
                credit_id: credit.id.clone(),
                upstream_id: vault::digest(&serde_json::to_vec(&(
                    &caller.user,
                    &record.account_id,
                    &request.redeem_request_id,
                ))?),
                joined_requests: Default::default(),
                actor: caller.dashboard_actor.clone(),
            });
        }
        if central
            .write_reset_journal(
                &record.account_id,
                version,
                &serde_json::to_value(&journal)?,
                ResetWrite::Reserve {
                    caller,
                    account_revision: record.revision,
                    holder: &broker.holder_id,
                },
            )
            .await?
        {
            prepared = journal.pending;
            break;
        }
    }
    let pending = prepared.ok_or(Refusal("reset_busy"))?;
    let response = match api::consume_reset_credit_at(
        &reader.client,
        &reader.credits_url,
        token,
        Some(&workspace),
        &pending.upstream_id,
        Some(&pending.credit_id),
    )
    .await
    {
        Ok(response) => response,
        Err(error) if error.is::<api::ResetRejected>() => api::ConsumeResetResponse {
            code: api::ConsumeResetCode::Unknown,
            windows_reset: 0,
        },
        Err(error) => return Err(error),
    };
    for _ in 0..8 {
        let (version, value) = central.reset_journal(&record.account_id).await?;
        let mut journal: Journal = serde_json::from_value(value)?;
        if let Some(completed) = journal.completed.get(&request.redeem_request_id) {
            return receipt(completed, true);
        }
        let current = journal.pending.take().ok_or(Refusal("reset_busy"))?;
        anyhow::ensure!(
            current.upstream_id == pending.upstream_id,
            "reset intent changed"
        );
        let audit_key = format!("reset-outcome:{}", current.upstream_id);
        let audit = current
            .actor
            .as_deref()
            .map(|actor| crate::central::storage::DashboardAudit {
                operation: crate::central::storage::DashboardOperation::AccountReset,
                actor,
                attempt: &audit_key,
            });
        let result = match response.code {
            api::ConsumeResetCode::Reset => "ok:reset",
            api::ConsumeResetCode::AlreadyRedeemed => "ok:already_redeemed",
            api::ConsumeResetCode::NothingToReset => "refused:nothing_to_reset",
            api::ConsumeResetCode::NoCredit => "refused:no_credit",
            api::ConsumeResetCode::Unknown => "failed:reset_rejected",
        };
        for id in current
            .joined_requests
            .into_iter()
            .chain([current.request_id])
        {
            journal.completed.insert(id, response.clone());
        }
        if central
            .write_reset_journal(
                &record.account_id,
                version,
                &serde_json::to_value(&journal)?,
                ResetWrite::Settle { audit, result },
            )
            .await?
        {
            return receipt(&response, false);
        }
    }
    Err(Refusal("reset_busy").into())
}
