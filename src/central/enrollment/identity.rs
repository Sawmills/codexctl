//! Resolve verified company sign-ins without changing account or machine ownership.
use super::{Configuration, managed, vault};
use crate::store;
use anyhow::Result;
use std::path::Path;

pub(super) type Resolution = crate::central::storage::BrowserIdentity;

fn identity(issuer: &str, subject: &str) -> String {
    vault::digest(format!("{issuer}\0{subject}").as_bytes())
}

pub(super) async fn resolve_shared(
    central: &crate::central::storage::CentralStore,
    config: &Configuration,
    subject: &str,
    email: &str,
) -> Result<Resolution> {
    let source = authorized_source(config, email);
    central
        .resolve_browser_identity(
            &identity(&config.issuer, subject),
            email,
            config.hosted_domains().is_some(),
            source,
        )
        .await
}

fn authorized_source<'a>(config: &'a Configuration, email: &str) -> Option<&'a str> {
    config.clerk_migration.as_ref().and_then(|migration| {
        migration
            .users
            .iter()
            .find(|link| link.email.eq_ignore_ascii_case(email))
            .map(|link| link.user_id.as_str())
    })
}

pub(in crate::central) enum Decision {
    Known(Resolution),
    Link(usize),
    Reserve,
}

// Each backend holds its registry lock through the resulting write.
pub(in crate::central) fn decide(
    users: &[managed::User],
    arriving: &str,
    email: &str,
    enforce_email_link: bool,
    source: Option<&str>,
) -> Decision {
    if let Some(user) = users
        .iter()
        .find(|u| u.oidc_identity.as_deref() == Some(arriving))
    {
        return Decision::Known(if user.enabled {
            Resolution::User(user.id.clone())
        } else {
            Resolution::Disabled
        });
    }
    if let Some(user) = users.iter().find(|u| u.id == arriving) {
        return Decision::Known(if !user.enabled {
            Resolution::Disabled
        } else if user.oidc_identity.is_some() {
            // A consumed source identity must not regain browser access after rollback.
            Resolution::Conflict
        } else {
            Resolution::User(arriving.to_owned())
        });
    }
    if !enforce_email_link {
        return Decision::Reserve;
    }
    let matches: Vec<_> = users
        .iter()
        .enumerate()
        .filter(|(_, user)| user.email.eq_ignore_ascii_case(email))
        .map(|(index, _)| index)
        .collect();
    match (matches.as_slice(), source) {
        ([index], Some(source))
            if users[*index].id == source && users[*index].oidc_identity.is_none() =>
        {
            if users[*index].enabled {
                Decision::Link(*index)
            } else {
                Decision::Known(Resolution::Disabled)
            }
        }
        ([], None) => Decision::Reserve,
        _ => Decision::Known(Resolution::Conflict),
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
    let enforce_email_link = config.hosted_domains().is_some();
    match decide(
        &users,
        &arriving,
        email,
        enforce_email_link,
        authorized_source(config, email),
    ) {
        Decision::Known(resolution) => Ok(resolution),
        Decision::Link(index) => {
            users[index].oidc_identity = Some(arriving.clone());
            let stable_id = users[index].id.clone();
            store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)?;
            // Bounded digests only, never email, subject, tokens, or client secrets.
            eprintln!("SSO_IDENTITY_LINKED company_user={stable_id} oidc_identity={arriving}");
            Ok(Resolution::User(stable_id))
        }
        Decision::Reserve => {
            if enforce_email_link {
                // Reserve before releasing the lock so a competing subject sees this email.
                users.push(managed::User {
                    id: arriving.clone(),
                    email: email.into(),
                    enabled: true,
                    oidc_identity: None,
                });
                store::atomic_write(&state.join("users.json"), &serde_json::to_vec(&users)?)?;
            }
            Ok(Resolution::User(arriving))
        }
    }
}
