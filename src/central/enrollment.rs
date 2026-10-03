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
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    core::{CoreAuthenticationFlow, CoreProviderMetadata},
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

// Keep provider extension claims inside the library's signature/issuer/nonce verification.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompanyClaims {
    #[serde(default)]
    hd: Option<String>,
}
impl openidconnect::AdditionalClaims for CompanyClaims {}
type CompanyTokenResponse = openidconnect::StandardTokenResponse<
    openidconnect::IdTokenFields<
        CompanyClaims,
        openidconnect::EmptyExtraTokenFields,
        openidconnect::core::CoreGenderClaim,
        openidconnect::core::CoreJweContentEncryptionAlgorithm,
        openidconnect::core::CoreJwsSigningAlgorithm,
    >,
    openidconnect::core::CoreTokenType,
>;
type CompanyClient = openidconnect::Client<
    CompanyClaims,
    openidconnect::core::CoreAuthDisplay,
    openidconnect::core::CoreGenderClaim,
    openidconnect::core::CoreJweContentEncryptionAlgorithm,
    openidconnect::core::CoreJsonWebKey,
    openidconnect::core::CoreAuthPrompt,
    openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    CompanyTokenResponse,
    openidconnect::core::CoreTokenIntrospectionResponse,
    openidconnect::core::CoreRevocableToken,
    openidconnect::core::CoreRevocationErrorResponse,
    openidconnect::EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointMaybeSet,
    openidconnect::EndpointMaybeSet,
>;
const GOOGLE_ISSUER: &str = "https://accounts.google.com";

