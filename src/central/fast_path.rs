//! Valid-token fast path. In PostgreSQL mode a pod can serve a committed
//! credential without the account lease or a native child, when fresh evidence
//! for that exact credential revision proves billing and routing.
use super::server::{TokenRequest, TokenResponse};
use crate::api;
use serde_json::Value;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Keep this much token life, so an OpenAI auth outage still leaves time for
/// the lease path to refresh early.
pub(super) const MIN_REMAINING_SECONDS: i64 = 3600;
/// One freshness bound for routing and billing evidence.
pub(super) const EVIDENCE_MAX_AGE: Duration = Duration::from_secs(60);
/// A rate-limited account at or above this use takes a live read instead.
pub(super) const USAGE_CEILING_PERCENT: f64 = 90.0;

/// Evidence the lease path observed on one credential revision.
#[derive(Clone, Debug)]
pub(super) struct Evidence {
    pub auth_revision: String,
    pub account_revision: i64,
    pub routing_supported: bool,
    pub routing_age: Duration,
    /// Present only when a billing read observed this revision.
    pub billing: Option<Billing>,
}

#[derive(Clone, Debug)]
pub(super) struct Billing {
    pub class: api::BillingClass,
    pub plan_type: Option<String>,
    pub limits: Option<Value>,
    pub age: Duration,
}

/// What the lease path observed, written with the credential it published.
#[derive(Clone, Debug)]
pub(super) struct Observation {
    pub auth_revision: String,
    pub routing_age: Duration,
    pub billing: Option<Billing>,
}

/// The committed record and its evidence, read without a lease.
pub(super) struct FastRead {
    pub user_id: Option<String>,
    pub alias: String,
    pub revision: i64,
    pub vault: Value,
    /// A login journal or identity reservation fences this account.
    pub fenced: bool,
    pub evidence: Option<Evidence>,
    /// Database clock, in Unix seconds.
    pub now: i64,
}

/// The committed credential as PostgreSQL holds it.
pub(super) struct Candidate<'a> {
    pub auth: &'a Value,
    pub account_revision: i64,
    pub label: Option<String>,
    /// Database clock, in Unix seconds.
    pub now: i64,
    pub evidence: Option<&'a Evidence>,
}

/// `CODEXCTL_CENTRAL_TOKEN_FAST_PATH=1` turns the fast path on. Turn it on
/// only after every replica runs layout 8, so all writers publish evidence.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CODEXCTL_CENTRAL_TOKEN_FAST_PATH").is_ok_and(|value| value == "1")
    })
}

const RESULTS: [&str; 11] = [
    "served",
    "disabled",
    "expiring",
    "forced",
    "evidence_missing",
    "evidence_stale",
    "routing",
    "usage_high",
    "fenced",
    "identity",
    "read_failed",
];
static RESULT_COUNTS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

/// Count one fast-path decision by its bounded result label.
pub(super) fn record(result: Result<(), Miss>) {
    let label = match result {
        Ok(()) => "served",
        Err(miss) => miss.label(),
    };
    if let Some(index) = RESULTS.iter().position(|known| *known == label) {
        RESULT_COUNTS[index].fetch_add(1, Ordering::Relaxed);
    }
}

/// How a request that met another holder's lease ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// The holder committed a new revision; the request served it.
    Committed,
    /// The holder released without a new revision; the request took the lease.
    Acquired,
    /// The wait ran out; the request failed with refresh_in_progress.
    Timeout,
    /// After the wait, the lease path refused for another reason.
    Refused,
}
const WAITS: [&str; 4] = ["committed", "acquired", "timeout", "refused"];
static WAIT_COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static WAIT_NANOS: AtomicU64 = AtomicU64::new(0);

