//! Browser mutation checks. Form tokens bind the session without storing another secret.
use super::{Broker, HttpError, enrollment, vault};
use axum::{
    Form,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

#[derive(Deserialize)]
pub(super) struct RevokeForm {
    form_token: Option<String>,
    #[serde(default)]
    id: String,
}

fn form_mac(broker: &Broker, session: &str) -> Result<Hmac<Sha256>, HttpError> {
    let key = vault::private_read(&broker.key).map_err(|_| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard_guard_unavailable",
        )
    })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard_guard_unavailable",
        )
    })?;
    mac.update(b"codexctl-form:");
    mac.update(session.as_bytes());
    Ok(mac)
}

fn check_form(
    broker: &Broker,
    headers: &HeaderMap,
    form_token: Option<&str>,
) -> Result<(), HttpError> {
    let expected = reqwest::Url::parse(enrollment::browser_origin(broker)?).map_err(|_| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard_guard_unavailable",
        )
    })?;
    let origin = headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (_, authority) = value.split_once("://")?;
            if authority.contains(['/', '\\', '?', '#', '@'])
                || value.chars().any(char::is_whitespace)
            {
                return None;
            }
            reqwest::Url::parse(value).ok()
        })
        .map(|url| url.origin());
    if origin != Some(expected.origin()) {
        return Err(broker.error(StatusCode::FORBIDDEN, "dashboard_origin_refused"));
    }
    let session = enrollment::browser_session_token(broker, headers)
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "dashboard_form_refused"))?;
    let signature = form_token
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "dashboard_form_refused"))?;
    form_mac(broker, session)?
        .verify_slice(&signature)
        .map_err(|_| broker.error(StatusCode::FORBIDDEN, "dashboard_form_refused"))?;
    Ok(())
}

pub(super) async fn check_request(
    broker: &Broker,
    headers: &HeaderMap,
    form_token: Option<&str>,
) -> Result<super::managed::User, HttpError> {
    check_form(broker, headers, form_token)?;
    enrollment::browser_user(broker, headers)
        .await?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))
}

pub(super) fn form_token(broker: &Broker, headers: &HeaderMap) -> Result<String, HttpError> {
    let session = enrollment::browser_session_token(broker, headers)
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    Ok(URL_SAFE_NO_PAD.encode(form_mac(broker, session)?.finalize().into_bytes()))
}

pub(super) fn render(
    broker: &Broker,
    headers: &HeaderMap,
    snapshot: &super::Snapshot,
) -> Result<String, HttpError> {
    let token = if broker
        .central
        .as_ref()
        .is_some_and(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
    {
        let session = enrollment::browser_session_token(broker, headers)
            .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
        Some(URL_SAFE_NO_PAD.encode(form_mac(broker, session)?.finalize().into_bytes()))
    } else {
        None
    };
    Ok(super::render::overview_with_actions(
        snapshot,
        token.as_deref(),
        broker.browser_logins_enabled,
        broker.browser_remove_enabled,
        broker.browser_resets_enabled,
    ))
}

pub(super) async fn revoke(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<RevokeForm>,
) -> Response {
    let result = run_revoke(&broker, &headers, &input).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            let failure = error.into_response();
            let mut response = super::document(
                "<main id=\"main\" class=\"wrap\"><h1>Machine could not be revoked</h1><p>Return to your accounts to check the machine status or sign in again.</p><p><a href=\"/accounts\">Back to accounts</a></p></main>",
            );
            *response.status_mut() = failure.status();
            response
                .extensions_mut()
                .extend(failure.into_parts().0.extensions);
            response
        }
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("static header"));
    response
}

pub(super) async fn no_store(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("static header"));
    response
}

#[derive(Deserialize)]
pub(super) struct SignOutForm {
    form_token: Option<String>,
}

