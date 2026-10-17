//! Shared reset operations use committed credentials, never a native refresh or billing probe.
use super::*;
use crate::central::{managed::HttpError, storage::CentralStore, vault};
use crate::central::{
    relogin::shared::Caller,
    storage::resets::{ResetAudit, ResetWrite},
};
use futures::{StreamExt, stream};
use serde_json::Value;

const INVENTORY_CONCURRENCY: usize = 8;

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
    let user = &device.user;
    let mut accounts = stream::iter(aliases.into_iter().enumerate())
        .map(|(index, alias)| async move {
            let outcome = match credential(central, user, &alias).await {
                Ok(saved) => {
                    let result = async {
                        let token = vault::token(&saved.auth)?;
                        let account = vault::account(&saved.auth)?;
                        broker.reset_reader.read(token, &account).await
                    }
                    .await;
                    if let Err(error) = &result {
                        log_error("inventory", error, &saved.auth);
                    }
                    result
                }
                Err(error) => Err(error),
            }
            .unwrap_or_else(|_| {
                broker.record_failure("reset_read_failed", "resets", StatusCode::BAD_GATEWAY);
                Outcome::Failed {
                    error: "reset_read_failed".into(),
                }
            });
            (index, Account { alias, outcome })
        })
        .buffer_unordered(INVENTORY_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    accounts.sort_unstable_by_key(|(index, _)| *index);
    let accounts = accounts.into_iter().map(|(_, account)| account).collect();
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
    let result = async {
        let saved = checked_credential(&record, user)?;
        require_routing(central, &record, &saved).await?;
        Ok(saved)
    }
    .await;
    if let Err(error) = &result {
        log_error("credential", error, &record.vault);
    }
    result
}

fn checked_credential(
    record: &crate::central::storage::CredentialRecord,
    user: &str,
) -> Result<vault::Vault> {
    let saved: vault::Vault = serde_json::from_value(record.vault.clone())?;
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

async fn require_routing(
    central: &CentralStore,
    record: &crate::central::storage::CredentialRecord,
    saved: &vault::Vault,
) -> Result<String> {
    let revision = vault::digest(&serde_json::to_vec(&saved.auth)?);
    let observed = central.fast_token_read(&record.account_id).await?;
    let proved = observed
        .as_ref()
        .filter(|read| read.revision == record.revision && read.user_id == record.user_id)
        .and_then(|read| read.evidence.as_ref())
        .is_some_and(|proof| {
            proof.routing_supported
                && proof.account_revision == record.revision
                && proof.auth_revision == revision
                && proof.routing_age < crate::central::fast_path::EVIDENCE_MAX_AGE
        });
    if !proved {
        return Err(Refusal("reset_routing_unverified").into());
    }
    Ok(revision)
}

fn log_error(stage: &'static str, error: &anyhow::Error, secrets: &Value) {
    let mut detail = format!("{error:#}");
    redact_auth_strings(secrets, &mut detail);
    let detail: String = detail.chars().take(4096).collect();
    let status = error
        .downcast_ref::<api::ResetRejected>()
        .map(|error| error.0.as_u16());
    eprintln!(
        "{}",
        serde_json::json!({"operation":"reset_redemption","stage":stage,"error":detail,"provider_status":status})
    );
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
        let _permit = permit;
        let mut secrets = Value::Null;
        run(&worker, &caller, &request, &mut secrets)
            .await
            .map_err(|error| {
                if let Some(refusal) = error.downcast_ref::<Refusal>() {
                    let status = match refusal.0 {
                        "account_not_found" => StatusCode::NOT_FOUND,
                        "reset_authority_ended" => StatusCode::UNAUTHORIZED,
                        "refresh_in_progress"
                        | "login_pending"
                        | "reset_in_progress"
                        | "reset_routing_unverified"
                        | "reset_custody_lost" => StatusCode::SERVICE_UNAVAILABLE,
                        _ => StatusCode::CONFLICT,
                    };
                    worker.error(status, refusal.0)
                } else if error.is::<TerminalRejection>() {
                    worker.error(StatusCode::UNPROCESSABLE_ENTITY, "reset_rejected")
                } else {
                    log_error("shared", &error, &secrets);
                    worker.error(StatusCode::BAD_GATEWAY, "reset_redeem_failed")
                }
            })
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "reset_redeem_failed"))?
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
    secrets: &mut Value,
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
    // Replaying a receipt requires live caller authority and account scope, but
    // performs no provider work and must survive saved credential expiry.
    let (_, value) = central
        .reset_journal(&caller.user, &record.account_id)
        .await?;
    let journal: Journal = serde_json::from_value(value)?;
    if let Some(completed) = journal.completed.get(&request.redeem_request_id) {
        return receipt(completed, true);
    }
    let owner = broker
        .owners
        .read()
        .await
        .get(&record.account_id)
        .map(|(_, owner)| owner.clone());
    let local = owner
        .as_ref()
        .map(|owner| owner.try_lock())
        .transpose()
        .map_err(|_| Refusal("refresh_in_progress"))?;
    if local.as_ref().is_some_and(|owner| owner.import_settling) {
        return Err(Refusal("refresh_in_progress").into());
    }
    if local.as_ref().is_some_and(|owner| owner.routing_refused) {
        return Err(Refusal("reset_routing_unverified").into());
    }
    let custody = central.reset_custody(&record.account_id).await?;
    let current = central
        .load_account_by_alias(&caller.user, &request.alias)
        .await?
        .ok_or(Refusal("account_not_found"))?;
    *secrets = current.vault.clone();
    if current.account_id != record.account_id {
        return Err(Refusal("account_changed").into());
    }
    tokio::select! {
        biased;
        _ = custody.lost() => Err(Refusal("reset_custody_lost").into()),
        result = execute(broker,central,caller,request,&current) => result,
    }
}