impl Wait {
    pub(super) fn record(self, waited: Duration) {
        WAIT_COUNTS[self as usize].fetch_add(1, Ordering::Relaxed);
        WAIT_NANOS.fetch_add(
            waited.as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }
}

/// Fast-path results and lease waits, for `/metrics`.
pub(super) fn metrics() -> String {
    let mut output = String::new();
    for (label, count) in RESULTS.iter().zip(&RESULT_COUNTS) {
        output.push_str(&format!(
            "codexctl_central_token_fast_path_total{{result=\"{label}\"}} {}\n",
            count.load(Ordering::Relaxed)
        ));
    }
    let mut waits = 0;
    for (label, count) in WAITS.iter().zip(&WAIT_COUNTS) {
        let count = count.load(Ordering::Relaxed);
        waits += count;
        output.push_str(&format!(
            "codexctl_central_lease_waits_total{{outcome=\"{label}\"}} {count}\n"
        ));
    }
    output.push_str(&format!(
        "codexctl_central_lease_wait_seconds_sum {}\ncodexctl_central_lease_wait_seconds_count {waits}\n",
        WAIT_NANOS.load(Ordering::Relaxed) as f64 / 1_000_000_000.0
    ));
    output
}

/// A lease loser waits this long for the holder.
pub(super) const LEASE_WAIT: Duration = Duration::from_secs(15);

/// What a lease loser saw while it waited, holding no lock or permit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    Committed,
    Released,
    Timeout,
}

/// Poll the account revision with jittered backoff until the holder commits,
/// releases its lease, or the deadline passes.
pub(super) async fn wait_for_holder(
    central: &super::storage::CentralStore,
    account_id: &str,
    holder_id: &str,
    revision: i64,
    deadline: tokio::time::Instant,
) -> Seen {
    let mut backoff = Duration::from_millis(50);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Seen::Timeout;
        }
        tokio::time::sleep(jittered(backoff).min(deadline - now)).await;
        backoff = (backoff * 2).min(Duration::from_secs(1));
        if central
            .account_revision(account_id)
            .await
            .is_ok_and(|current| current.is_some_and(|current| current != revision))
        {
            return Seen::Committed;
        }
        if !central
            .lease_held_elsewhere(account_id, holder_id)
            .await
            .unwrap_or(true)
        {
            return Seen::Released;
        }
    }
}

/// Half to one and a half times `base`, so losers do not poll in step.
fn jittered(base: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let base = base.as_millis().max(1) as u64;
    Duration::from_millis(base / 2 + random % base)
}

/// Why a request took the lease path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Miss {
    Disabled,
    Expiring,
    Forced,
    EvidenceMissing,
    EvidenceStale,
    Routing,
    UsageHigh,
    Fenced,
    Identity,
    ReadFailed,
}

impl Miss {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Expiring => "expiring",
            Self::Forced => "forced",
            Self::EvidenceMissing => "evidence_missing",
            Self::EvidenceStale => "evidence_stale",
            Self::Routing => "routing",
            Self::UsageHigh => "usage_high",
            Self::Fenced => "fenced",
            Self::Identity => "identity",
            Self::ReadFailed => "read_failed",
        }
    }
}

/// Serve the committed credential, or name why the lease path must run.
pub(super) fn decide(
    candidate: &Candidate<'_>,
    request: &TokenRequest,
) -> Result<TokenResponse, Miss> {
    let auth = candidate.auth;
    let access_token = super::vault::token(auth).map_err(|_| Miss::Identity)?;
    let expires = api::token_expiry(access_token).ok_or(Miss::Expiring)?;
    if expires - candidate.now < MIN_REMAINING_SECONDS {
        return Err(Miss::Expiring);
    }
    let revision = super::vault::digest(&serde_json::to_vec(auth).map_err(|_| Miss::Identity)?);
    // Equal means the client saw this exact token rejected.
    if request.previous_revision.as_deref() == Some(revision.as_str()) {
        return Err(Miss::Forced);
    }
    let evidence = candidate.evidence.ok_or(Miss::EvidenceMissing)?;
    if evidence.auth_revision != revision || evidence.account_revision != candidate.account_revision
    {
        return Err(Miss::EvidenceStale);
    }
    if evidence.routing_age > EVIDENCE_MAX_AGE {
        return Err(Miss::EvidenceStale);
    }
    if !evidence.routing_supported {
        return Err(Miss::Routing);
    }
    let mut token = TokenResponse {
        user_id: None,
        chatgpt_account_id: super::vault::account(auth).map_err(|_| Miss::Identity)?,
        chatgpt_plan_type: api::token_identity(access_token).and_then(|identity| identity.plan),
        revision,
        access_token: access_token.to_owned(),
        billing_class: None,
        native_routing_supported: true,
        statusline_usage: None,
        label: candidate.label.clone(),
    };
    if request.billing {
        let billing = evidence.billing.as_ref().ok_or(Miss::EvidenceMissing)?;
        if billing.age > EVIDENCE_MAX_AGE {
            return Err(Miss::EvidenceStale);
        }
        if billing.class == api::BillingClass::RateLimited
            && !billing.limits.as_ref().is_some_and(below_ceiling)
        {
            return Err(Miss::UsageHigh);
        }
        token.billing_class = Some(billing.class);
        token.chatgpt_plan_type = billing.plan_type.clone();
        token.statusline_usage = billing
            .limits
            .as_ref()
            .and_then(|limits| super::server::usage(limits).ok())
            .map(|usage| {
                let mut usage = crate::statusline::Usage::from_usage(&usage);
                usage.age_seconds = billing.age.as_secs();
                usage
            });
    }
    Ok(token)
}