pub(super) async fn sign_out(
    State(broker): State<Broker>,
    headers: HeaderMap,
    input: Result<Form<SignOutForm>, axum::extract::rejection::FormRejection>,
) -> Result<Response, HttpError> {
    if broker
        .central
        .as_ref()
        .is_some_and(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
        && enrollment::browser_session_token(&broker, &headers).is_some()
    {
        let input = input.ok();
        check_form(
            &broker,
            &headers,
            input.as_ref().and_then(|v| v.form_token.as_deref()),
        )?;
    }
    enrollment::accounts_sign_out(State(broker), headers).await
}

async fn run_revoke(
    broker: &Broker,
    headers: &HeaderMap,
    input: &RevokeForm,
) -> Result<Response, HttpError> {
    let user = check_request(broker, headers, input.form_token.as_deref()).await?;
    let attempt = vault::digest(&enrollment::random_bytes());
    let central = broker
        .central
        .as_ref()
        .filter(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
        .ok_or_else(|| {
            broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "dashboard_actions_unavailable",
            )
        })?;
    let session = enrollment::browser_session_token(broker, headers)
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    let signed_in_at = match central.browser_session_signed_in_at(session).await {
        Ok(time) => time,
        Err(cause) => {
            super::managed::log_dashboard_failure("freshness", &cause);
            audit(
                broker,
                &user.email,
                &attempt,
                &input.id,
                "failed:guard_unavailable",
            )
            .await?;
            return Err(broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "dashboard_guard_unavailable",
            ));
        }
    };
    if !signed_in_at.is_some_and(recent_sign_in) {
        let result = "refused:fresh_sign_in_required";
        audit(broker, &user.email, &attempt, &input.id, result).await?;
        return Ok(Redirect::to("/accounts/sign-in?fresh=true").into_response());
    }
    let actor = format!("dashboard:{}", user.email);
    match super::managed::revoke_machine(
        broker,
        "sawmills",
        &user.id,
        &input.id,
        Some(crate::central::storage::DashboardAudit {
            operation: crate::central::storage::DashboardOperation::MachineRevoke,
            actor: &actor,
            attempt: &attempt,
        }),
    )
    .await
    {
        Ok(super::managed::RevokeOutcome::Revoked) => Ok(super::document(
            "<h1>Machine revoked</h1><p>This machine can no longer obtain tokens from the account server.</p><p><a href=\"/accounts\">Back to accounts</a></p>",
        )),
        Ok(super::managed::RevokeOutcome::AlreadyRevoked) => {
            let failure = broker
                .error(StatusCode::CONFLICT, "dashboard_already_revoked")
                .into_response();
            let mut response = super::document(
                "<h1>Already revoked</h1><p><a href=\"/accounts\">Back to accounts</a></p>",
            );
            *response.status_mut() = StatusCode::CONFLICT;
            response
                .extensions_mut()
                .extend(failure.into_parts().0.extensions);
            Ok(response)
        }
        Ok(super::managed::RevokeOutcome::NotFound) => {
            Err(broker.error(StatusCode::NOT_FOUND, "dashboard_machine_not_found"))
        }
        Ok(super::managed::RevokeOutcome::RegistryChanged) => {
            Err(broker.error(StatusCode::CONFLICT, "dashboard_registry_changed"))
        }
        Err(error) => {
            audit(
                broker,
                &user.email,
                &attempt,
                &input.id,
                &format!("failed:{}", error.reason),
            )
            .await?;
            Err(broker.error(error.status, "dashboard_revoke_failed"))
        }
    }
}

pub(in crate::central) fn recent_sign_in(signed_in_at: i64) -> bool {
    let now = chrono::Utc::now().timestamp();
    signed_in_at <= now.saturating_add(30) && signed_in_at >= now.saturating_sub(630)
}

async fn audit(
    broker: &Broker,
    email: &str,
    attempt: &str,
    target: &str,
    result: &str,
) -> Result<(), HttpError> {
    let actor = format!("dashboard:{email}");
    // Refusals before a scoped lookup must not persist arbitrary request text.
    let target = format!("unresolved:{}", vault::digest(target.as_bytes()));
    broker
        .central
        .as_ref()
        .expect("action checked PostgreSQL")
        .dashboard_audit(
            crate::central::storage::DashboardAudit {
                operation: crate::central::storage::DashboardOperation::MachineRevoke,
                actor: &actor,
                attempt,
            },
            &target,
            result,
        )
        .await
        .map_err(|cause| {
            super::managed::log_dashboard_failure("audit", &cause);
            broker.error(StatusCode::SERVICE_UNAVAILABLE, "dashboard_audit_failed")
        })?;
    Ok(())
}
