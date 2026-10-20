//! Shared, bounded account/model evidence and same-session recovery decisions.
use super::{managed::Account, remote, storage::CentralStore};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const WINDOW: i64 = 120;
const COOLDOWN: i64 = 300;
const RETENTION: i64 = 3600;
const MAX_ENTRIES: usize = 128;
const MAX_EVENTS: usize = 256;
const MAX_TERMINALS: usize = 16;
const MAX_SESSIONS: usize = 128;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub alias: String,
    pub model: String,
    pub event_id: String,
    pub outcome: Outcome,
    pub session_id: Option<String>,
    pub observed_at: Option<i64>,
    #[serde(skip)]
    pub identity: Option<super::loans::CredentialSubject>,
}

#[derive(Clone)]
pub struct CapacityAccount {
    pub account: Account,
    pub identity: super::loans::CredentialSubject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Overloaded,
    Completed,
    Terminal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub model: String,
    pub session_id: Option<String>,
    pub failed_alias: Option<String>,
    #[serde(default)]
    pub tried: Vec<String>,
    #[serde(default)]
    pub recent_429_rates: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub alias: Option<String>,
    pub reason: Option<String>,
}
impl PlanRequest {
    fn validate(&self) -> Result<()> {
        if self.recent_429_rates.len() > MAX_ENTRIES
            || self
                .recent_429_rates
                .values()
                .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            || !known_model(&self.model)
            || self.tried.len() > 64
            || self
                .tried
                .iter()
                .any(|a| super::loans::AccountRef::parse(a).is_err())
            || self.session_id.as_deref().is_some_and(|s| !session_id(s))
            || self.session_id.is_some() != self.failed_alias.is_some()
            || self
                .failed_alias
                .as_deref()
                .is_some_and(|a| super::loans::AccountRef::parse(a).is_err())
            || (self.session_id.is_none() && !self.tried.is_empty())
        {
            bail!("invalid capacity plan");
        }
        Ok(())
    }
}

impl Plan {
    fn stop(reason: &str) -> Self {
        Self {
            alias: None,
            reason: Some(reason.into()),
        }
    }
    fn selected(alias: String) -> Self {
        Self {
            alias: Some(alias),
            reason: None,
        }
    }
}

