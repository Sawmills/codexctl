//! Resolve verified company sign-ins without changing account or machine ownership.
use super::{Configuration, managed, vault};
use crate::store;
use anyhow::Result;
use std::path::Path;

pub(super) enum Resolution {
    User(String),
    Disabled,
    Conflict,
}

fn identity(issuer: &str, subject: &str) -> String {
    vault::digest(format!("{issuer}\0{subject}").as_bytes())
}

pub(super) async fn resolve_shared(
    central: &crate::central::storage::CentralStore,
    config: &Configuration,
    subject: &str,
    email: &str,
) -> Result<Resolution> {
    let arriving = identity(&config.issuer, subject);
    let rows = central.load_registry_entity_revisions("users").await?;
    let users = rows
        .iter()
        .map(|(_, payload, _)| serde_json::from_slice::<managed::User>(payload))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(user) = users
        .iter()
        .find(|u| u.oidc_identity.as_ref() == Some(&arriving))
    {
        return Ok(if user.enabled {
            Resolution::User(user.id.clone())
        } else {
            Resolution::Disabled
        });
    }
    if let Some(user) = users.iter().find(|u| u.id == arriving) {
        return Ok(if !user.enabled {
            Resolution::Disabled
        } else if user.oidc_identity.is_some() {
            Resolution::Conflict
        } else {
            Resolution::User(arriving)
        });
    }
    if config.hosted_domains().is_none() {
        return Ok(Resolution::User(arriving));
    }
    let matches: Vec<_> = users
        .iter()
        .enumerate()
        .filter(|(_, u)| u.email.eq_ignore_ascii_case(email))
        .collect();
    let source = config.clerk_migration.as_ref().and_then(|migration| {
        migration
            .users
            .iter()
            .find(|link| link.email.eq_ignore_ascii_case(email))
    });
    match (matches.as_slice(), source) {
        ([(index, user)], Some(source))
            if user.id == source.user_id && user.oidc_identity.is_none() =>
        {
            if !user.enabled {
                return Ok(Resolution::Disabled);
            }
            let mut linked = (*user).clone();
            linked.oidc_identity = Some(arriving);
            if !central
                .save_registry_entity_cas(
                    "users",
                    &linked.id,
                    &serde_json::to_vec(&linked)?,
                    Some(rows[*index].2),
                )
                .await?
            {
                anyhow::bail!("company identity changed concurrently");
            }
            Ok(Resolution::User(linked.id))
        }
        ([], None) => Ok(Resolution::User(arriving)),
        _ => Ok(Resolution::Conflict),
    }
}

// Only called after signature, issuer, nonce, verified email and hosted-domain checks.
// The administrator authorizes source identities in configuration; a browser cannot.
pub(super) fn resolve(
    state: &Path,
    config: &Configuration,
    subject: &str,
    email: &str,
) -> Result<Resolution> {
    let arriving = identity(&config.issuer, subject);
    let _lock = vault::registry_lock(state, "users.lock")?;
    let mut users = managed::users(state)?;
    if let Some(user) = users
        .iter()
        .find(|u| u.oidc_identity.as_ref() == Some(&arriving))
    {
        return Ok(if user.enabled {
            Resolution::User(user.id.clone())
        } else {
            Resolution::Disabled
        });
    }
    if let Some(user) = users.iter().find(|u| u.id == arriving) {
        return Ok(if !user.enabled {
            Resolution::Disabled
        } else if user.oidc_identity.is_some() {
            // A consumed source identity must not regain browser access after rollback.
            Resolution::Conflict
        } else {
            Resolution::User(arriving)
        });
    }
    if config.hosted_domains().is_none() {
        return Ok(Resolution::User(arriving));
    }

    let matches: Vec<_> = users
        .iter()
        .enumerate()
        .filter(|(_, user)| user.email.eq_ignore_ascii_case(email))
        .map(|(index, _)| index)
        .collect();
    let authorized_source = config.clerk_migration.as_ref().and_then(|migration| {
        migration
            .users
            .iter()
            .find(|link| link.email.eq_ignore_ascii_case(email))
            .map(|link| link.user_id.as_str())
    });
    match (matches.as_slice(), authorized_source) {
        ([index], Some(source)) => {
            let user = &mut users[*index];
            if user.id != source || user.oidc_identity.is_some() {
                return Ok(Resolution::Conflict);
            }
            if !user.enabled {
                return Ok(Resolution::Disabled);
            }
            user.oidc_identity = Some(arriving.clone());
            let stable_id = user.id.clone();
            store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)?;
            // Bounded digests only, never email, subject, tokens, or client secrets.
            eprintln!("SSO_IDENTITY_LINKED company_user={stable_id} oidc_identity={arriving}");
            Ok(Resolution::User(stable_id))
        }
        ([], None) => {
            // Reserve new identities under the same lock as linking. Concurrent callbacks
            // cannot create a competing company user with the same email.
            users.push(managed::User {
                id: arriving.clone(),
                email: email.into(),
                enabled: true,
                oidc_identity: None,
            });
            store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)?;
            Ok(Resolution::User(arriving))
        }
        _ => Ok(Resolution::Conflict),
    }
}