/// Every reported window is below the ceiling. A missing or malformed window
/// is not proof of headroom.
fn below_ceiling(limits: &Value) -> bool {
    let windows: Vec<_> = ["primary", "secondary"]
        .into_iter()
        .map(|name| &limits["rateLimits"][name])
        .filter(|window| !window.is_null())
        .collect();
    !windows.is_empty()
        && windows.iter().all(|window| {
            window["usedPercent"].as_f64().is_some_and(|used| {
                used.is_finite() && (0.0..USAGE_CEILING_PERCENT).contains(&used)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    const NOW: i64 = 2_000_000_000;

    // TokenResponse carries an access token, so it has no Debug.
    fn miss(result: Result<TokenResponse, Miss>) -> Miss {
        match result {
            Err(miss) => miss,
            Ok(_) => panic!("served from the fast path"),
        }
    }

    fn served(result: Result<TokenResponse, Miss>) -> TokenResponse {
        match result {
            Ok(token) => token,
            Err(miss) => panic!("lease path: {miss:?}"),
        }
    }

    fn auth(exp: i64) -> Value {
        let claims = json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":"seat-workspace","chatgpt_plan_type":"pro"}});
        json!({"tokens":{
            "access_token":format!("header.{}.", URL_SAFE_NO_PAD.encode(claims.to_string())),
            "refresh_token":"synthetic-refresh","account_id":"seat-workspace"}})
    }

    fn revision(auth: &Value) -> String {
        super::super::vault::digest(&serde_json::to_vec(auth).unwrap())
    }

    fn limits(primary: f64) -> Value {
        json!({"rateLimits":{"planType":"plus","primary":{"usedPercent":primary,"windowDurationMins":300,"resetsAt":4102444800_u64},"secondary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":4102444800_u64}}})
    }

    fn evidence(auth: &Value, billing: Option<Billing>) -> Evidence {
        Evidence {
            auth_revision: revision(auth),
            account_revision: 7,
            routing_supported: true,
            routing_age: Duration::from_secs(5),
            billing,
        }
    }

    fn billing(class: api::BillingClass, primary: f64) -> Billing {
        Billing {
            class,
            plan_type: Some("plus".into()),
            limits: Some(limits(primary)),
            age: Duration::from_secs(5),
        }
    }

    fn request(billing: bool, previous: Option<String>) -> TokenRequest {
        TokenRequest {
            previous_revision: previous,
            billing,
            ..Default::default()
        }
    }

    fn candidate<'a>(auth: &'a Value, evidence: Option<&'a Evidence>) -> Candidate<'a> {
        Candidate {
            auth,
            account_revision: 7,
            label: Some("Seat".into()),
            now: NOW,
            evidence,
        }
    }

    #[test]
    fn serves_a_valid_token_with_fresh_routing_evidence() {
        let auth = auth(NOW + 7200);
        let evidence = evidence(&auth, None);
        let token = served(decide(
            &candidate(&auth, Some(&evidence)),
            &request(false, None),
        ));
        assert_eq!(token.revision, revision(&auth));
        assert_eq!(token.chatgpt_account_id, "seat-workspace");
        assert_eq!(token.chatgpt_plan_type.as_deref(), Some("pro"));
        assert!(token.native_routing_supported);
        assert_eq!(token.billing_class, None);
        assert!(token.statusline_usage.is_none());
        assert_eq!(token.label.as_deref(), Some("Seat"));
    }

    #[test]
    fn serves_billing_from_fresh_billing_evidence() {
        let auth = auth(NOW + 7200);
        let evidence = evidence(&auth, Some(billing(api::BillingClass::RateLimited, 40.0)));
        let token = served(decide(
            &candidate(&auth, Some(&evidence)),
            &request(true, None),
        ));
        assert_eq!(token.billing_class, Some(api::BillingClass::RateLimited));
        assert_eq!(token.chatgpt_plan_type.as_deref(), Some("plus"));
        let usage = token.statusline_usage.expect("usage from cached limits");
        assert_eq!(usage.age_seconds, 5);
    }

    #[test]
    fn keeps_an_hour_of_token_life() {
        let auth = auth(NOW + MIN_REMAINING_SECONDS - 1);
        let evidence = evidence(&auth, None);
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(false, None)
            )),
            Miss::Expiring
        );
    }

    #[test]
    fn an_extreme_expiry_claim_fails_closed() {
        for exp in [i64::MIN, i64::MIN + 1] {
            let auth = auth(exp);
            let evidence = evidence(&auth, None);
            assert_eq!(
                miss(decide(
                    &candidate(&auth, Some(&evidence)),
                    &request(false, None)
                )),
                Miss::Expiring
            );
        }
    }

    #[test]
    fn a_rejected_current_revision_forces_the_lease_path() {
        let auth = auth(NOW + 7200);
        let evidence = evidence(&auth, None);
        let candidate = candidate(&auth, Some(&evidence));
        assert_eq!(
            miss(decide(&candidate, &request(false, Some(revision(&auth))))),
            Miss::Forced
        );
        // An older rejected revision is served the current one, as today.
        assert!(decide(&candidate, &request(false, Some("older".into()))).is_ok());
    }

    #[test]
    fn evidence_binds_to_both_revisions() {
        let auth = auth(NOW + 7200);
        let mut evidence = evidence(&auth, None);
        evidence.auth_revision = "other".into();
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(false, None)
            )),
            Miss::EvidenceStale
        );
        let mut evidence = self::evidence(&auth, None);
        evidence.account_revision = 6;
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(false, None)
            )),
            Miss::EvidenceStale
        );
        assert_eq!(
            miss(decide(&candidate(&auth, None), &request(false, None))),
            Miss::EvidenceMissing
        );
    }

    #[test]
    fn old_or_refused_evidence_takes_the_lease_path() {
        let auth = auth(NOW + 7200);
        let mut evidence = evidence(&auth, None);
        evidence.routing_age = EVIDENCE_MAX_AGE + Duration::from_secs(1);
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(false, None)
            )),
            Miss::EvidenceStale
        );
        let mut evidence = self::evidence(&auth, None);
        evidence.routing_supported = false;
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(false, None)
            )),
            Miss::Routing
        );
        let mut stale = billing(api::BillingClass::RateLimited, 40.0);
        stale.age = EVIDENCE_MAX_AGE + Duration::from_secs(1);
        let evidence = self::evidence(&auth, Some(stale));
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(true, None)
            )),
            Miss::EvidenceStale
        );
        let evidence = self::evidence(&auth, None);
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(true, None)
            )),
            Miss::EvidenceMissing
        );
    }

    #[test]
    fn a_nearly_spent_rate_limited_account_takes_a_live_read() {
        let auth = auth(NOW + 7200);
        let evidence = evidence(&auth, Some(billing(api::BillingClass::RateLimited, 95.0)));
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(true, None)
            )),
            Miss::UsageHigh
        );
        let evidence = self::evidence(&auth, Some(billing(api::BillingClass::RateLimited, 90.0)));
        assert_eq!(
            miss(decide(
                &candidate(&auth, Some(&evidence)),
                &request(true, None)
            )),
            Miss::UsageHigh
        );
        // The client refuses usage-based and unknown classes itself.
        for class in [api::BillingClass::UsageBased, api::BillingClass::Unknown] {
            let evidence = self::evidence(&auth, Some(billing(class, 95.0)));
            assert_eq!(
                served(decide(
                    &candidate(&auth, Some(&evidence)),
                    &request(true, None)
                ))
                .billing_class,
                Some(class)
            );
        }
    }
}
