//! One disk inventory for migration, login renewal, and refresh ownership checks.
use super::*;

/// A fresh verification may repair a stopped quarantine; ordinary restart may not.
#[derive(Clone, Copy, PartialEq)]
pub(in crate::central) enum AdmissionKind {
    Migration,
    Renewal,
    Restore,
}
pub(in crate::central) struct Admission {
    pub quarantine_repair: bool,
}
#[derive(Debug)]
pub(in crate::central) enum AdmissionDenied {
    Reserved,
    Owned,
    Unsettled,
    IdentityConflict,
}
impl std::fmt::Display for AdmissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Reserved => "login renewal reserves this server account",
            Self::Owned => "server account already belongs to a company user",
            Self::IdentityConflict => "migration conflicts with retained login identity",
            Self::Unsettled => "previous refresh or login process is not cleared",
        })
    }
}
impl std::error::Error for AdmissionDenied {}

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
        kind: AdmissionKind,
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
                kind,
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

/// A login grant reserves its identity: only a stopped one may yield, and a
/// migration must preserve every login claim it makes.
fn admit_candidate(
    candidate: &Reservation,
    auth: &Value,
    kind: AdmissionKind,
    admission: &mut Admission,
) -> Result<()> {
    if !overlaps(&candidate.auth, auth) {
        return Ok(());
    }
    if kind == AdmissionKind::Restore || candidate.process != ProcessState::Stopped {
        return Err(AdmissionDenied::Reserved.into());
    }
    if kind == AdmissionKind::Migration {
        // A new grant must preserve every known login claim before it
        // can retire a quarantine, even when no server account exists.
        super::super::server::validate_owned_identity(&candidate.auth, auth)
            .map_err(|_| AdmissionDenied::IdentityConflict)?;
    }
    admission.quarantine_repair = true;
    Ok(())
}

/// The sole registry admission policy, shared by migration, renewal and launch.
/// Call under the migration lock, after settling any conflicting refresh processes.
pub(in crate::central) fn clear_registry(
    accounts: &Path,
    key: &Path,
    selected: &Path,
    auth: &Value,
    kind: AdmissionKind,
) -> Result<Admission> {
    if kind == AdmissionKind::Migration
        && current(selected)?.is_some_and(|r| r.phase != Phase::Completed)
    {
        return Err(AdmissionDenied::Reserved.into());
    }
    let restoring_verified = kind == AdmissionKind::Restore;
    let mut admission = Admission {
        quarantine_repair: false,
    };
    // New-account logins hold grants outside the registry until admission.
    for candidate in add::reservations(accounts)? {
        admit_candidate(&candidate, auth, kind, &mut admission)?;
    }
    for entry in std::fs::read_dir(accounts)? {
        let state = entry?.path();
        if state == selected {
            continue;
        }
        let inventory = identity_inventory(&state, key, &state.join("runtime"));
        let conflict = inventory.journal_conflicts();
        for candidate in inventory.candidates? {
            admit_candidate(&candidate, auth, kind, &mut admission)?;
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
                return Err(AdmissionDenied::Unsettled.into());
            }
            if overlaps(&saved.auth, auth) || journal.as_ref().is_some_and(|a| overlaps(a, auth)) {
                return Err(AdmissionDenied::Unsettled.into());
            }
        }
        if !overlaps(&saved.auth, auth) {
            continue;
        }
        if inventory.login_reserved {
            return Err(AdmissionDenied::Reserved.into());
        }
        if saved.verified {
            return Err(AdmissionDenied::Owned.into());
        }
        if inventory.runtime != ProcessState::Stopped
            || (!saved.import_rejected && !managed::definitely_not_started(&inventory.home))
        {
            return Err(AdmissionDenied::Unsettled.into());
        }
    }
    Ok(admission)
}
