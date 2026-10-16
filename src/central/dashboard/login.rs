//! Browser adapters for the shared login lifecycle; credentials stay in that module.
use super::{Broker, HttpError, actions, document, enrollment};
use crate::central::{
    relogin::{
        self,
        shared::{self, Caller},
    },
    storage::login::LoginAuthority,
};
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
            "<main id=\"main\" class=\"wrap login\"><h1>Login could not continue</h1><p>Check your sign-in and account status before trying again.</p><p><a href=\"/accounts\">Back to accounts</a></p></main>",
        );
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
    #[serde(default)]
    alias: String,
    #[serde(default)]
    id: String,
    form_token: Option<String>,
    label: Option<String>,
}

async fn caller(broker: &Broker, headers: &HeaderMap) -> Result<Caller, HttpError> {
    if !broker
        .central
        .as_ref()
        .is_some_and(|s| s.mode() == crate::central::storage::StoreMode::Postgres)
    {
        return Err(broker.error(StatusCode::NOT_FOUND, "dashboard_login_unavailable"));
    }
    let user = enrollment::browser_user(broker, headers)
        .await?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    let cookie = enrollment::browser_session_token(broker, headers)
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    Ok(Caller {
        user: user.id,
        dashboard_actor: Some(format!("dashboard:{}", user.email)),
        authority: LoginAuthority::Browser(crate::central::vault::digest(cookie.as_bytes())),
    })
}

fn require_starts(broker: &Broker) -> Result<(), HttpError> {
    if !broker.browser_logins_enabled {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "dashboard_login_disabled"));
    }
    Ok(())
}

pub(super) async fn add_page(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    caller(&broker, &headers).await?;
    require_starts(&broker)?;
    let token = actions::form_token(&broker, &headers)?;
    let id = crate::central::vault::digest(&enrollment::random_bytes());
    Ok(document(&format!(
        "<main id=\"main\" class=\"wrap login\"><h1>Add a server account</h1><p>Sign in to OpenAI to keep an account on this server.</p><form class=\"login-fields\" method=\"post\" action=\"/accounts/add\"><label>Alias <input name=\"alias\" required autocomplete=\"off\"></label><label>Label (optional) <input name=\"label\" autocomplete=\"off\"></label><input type=\"hidden\" name=\"id\" value=\"{id}\"><input type=\"hidden\" name=\"form_token\" value=\"{token}\"><button class=\"button\">Add account</button></form><p><a href=\"/accounts\">Back to accounts</a></p></main>"
    )))
}

pub(super) async fn add(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<Input>,
) -> Result<Response, HttpError> {
    actions::check_request(&broker, &headers, input.form_token.as_deref()).await?;
    let caller = caller(&broker, &headers).await?;
    let op = shared::browser_start(
        broker,
        caller,
        relogin::Request {
            alias: input.alias,
            id: input.id,
        },
        input.label,
    )
    .await?;
    Ok(Redirect::to(&format!("/accounts/login?id={}", op.id)).into_response())
}

pub(super) async fn page(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Query(input): Query<Input>,
) -> Result<Response, HttpError> {
    let caller = caller(&broker, &headers).await?;
    let op = shared::browser_receipt(
        &broker,
        &caller,
        &relogin::Request {
            alias: input.alias,
            id: input.id,
        },
        shared::BrowserAction::Read,
    )
    .await?;
    render_view(&broker, &headers, &op)
}

fn render_view(
    broker: &Broker,
    headers: &HeaderMap,
    op: &shared::BrowserView,
) -> Result<Response, HttpError> {
    let token = actions::form_token(broker, headers)?;
    let status = match op.status {
        "starting" => "Starting login",
        "pending" => "Finish your OpenAI sign-in",
        "verifying" => "Checking the sign-in",
        "completed" => "Login completed",
        "canceled" => "Login canceled",
        "expired" => "Login expired",
        _ => "Login failed",
    };
    let active = matches!(op.status, "starting" | "pending" | "verifying");
    let code = op
        .user_code
        .as_deref()
        .map(enrollment::escape)
        .unwrap_or_default();
    let alias = enrollment::escape(op.landed_alias.as_deref().unwrap_or(&op.alias));
    let result = if op.status == "completed" {
        format!("Server account {alias} is ready.")
    } else if op.status == "failed" {
        "Return to accounts and start a new login.".into()
    } else {
        String::new()
    };
    let id = enrollment::escape(&op.id);
    let fields = format!(
        r#"<input type="hidden" name="id" value="{id}"><input type="hidden" name="form_token" value="{token}">"#
    );
    let content = format!(
        r#"<main id="main" class="wrap login" data-browser-login data-status="{}" {}>
<h1>Add a server account</h1><p id="login-state" role="status">{status}</p>
<p id="login-result">{result}</p>
<section id="login-challenge" {}><p>Open this link and enter the one-time code:</p><p><a href="https://auth.openai.com/codex/device" rel="noreferrer">Sign in to OpenAI</a></p><p><code id="login-code" translate="no">{code}</code></p></section>
<form id="login-status-form" method="post" action="/accounts/login/status">{fields}<button class="button small">Check status</button></form>
<form id="login-cancel-form" method="post" action="/accounts/login/cancel" {}>{fields}<button class="button small">Cancel login</button></form>
<p><a href="/accounts">Back to accounts</a></p></main>"#,
        op.status,
        if active { "data-login-active" } else { "" },
        if code.is_empty() { "hidden" } else { "" },
        if active { "" } else { "hidden" }
    );
    let mut response = document(&content);
    response.headers_mut().insert(
        "referrer-policy",
        "same-origin".parse().expect("static header"),
    );
    Ok(response)
}

pub(super) async fn cancel(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<Input>,
) -> Result<Response, HttpError> {
    actions::check_request(&broker, &headers, input.form_token.as_deref()).await?;
    let caller = caller(&broker, &headers).await?;
    let op = shared::browser_receipt(
        &broker,
        &caller,
        &relogin::Request {
            alias: input.alias,
            id: input.id,
        },
        shared::BrowserAction::Cancel,
    )
    .await?;
    Ok(Redirect::to(&format!("/accounts/login?id={}", op.id)).into_response())
}

pub(super) async fn status(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Form(input): Form<Input>,
) -> Result<Response, HttpError> {
    actions::check_request(&broker, &headers, input.form_token.as_deref()).await?;
    let caller = caller(&broker, &headers).await?;
    let view = shared::browser_receipt(
        &broker,
        &caller,
        &relogin::Request {
            alias: input.alias,
            id: input.id,
        },
        shared::BrowserAction::Poll,
    )
    .await?;
    if headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(enrollment::accepts_html)
    {
        return Ok(Redirect::to(&format!("/accounts/login?id={}", view.id)).into_response());
    }
    Ok(axum::Json(view).into_response())
}
