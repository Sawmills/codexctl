//! Read-only company-user browser view. Credentials never enter this response.
use super::{
    enrollment,
    managed::{self, Broker, HttpError},
    relogin, vault,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
mod render;
use render::{Account, Identity, Machine, Resets, Snapshot, Window};
use sha2::{Digest, Sha256};

async fn page(State(broker): State<Broker>, headers: HeaderMap) -> Response {
    match snapshot(&broker, &headers).await {
        Ok(snapshot) => document(&render::overview(&snapshot)),
        Err(error) => {
            let error = error.into_response();
            if error.status() == StatusCode::UNAUTHORIZED {
                return (
                    [("cache-control", "no-store")],
                    Redirect::to("/accounts/sign-in"),
                )
                    .into_response();
            }
            let mut response = document(include_str!("dashboard/error.html"));
            *response.status_mut() = error.status();
            // Keep the already-recorded failure marker for the HTTP observer.
            response
                .extensions_mut()
                .extend(error.into_parts().0.extensions);
            response
        }
    }
}
async fn data(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let snapshot = snapshot(&broker, &headers).await?;
    if headers.get("accept").is_some_and(|v| v == "text/html") {
        return Ok(document(&render::overview(&snapshot)));
    }
    Ok((
        [
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
        ],
        Json(snapshot),
    )
        .into_response())
}
async fn snapshot(broker: &Broker, headers: &HeaderMap) -> Result<Snapshot, HttpError> {
    let identity = enrollment::browser_user(broker, headers)?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    let user = identity.id;
    let catalog =
        managed::account_catalog(broker, &user, super::catalog::Freshness::RefreshAhead).await?;
    let sampled_at = std::time::Instant::now();
    let tasks = catalog
        .into_iter()
        .map(|account| account_snapshot(broker, &user, account));
    let mut accounts: Vec<_> = futures::future::try_join_all(tasks).await?;
    for account in &mut accounts {
        if let Some(age) = account.usage_age_seconds {
            let age = age.saturating_add(sampled_at.elapsed().as_secs());
            account.usage_age_seconds = Some(age);
            if age >= super::catalog::TTL.as_secs() {
                account.usage_stale = true;
                account.banked_resets.stale = true;
            }
        }
    }
    let machines: Vec<_> = vault::devices(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
        .into_iter()
        .filter(|d| d.user == user && d.tenant == "sawmills")
        .map(|d| {
            let last_use = broker.activity.last_use(&d);
            Machine {
                name: d.id,
                status: if d.revoked { "revoked" } else { "registered" }.into(),
                last_seen_at: last_use.as_ref().map(|u| u.seen_at),
                last_used_alias: last_use.map(|u| u.alias),
            }
        })
        .collect();
    // Revalidate after awaited observations, as on the JSON endpoint.
    let identity = enrollment::browser_user(broker, headers)?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    Ok(Snapshot {
        version: 1,
        identity: Identity {
            email: identity.email,
        },
        server_time: chrono::Utc::now().timestamp(),
        accounts,
        machines,
    })
}
async fn account_snapshot(
    broker: &Broker,
    user: &str,
    account: managed::Account,
) -> Result<Account, HttpError> {
    let owners = broker.owners.read().await;
    let (key, (_, owner)) = owners
        .iter()
        .find(|(_, (identity, _))| identity.user == user && identity.alias == account.alias)
        .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "account_unavailable"))?;
    let key = key.clone();
    let owner = owner.clone();
    drop(owners);
    let (pending, revision, access) = {
        let owner = owner.lock().await;
        let pending = relogin::renewal_pending(&owner.state)
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "account_unavailable"))?;
        (
            pending,
            vault::digest(owner.vault.auth.to_string().as_bytes()),
            vault::token(&owner.vault.auth).ok().map(str::to_owned),
        )
    };
    let (usage_count, redeemable) = broker.catalog.reset_counts(&key).await;
    let observed = if account.available && !pending {
        broker
            .reset_reader
            .dashboard_expiry(&key, &revision, access.as_deref(), &account.account_id)
            .await
    } else {
        super::resets::ResetObservation {
            stale: true,
            ..Default::default()
        }
    };
    let count = match (usage_count, observed.count) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let current = owner.lock().await;
    let changed = vault::digest(current.vault.auth.to_string().as_bytes()) != revision;
    let pending = relogin::renewal_pending(&current.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "account_unavailable"))?;
    let routing_refused = current.routing_refused;
    let available = current.available && !routing_refused;
    drop(current);
    let expiry = if changed {
        None
    } else {
        observed.nearest_expiry
    };
    let reset_stale = observed.stale || changed || pending || !available;
    Ok(Account {
        alias: account.alias,
        label: account.label,
        plan: account.plan,
        state: if pending {
            "renewal_pending"
        } else if available {
            "available"
        } else {
            "unavailable"
        }
        .into(),
        routing_refused,
        billing_class: account.billing_class,
        primary: window(
            account.primary_used,
            account.primary_window_seconds,
            account.primary_resets_at,
        ),
        secondary: window(
            account.secondary_used,
            account.secondary_window_seconds,
            account.resets_at,
        ),
        usage_age_seconds: account.usage_age_seconds,
        usage_stale: account.usage_stale || changed,
        usage_error: account.usage_error,
        banked_resets: Resets {
            count,
            redeemable_now: redeemable,
            nearest_expiry: expiry,
            stale: reset_stale || account.usage_stale,
        },
    })
}
fn window(used: Option<f64>, seconds: Option<u64>, reset: Option<i64>) -> Window {
    let used = used.filter(|n| n.is_finite() && *n >= 0.0);
    Window {
        used_percent: used,
        left_percent: used.map(|n| (100.0 - n).max(0.0)),
        window_seconds: seconds,
        resets_at: reset,
    }
}

