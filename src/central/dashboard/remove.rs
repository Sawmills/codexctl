//! HTML adaptation only; removal policy and credential custody stay in removal.
use super::{Broker, HttpError, actions, document, enrollment, login, vault};
use axum::{
    Form,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use serde::Deserialize;

pub(super) async fn errors(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let html = request
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(enrollment::accepts_html);
    let response = next.run(request).await;
    let mut response = if html
        && (response.status().is_client_error() || response.status().is_server_error())
    {
        let (parts, body) = response.into_parts();
        let reason = axum::body::to_bytes(body, 4096)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let detail = match reason.as_str() {
            "removal_confirmation_changed" => {
                "Open Manage again and type the exact displayed alias."
            }
            "account_lent" => "End the active loan before removing this account.",
            "login_pending" => "Finish or cancel the pending login before removing this account.",
            "refresh_in_progress" => {
                "The account is busy. Wait for its current operation to finish, then open Manage again."
            }
            "dashboard_remove_disabled" => "Account removal is not enabled on this server.",
            _ => "Return to your accounts to check their status or sign in again.",
        };
        let mut page = document(&format!(
            "<main id=\"main\" class=\"wrap\"><h1>Account could not be removed</h1><p>{detail}</p><p><a href=\"/accounts\">Back to accounts</a></p></main>"
        ));
        *page.status_mut() = parts.status;
        page.extensions_mut().extend(parts.extensions);
        page
    } else {
        response
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("static header"));
    response.headers_mut().insert(
        "referrer-policy",
        "same-origin".parse().expect("static header"),
    );
    response
}

#[derive(Deserialize)]
pub(super) struct Input {
    form_token: Option<String>,
    alias: String,
    attempt: String,
    account_id: String,
    revision: i64,
    confirmation: String,
}

pub(super) async fn submit(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<Input>,
) -> Result<Response, HttpError> {
    actions::check_request(&broker, &headers, input.form_token.as_deref()).await?;
    let caller = login::caller(&broker, &headers).await?;
    let request = crate::central::removal::Request {
        alias: input.alias,
        attempt: input.attempt,
        account_id: input.account_id,
        revision: input.revision,
        confirmation: input.confirmation,
    };
    // The worker retains custody even if the browser disconnects mid-stop.
    let worker = broker.clone();
    let result =
        tokio::spawn(
            async move { crate::central::removal::remove(&worker, &caller, request).await },
        )
        .await
        .map_err(|_| {
            broker.error(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "dashboard_remove_unavailable",
            )
        })?;
    if let Err(error) = result {
        if error.reason == "fresh_sign_in_required" {
            use axum::response::IntoResponse;
            return Ok(axum::response::Redirect::to("/accounts/sign-in?fresh=true").into_response());
        }
        return Err(error);
    }
    Ok(document(
        "<main id=\"main\" class=\"wrap\"><h1>Server account removed</h1><p><a href=\"/accounts\">Back to accounts</a></p></main>",
    ))
}

#[derive(Deserialize)]
pub(super) struct QueryInput {
    alias: String,
}

pub(super) async fn page(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Query(input): Query<QueryInput>,
) -> Result<Response, HttpError> {
    let caller = login::caller(&broker, &headers).await?;
    let target = crate::central::removal::view(&broker, &caller, &input.alias).await?;
    let alias = enrollment::escape(&target.alias);
    let account_id = enrollment::escape(&target.account_id);
    let revision = target.revision;
    let recent = target.recent_machines;
    let token = actions::form_token(&broker, &headers)?;
    let attempt = vault::digest(&enrollment::random_bytes());
    Ok(document(&format!(
        r#"<main id="main" class="wrap"><h1>Remove a server account</h1><p>Remove <strong>{alias}</strong> from this account server. This ends service from this server and does not revoke its OpenAI credentials.</p><p>{recent} machines received a token in the last 5 minutes. Previously issued tokens can remain usable.</p><form method="post" action="/accounts/remove"><input type="hidden" name="alias" value="{alias}"><input type="hidden" name="account_id" value="{account_id}"><input type="hidden" name="revision" value="{revision}"><input type="hidden" name="attempt" value="{attempt}"><input type="hidden" name="form_token" value="{token}"><label>Type the exact alias to confirm <input name="confirmation" required autocomplete="off"></label><button class="button">Remove account</button></form><p><a href="/accounts">Back to accounts</a></p></main>"#
    )))
}