fn known_model(model: &str) -> bool {
    crate::RELAY_KNOWN_MODELS.contains(&model)
}
pub(crate) fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
}
fn session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
impl Evidence {
    pub(crate) fn validate(&self) -> Result<()> {
        super::loans::AccountRef::parse(&self.alias)?;
        if !known_model(&self.model)
            || !valid_event_id(&self.event_id)
            || self.session_id.as_deref().is_some_and(|s| !session_id(s))
            || (self.outcome == Outcome::Terminal && self.session_id.is_none())
        {
            bail!("invalid capacity evidence");
        }
        Ok(())
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::central) struct State {
    pub(super) histories: BTreeMap<String, History>,
    pub(super) sessions: BTreeMap<String, Session>,
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct History {
    pub(super) identity: super::loans::CredentialSubject,
    pub(super) model: String,
    pub(super) anchor_until: i64,
    #[serde(default)]
    pub(super) completed_at: Option<i64>,
    #[serde(default)]
    pub(super) completed_ids: BTreeMap<String, i64>,
    pub(super) seen: BTreeMap<String, i64>,
    pub(super) failures: Vec<i64>,
    pub(super) cooling_until: i64,
    pub(super) terminal: BTreeMap<String, (String, i64)>,
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Session {
    pub(super) moves: Vec<(i64, String, String, String)>,
}

impl State {
    fn identity_key(&self, identity: &super::loans::CredentialSubject, model: &str) -> String {
        self.histories
            .iter()
            .find(|(_, h)| h.model == model && h.identity.same_login(identity))
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| {
                super::vault::digest(
                    format!(
                        "{model}:{}:{:?}:{:?}",
                        identity.workspace, identity.uid, identity.sub
                    )
                    .as_bytes(),
                )
            })
    }
    fn prune(&mut self, now: i64) {
        self.histories.retain(|_, h| {
            h.seen.retain(|_, at| now.saturating_sub(*at) <= WINDOW);
            h.failures.retain(|at| now.saturating_sub(*at) <= WINDOW);
            h.completed_ids
                .retain(|_, at| now.saturating_sub(*at) <= WINDOW);
            h.terminal
                .retain(|_, (_, at)| now.saturating_sub(*at) < COOLDOWN);
            !h.seen.is_empty()
                || h.cooling_until > now
                || !h.terminal.is_empty()
                || h.anchor_until > now
                || h.completed_at
                    .is_some_and(|at| now.saturating_sub(at) <= WINDOW)
        });
        self.sessions.retain(|_, s| {
            s.moves
                .retain(|(at, _, _, _)| now.saturating_sub(*at) < RETENTION);
            !s.moves.is_empty()
        });
    }
    fn observe(&mut self, evidence: Evidence, now: i64) -> Result<()> {
        evidence.validate()?;
        let observed = evidence.observed_at.unwrap_or(now);
        if observed > now + 30 || now.saturating_sub(observed) > WINDOW {
            bail!("expired capacity observation");
        }
        self.prune(now);
        let identity = evidence
            .identity
            .as_ref()
            .context("capacity evidence lacks resolved account identity")?;
        if identity.workspace.is_empty() || (identity.uid.is_none() && identity.sub.is_none()) {
            bail!("capacity identity is unproven");
        }
        let key = self.identity_key(identity, &evidence.model);
        if !self.histories.contains_key(&key) && self.histories.len() >= MAX_ENTRIES {
            bail!("capacity history limit");
        }
        let history = self.histories.entry(key).or_insert_with(|| History {
            identity: identity.clone(),
            model: evidence.model.clone(),
            ..History::default()
        });
        if history.identity.uid.is_none() {
            history.identity.uid.clone_from(&identity.uid);
        }
        if history.identity.sub.is_none() {
            history.identity.sub.clone_from(&identity.sub);
        }
        let event_key = super::vault::digest(evidence.event_id.as_bytes())[..32].to_owned();
        if evidence.outcome == Outcome::Completed {
            if history.completed_at.is_some_and(|at| observed < at)
                || history.completed_ids.contains_key(&event_key)
            {
                return Ok(());
            }
            // Observation seconds order reports across replicas. Ties use arrival
            // order, so a real failure in the same second can start a new streak.
            history.completed_at = Some(observed);
            history.completed_ids.retain(|_, at| *at >= observed);
            if history.completed_ids.len() >= MAX_EVENTS {
                let oldest = history.completed_ids.keys().next().cloned().unwrap();
                history.completed_ids.remove(&oldest);
            }
            history.completed_ids.insert(event_key, observed);
            history.failures.retain(|at| *at > observed);
            if history.failures.len() < 3 {
                history.cooling_until = 0;
            }
            history.terminal.retain(|_, (_, at)| *at > observed);
            return Ok(());
        }
        if history.completed_at.is_some_and(|at| observed < at) {
            return Ok(());
        }
        if evidence.outcome == Outcome::Terminal {
            let session = evidence.session_id.context("terminal session missing")?;
            if history
                .terminal
                .get(&session)
                .is_some_and(|(id, _)| id == &evidence.event_id)
            {
                return Ok(());
            }
            if !history.terminal.contains_key(&session) && history.terminal.len() >= MAX_TERMINALS {
                bail!("capacity terminal limit");
            }
            history
                .terminal
                .insert(session, (evidence.event_id, observed));
            return Ok(());
        }
        // Keep bounded failure evidence during cooling: an older success may
        // arrive later and must not erase failures that followed it.
        if history.seen.contains_key(&event_key) {
            return Ok(());
        }
        if history.seen.len() >= MAX_EVENTS {
            bail!("capacity evidence limit");
        }
        history.seen.insert(event_key, observed);
        history.failures.push(observed);
        if history.failures.len() >= 3 && history.cooling_until <= now {
            history.cooling_until = now + COOLDOWN;
        }
        Ok(())
    }
    fn plan(
        &mut self,
        request: PlanRequest,
        accounts: &[CapacityAccount],
        now: i64,
        user: &str,
    ) -> Result<Plan> {
        request.validate()?;
        self.prune(now);
        for account in accounts {
            let key = self.identity_key(&account.identity, &request.model);
            if let Some(history) = self.histories.get_mut(&key) {
                if history.identity.uid.is_none() {
                    history.identity.uid.clone_from(&account.identity.uid);
                }
                if history.identity.sub.is_none() {
                    history.identity.sub.clone_from(&account.identity.sub);
                }
            }
        }
        let account_key = |alias: &str| {
            accounts
                .iter()
                .find(|a| a.account.alias.eq_ignore_ascii_case(alias))
                .map(|a| self.identity_key(&a.identity, &request.model))
        };
        let mut excluded = request
            .tried
            .iter()
            .filter_map(|a| account_key(a))
            .collect::<Vec<_>>();
        let mut terminal = None;
        if let Some(session) = &request.session_id {
            let failed_alias = request
                .failed_alias
                .as_deref()
                .context("missing failed account")?;
            let Some(failed) = account_key(failed_alias) else {
                return Ok(Plan::stop("evidence_pending"));
            };
            let Some(history) = self.histories.get(&failed) else {
                return Ok(Plan::stop("evidence_pending"));
            };
            let Some((id, _)) = history.terminal.get(session) else {
                return Ok(Plan::stop("evidence_pending"));
            };
            let session_key = format!("{user}:{session}");
            let move_event = format!("{failed}:{id}");
            if let Some(previous) = self.sessions.get(&session_key) {
                if let Some((_, _, selected, _)) = previous
                    .moves
                    .iter()
                    .find(|(_, _, _, event)| event == &move_event)
                {
                    let candidate = accounts
                        .iter()
                        .filter(|a| {
                            !request
                                .tried
                                .iter()
                                .any(|alias| alias.eq_ignore_ascii_case(&a.account.alias))
                                && self.identity_key(&a.identity, &request.model) == *selected
                        })
                        .map(|a| a.account.clone())
                        .collect::<Vec<_>>();
                    if self
                        .histories
                        .get(selected)
                        .is_some_and(|h| h.cooling_until > now)
                    {
                        return Ok(Plan::stop("alternate_unavailable"));
                    }
                    return Ok(match remote::select_for_codex(&candidate) {
                        Ok(alias) => Plan::selected(alias),
                        Err(_) => Plan::stop("alternate_unavailable"),
                    });
                }
                if previous.moves.len() >= 3 {
                    return Ok(Plan::stop("recovery_budget"));
                }
                for (_, from, to, _) in &previous.moves {
                    excluded.extend([from.clone(), to.clone()]);
                }
            }
            if history.cooling_until <= now {
                return Ok(Plan::stop("below_threshold"));
            }
            excluded.push(failed.clone());
            terminal = Some((session_key, failed, move_event));
        }
        let candidates = accounts
            .iter()
            .filter(|a| {
                let key = self.identity_key(&a.identity, &request.model);
                !a.identity.workspace.is_empty()
                    && (a.identity.uid.is_some() || a.identity.sub.is_some())
                    && !excluded.contains(&key)
                    && self.histories.get(&key).is_none_or(|h| {
                        h.cooling_until <= now
                            && (request.session_id.is_none() || h.terminal.is_empty())
                    })
            })
            .map(|a| {
                let mut account = a.account.clone();
                account.recent_429_rate = request.recent_429_rates.get(&account.alias).copied();
                account
            })
            .collect::<Vec<_>>();
        let Ok(alias) = remote::select_for_codex(&candidates) else {
            return Ok(Plan::stop("no_included_alternate"));
        };
        let selected = account_key(&alias).context("selected capacity identity missing")?;
        if let Some((session, failed, event)) = terminal {
            if !self.histories.contains_key(&selected) {
                if self.histories.len() >= MAX_ENTRIES {
                    bail!("capacity history limit");
                }
                let identity = accounts
                    .iter()
                    .find(|a| a.account.alias.eq_ignore_ascii_case(&alias))
                    .context("selected identity missing")?
                    .identity
                    .clone();
                self.histories.insert(
                    selected.clone(),
                    History {
                        identity,
                        model: request.model.clone(),
                        anchor_until: now + RETENTION,
                        ..History::default()
                    },
                );
            } else if let Some(history) = self.histories.get_mut(&selected) {
                history.anchor_until = now + RETENTION;
            }
            if !self.sessions.contains_key(&session) && self.sessions.len() >= MAX_SESSIONS {
                bail!("capacity session limit");
            }
            self.sessions
                .entry(session)
                .or_default()
                .moves
                .push((now, failed, selected, event));
        }
        Ok(Plan::selected(alias))
    }
}

/// Policy and persistence share one interface. Callers cannot update a counter
/// and forget the cooldown, or charge a move outside the shared transaction.
pub struct CapacityRecovery {
    store: CentralStore,
    user: String,
}
impl CapacityRecovery {
    pub fn new(store: CentralStore, user: String) -> Self {
        Self { store, user }
    }
    pub async fn observe(&self, evidence: Evidence, now: i64) -> Result<()> {
        self.store
            .capacity_state(&format!("model:{}", evidence.model), now, move |state| {
                state.observe(evidence, now)
            })
            .await
    }
    pub async fn plan(
        &self,
        request: PlanRequest,
        accounts: Vec<CapacityAccount>,
        now: i64,
    ) -> Result<Plan> {
        let user = self.user.clone();
        let model_scope = format!("model:{}", request.model);
        let session_scope = format!("sessions:{}", super::vault::digest(user.as_bytes()));
        let scopes = if request.session_id.is_some() {
            vec![model_scope.clone(), session_scope.clone()]
        } else {
            vec![model_scope.clone()]
        };
        self.store
            .capacity_states(scopes, now, move |states| {
                let mut model = states
                    .remove(&model_scope)
                    .context("capacity model absent")?;
                let mut budget = states.remove(&session_scope);
                if let Some(budget) = &mut budget {
                    model.sessions = std::mem::take(&mut budget.sessions);
                }
                let result = model.plan(request, &accounts, now, &user);
                if let Some(mut budget) = budget {
                    budget.sessions = std::mem::take(&mut model.sessions);
                    states.insert(session_scope, budget);
                }
                states.insert(model_scope, model);
                result
            })
            .await
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub account_capacity: Evidence,
}

pub(super) fn policy(broker: &super::managed::Broker, user: &str) -> CapacityRecovery {
    CapacityRecovery::new(
        broker
            .central
            .clone()
            .unwrap_or_else(|| CentralStore::file(&broker.state, &broker.key)),
        user.into(),
    )
}

async fn subject_for(
    broker: &super::managed::Broker,
    device: &super::vault::Device,
    alias: &str,
) -> Result<super::loans::CredentialSubject, super::managed::HttpError> {
    use axum::http::StatusCode;
    let owner = match super::loans::AccountRef::parse(alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?
    {
        super::loans::AccountRef::Owned(_) => broker.owner(device, alias).await?,
        super::loans::AccountRef::Borrowed { .. } => {
            broker.borrowed_owner(device, alias, None).await?.owner
        }
    };
    let held = tokio::time::timeout(std::time::Duration::from_secs(2), owner.lock())
        .await
        .map_err(|_| {
            broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_identity_unavailable",
            )
        })?;
    super::loans::http::owner_subject(&held).ok_or_else(|| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity_identity_unavailable",
        )
    })
}

pub(super) async fn accept(
    broker: &super::managed::Broker,
    device: &super::vault::Device,
    report: Report,
) -> Result<(), super::managed::HttpError> {
    use axum::http::StatusCode;
    let mut evidence = report.account_capacity;
    evidence
        .validate()
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_capacity_evidence"))?;
    if evidence.observed_at.is_none() {
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_capacity_evidence"));
    }
    evidence.identity = Some(subject_for(broker, device, &evidence.alias).await?);
    policy(broker, &device.user)
        .observe(evidence, chrono::Utc::now().timestamp())
        .await
        .map_err(|_| {
            broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_policy_unavailable",
            )
        })
}

pub(super) async fn plan_http(
    axum::extract::State(broker): axum::extract::State<super::managed::Broker>,
    headers: axum::http::HeaderMap,
    axum::Json(request): axum::Json<PlanRequest>,
) -> Result<axum::Json<Plan>, super::managed::HttpError> {
    use axum::http::StatusCode;
    let device = broker.authorize(&headers).await?;
    request
        .validate()
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_capacity_plan"))?;
    let accounts =
        super::managed::account_catalog(&broker, &device.user, super::catalog::Freshness::Cached)
            .await?;
    use futures::StreamExt;
    let resolved = futures::stream::iter(accounts.into_iter().map(|account| {
        let broker = &broker;
        let device = &device;
        async move {
            subject_for(broker, device, &account.alias)
                .await
                .ok()
                .map(|identity| CapacityAccount { account, identity })
        }
    }))
    .buffer_unordered(32)
    .filter_map(|account| async { account })
    .collect()
    .await;
    broker.authorize(&headers).await?;
    let plan = policy(&broker, &device.user)
        .plan(request, resolved, chrono::Utc::now().timestamp())
        .await
        .map_err(|_| {
            broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_policy_unavailable",
            )
        })?;
    Ok(axum::Json(plan))
}
