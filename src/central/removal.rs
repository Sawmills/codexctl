//! One owner for removal confirmation, process custody, tombstone and audit.
use super::{
    managed::{self, Broker, HttpError},
    relogin::shared::Caller,
    storage::{
        DashboardAudit, DashboardOperation, RemovalAction, RemovalResult, RemovalTarget,
        login::LoginAuthority,
    },
};
use anyhow::Context;
use axum::http::StatusCode;
use std::{path::Path, time::Duration};

pub(super) struct Request {
    pub alias: String,
    pub attempt: String,
    pub account_id: String,
    pub revision: i64,
    pub confirmation: String,
}

pub(super) async fn remove(
    broker: &Broker,
    caller: &Caller,
    request: Request,
) -> Result<(), HttpError> {
    let actor = caller
        .dashboard_actor
        .as_deref()
        .filter(|_| matches!(caller.authority, LoginAuthority::Browser(_)))
        .ok_or_else(|| broker.error(StatusCode::UNAUTHORIZED, "browser_sign_in_required"))?;
    if request.attempt.len() != 64 || !request.attempt.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(broker.error(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let central = broker.central.as_ref().ok_or_else(|| {
        broker.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard_actions_unavailable",
        )
    })?;
    let result = if !broker.browser_remove_enabled {
        Err("dashboard_remove_disabled")
    } else if request.revision < 1 || request.confirmation != request.alias {
        Err("removal_confirmation_changed")
    } else {
        run(broker, caller, &request).await
    };
    if let Err(reason) = result {
        central
            .dashboard_audit(
                DashboardAudit {
                    operation: DashboardOperation::AccountRemove,
                    actor,
                    attempt: &request.attempt,
                },
                &format!(
                    "unresolved:{}",
                    super::vault::digest(request.alias.as_bytes())
                ),
                &format!("refused:{reason}"),
            )
            .await
            .map_err(|cause| {
                managed::log_dashboard_failure("remove_audit", &cause);
                broker.error(StatusCode::SERVICE_UNAVAILABLE, "dashboard_audit_failed")
            })?;
        let status = match reason {
            "account_not_found" | "account_removed" => StatusCode::NOT_FOUND,
            "browser_sign_in_required" => StatusCode::UNAUTHORIZED,
            "dashboard_remove_disabled"
            | "persistence_failed"
            | "refresh_in_progress"
            | "owner_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        };
        return Err(broker.error(status, reason));
    }
    Ok(())
}

async fn run(broker: &Broker, caller: &Caller, request: &Request) -> Result<(), &'static str> {
    let central = broker
        .central
        .as_ref()
        .ok_or("dashboard_actions_unavailable")?;
    let inspect = central
        .remove_account(caller, request, &broker.holder_id, RemovalAction::Inspect)
        .await
        .map_err(|cause| {
            managed::log_dashboard_failure("remove_inspect", &cause);
            "persistence_failed"
        })??;
    if matches!(inspect, RemovalResult::AlreadyRemoved) {
        return Ok(());
    }
    // Match local migration/token ordering; no database transaction spans stop.
    let _imports = broker
        .imports
        .try_lock()
        .map_err(|_| "refresh_in_progress")?;
    let owner = broker
        .owners
        .read()
        .await
        .get(&request.account_id)
        .map(|(_, o)| o.clone());
    let mut held = owner
        .as_ref()
        .map(|o| o.try_lock())
        .transpose()
        .map_err(|_| "refresh_in_progress")?;
    let previous = held.as_ref().map(|o| {
        (
            o.available,
            o.refresh_enabled,
            o.routing_refused,
            o.retryable_unavailable,
        )
    });
    if let Some(owner) = held.as_mut() {
        if owner.import_settling {
            return Err("refresh_in_progress");
        }
        managed::stop_removed_owner(owner).await.map_err(|cause| {
            managed::log_dashboard_failure("remove_stop", &cause);
            "owner_unavailable"
        })?;
    } else {
        stop_retained_runtime(
            &broker
                .state
                .join("accounts")
                .join(&request.account_id)
                .join("runtime"),
        )
        .await
        .map_err(|cause| {
            managed::log_dashboard_failure("remove_stop", &cause);
            "owner_unavailable"
        })?;
    }
    let committed = central
        .remove_account(caller, request, &broker.holder_id, RemovalAction::Commit)
        .await
        .map_err(|cause| {
            managed::log_dashboard_failure("remove_commit", &cause);
            "persistence_failed"
        })
        .and_then(|r| r);
    match committed {
        Ok(RemovalResult::Removed | RemovalResult::AlreadyRemoved) => {
            drop(held);
            broker.owners.write().await.remove(&request.account_id);
            Ok(())
        }
        Ok(RemovalResult::Ready) => unreachable!("commit cannot return an inspection"),
        Err(reason) => {
            // Refused confirmation must not disable an active account. Its
            // child is proven stopped and its journal remains untouched.
            if let (Some(o), Some((available, refresh, routing, retryable))) =
                (held.as_mut(), previous)
            {
                o.available = available;
                o.refresh_enabled = refresh;
                o.routing_refused = routing;
                o.retryable_unavailable = retryable;
            }
            Err(reason)
        }
    }
}

pub(super) async fn stop_retained_runtime(home: &Path) -> anyhow::Result<()> {
    if managed::previous_owner_exited(home).is_ok() {
        return Ok(());
    }
    let process: super::process::Process =
        serde_json::from_slice(&super::vault::private_read(&home.join("pid"))?)?;
    if process.alive()? {
        let result = unsafe { libc::kill(process.pid() as i32, libc::SIGKILL) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while process.alive()? {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("removed runtime exit deadline")??;
    managed::previous_owner_exited(home)
}

pub(super) async fn view(
    broker: &Broker,
    caller: &Caller,
    alias: &str,
) -> Result<RemovalTarget, HttpError> {
    if !matches!(caller.authority, LoginAuthority::Browser(_)) {
        return Err(broker.error(StatusCode::NOT_FOUND, "account_not_found"));
    }
    if !broker.browser_remove_enabled {
        return Err(broker.error(StatusCode::SERVICE_UNAVAILABLE, "dashboard_remove_disabled"));
    }
    let alias = managed::normalize_alias(alias)
        .map_err(|_| broker.error(StatusCode::BAD_REQUEST, "invalid_alias"))?;
    broker
        .central
        .as_ref()
        .ok_or_else(|| {
            broker.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "dashboard_actions_unavailable",
            )
        })?
        .removal_target(&caller.user, alias)
        .await
        .map_err(|_| broker.error(StatusCode::SERVICE_UNAVAILABLE, "persistence_failed"))?
        .ok_or_else(|| broker.error(StatusCode::NOT_FOUND, "account_not_found"))
}
