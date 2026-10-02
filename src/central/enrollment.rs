//! Company OIDC sign-in around an application device-enrollment flow.
use super::{
    managed::{self, Broker, HttpError},
    transport, vault,
};
use aes_gcm::aead::{OsRng, rand_core::RngCore};
use anyhow::{Result, bail};
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

pub fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
fn secret() -> String {
    vault::digest(&random_bytes())
}
const TTL: Duration = Duration::from_secs(300);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    issuer: String,
    client_id: String,
    client_secret_file: std::path::PathBuf,
    allowed_domains: Vec<String>,
}
struct Pending {
    name: String,
    user_code: String,
    created: Instant,
    last_poll: Option<Instant>,
    grant: Option<String>,
}
struct Login {
    device_hash: String,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    created: Instant,
}
struct Approval {
    device_hash: String,
    user: String,
    email: String,
    created: Instant,
}
#[derive(Default)]
struct Flows {
    devices: BTreeMap<String, Pending>,
    logins: BTreeMap<String, Login>,
    approvals: BTreeMap<String, Approval>,
}
pub(super) struct Sso {
    config: Configuration,
    public_url: String,
    http: reqwest::Client,
    flows: Mutex<Flows>,
    client_secret: String,
}
impl Sso {
    pub async fn load(path: &Path, public_url: &str) -> Result<Self> {
        let config: Configuration = serde_json::from_slice(&std::fs::read(path)?)?;
        if config.allowed_domains.is_empty()
            || config
                .allowed_domains
                .iter()
                .any(|d| d.is_empty() || d.contains('@'))
        {
            bail!("SSO requires allowed company email domains");
        }
        let issuer = reqwest::Url::parse(&config.issuer)?;
        if issuer.scheme() != "https"
            && !(issuer.scheme() == "http"
                && issuer.host_str() == Some("127.0.0.1")
                && reqwest::Url::parse(public_url)?.scheme() == "http")
        {
            bail!("SSO issuer must use HTTPS");
        }
        transport::origin(public_url)?;
        let client_secret = String::from_utf8(vault::private_read(&config.client_secret_file)?)?;
        if client_secret.trim().is_empty() {
            bail!("SSO client secret is empty");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        let result = Self {
            config,
            public_url: public_url.trim_end_matches('/').into(),
            http,
            flows: Mutex::new(Flows::default()),
            client_secret,
        };
        result.metadata().await?;
        Ok(result)
    }
    async fn metadata(&self) -> Result<CoreProviderMetadata> {
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(self.config.issuer.clone())?,
            &self.http,
        )
        .await
        .map_err(|_| anyhow::anyhow!("company SSO discovery failed"))?;
        let insecure = metadata.authorization_endpoint().url().scheme() != "https"
            || metadata
                .token_endpoint()
                .is_none_or(|u| u.url().scheme() != "https")
            || metadata.jwks_uri().url().scheme() != "https";
        if insecure && reqwest::Url::parse(&self.public_url)?.scheme() != "http" {
            bail!("company SSO endpoints require HTTPS");
        }
        Ok(metadata)
    }
    fn cleanup(flows: &mut Flows) {
        flows.devices.retain(|_, d| d.created.elapsed() < TTL);
        flows.logins.retain(|_, d| d.created.elapsed() < TTL);
        flows.approvals.retain(|_, d| d.created.elapsed() < TTL);
    }
}
fn sso(broker: &Broker) -> Result<&Sso, HttpError> {
    broker
        .sso
        .as_deref()
        .ok_or_else(|| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))
}
fn page(html: String) -> Response {
    let styles = include_str!("enrollment/style.css");
    let style_hash = STANDARD.encode(Sha256::digest(styles.as_bytes()));
    let policy = format!(
        "default-src 'none'; style-src 'sha256-{style_hash}'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
    );
    let document = include_str!("enrollment/page.html")
        .replace("<!-- STYLES -->", &format!("<style>{styles}</style>"))
        .replace("<!-- CONTENT -->", &html);
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
            ("content-security-policy", policy.as_str()),
            ("x-content-type-options", "nosniff"),
        ],
        Html(document),
    )
        .into_response()
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub name: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Poll {
    pub device_code: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub device_token: String,
}
async fn start(
    State(broker): State<Broker>,
    Json(input): Json<Start>,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    if input.name.is_empty() || input.name.len() > 80 || input.name.chars().any(char::is_control) {
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_device_name"));
    }
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    if flows.devices.len() >= 1024 {
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "enrollment_capacity"));
    }
    let device_code = secret();
    let user_code = secret()[..16].to_uppercase();
    let verification_url = format!("{}/enroll?code={user_code}", sso.public_url);
    flows.devices.insert(
        vault::digest(device_code.as_bytes()),
        Pending {
            name: input.name,
            user_code: user_code.clone(),
            created: Instant::now(),
            last_poll: None,
            grant: None,
        },
    );
    Ok((
        [("cache-control", "no-store")],
        Json(Challenge {
            device_code,
            user_code,
            verification_url,
            expires_in: 300,
            interval: 3,
        }),
    )
        .into_response())
}
async fn poll(
    State(broker): State<Broker>,
    Json(input): Json<Poll>,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    let hash = vault::digest(input.device_code.as_bytes());
    let device = flows
        .devices
        .get_mut(&hash)
        .ok_or_else(|| broker.error(StatusCode::GONE, "enrollment_expired"))?;
    if device
        .last_poll
        .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    {
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "slow_down"));
    }
    device.last_poll = Some(Instant::now());
    let Some(token) = device.grant.take() else {
        return Ok((StatusCode::ACCEPTED, Json(json!({"status":"pending"}))).into_response());
    };
    flows.devices.remove(&hash);
    Ok((
        [("cache-control", "no-store")],
        Json(Grant {
            device_token: token,
        }),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Verify {
    code: String,
}
async fn verify(
    State(broker): State<Broker>,
    Query(input): Query<Verify>,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    let device_hash = {
        let mut flows = sso.flows.lock().expect("enrollment lock");
        Sso::cleanup(&mut flows);
        flows
            .devices
            .iter()
            .find(|(_, d)| d.user_code == input.code && d.grant.is_none())
            .map(|(k, _)| k.clone())
            .ok_or_else(|| broker.error(StatusCode::GONE, "enrollment_expired"))?
    };
    let metadata = sso
        .metadata()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(sso.config.client_id.clone()),
        Some(ClientSecret::new(sso.client_secret.trim().into())),
    )
    .set_redirect_uri(
        RedirectUrl::new(format!("{}/auth/callback", sso.public_url))
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?,
    );
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    if flows.logins.len() >= 1024 {
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "enrollment_capacity"));
    }
    flows.logins.insert(
        vault::digest(state.secret().as_bytes()),
        Login {
            device_hash,
            nonce,
            verifier,
            created: Instant::now(),
        },
    );
    Ok((
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
        ],
        Redirect::to(url.as_str()),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
}
async fn callback(
    State(broker): State<Broker>,
    Query(input): Query<Callback>,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    let login = {
        let mut flows = sso.flows.lock().expect("enrollment lock");
        Sso::cleanup(&mut flows);
        flows
            .logins
            .remove(&vault::digest(input.state.as_bytes()))
            .ok_or_else(|| broker.error(StatusCode::BAD_REQUEST, "invalid_sso_state"))?
    };
    let code = input
        .code
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
    let metadata = sso
        .metadata()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(sso.config.client_id.clone()),
        Some(ClientSecret::new(sso.client_secret.trim().into())),
    )
    .set_redirect_uri(
        RedirectUrl::new(format!("{}/auth/callback", sso.public_url))
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?,
    );
    let tokens = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?
        .set_pkce_verifier(login.verifier)
        .request_async(&sso.http)
        .await
        .map_err(|_| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
    let verifier = client.id_token_verifier();
    let id = tokens
        .extra_fields()
        .id_token()
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
    let claims = id
        .claims(&verifier, &login.nonce)
        .map_err(|_| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            tokens.access_token(),
            id.signing_alg()
                .map_err(|_| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?,
            id.signing_key(&verifier)
                .map_err(|_| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?,
        )
        .map_err(|_| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
        if actual != *expected {
            return Err(broker.error(StatusCode::UNAUTHORIZED, "sso_denied"));
        }
    }
    let email = claims
        .email()
        .filter(|_| claims.email_verified() == Some(true))
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "company_identity_required"))?
        .as_str();
    if !email.rsplit_once('@').is_some_and(|(_, domain)| {
        sso.config
            .allowed_domains
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain))
    }) {
        return Err(broker.error(StatusCode::FORBIDDEN, "company_identity_required"));
    }
    let user =
        vault::digest(format!("{}\0{}", sso.config.issuer, claims.subject().as_str()).as_bytes());
    if managed::users(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
        .iter()
        .any(|u| u.id == user && !u.enabled)
    {
        return Err(broker.error(StatusCode::FORBIDDEN, "user_disabled"));
    }
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    let device = flows
        .devices
        .get(&login.device_hash)
        .ok_or_else(|| broker.error(StatusCode::GONE, "enrollment_expired"))?;
    let name = escape(&device.name);
    let code = escape(&device.user_code);
    let approval = secret();
    let html = format!(
        include_str!("enrollment/approval.html"),
        email = escape(email),
        name = name,
        code = code,
        approval = approval,
    );
    if flows.approvals.len() >= 1024 {
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "enrollment_capacity"));
    }
    flows.approvals.insert(
        vault::digest(approval.as_bytes()),
        Approval {
            device_hash: login.device_hash,
            user,
            email: email.into(),
            created: Instant::now(),
        },
    );
    Ok(page(html))
}
#[derive(Deserialize)]
struct Approve {
    approval: String,
}
async fn approve(
    State(broker): State<Broker>,
    Form(input): Form<Approve>,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    let approval = flows
        .approvals
        .remove(&vault::digest(input.approval.as_bytes()))
        .ok_or_else(|| broker.error(StatusCode::BAD_REQUEST, "invalid_approval"))?;
    let device = flows
        .devices
        .get_mut(&approval.device_hash)
        .filter(|d| d.grant.is_none())
        .ok_or_else(|| broker.error(StatusCode::GONE, "enrollment_expired"))?;
    match managed::record_user(&broker.state, &approval.user, &approval.email)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
    {
        managed::UserEnrollment::Recorded => {}
        managed::UserEnrollment::Disabled => {
            return Err(broker.error(StatusCode::FORBIDDEN, "user_unavailable"));
        }
    }
    let token = secret();
    let _lock = vault::registry_lock(&broker.state, "devices.lock")
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_busy"))?;
    let mut devices = vault::devices(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?;
    devices.push(vault::Device {
        id: format!("{}-{}", device.name, &secret()[..12]),
        tenant: "sawmills".into(),
        user: approval.user,
        token_hash: vault::digest(token.as_bytes()),
        revoked: false,
    });
    vault::save_devices(&broker.state, &devices)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?;
    device.grant = Some(token);
    Ok(page(include_str!("enrollment/connected.html").into()))
}
pub(super) fn routes(router: Router<Broker>) -> Router<Broker> {
    router
        .route("/v1/enrollment/start", post(start))
        .route("/v1/enrollment/poll", post(poll))
        .route("/enroll", get(verify))
        .route("/auth/callback", get(callback))
        .route("/auth/approve", post(approve))
}
