//! Server-owned native login with durable operation records.
use super::{
    managed::{self, Broker, HttpError},
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
use managed::{launch_owner, overlaps, previous_owner_exited};
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
pub(super) mod add;
mod http;
mod inventory;
mod shared;
pub(super) use inventory::{
    AdmissionDenied, AdmissionKind, ClearedIdentity, ProcessState, clear_registry,
    clear_shared_registry, identity_inventory,
};
mod recover;
mod state;
mod worker;
pub(super) use http::{cancel, start, status};
use recover::*;
pub(super) use recover::{
    needs_verification, recover, retire_reservations, verifier_parent_exited, verify_replacement,
};
use state::*;
use worker::*;
#[cfg(test)]
mod tests;

/// A display fact only; never authorizes a renewal or credential replacement.
pub(super) fn renewal_pending(state: &Path) -> Result<bool> {
    Ok(current(state)?.is_some_and(|r| {
        matches!(
            r.phase,
            Phase::Starting
                | Phase::Pending
                | Phase::Committing
                | Phase::Promoted
                | Phase::Retiring
        )
    }))
}
