//! Provider-specific HTTP contracts share company-user and machine authorization.
use super::{
    anthropic::{self, Engine},
    managed::{Broker, HttpError},
    providers::Provider,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn engine(broker: &Broker) -> Result<Arc<Engine>, HttpError> {
    broker
        .anthropic
        .clone()
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "provider_not_enabled"))
}
fn response<T: serde::Serialize>(value: T) -> Response {
    ([("cache-control", "no-store")], Json(value)).into_response()
}
async fn accounts(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    Ok(response(engine(&broker)?.accounts(&machine.user).await))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Token {
    account_id: String,
    #[serde(default)]
    previous_revision: Option<String>,
}
async fn token(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Token>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let engine = engine(&broker)?;
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let result = tokio::spawn(async move {
        let _permit = permit;
        engine
            .acquire(
                &machine.user,
                &input.account_id,
                input.previous_revision.as_deref(),
            )
            .await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "owner_unavailable"))?
    .map_err(|_| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "account_unavailable_or_login_required",
        )
    })?;
    broker.authorize_provider(&headers, Provider::Anthropic)?;
    Ok(response(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    alias: String,
    migration_id: String,
    grant: anthropic::Grant,
    exclusive_owner: bool,
}
async fn import(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<Import>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if !input.exclusive_owner {
        return Err(broker.error(StatusCode::CONFLICT, "exclusive_owner_required"));
    }
    let engine = engine(&broker)?;
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let result = tokio::spawn(async move {
        let _permit = permit;
        engine
            .admit(
                &machine.user,
                &input.alias,
                &input.migration_id,
                input.grant,
                None,
            )
            .await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "admission_unavailable"))?
    .map_err(|_| broker.error(StatusCode::CONFLICT, "admission_refused_reconcile_receipt"))?;
    broker.authorize_provider(&headers, Provider::Anthropic)?;
    Ok(response(result))
}
#[derive(Deserialize)]
struct Receipt {
    migration_id: String,
}
async fn receipt(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Query(input): Query<Receipt>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let result = engine(&broker)?
        .receipt(&machine.user, &input.migration_id)
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "receipt_unavailable"))?;
    Ok(response(json!({"receipt":result})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageRequest {
    account_id: String,
    #[serde(default)]
    cached: bool,
}
async fn usage(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Query(input): Query<UsageRequest>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let engine = engine(&broker)?;
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let result = tokio::spawn(async move {
        let _permit = permit;
        engine
            .usage(&machine.user, &input.account_id, input.cached)
            .await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "usage_unavailable"))?
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "usage_unavailable"))?;
    broker.authorize_provider(&headers, Provider::Anthropic)?;
    Ok(response(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginStart {
    alias: String,
    #[serde(default)]
    renew: bool,
}
async fn login_start(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<LoginStart>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let login = engine(&broker)?
        .start_login(&machine.user, &machine.id, &input.alias, input.renew)
        .await
        .map_err(|_| broker.error(StatusCode::CONFLICT, "login_refused"))?;
    Ok(response(login))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginComplete {
    id: String,
    code: String,
}
async fn login_complete(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Result<Json<LoginComplete>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, HttpError> {
    let machine = broker.authorize_provider(&headers, Provider::Anthropic)?;
    let Json(input) = body.map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let engine = engine(&broker)?;
    let permit = broker
        .work
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "server_stopping"))?;
    let result = tokio::spawn(async move {
        let _permit = permit;
        engine
            .finish_login(&machine.user, &machine.id, &input.id, &input.code)
            .await
    })
    .await
    .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "login_unavailable"))?
    .map_err(|_| broker.error(StatusCode::CONFLICT, "login_incomplete_grant_retained"))?;
    broker.authorize_provider(&headers, Provider::Anthropic)?;
    Ok(response(result))
}
pub(super) fn routes() -> Router<Broker> {
    Router::new()
        .route("/v2/anthropic/login/start", post(login_start))
        .route("/v2/anthropic/login/complete", post(login_complete))
        .route("/v2/anthropic/accounts", get(accounts))
        .route("/v2/anthropic/token", post(token))
        .route("/v2/anthropic/usage", get(usage))
        .route("/v2/anthropic/migrations", post(import).get(receipt))
}
