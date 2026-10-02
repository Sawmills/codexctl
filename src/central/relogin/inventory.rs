//! One disk inventory for import, renewal, and startup ownership decisions.
use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(in crate::central) enum ProcessState {
    Stopped,
    Live,
    Unknown,
}
pub(in crate::central) struct Reservation {
    pub auth: Value,
    pub process: ProcessState,
}
pub(in crate::central) struct IdentityInventory {
    pub saved: Result<Vault>,
    pub journal: Result<Option<Value>>,
    pub runtime: ProcessState,
    pub candidates: Result<Vec<Reservation>>,
    pub login_reserved: bool,
}
impl IdentityInventory {
    pub fn journal_conflicts(&self) -> bool {
        match (&self.saved, &self.journal) {
            (Ok(saved), Ok(Some(auth))) => {
                super::super::server::validate_owned_identity(&saved.auth, auth).is_err()
            }
            (_, Ok(None)) => false,
            _ => true,
        }
    }
}

/// Call under the import lock, or the exclusive startup owner lock. The result
/// retains unreadable evidence rather than treating it as an absent identity.
/// This function never stops a process or changes credentials. Import may settle
/// a conflicting RPC and reread; renewal must refuse until its exit is proven.
pub(in crate::central) fn identity_inventory(state: &Path, key: &Path) -> IdentityInventory {
    let home = state.join("runtime");
    let saved = vault::load(state, key);
    let journal = (|| {
        if !home.try_exists()? {
            return Ok(None);
        }
        let auth = managed::retained_auth(&home)?;
        vault::validate_auth(&auth)?;
        Ok(Some(auth))
    })();
    let runtime = if previous_owner_exited(&home).is_ok() {
        ProcessState::Stopped
    } else {
        let process = vault::private_read(&home.join("pid"))
            .and_then(|b| Ok(serde_json::from_slice::<process::Process>(&b)?));
        if process.and_then(|p| p.alive()).is_ok_and(|alive| alive) {
            ProcessState::Live
        } else {
            ProcessState::Unknown
        }
    };
    let candidates = (|| {
        let mut candidates = Vec::new();
        for record in records(state)? {
            if record.retired || record.phase == Phase::Completed {
                continue;
            }
            let process = if stopped(state, &record).is_ok() {
                ProcessState::Stopped
            } else if matches!(&record.child, Child::Running(p) if p.alive().is_ok_and(|alive| alive))
            {
                ProcessState::Live
            } else {
                ProcessState::Unknown
            };
            // A child with no identifiable process may hold any identity.
            if process == ProcessState::Unknown {
                bail!("unidentified login process");
            }
            if let Some(auth) = reservation(state, &record)? {
                candidates.push(Reservation { auth, process });
            }
        }
        Ok(candidates)
    })();
    // Corrupt, known-unspawned records still reserve their selected alias.
    let login_reserved = current(state)
        .map(|r| r.is_some_and(|r| r.phase != Phase::Completed))
        .unwrap_or(true);
    IdentityInventory {
        saved,
        journal,
        runtime,
        candidates,
        login_reserved,
    }
}