mod identity;

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
    #[serde(default)]
    allowed_hosted_domains: Option<Vec<String>>,
    #[serde(default)]
    clerk_migration: Option<ClerkMigration>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClerkMigration {
    issuer: String,
    users: Vec<ClerkLink>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClerkLink {
    subject: String,
    email: String,
}
impl Configuration {
    fn hosted_domains(&self) -> Option<&[String]> {
        self.allowed_hosted_domains
            .as_deref()
            .or_else(|| (self.issuer == GOOGLE_ISSUER).then_some(self.allowed_domains.as_slice()))
    }
}
struct Pending {
    name: String,
    user_code: String,
    created: Instant,
    last_poll: Option<Instant>,
    grant: Option<String>,
}
enum Destination {
    Enrollment(String),
    Accounts(String),
}
struct Login {
    destination: Destination,
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
struct Session {
    user: String,
    created: Instant,
}
const SESSION_TTL: Duration = Duration::from_secs(3600);
#[derive(Default)]
struct Flows {
    devices: BTreeMap<String, Pending>,
    logins: BTreeMap<String, Login>,
    approvals: BTreeMap<String, Approval>,
    sessions: BTreeMap<String, Session>,
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
        if config.hosted_domains().is_some_and(|domains| {
            domains.is_empty() || domains.iter().any(|d| d.is_empty() || d.contains('@'))
        }) {
            bail!("SSO requires nonempty allowed hosted domains");
        }
        if let Some(migration) = &config.clerk_migration {
            if config.hosted_domains().is_none()
                || reqwest::Url::parse(&migration.issuer)?.scheme() != "https"
                || migration.issuer == config.issuer
                || migration.users.is_empty()
            {
                bail!("Clerk migration requires a distinct HTTPS source issuer and hosted domains");
            }
            let mut subjects = std::collections::BTreeSet::new();
            let mut emails = std::collections::BTreeSet::new();
            for user in &migration.users {
                if user.subject.is_empty()
                    || !subjects.insert(&user.subject)
                    || !emails.insert(user.email.to_ascii_lowercase())
                    || !user.email.rsplit_once('@').is_some_and(|(local, domain)| {
                        !local.is_empty()
                            && config
                                .allowed_domains
                                .iter()
                                .any(|d| d.eq_ignore_ascii_case(domain))
                    })
                {
                    bail!("Clerk migration requires unique subjects and company emails");
                }
            }
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
        flows
            .sessions
            .retain(|_, s| s.created.elapsed() < SESSION_TTL);
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
pub(super) fn escape(s: &str) -> String {
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
    begin_login(&broker, Destination::Enrollment(device_hash)).await
}
async fn begin_login(broker: &Broker, destination: Destination) -> Result<Response, HttpError> {
    let sso = sso(broker)?;
    let metadata = sso
        .metadata()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?;
    let client = CompanyClient::from_provider_metadata(
        metadata,
        ClientId::new(sso.config.client_id.clone()),
        Some(ClientSecret::new(sso.client_secret.trim().into())),
    )
    .set_redirect_uri(
        RedirectUrl::new(format!("{}/auth/callback", sso.public_url))
            .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?,
    );
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut authorization = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge);
    if let Some(domains) = sso.config.hosted_domains() {
        // This affects Google's account chooser only. The signed claim is enforced below.
        let hint = if domains.len() == 1 {
            domains[0].as_str()
        } else {
            "*"
        };
        authorization = authorization.add_extra_param("hd", hint);
    }
    let (url, state, nonce) = authorization.url();
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    if flows.logins.len() >= 1024 {
        return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "enrollment_capacity"));
    }
    flows.logins.insert(
        vault::digest(state.secret().as_bytes()),
        Login {
            destination,
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
    headers: HeaderMap,
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
    if let Destination::Accounts(expected) = &login.destination
        && cookie(&headers, sso.cookie_name("login"))
            .map(|v| vault::digest(v.as_bytes()))
            .as_ref()
            != Some(expected)
    {
        return Err(broker.error(StatusCode::UNAUTHORIZED, "invalid_browser_login"));
    }
    let code = input
        .code
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "sso_denied"))?;
    let metadata = sso
        .metadata()
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "sso_unavailable"))?;
    let client = CompanyClient::from_provider_metadata(
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
    if let Some(domains) = sso.config.hosted_domains()
        && !claims
            .additional_claims()
            .hd
            .as_deref()
            .is_some_and(|hd| domains.iter().any(|domain| domain.eq_ignore_ascii_case(hd)))
    {
        return Err(broker.error(StatusCode::FORBIDDEN, "company_identity_required"));
    }
    let user = match identity::resolve(&broker.state, &sso.config, claims.subject().as_str(), email)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
    {
        identity::Resolution::User(user) => user,
        identity::Resolution::Disabled => {
            return Err(broker.error(StatusCode::FORBIDDEN, "user_disabled"));
        }
        identity::Resolution::Conflict => {
            return Err(broker.error(StatusCode::FORBIDDEN, "identity_link_refused"));
        }
    };
    let device_hash = match login.destination {
        Destination::Enrollment(hash) => hash,
        Destination::Accounts(_) => {
            match managed::record_user(&broker.state, &user, email).map_err(|_| {
                broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
            })? {
                managed::UserEnrollment::Recorded => {}
                managed::UserEnrollment::Disabled => {
                    return Err(broker.error(StatusCode::FORBIDDEN, "user_disabled"));
                }
            }
            let token = secret();
            let mut flows = sso.flows.lock().expect("enrollment lock");
            Sso::cleanup(&mut flows);
            if flows.sessions.len() >= 1024 {
                return Err(broker.error(StatusCode::TOO_MANY_REQUESTS, "session_capacity"));
            }
            flows.sessions.insert(
                vault::digest(token.as_bytes()),
                Session {
                    user,
                    created: Instant::now(),
                },
            );
            let mut response = (
                [
                    ("cache-control", "no-store"),
                    ("referrer-policy", "no-referrer"),
                ],
                Redirect::to("/accounts"),
            )
                .into_response();
            response.headers_mut().append(
                "set-cookie",
                sso.cookie("session", &token, SESSION_TTL.as_secs())
                    .parse()
                    .expect("generated cookie"),
            );
            response.headers_mut().append(
                "set-cookie",
                sso.cookie("login", "", 0)
                    .parse()
                    .expect("generated cookie"),
            );
            return Ok(response);
        }
    };
    let mut flows = sso.flows.lock().expect("enrollment lock");
    Sso::cleanup(&mut flows);
    let device = flows
        .devices
        .get(&device_hash)
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
            device_hash,
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

impl Sso {
    fn cookie_name(&self, kind: &str) -> &'static str {
        match (self.public_url.starts_with("https:"), kind) {
            (true, "login") => "__Host-codexctl-login",
            (true, _) => "__Host-codexctl-session",
            (false, "login") => "codexctl-login",
            (false, _) => "codexctl-session",
        }
    }
    fn cookie(&self, kind: &str, value: &str, age: u64) -> String {
        let secure = if self.public_url.starts_with("https:") {
            "; Secure"
        } else {
            ""
        };
        format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={age}{secure}",
            self.cookie_name(kind)
        )
    }
}
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}
pub(super) fn browser_user(
    broker: &Broker,
    headers: &HeaderMap,
) -> Result<Option<managed::User>, HttpError> {
    let Some(sso) = broker.sso.as_deref() else {
        return Ok(None);
    };
    let Some(token) = cookie(headers, sso.cookie_name("session")) else {
        return Ok(None);
    };
    let user = {
        let mut flows = sso.flows.lock().expect("enrollment lock");
        Sso::cleanup(&mut flows);
        flows
            .sessions
            .get(&vault::digest(token.as_bytes()))
            .map(|s| s.user.clone())
    };
    let Some(user) = user else {
        return Ok(None);
    };
    let identity = managed::users(&broker.state)
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"))?
        .into_iter()
        .find(|u| u.id == user && u.enabled)
        .ok_or_else(|| broker.error(StatusCode::FORBIDDEN, "user_disabled"))?;
    Ok(Some(identity))
}

