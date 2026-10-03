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
use serde_json::json;
use sha2::{Digest, Sha256};

async fn page(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    if enrollment::browser_user(&broker, &headers)?.is_none() {
        return Ok((
            [("cache-control", "no-store")],
            Redirect::to("/accounts/sign-in"),
        )
            .into_response());
    }
    Ok(document(include_str!("dashboard/accounts.html")))
}
async fn data(State(broker): State<Broker>, headers: HeaderMap) -> Result<Response, HttpError> {
    let identity = enrollment::browser_user(&broker, &headers)?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    let user = identity.id;
    let catalog = managed::account_catalog(&broker, &user).await?;
    let sampled_at = std::time::Instant::now();
    let tasks = catalog
        .into_iter()
        .map(|account| account_snapshot(&broker, &user, account));
    let mut accounts: Vec<_> = futures::future::try_join_all(tasks).await?;
    for account in &mut accounts {
        if let Some(age) = account["usage_age_seconds"].as_u64() {
            let age = age.saturating_add(sampled_at.elapsed().as_secs());
            account["usage_age_seconds"] = json!(age);
            if age >= super::catalog::TTL.as_secs() {
                account["usage_stale"] = json!(true);
                account["banked_resets"]["stale"] = json!(true);
            }
        }
    }
    let machines: Vec<_> = vault::devices(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
        .into_iter().filter(|d| d.user == user && d.tenant == "sawmills")
        .map(|d| {
            let last_use = broker.activity.last_use(&d);
            let mut machine = json!({"name":d.id,"status":if d.revoked {"revoked"} else {"registered"},"last_seen_at":last_use.as_ref().map(|u| u.seen_at)});
            if let Some(last_use) = last_use { machine["last_used_alias"] = json!(last_use.alias); }
            machine
        }).collect();
    let identity = enrollment::browser_user(&broker, &headers)?
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    Ok((
        [
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
        ],
        Json(json!({"version":1,"identity":{"email":identity.email},"server_time":chrono::Utc::now().timestamp(),"accounts":accounts,"machines":machines})),
    )
        .into_response())
}
async fn account_snapshot(
    broker: &Broker,
    user: &str,
    account: managed::Account,
) -> Result<serde_json::Value, HttpError> {
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
    let available = current.available && !current.routing_refused;
    drop(current);
    let expiry = if changed {
        None
    } else {
        observed.nearest_expiry
    };
    let reset_stale = observed.stale || changed || pending || !available;
    Ok::<_, HttpError>(json!({
        "alias": account.alias, "label": account.label, "plan": account.plan,
        "state": if pending { "renewal_pending" } else if available { "available" } else { "unavailable" },
        "billing_class": account.billing_class,
        "primary": window(account.primary_used, account.primary_window_seconds, account.primary_resets_at),
        "secondary": window(account.secondary_used, account.secondary_window_seconds, account.resets_at),
        "usage_age_seconds": account.usage_age_seconds, "usage_stale": account.usage_stale || changed, "usage_error": account.usage_error,
        "banked_resets": {"count": count, "redeemable_now": redeemable, "nearest_expiry": expiry, "stale": reset_stale || account.usage_stale}
    }))
}

fn window(used: Option<f64>, seconds: Option<u64>, reset: Option<i64>) -> serde_json::Value {
    let used = used.filter(|n| n.is_finite() && *n >= 0.0);
    json!({"used_percent":used,"left_percent":used.map(|n| (100.0-n).max(0.0)),"window_seconds":seconds,"resets_at":reset})
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
    Ok(document(&content))
}
fn document(content: &str) -> Response {
    let styles = include_str!("dashboard/style.css");
    let script = include_str!("dashboard/script.js");
    let style_hash = STANDARD.encode(Sha256::digest(styles.as_bytes()));
    let script_hash = STANDARD.encode(Sha256::digest(script.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{style_hash}'; script-src 'sha256-{script_hash}'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
    );
    let html = include_str!("dashboard/page.html")
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