async fn execute(
    broker: &Broker,
    central: &CentralStore,
    caller: &Caller,
    request: &Redemption,
    record: &crate::central::storage::CredentialRecord,
) -> Result<api::ConsumeResetResponse> {
    let saved = checked_credential(record, &caller.user)?;
    let auth_revision = require_routing(central, record, &saved).await?;
    let token = vault::token(&saved.auth)?;
    let workspace = vault::account(&saved.auth)?;
    let reader = &broker.reset_reader;
    let mut prepared = None;
    for _ in 0..8 {
        let (version, value) = central
            .reset_journal(&caller.user, &record.account_id)
            .await?;
        let mut journal: Journal = serde_json::from_value(value)?;
        if let Some(completed) = journal.completed.get(&request.redeem_request_id) {
            return receipt(completed, true);
        }
        if let Some(pending) = journal.pending.as_mut() {
            pending.join(&request.redeem_request_id)?;
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
                &caller.user,
                &record.account_id,
                version,
                &serde_json::to_value(&journal)?,
                ResetWrite::Reserve {
                    caller,
                    account_revision: record.revision,
                    auth_revision: &auth_revision,
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
        Err(error) if error.is::<api::ResetRejected>() => {
            log_error("provider_rejected", &error, &record.vault);
            api::ConsumeResetResponse {
                code: api::ConsumeResetCode::Unknown,
                windows_reset: 0,
            }
        }
        Err(error) => return Err(error),
    };
    for _ in 0..8 {
        let (version, value) = central
            .reset_journal(&caller.user, &record.account_id)
            .await?;
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
        let audit = current.actor.as_deref().map(|actor| ResetAudit {
            actor,
            attempt: &audit_key,
        });
        journal.complete(
            current
                .joined_requests
                .into_iter()
                .chain([current.request_id]),
            &response,
        )?;
        if central
            .write_reset_journal(
                &caller.user,
                &record.account_id,
                version,
                &serde_json::to_value(&journal)?,
                ResetWrite::Settle {
                    audit,
                    code: response.code,
                },
            )
            .await?
        {
            return receipt(&response, false);
        }
    }
    Err(Refusal("reset_busy").into())
}