pub(super) fn routes(public_url: &str) -> Router<Broker> {
    // This is operator configuration validated at startup, never a request header.
    // Quote for the shell, then escape for the HTML text node.
    let server = format!(
        "'{}'",
        public_url.trim_end_matches('/').replace('\'', "'\\''")
    );
    let content = include_str!("dashboard/landing.html")
        .replace("<!-- SERVER -->", &enrollment::escape(&server));
    Router::new()
        .route(
            "/",
            get(move |state, headers| landing(state, headers, content.clone())),
        )
        .route(
            "/assets/fonts/instrument-sans.woff2",
            get(|| async {
                font(include_bytes!(
                    "../../design/v3/fonts/instrument-sans.woff2"
                ))
            }),
        )
        .route(
            "/assets/fonts/jetbrains-mono.woff2",
            get(|| async { font(include_bytes!("../../design/v3/fonts/jetbrains-mono.woff2")) }),
        )
        .route("/accounts", get(page))
        .route("/accounts/data", get(data))
        .route("/accounts/sign-in", get(enrollment::accounts_sign_in))
        .route("/accounts/sign-out", post(enrollment::accounts_sign_out))
}

async fn landing(
    State(broker): State<Broker>,
    headers: HeaderMap,
    content: String,
) -> Result<Response, HttpError> {
    // Session lookup is optional on the public page. Only an enabled identity redirects.
    if matches!(enrollment::browser_user(&broker, &headers), Ok(Some(_))) {
        return Ok(([("cache-control", "no-store")], Redirect::to("/accounts")).into_response());
    }
    let ready = managed::readiness(&broker).is_success();
    let content = content
        .replace("<!-- READY_STATE -->", if ready { "ok" } else { "bad" })
        .replace(
            "<!-- READY_LABEL -->",
            if ready {
                "Account server ready"
            } else {
                "Account server not ready"
            },
        );
    Ok(document(&content))
}
fn document(content: &str) -> Response {
    let styles = include_str!("dashboard/style.css");
    let script = concat!(
        include_str!("dashboard/copy.js"),
        "\n",
        include_str!("dashboard/script.js")
    );
    let style_hash = STANDARD.encode(Sha256::digest(styles.as_bytes()));
    let script_hash = STANDARD.encode(Sha256::digest(script.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{style_hash}'; script-src 'sha256-{script_hash}'; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
    );
    let refresh = if content.contains("data-has-accounts") {
        r#"<noscript><meta http-equiv="refresh" content="60"></noscript>"#
    } else {
        ""
    };
    let html = include_str!("dashboard/page.html")
        .replace("<!-- ACCOUNT_REFRESH -->", refresh)
        .replace("<!-- STYLES -->", &format!("<style>{styles}</style>"))
        .replace("<!-- CONTENT -->", content)
        .replace("<!-- SCRIPT -->", &format!("<script>{script}</script>"))
        .replace("<!-- VERSION -->", env!("CARGO_PKG_VERSION"));
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "same-origin"),
            ("x-content-type-options", "nosniff"),
            ("content-security-policy", policy.as_str()),
        ],
        Html(html),
    )
        .into_response()
}

fn font(bytes: &'static [u8]) -> Response {
    (
        [
            ("content-type", "font/woff2"),
            ("cache-control", "public, max-age=86400"),
            ("x-content-type-options", "nosniff"),
        ],
        bytes,
    )
        .into_response()
}
