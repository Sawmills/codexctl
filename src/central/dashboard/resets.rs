//! Browser transport for explicit banked-reset approval.
use super::{Broker, HttpError, actions, document, enrollment, login};
use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
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
        let (parts, _) = response.into_parts();
        let mut page = document(
            "<main id=\"main\" class=\"wrap\"><h1>Reset could not be applied</h1><p>Check your sign-in and account status, then try again.</p><p><a href=\"/accounts\">Back to accounts</a></p></main>",
        );
        *page.status_mut() = parts.status;
        page.extensions_mut().extend(parts.extensions);
        if let Some(retry) = parts.headers.get("retry-after") {
            page.headers_mut().insert("retry-after", retry.clone());
        }
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

fn require_gate(broker: &Broker) -> Result<(), HttpError> {
    if !broker.browser_resets_enabled {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "dashboard_reset_disabled"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct Input {
    alias: String,
    #[serde(default)]
    id: String,
    form_token: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Selection {
    alias: String,
}

pub(super) async fn page(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Query(selection): Query<Selection>,
) -> Result<Response, HttpError> {
    require_gate(&broker)?;
    let caller = login::caller(&broker, &headers).await?;
    let view = crate::central::resets::view(&broker, &caller, &selection.alias).await?;
    let alias = enrollment::escape(&view.alias);
    let count = view.count;
    let expiry = view
        .nearest_expiry
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
        .map(|at| format!("Nearest expiry: {}.", at.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_else(|| "Expiry is not reported.".into());
    let disabled = if view.applicable { "" } else { "disabled" };
    let reason = if view.applicable {
        "Spends the qualifying reset closest to expiry."
    } else {
        "A banked reset can only be applied once this account has an exhausted usage window."
    };
    let id = crate::central::vault::digest(&enrollment::random_bytes());
    let token = actions::form_token(&broker, &headers)?;
    Ok(document(&format!(
        r#"<main id="main" class="wrap"><h1>Apply a banked reset</h1><p>Spend one banked reset for <strong>{alias}</strong> to clear an exhausted usage window.</p><p>{count} banked resets. {expiry}</p><p>{reason}</p><form method="post" action="/accounts/reset"><input type="hidden" name="alias" value="{alias}"><input type="hidden" name="id" value="{id}"><input type="hidden" name="form_token" value="{token}"><button class="button" {disabled}>Apply reset</button></form><p><a href="/accounts">Back to accounts</a></p></main>"#
    )))
}

pub(super) async fn submit(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<Input>,
) -> Result<Response, HttpError> {
    require_gate(&broker)?;
    actions::check_request(&broker, &headers, input.form_token.as_deref()).await?;
    let caller = login::caller(&broker, &headers).await?;
    let response = crate::central::resets::apply(
        &broker,
        caller,
        crate::central::resets::Redemption {
            alias: input.alias,
            redeem_request_id: input.id,
        },
    )
    .await;
    let response = match response {
        Err(error) if error.reason == "fresh_sign_in_required" => {
            return Ok(Redirect::to("/accounts/sign-in?fresh=true").into_response());
        }
        other => other?,
    };
    let title = match response.code {
        crate::api::ConsumeResetCode::Reset | crate::api::ConsumeResetCode::AlreadyRedeemed => {
            "Reset applied"
        }
        crate::api::ConsumeResetCode::NothingToReset => "No exhausted window to reset",
        crate::api::ConsumeResetCode::NoCredit => "No banked reset available",
        crate::api::ConsumeResetCode::Unknown => "Reset could not be applied",
    };
    Ok(document(&format!(
        "<main id=\"main\" class=\"wrap\"><h1>{title}</h1><p><a href=\"/accounts\">Back to accounts</a></p></main>"
    )))
}