pub(super) async fn accounts_sign_out(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    // A link-styled form submits same-origin POST; cross-site forms cannot end a session.
    if headers.get("origin").and_then(|h| h.to_str().ok()) != Some(sso.public_url.as_str()) {
        return Err(broker.error(StatusCode::FORBIDDEN, "invalid_browser_origin"));
    }
    if let Some(token) = cookie(&headers, sso.cookie_name("session")) {
        sso.flows
            .lock()
            .expect("enrollment lock")
            .sessions
            .remove(&vault::digest(token.as_bytes()));
    }
    let mut response = (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
        ],
        Redirect::to("/"),
    )
        .into_response();
    response.headers_mut().insert(
        "set-cookie",
        sso.cookie("session", "", 0)
            .parse()
            .expect("generated cookie"),
    );
    Ok(response)
}

pub(super) async fn accounts_sign_in(State(broker): State<Broker>) -> Result<Response, HttpError> {
    let sso = sso(&broker)?;
    let binding = secret();
    let mut response = begin_login(
        &broker,
        Destination::Accounts(vault::digest(binding.as_bytes())),
    )
    .await?;
    response.headers_mut().insert(
        "set-cookie",
        sso.cookie("login", &binding, TTL.as_secs())
            .parse()
            .expect("generated cookie"),
    );
    Ok(response)
}

#[cfg(test)]
impl Sso {
    pub(super) fn testing_session(user: &str) -> Self {
        Self {
            config: Configuration {
                issuer: "http://127.0.0.1".into(),
                client_id: "test".into(),
                client_secret_file: "/unused".into(),
                allowed_domains: vec!["example.invalid".into()],
                allowed_hosted_domains: None,
                clerk_migration: None,
            },
            public_url: "http://127.0.0.1".into(),
            http: reqwest::Client::new(),
            client_secret: "synthetic".into(),
            flows: Mutex::new(Flows {
                sessions: BTreeMap::from([(
                    vault::digest(b"synthetic-session"),
                    Session {
                        user: user.into(),
                        created: Instant::now(),
                    },
                )]),
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn google_requires_hosted_domains_by_default_and_allows_an_explicit_list() {
        let mut config: Configuration = serde_json::from_value(json!({
            "issuer": GOOGLE_ISSUER, "client_id": "test", "client_secret_file": "/unused",
            "allowed_domains": ["sawmills.ai"]
        }))
        .unwrap();
        assert_eq!(config.hosted_domains().unwrap(), ["sawmills.ai"]);
        config.allowed_hosted_domains = Some(vec!["workspace.example".into()]);
        assert_eq!(config.hosted_domains().unwrap(), ["workspace.example"]);
        config.allowed_hosted_domains = None;
        config.issuer = "https://clerk.example".into();
        assert!(config.hosted_domains().is_none());
    }
}
