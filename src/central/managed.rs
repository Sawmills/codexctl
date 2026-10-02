//! Multi-user broker. One process and one persistent disk own every refresh token.
use super::{
    catalog, enrollment, relogin,
    rpc::Rpc,
    server::{Owner, TokenFailure, TokenRequest},
    transport,
    vault::{self, Vault},
};
use crate::{api, store};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, RwLock, Semaphore};

#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub enabled: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub user_id: String,
    pub alias: String,
    pub label: Option<String>,
    pub account_id: String,
    pub plan: Option<String>,
    pub billing_class: api::BillingClass,
    pub primary_used: Option<f64>,
    pub secondary_used: Option<f64>,
    pub resets_at: Option<i64>,
    pub available: bool,
    pub usage_score: Option<f64>,
    #[serde(default)]
    pub usage_age_seconds: Option<u64>,
    #[serde(default)]
    pub usage_stale: bool,
    #[serde(default)]
    pub usage_error: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub alias: String,
    pub label: Option<String>,
    pub auth: Value,
}
#[derive(Default)]
struct Failure {
    count: u64,
    last: i64,
}
#[derive(Clone)]
pub(super) struct AccountIndex {
    pub(super) user: String,
    pub(super) alias: String,
}
type Owners = BTreeMap<String, (AccountIndex, Arc<Mutex<Owner>>)>;
// Overlap retains a seat reservation even if UID evidence is missing or conflicts.
// It is never permission to replace credentials.
pub(super) fn overlaps(left: &Value, right: &Value) -> bool {
    vault::account(left).ok() == vault::account(right).ok()
        && api::token_subject(vault::token(left).unwrap_or(""))
            == api::token_subject(vault::token(right).unwrap_or(""))
}
#[derive(Clone)]
pub(super) struct Broker {
    pub state: PathBuf,
    pub key: PathBuf,
    pub(super) binary: PathBuf,
    pub(super) read_only: bool,
    pub(super) ownership_unresolved: Arc<AtomicBool>,
    pub(super) owners: Arc<RwLock<Owners>>,
    pub(super) imports: Arc<Mutex<()>>,
    pub sso: Option<Arc<enrollment::Sso>>,
    pub(super) reset_reader: super::resets::Reader,
    catalog: Arc<catalog::Reader>,
