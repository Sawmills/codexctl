//! Server-owned native login with durable operation records.
use super::{
    managed::{self, Broker, HttpError, Import},
    process,
    server::{Owner, TokenRequest},
    vault::{self, Vault},
};
use crate::store;
use anyhow::{Context, Result, bail};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use managed::{account_key, launch_owner, overlaps, previous_owner_exited};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;
mod http;
mod recover;
mod state;
mod worker;
pub(super) use http::{cancel, start, status};
use recover::*;
pub(super) use recover::{check_import, needs_verification, recover, verify_replacement};
use state::*;
use worker::*;
#[cfg(test)]
mod tests;
