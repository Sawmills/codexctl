//! One disk inventory for migration, login renewal, and refresh ownership checks.
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
    state: PathBuf,
    home: PathBuf,
    pub saved: Result<Vault>,
    pub journal: Result<Option<Value>>,
    pub runtime: ProcessState,
    pub candidates: Result<Vec<Reservation>>,
    pub login_reserved: bool,
    live_unbound_login: bool,
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

/// Call under the migration lock. The result
/// retains unreadable evidence rather than treating it as an absent identity.
/// This function never stops a process or changes credentials. Migration may settle
/// a conflicting RPC and reread; renewal must refuse until its exit is proven.
pub(in crate::central) fn identity_inventory(
    state: &Path,
    key: &Path,
    home: &Path,
) -> IdentityInventory {
    let saved = vault::load(state, key);
    let journal = (|| {
        if !home.try_exists()? {
            return Ok(None);
        }
        let auth = managed::retained_auth(home)?;
        vault::validate_auth(&auth)?;
        Ok(Some(auth))
    })();
    let runtime = if previous_owner_exited(home).is_ok() {
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
    let mut live_unbound_login = false;
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
            } else if process != ProcessState::Stopped {
                live_unbound_login = true;
            }
        }
        Ok(candidates)
    })();
    // Corrupt, known-unspawned records still reserve their selected alias.
    let login_reserved = current(state)
        .map(|r| r.is_some_and(|r| r.phase != Phase::Completed))
        .unwrap_or(true);
    IdentityInventory {
        state: state.into(),
        home: home.into(),
        saved,
        journal,
        runtime,
        candidates,
        login_reserved,
        live_unbound_login,
    }
}

/// Linear authority to start one refresh process while the migration lock remains
/// held. Fields and construction stay private to this inventory module.
#[must_use]
pub(in crate::central) struct ClearedIdentity<'a> {
    state: PathBuf,
    home: PathBuf,
    company_user: String,
    alias: String,
    auth_digest: String,
    journal_digest: String,
    _lock: std::marker::PhantomData<&'a ()>,
}
impl ClearedIdentity<'_> {
    pub(in crate::central) fn validate(&self, refresh: &Owner) -> Result<()> {
        if self.state != refresh.state
            || self.home != refresh.home
            || self.company_user != refresh.vault.user
            || self.alias != refresh.vault.alias
            || self.auth_digest != vault::digest(&serde_json::to_vec(&refresh.vault.auth)?)
        {
            bail!("refresh clearance belongs to different server account state");
        }
        self.validate_home(&refresh.home)
    }
    pub(in crate::central) fn validate_home(&self, home: &Path) -> Result<()> {
        if home != self.home
            || self.journal_digest != vault::digest(&vault::private_read(&home.join("auth.json"))?)
        {
            bail!("refresh clearance journal changed before launch");
        }
        Ok(())
    }
}
impl IdentityInventory {
    /// Only this inventory module can issue launch authority. The lock borrow
    /// prevents callers from retaining the proof beyond the migration critical section.
    pub(in crate::central) fn clear_for_launch<'a>(
        self,
        refresh: &Owner,
        _lock: &'a tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<ClearedIdentity<'a>> {
        if self.state != refresh.state || self.home != refresh.home || refresh.rpc.is_some() {
            bail!("refresh process already exists or inventory differs");
        }
        if self.runtime != ProcessState::Stopped || self.live_unbound_login {
            bail!("previous refresh process exit is not proven");
        }
        let saved = self.saved?;
        if saved.user != refresh.vault.user
            || saved.alias != refresh.vault.alias
            || saved.auth != refresh.vault.auth
        {
            bail!("server account changed after inventory");
        }
        let journal = self.journal?.context("refresh journal is absent")?;
        super::super::server::validate_owned_identity(&saved.auth, &journal)?;
        for candidate in self.candidates? {
            if candidate.process != ProcessState::Stopped {
                bail!("login renewal process has not exited");
            }
        }
        if self
            .state
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "accounts")
        {
            clear_registry(
                self.state.parent().context("missing registry")?,
                &refresh.key,
                &self.state,
                &saved.auth,
                saved.verified,
            )?;
        }
        Ok(ClearedIdentity {
            state: self.state,
            home: self.home.clone(),
            company_user: saved.user,
            alias: saved.alias,
            auth_digest: vault::digest(&serde_json::to_vec(&saved.auth)?),
            journal_digest: vault::digest(&vault::private_read(&self.home.join("auth.json"))?),
            _lock: std::marker::PhantomData,
        })
    }

    /// Unknown identity evidence must be fenced too. Reading only the vault here
    /// would miss a live refresh process that wrote a different runtime identity.
    pub(in crate::central) fn needs_fence(&self, auth: &Value) -> bool {
        self.live_unbound_login
            || self
                .saved
                .as_ref()
                .map_or(true, |v| overlaps(&v.auth, auth))
            || self
                .journal
                .as_ref()
                .map_or(true, |j| j.as_ref().is_some_and(|a| overlaps(a, auth)))
            || self
                .candidates
                .as_ref()
                .map_or(true, |c| c.iter().any(|r| overlaps(&r.auth, auth)))
    }
}

pub(super) fn check_registry_claim(
    accounts: &Path,
    key: &Path,
    selected: &Path,
    auth: &Value,
) -> Result<()> {
    clear_registry(accounts, key, selected, auth, false)
}
fn clear_registry(
    accounts: &Path,
    key: &Path,
    selected: &Path,
    auth: &Value,
    restoring_verified: bool,
) -> Result<()> {
    for entry in std::fs::read_dir(accounts)? {
        let state = entry?.path();
        if state == selected {
            continue;
        }
        let inventory = identity_inventory(&state, key, &state.join("runtime"));
        let conflict = inventory.journal_conflicts();
        for candidate in inventory.candidates? {
            if overlaps(&candidate.auth, auth)
                && (restoring_verified || candidate.process != ProcessState::Stopped)
            {
                bail!("matching quarantine reserves this server account");
            }
        }
        let journal = match inventory.journal {
            Ok(journal) => journal,
            Err(error) => {
                // Resume only established, unrelated grants beside a stopped
                // partial journal. Migration and login renewal still refuse.
                if restoring_verified
                    && inventory.runtime == ProcessState::Stopped
                    && inventory
                        .saved
                        .as_ref()
                        .is_ok_and(|saved| !overlaps(&saved.auth, auth))
                {
                    continue;
                }
                return Err(error);
            }
        };
        let saved = match inventory.saved {
            Ok(saved) => saved,
            Err(error) => {
                // An established server account may restart beside an unreadable
                // vault only after its process exited and its readable journal
                // does not reserve the selected identity. New claims still refuse.
                if restoring_verified
                    && inventory.runtime == ProcessState::Stopped
                    && !journal.as_ref().is_some_and(|a| overlaps(a, auth))
                {
                    continue;
                }
                return Err(error);
            }
        };
        if conflict {
            if inventory.runtime != ProcessState::Stopped {
                bail!("conflicting journal owner has not stopped");
            }
            if overlaps(&saved.auth, auth) || journal.as_ref().is_some_and(|a| overlaps(a, auth)) {
                bail!("conflicting journal reserves this account");
            }
        }
        if !overlaps(&saved.auth, auth) {
            continue;
        }
        if saved.verified || !saved.import_rejected || inventory.login_reserved {
            bail!("account already owned or reserved");
        }
        if inventory.runtime != ProcessState::Stopped {
            bail!("previous credential owner has not stopped");
        }
    }
    Ok(())
}
