use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api;
use crate::config::{self, Paths};
use crate::store;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Meta {
    pub alias: String,
    /// Operator-set display name. The one field here a human writes; everything
    /// else is re-derived from the stored token on each save.
    pub label: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// `chatgpt_account_id`: which workspace this profile holds. This is what
    /// separates two profiles that share one email address.
    pub account_id: Option<String>,
    /// `chatgpt_user_id`: which login. Two workspace seats for one human share it.
    pub user_id: Option<String>,
    pub saved_at: String,
}

pub struct Profile {
    pub meta: Meta,
    pub dir: PathBuf,
}

impl Profile {
    pub fn auth_json_path(&self) -> PathBuf {
        self.dir.join("auth.json")
    }
}

// === Paths-accepting versions (testable) ===

pub fn list_profiles_from(paths: &Paths) -> Result<Vec<Profile>> {
    let profiles_dir = paths.profiles_dir();
    if !profiles_dir.exists() {
        return Ok(vec![]);
    }
    let mut profiles = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&profiles_dir)
        .with_context(|| format!("failed to read {}", profiles_dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut seen_aliases = HashSet::new();
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(directory_alias) = entry.file_name().to_str().map(str::to_owned) else {
            eprintln!("warning: ignored profile directory with a non-UTF-8 name");
            continue;
        };
        let Ok(alias) = store::validate_alias(&directory_alias) else {
            eprintln!("warning: ignored profile directory with an invalid alias");
            continue;
        };
        if !seen_aliases.insert(alias.to_ascii_lowercase()) {
            eprintln!("warning: ignored profile directory with a case-colliding alias");
            continue;
        }
        let path = entry.path();
        let meta_path = path.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let mut meta: Meta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        if meta.alias != alias {
            eprintln!(
                "warning: profile metadata alias did not match its directory; using the directory alias"
            );
            meta.alias = alias.to_string();
        }
        profiles.push(Profile { meta, dir: path });
    }
    profiles.sort_by(|a, b| a.meta.alias.cmp(&b.meta.alias));
    Ok(profiles)
}

pub fn get_profile_from(paths: &Paths, alias: &str) -> Result<Profile> {
    let alias = store::validate_alias(alias)?;
    let dir = store::profile_dir(paths, alias)?;
    if !dir.exists() {
        anyhow::bail!("profile '{}' not found", alias);
    }
    let meta_path = dir.join("meta.json");
    let contents = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let mut meta: Meta = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;
    meta.alias = alias.to_string();
    Ok(Profile { meta, dir })
}

pub fn save_profile_to(
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let _lock = store::lock(paths)?;
    save_profile_unlocked(paths, alias, email, auth_json_src)
}

/// Save a profile and make it active under one store lock.
///
/// When the source is an isolated login home, install it into the live Codex
/// home. Capturing the outgoing live token deliberately skips the alias being
/// saved, so a stale live token cannot overwrite the login that just replaced
/// it. Being active is only one way the live file can belong to this alias —
/// the token itself is the authority.
pub fn save_profile_and_activate_to(
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let lock = store::lock(paths)?;
    save_profile_and_activate_locked(&lock, paths, alias, email, auth_json_src)
}

/// Record the credential that is *already* live under `alias` and make it
/// active, without writing anything back to the live Codex home.
///
/// `save` reads the live file, so installing it again could only overwrite it —
/// and a native `codex` refresh does not take this lock, so by then the live
/// file may legitimately hold something newer. Copying a snapshot back over it
/// would roll that away.
pub fn save_live_profile_locked(
    _lock: &store::StoreLock,
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    save_profile_unlocked(paths, alias, email, auth_json_src)?;
    set_active_unlocked(paths, alias)
}

/// [`save_profile_and_activate_to`] for a caller that already holds the store
/// lock, so it can decide *and* write without releasing it in between.
///
/// The lock is taken by reference as proof rather than for use: which alias a
/// save lands on depends on what the store already holds, and a decision made
/// outside the lock answers for a store another writer can still change before
/// the write lands.
pub fn save_profile_and_activate_locked(
    _lock: &store::StoreLock,
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let live_auth = paths.codex_auth_json();
    save_profile_unlocked(paths, alias, email, auth_json_src)?;

    if auth_json_src != live_auth {
        // Capture protects the *outgoing* profile's rotated tokens. The alias
        // being written is never outgoing, so it is excluded outright rather
        // than by inferring ownership — an unreadable or absent stored token
        // must not be able to route the stale live file back over this login.
        capture_auth_file_profile_tokens(paths, &live_auth, Some(alias));
        let saved_auth = store::profile_dir(paths, alias)?.join("auth.json");
        store::atomic_copy(&saved_auth, &live_auth)
            .with_context(|| format!("failed to install {}", live_auth.display()))?;
    }
    set_active_unlocked(paths, alias)
}

fn save_profile_unlocked(
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    store::ensure_private_dir(&paths.codexctl_dir())?;
    store::ensure_private_dir(&paths.profiles_dir())?;
    let dir = store::profile_dir(paths, alias)?;
    store::ensure_private_dir(&dir)?;

    let dest = dir.join("auth.json");
    store::atomic_copy(auth_json_src, &dest)
        .with_context(|| format!("failed to save auth.json to {}", dest.display()))?;

    let meta_path = dir.join("meta.json");
    // The label is the operator's, so a re-save carries it over. Every other
    // field is re-derived from the token that was just stored.
    let previous_label = read_meta(&meta_path).and_then(|meta| meta.label);
    let identity = identity_of_auth_file(&dest);

    let meta = Meta {
        alias: alias.to_string(),
        label: previous_label,
        email: identity.email.or_else(|| email.map(str::to_string)),
        plan: identity.plan,
        account_id: identity.account_id,
        user_id: identity.user_id,
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_json = serde_json::to_vec_pretty(&meta)?;
    store::atomic_write(&meta_path, &meta_json)?;
    Ok(())
}

fn read_meta(meta_path: &Path) -> Option<Meta> {
    let contents = std::fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Identity asserted by the token in an auth file. An unreadable file or a
/// non-JWT token yields an empty identity rather than an error, because a
/// profile must remain saveable even when its token cannot be understood.
fn identity_of_auth_file(auth_json: &Path) -> api::TokenIdentity {
    let Ok(auth) = api::read_auth_json(auth_json) else {
        return api::TokenIdentity::default();
    };
    let mut identity = api::token_identity(&auth.access_token).unwrap_or_default();
    // `read_auth_json` already applies the documented precedence: the explicit
    // auth.json field first, the JWT claim only as a fallback. Use its answer so
    // the recorded workspace matches what every other call path resolves.
    identity.account_id = auth.account_id.or(identity.account_id);
    identity
}

/// The workspace `alias` already holds, when that is positively a *different*
/// account than `incoming_account`.
///
/// `None` means saving is safe, or there is not enough evidence to refuse: a
/// missing identifier on either side is not proof of a conflict. Both `save`
/// and `login` gate on this, because either one can replace the credentials of
/// a profile that belongs to another account on the same login.
/// Which login a saved profile holds, by the same precedence as its workspace.
///
/// Like [`workspace_of_profile`], this reads the stored files directly: a
/// profile whose `meta.json` will not parse still has credentials worth
/// protecting, and refusing to look at them is what lets a damaged profile be
/// overwritten as though it held nothing.
///
/// A workspace is not an owner: several people hold seats in one team
/// workspace, so the login is what separates their credentials.
pub fn user_of_profile(paths: &Paths, alias: &str) -> Option<String> {
    let dir = store::profile_dir(paths, alias).ok()?;
    let stored_login = api::read_auth_json(&dir.join("auth.json"))
        .ok()
        .and_then(|auth| api::token_login(&auth.access_token));
    stored_login.or_else(|| read_meta(&dir.join("meta.json")).and_then(|meta| meta.user_id))
}

/// Which workspace a saved profile holds.
///
/// The stored token is asked first because it *is* the credential; metadata is
/// a derived copy of it. A save writes `auth.json` before `meta.json`, so an
/// interrupted one leaves metadata describing the previous workspace — trusting
/// it there would reject the account actually stored and leave the profile
/// unrepairable without deleting it. Metadata still answers for a profile whose
/// token carries no claim.
pub fn workspace_of_profile(paths: &Paths, alias: &str) -> Option<String> {
    let dir = store::profile_dir(paths, alias).ok()?;
    identity_of_auth_file(&dir.join("auth.json"))
        .account_id
        .or_else(|| read_meta(&dir.join("meta.json")).and_then(|meta| meta.account_id))
}

/// Whether a candidate auth file may be attributed to a profile's workspace.
///
/// This is the single rule every attribution path shares, because each path
/// that reimplemented it left a different way in. It is deliberately not
/// symmetric: a candidate declaring no workspace cannot prove it belongs to a
/// profile that declares one, which is how a second seat's token lands on the
/// first seat's profile. The reverse is safe — a profile saved before
/// workspaces were recorded still owns its own login's rotations, and a
/// claimed candidate contradicts nothing about it.
fn claim_permits(candidate: Option<&str>, stored: Option<&str>) -> bool {
    match (candidate, stored) {
        (Some(candidate), Some(stored)) => candidate == stored,
        // Declaring nothing cannot prove membership of a declared account.
        (None, Some(_)) => false,
        (Some(_), None) | (None, None) => true,
    }
}

/// Whether two claims positively agree, for a write that destroys what is
/// already stored.
///
/// [`claim_permits`] is deliberately lenient where a claim is missing, because
/// capturing a rotation into a profile that never recorded one loses nothing.
/// An overwrite is the opposite: a profile whose workspace was never recorded
/// cannot confirm that an arriving workspace is the same account, and reading
/// "cannot confirm" as "yes" is what destroys a legacy profile when its owner
/// signs into a second workspace. Only a claim that matches, or the absence of
/// any claim on both sides, is agreement.
fn claims_agree(candidate: Option<&str>, stored: Option<&str>) -> bool {
    match (candidate, stored) {
        (Some(candidate), Some(stored)) => candidate == stored,
        (None, None) => true,
        _ => false,
    }
}

/// The same rule over a whole account identity. Both claims have to permit the
/// attribution: one workspace holds many logins, and one login holds seats in
/// many workspaces, so neither alone identifies whose credentials these are.
fn workspace_permits(candidate: Option<&str>, stored: Option<&str>) -> bool {
    claim_permits(candidate, stored)
}

/// Every alias the store holds a directory for.
///
/// Deliberately not `list_profiles_from`, which skips a directory with no
/// `meta.json`: a save writes `auth.json` first, so an interrupted one leaves a
/// real seat in exactly that shape. Treating it as absent is how a second alias
/// gets created for an account that is already saved.
fn stored_aliases(paths: &Paths) -> Result<Vec<String>> {
    let profiles_dir = paths.profiles_dir();
    if !profiles_dir.exists() {
        return Ok(Vec::new());
    }
    let mut aliases = Vec::new();
    for entry in std::fs::read_dir(&profiles_dir)
        .with_context(|| format!("failed to read {}", profiles_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if store::validate_alias(&name).is_ok() {
            aliases.push(name);
        }
    }
    aliases.sort();
    Ok(aliases)
}

/// What the store already holds for one account.
///
/// Three outcomes, kept apart on purpose: an `Option` would collapse "nothing
/// matched" together with "the store could not be read" and "several aliases
/// matched", and a caller reading either of those as "nothing" adds yet another
/// copy of an account that is already saved.
pub enum ExistingSeat {
    /// No saved profile holds this account.
    None,
    /// Exactly one does, and it is the profile to refresh.
    One(String),
    /// Several do. The store is already ambiguous about this account, and
    /// guessing between them is what would make it worse.
    Ambiguous(Vec<String>),
}

/// Which saved profile, if any, already holds exactly this account.
///
/// The alias is the store key, not the account, so a seat saved under some
/// alias should be refreshed rather than duplicated when the operator gives it
/// a different label. A store that cannot be scanned is an error rather than an
/// answer, because "no match" and "could not look" are not the same thing.
pub fn existing_seat(
    paths: &Paths,
    workspace: Option<&str>,
    user: Option<&str>,
) -> Result<ExistingSeat> {
    // Reuse replaces a profile, so it needs both halves positively matched.
    // Comparing the claims as options would let `None == None` stand in for
    // identity, and every legacy profile would look like the same seat as any
    // token that happens to omit a claim.
    if workspace.is_none() || user.is_none() {
        return Ok(ExistingSeat::None);
    }
    let matching: Vec<String> = stored_aliases(paths)?
        .into_iter()
        .filter(|alias| {
            workspace_of_profile(paths, alias).as_deref() == workspace
                && user_of_profile(paths, alias).as_deref() == user
        })
        .collect();
    Ok(match matching.len() {
        0 => ExistingSeat::None,
        1 => ExistingSeat::One(matching.into_iter().next().expect("one match")),
        _ => ExistingSeat::Ambiguous(matching),
    })
}

/// Whether `alias` holds a profile whose account cannot be identified at all —
/// no workspace in its metadata and none in the token it stored.
///
/// Such a profile cannot be shown to be the same account as an incoming login,
/// so an alias codexctl *derived* rather than the operator naming it must not
/// be reused: the overwrite would rest on an unprovable match.
pub fn unidentifiable_profile(paths: &Paths, alias: &str) -> bool {
    let Ok(dir) = store::profile_dir(paths, alias) else {
        return false;
    };
    if !dir.exists() {
        // Nothing is there to overwrite.
        return false;
    }
    // Something is there. Metadata that cannot be read is the strongest reason
    // to treat it as unidentifiable, not a reason to treat it as absent — an
    // interrupted save or a damaged file leaves exactly this shape.
    if get_profile_from(paths, alias).is_err() {
        return true;
    }
    // Both halves are required. A workspace holds many logins, so knowing only
    // the workspace does not say whose credentials these are — and this guard
    // protects an alias the operator never named, where "not proven different"
    // is not good enough to overwrite.
    workspace_of_profile(paths, alias).is_none() || user_of_profile(paths, alias).is_none()
}

pub fn conflicting_workspace(
    paths: &Paths,
    alias: &str,
    incoming_account: Option<&str>,
    incoming_user: Option<&str>,
) -> Option<String> {
    let dir = store::profile_dir(paths, alias).ok()?;
    let stored_workspace = workspace_of_profile(paths, alias);
    let stored_user = user_of_profile(paths, alias);
    if stored_workspace.is_none() && stored_user.is_none() {
        return None;
    }
    // This guard gates `login` and `save`, which replace what is stored.
    //
    // The workspace has to be settled before anything may be written over it:
    // the same claim on both sides, or no claim on either. "Stored declares
    // nothing" is not agreement — one login holds seats in several workspaces,
    // so a legacy profile that never recorded one cannot confirm that an
    // arriving workspace is the same account.
    let workspace_settled = claims_agree(incoming_account, stored_workspace.as_deref());
    // The login only has to not contradict. A workspace is shared, so a
    // different login in it is a different account; but a stored login that was
    // never recorded blocks nothing on its own, which is what keeps a profile
    // repairable when its token is unreadable and only metadata remains.
    // A stored login that is merely absent blocks nothing only when there is
    // genuinely nothing to read — an unreadable token is what sends an operator
    // back to `login` to repair the profile. A *readable* token that yields no
    // login is different: the profile is intact, its owner is simply unproven,
    // and a workspace does not identify its owner.
    let stored_auth_readable = api::read_auth_json(&dir.join("auth.json")).is_ok();
    let user_contradicts = match (incoming_user, stored_user.as_deref()) {
        (Some(incoming), Some(stored)) => incoming != stored,
        (Some(_), None) => stored_auth_readable,
        // The arriving token names no login at all. A workspace is shared, so
        // agreeing on it proves nothing about whose credentials these are, and
        // the profile does name someone.
        (None, Some(_)) => true,
        (None, None) => false,
    };
    if workspace_settled && !user_contradicts {
        return None;
    }
    Some(
        stored_workspace
            .or(stored_user)
            .unwrap_or_else(|| "unknown".to_string()),
    )
}

/// Short workspace id for an error message; the full uuid is noise.
pub fn short_workspace(account_id: &str) -> String {
    match account_id.char_indices().nth(8) {
        Some((index, _)) => format!("{}…", &account_id[..index]),
        None => account_id.to_string(),
    }
}

/// Set or clear a profile's display label. `None`, or text that is blank once
/// trimmed, clears it.
pub fn set_label_from(paths: &Paths, alias: &str, label: Option<&str>) -> Result<()> {
    let lock = store::lock(paths)?;
    set_label_locked(&lock, paths, alias, label)
}

/// [`set_label_from`] for a caller already holding the store lock, so a save
/// and its label land as one locked sequence instead of two.
pub fn set_label_locked(
    _lock: &store::StoreLock,
    paths: &Paths,
    alias: &str,
    label: Option<&str>,
) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let label = label.map(store::validate_label).transpose()?.flatten();
    let dir = store::profile_dir(paths, alias)?;
    let meta_path = dir.join("meta.json");
    let Some(mut meta) = read_meta(&meta_path) else {
        anyhow::bail!("profile '{}' not found", alias);
    };
    meta.alias = alias.to_string();
    meta.label = label.map(str::to_string);
    let meta_json = serde_json::to_vec_pretty(&meta)?;
    store::atomic_write(&meta_path, &meta_json)
}

pub fn delete_profile_from(paths: &Paths, alias: &str) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let _lock = store::lock(paths)?;
    let dir = store::profile_dir(paths, alias)?;
    if !dir.exists() {
        anyhow::bail!("profile '{}' not found", alias);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    Ok(())
}

pub fn get_active_from(paths: &Paths) -> Result<Option<String>> {
    let active_file = paths.active_file();
    if !active_file.exists() {
        return Ok(None);
    }
    let metadata = match std::fs::symlink_metadata(&active_file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        eprintln!("warning: ignored symbolic-link active profile marker");
        return Ok(None);
    }
    let contents = match std::fs::read_to_string(&active_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match store::validate_alias(&contents) {
        Ok(alias) => Ok(Some(alias.to_string())),
        Err(_) => {
            eprintln!("warning: ignored invalid active profile marker");
            Ok(None)
        }
    }
}

pub fn set_active_from(paths: &Paths, alias: &str) -> Result<()> {
    let _lock = store::lock(paths)?;
    set_active_unlocked(paths, alias)
}

fn set_active_unlocked(paths: &Paths, alias: &str) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    store::ensure_private_dir(&paths.codexctl_dir())?;
    store::atomic_write(&paths.active_file(), alias.as_bytes())
}

pub fn switch_to_from(paths: &Paths, alias: &str) -> Result<String> {
    switch_to_auth_json_from(paths, alias, &paths.codex_auth_json())
}

pub fn switch_to_auth_json_from(paths: &Paths, alias: &str, codex_auth: &Path) -> Result<String> {
    let alias = store::validate_alias(alias)?;
    let _lock = store::lock(paths)?;
    let profile = get_profile_from(paths, alias)?;

    // Capture the outgoing live tokens before installing the next profile.
    // The exact-token or token-subject guard prevents a foreign live auth file
    // from overwriting an unrelated saved profile. Nothing is excluded here:
    // when the live file belongs to the profile being switched to, folding its
    // rotated tokens in first is exactly right, since the install then copies
    // that same freshly-updated file back out.
    capture_auth_file_profile_tokens(paths, codex_auth, None);

    store::atomic_copy(&profile.auth_json_path(), codex_auth)
        .with_context(|| format!("failed to install auth.json at {}", codex_auth.display()))?;
    if codex_auth == paths.codex_auth_json() {
        // Write the marker last. A crash can leave the old marker, but it cannot
        // claim that a new alias is active before its auth file is installed.
        set_active_unlocked(paths, alias)?;
    }
    Ok(profile.meta.email.unwrap_or_else(|| "unknown".to_string()))
}

/// Seed a pinned exec home's auth file from its saved profile.
///
/// This is deliberately not a switch: it never installs into the live Codex
/// home and never writes the active profile marker, so a pinned launch leaves
/// global state alone. A token left behind by an earlier pinned run is folded
/// back into its owning profile first, which heals a run that was killed before
/// it could capture its own refreshed token.
pub fn seed_exec_auth_from(paths: &Paths, alias: &str, exec_auth: &Path) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let _lock = store::lock(paths)?;
    let profile = get_profile_from(paths, alias)?;
    capture_exec_auth_unlocked(paths, exec_auth, Some(alias));
    store::atomic_copy(&profile.auth_json_path(), exec_auth)
        .with_context(|| format!("failed to install auth.json at {}", exec_auth.display()))
}

/// Fold a pinned exec home's auth file back into the profile that owns it.
///
/// `pinned_alias` is the account the launch asked for. It settles the one case
/// token inspection cannot: one seat saved under two aliases is ambiguous by
/// subject, but the label the caller launched with is not.
pub fn capture_exec_auth_from(paths: &Paths, exec_auth: &Path, pinned_alias: &str) -> Result<()> {
    let pinned_alias = store::validate_alias(pinned_alias)?;
    let _lock = store::lock(paths)?;
    capture_exec_auth_unlocked(paths, exec_auth, Some(pinned_alias));
    Ok(())
}

/// Best-effort capture of a pinned exec home's tokens. Failures only warn,
/// because a capture problem must not fail a child run that already succeeded.
///
/// Subject matching keeps a foreign login inside an exec home out of the store.
/// The expiry guard keeps a stale exec-home copy from replacing a newer profile
/// token, which is what a re-login of the same alias during a long pinned run
/// would otherwise cause.
fn capture_exec_auth_unlocked(paths: &Paths, exec_auth: &Path, pinned_alias: Option<&str>) {
    if !exec_auth.exists() {
        return;
    }
    let Some(alias) = alias_for_auth_json_with_hint(paths, exec_auth, pinned_alias) else {
        return;
    };
    let Ok(dest) = store::profile_dir(paths, &alias).map(|dir| dir.join("auth.json")) else {
        return;
    };
    if !captured_auth_supersedes_profile(exec_auth, &dest) {
        return;
    }
    if let Err(error) = store::atomic_copy(exec_auth, &dest) {
        eprintln!("warning: failed to capture tokens for profile '{alias}': {error}");
    }
}

/// Which saved profile an auth file belongs to, given the alias the caller
/// already knows.
///
/// Evidence outranks the hint, but only evidence that discriminates. The named
/// alias wins first when it holds this exact token, because a store-wide scan
/// cannot tell two aliases saved from one login apart and would answer by
/// directory order. Failing that, an exact match elsewhere names its owner
/// outright. The hint then breaks the tie token inspection cannot: one seat
/// saved under two aliases matches both by subject. Recovery inside a pinned
/// lane can rotate the file to a different account, so a file that matches
/// nothing still falls back to a store-wide search.
/// The profile holding this exact access token, if any.
/// Choose the owner among profiles holding one access token.
///
/// A profile declaring the same workspace is a stronger match than one
/// declaring none, so directory order must not decide between them — the
/// weaker match would take the credentials and leave the real owner stale.
/// Equally strong matches are genuinely ambiguous and get no answer.
fn strongest_exact_match(
    target_workspace: Option<&str>,
    candidates: &[(String, Option<String>)],
) -> Option<String> {
    let declared: Vec<&(String, Option<String>)> = candidates
        .iter()
        .filter(|(_, workspace)| {
            target_workspace.is_some() && workspace.as_deref() == target_workspace
        })
        .collect();
    if let [only] = declared.as_slice() {
        return Some(only.0.clone());
    }
    if !declared.is_empty() {
        return None;
    }
    // Nothing declares the target's workspace, but if something declares
    // another one then this token demonstrably spans workspaces. A claimless
    // sibling has no better claim to it than the contradicting one, so the
    // answer is ambiguity rather than the lenient match.
    let contradicted = candidates
        .iter()
        .any(|(_, workspace)| workspace.is_some() && workspace.as_deref() != target_workspace);
    if contradicted {
        return None;
    }
    let permitted: Vec<&(String, Option<String>)> = candidates
        .iter()
        .filter(|(_, workspace)| workspace_permits(target_workspace, workspace.as_deref()))
        .collect();
    if let [only] = permitted.as_slice() {
        return Some(only.0.clone());
    }
    None
}

fn alias_for_exact_token_from(paths: &Paths, auth_json: &Path) -> Option<String> {
    let target = api::read_auth_json(auth_json).ok()?;
    let candidates: Vec<(String, Option<String>)> = list_profiles_from(paths)
        .ok()?
        .into_iter()
        .filter_map(|profile| {
            let stored = api::read_auth_json(&profile.auth_json_path()).ok()?;
            (stored.access_token == target.access_token).then_some({
                let workspace = workspace_of_profile(paths, &profile.meta.alias);
                (profile.meta.alias, workspace)
            })
        })
        .collect();
    strongest_exact_match(target.account_id.as_deref(), &candidates)
}

/// Whether a subject-only match on a claimless pair is really undecided.
///
/// When neither the file nor the hinted profile declares a workspace, the only
/// evidence is the shared login — and a claimless file can equally be a
/// rotation of a *declared* sibling on that same login, which need not carry
/// the claim. The hint settles ties between equals; it does not settle this,
/// so the store-wide resolver gets to answer (and refuses).
fn claimless_match_is_ambiguous(paths: &Paths, auth_json: &Path, hinted: &Profile) -> bool {
    let (Ok(target), Ok(stored)) = (
        api::read_auth_json(auth_json),
        api::read_auth_json(&hinted.auth_json_path()),
    ) else {
        return false;
    };
    // The hinted profile's workspace is its effective one, so a claim held only
    // in metadata still takes this out of the claimless case.
    if target.account_id.is_some()
        || stored.account_id.is_some()
        || workspace_of_profile(paths, &hinted.meta.alias).is_some()
    {
        return false;
    }
    let Some(subject) = api::token_subject(&target.access_token) else {
        return false;
    };
    // A store that cannot be read is not evidence that no sibling declares a
    // workspace, and a half-written profile is still a profile — both count as
    // ambiguity rather than permission for the hint. Siblings are judged by
    // their effective workspace, metadata included.
    let Ok(aliases) = stored_aliases(paths) else {
        return true;
    };
    aliases
        .into_iter()
        .filter(|alias| alias != &hinted.meta.alias)
        .any(|alias| {
            let Ok(dir) = store::profile_dir(paths, &alias) else {
                return true;
            };
            let Ok(sibling) = api::read_auth_json(&dir.join("auth.json")) else {
                return true;
            };
            workspace_of_profile(paths, &alias).is_some()
                && api::token_subject(&sibling.access_token).as_deref() == Some(subject.as_str())
        })
}

pub fn alias_for_auth_json_with_hint(
    paths: &Paths,
    auth_json: &Path,
    known_alias: Option<&str>,
) -> Option<String> {
    let hinted = known_alias.and_then(|alias| get_profile_from(paths, alias).ok());
    // The named alias holding this very token is the strongest confirmation the
    // hint can get. It outranks the store-wide scan, which would otherwise let
    // directory order pick between two aliases saved from the same login.
    if let Some(profile) = &hinted
        && auth_has_profile_access_token(paths, auth_json, profile)
    {
        return Some(profile.meta.alias.clone());
    }
    if let Some(alias) = alias_for_exact_token_from(paths, auth_json) {
        return Some(alias);
    }
    if let Some(profile) = hinted
        && auth_belongs_to_profile(paths, auth_json, &profile)
    {
        // Undecided means no owner. Falling through to the store-wide resolver
        // would undo this: it enumerates through `list_profiles_from`, which
        // skips the very half-written and unreadable profiles that made this
        // ambiguous, and could then rediscover the hint as a lone candidate.
        if claimless_match_is_ambiguous(paths, auth_json, &profile) {
            return None;
        }
        return Some(profile.meta.alias);
    }
    alias_for_auth_json_from(paths, auth_json).ok().flatten()
}

/// Whether captured credentials are worth writing over the saved profile.
///
/// Identical credentials are not worth a write. Otherwise the newer access
/// token wins, judged by its issued-at claim and falling back to its expiry:
/// an older snapshot carries an older refresh token too, so taking either would
/// undo a newer login.
///
/// A refresh token that rotates on its own is still captured: the access token
/// is unchanged in that case, so both sides report the same expiry and the
/// comparison passes. The expiry only ever refuses a file whose access token
/// also changed — that is, a whole older copy.
fn captured_auth_supersedes_profile(captured_auth: &Path, profile_auth: &Path) -> bool {
    let Ok(candidate) = api::read_auth_json(captured_auth) else {
        return false;
    };
    let Ok(current) = api::read_auth_json(profile_auth) else {
        return true;
    };
    // The workspace counts as part of the credential state: Codex can add or
    // change an explicit `account_id` without touching either token, and
    // discarding that leaves the profile claimless and its ownership ambiguous.
    if candidate.access_token == current.access_token
        && candidate.refresh_token == current.refresh_token
        && candidate.account_id == current.account_id
    {
        return false;
    }
    // Issued-at orders the two tokens directly. Expiry only stands in for it,
    // and stops being a proxy for recency the moment a shortened lifetime makes
    // the newer token expire first.
    if let (Some(candidate_iat), Some(current_iat)) = (
        api::token_issued_at(&candidate.access_token),
        api::token_issued_at(&current.access_token),
    ) && candidate_iat != current_iat
    {
        return candidate_iat > current_iat;
    }
    // Same issuance instant, or no issued-at to compare: let expiry decide.
    match (
        api::token_expiry(&candidate.access_token),
        api::token_expiry(&current.access_token),
    ) {
        (Some(candidate_exp), Some(current_exp)) => candidate_exp >= current_exp,
        _ => true,
    }
}

/// Pick the live auth file only when it belongs to the active saved profile.
/// Otherwise use the stored snapshot and avoid attributing a foreign session.
pub fn auth_json_path_for_profile_from(
    paths: &Paths,
    profile: &Profile,
    active: Option<&str>,
) -> PathBuf {
    if active != Some(profile.meta.alias.as_str()) {
        return profile.auth_json_path();
    }
    let live = paths.codex_auth_json();
    if auth_belongs_to_profile(paths, &live, profile) {
        live
    } else {
        profile.auth_json_path()
    }
}

fn auth_has_profile_access_token(paths: &Paths, auth_json: &Path, profile: &Profile) -> bool {
    let (Ok(left), Ok(stored)) = (
        api::read_auth_json(auth_json),
        api::read_auth_json(&profile.auth_json_path()),
    ) else {
        return false;
    };
    // Against the profile's *effective* workspace, metadata included: a stored
    // token with no claim does not make the profile anonymous, and this
    // shortcut runs before the fuller ownership check gets a say.
    left.access_token == stored.access_token
        && workspace_permits(
            left.account_id.as_deref(),
            workspace_of_profile(paths, &profile.meta.alias).as_deref(),
        )
}

fn auth_belongs_to_profile(paths: &Paths, auth_json: &Path, profile: &Profile) -> bool {
    let (Ok(left), Ok(stored)) = (
        api::read_auth_json(auth_json),
        api::read_auth_json(&profile.auth_json_path()),
    ) else {
        return false;
    };
    // The profile's workspace is what it *effectively* holds, metadata fallback
    // included: a stored token carrying no claim does not make the profile
    // anonymous when `meta.json` records the account.
    let right = api::AuthJson {
        account_id: workspace_of_profile(paths, &profile.meta.alias),
        ..stored
    };
    // An identical access token is not identity on its own: `read_auth_json`
    // takes an explicit `account_id` from the file ahead of the JWT claim, so
    // two files can carry one token and name different workspaces.
    if !workspace_permits(left.account_id.as_deref(), right.account_id.as_deref()) {
        return false;
    }
    if left.access_token == right.access_token {
        return true;
    }
    let left_subject = api::token_subject(&left.access_token);
    if left_subject.is_none() || left_subject != api::token_subject(&right.access_token) {
        return false;
    }
    // The same login is not the same account. Two workspace seats of one human
    // share a subject, so a declared workspace has to agree as well — otherwise
    // the live seat's usage renders under the other seat's row.
    //
    // `right` is always the saved profile and `left` the file being attributed
    // to it, which makes the missing-claim cases asymmetric. A candidate that
    // declares nothing cannot prove it belongs to a profile that declares a
    // workspace, and treating it as proof is how a second seat's token gets
    // captured over the first's. The reverse is safe: a profile saved before
    // workspaces were recorded still owns the rotations of its own login, and
    // nothing about them contradicts it.
    true
}

pub fn alias_for_auth_json_from(paths: &Paths, auth_json: &Path) -> Result<Option<String>> {
    let Ok(target_auth) = api::read_auth_json(auth_json) else {
        return Ok(None);
    };
    let target_sub = api::token_subject(&target_auth.access_token);
    let target_account = target_auth.account_id.clone();
    let mut profile_auths = Vec::new();
    for profile in list_profiles_from(paths)? {
        let Ok(profile_auth) = api::read_auth_json(&profile.auth_json_path()) else {
            continue;
        };
        profile_auths.push((profile, profile_auth));
    }

    let exact: Vec<(String, Option<String>)> = profile_auths
        .iter()
        .filter(|(_, profile_auth)| profile_auth.access_token == target_auth.access_token)
        .map(|(profile, _)| {
            let workspace = workspace_of_profile(paths, &profile.meta.alias);
            (profile.meta.alias.clone(), workspace)
        })
        .collect();
    if !exact.is_empty() {
        return Ok(strongest_exact_match(target_account.as_deref(), &exact));
    }

    let same_seat: Vec<(String, Option<String>)> = profile_auths
        .into_iter()
        .filter_map(|(profile, profile_auth)| {
            let profile_sub = api::token_subject(&profile_auth.access_token);
            (target_sub.is_some() && target_sub == profile_sub).then_some({
                let workspace = workspace_of_profile(paths, &profile.meta.alias);
                (profile.meta.alias, workspace)
            })
        })
        .collect();

    // One human holding two workspace seats produces two profiles with the same
    // subject, so the workspace is what tells them apart.
    //
    // A candidate that declares a *different* workspace is positively not the
    // owner. It can never win, and it must not be quietly dropped either: once
    // some candidate contradicts the live workspace, a claimless sibling has no
    // better claim to the tokens, and returning either would overwrite a saved
    // profile's credentials with an unrelated account's.
    let candidates: Vec<&String> = match &target_account {
        Some(target) => {
            let declares = |account: &Option<String>| account.as_deref() == Some(target.as_str());
            let same_workspace: Vec<&String> = same_seat
                .iter()
                .filter(|(_, account)| declares(account))
                .map(|(alias, _)| alias)
                .collect();
            let contradicted = same_seat
                .iter()
                .any(|(_, account)| account.is_some() && !declares(account));

            if !same_workspace.is_empty() {
                same_workspace
            } else if contradicted {
                Vec::new()
            } else {
                // Nothing declares a workspace at all: a profile saved before
                // the claim was recorded still owns its own rotated tokens.
                same_seat.iter().map(|(alias, _)| alias).collect()
            }
        }
        // The target declares no workspace, so it cannot prove it belongs to a
        // profile that declares one — the same reasoning `auth_belongs_to_profile`
        // applies, and this resolver is the other way credentials reach a profile.
        //
        // Nor is a claimless sibling then the answer by default: a rotation of
        // the declared account need not carry the claim either, so both remain
        // possible owners and neither is provable. Only a field where nothing
        // declares a workspace leaves a claimless profile as the owner.
        None => {
            if same_seat.iter().any(|(_, account)| account.is_some()) {
                Vec::new()
            } else {
                same_seat.iter().map(|(alias, _)| alias).collect()
            }
        }
    };

    if let [alias] = candidates.as_slice() {
        return Ok(Some((*alias).clone()));
    }
    Ok(None)
}

/// Best-effort: fold a live Codex auth file into the saved profile that owns it.
/// Failures only warn because token capture must not block a requested switch.
fn capture_auth_file_profile_tokens(paths: &Paths, codex_auth: &Path, skip_alias: Option<&str>) {
    if !codex_auth.exists() {
        return;
    }
    // The active marker names the account that installed the live auth file, so
    // it settles a seat saved under two aliases exactly as the pinned alias does
    // for an exec home. It is only a hint: a file that does not match the marked
    // profile is still resolved by inspecting the token.
    let known_alias = if codex_auth == paths.codex_auth_json() {
        get_active_from(paths).ok().flatten()
    } else {
        None
    };
    let Some(alias) = alias_for_auth_json_with_hint(paths, codex_auth, known_alias.as_deref())
    else {
        return;
    };
    if skip_alias == Some(alias.as_str()) {
        return;
    }
    let Ok(dest) = store::profile_dir(paths, &alias).map(|dir| dir.join("auth.json")) else {
        return;
    };
    // The live file is not always the newer copy. A pinned run that already
    // folded a rotated token into this profile leaves the live file behind, and
    // copying it now would undo that rotation.
    if !captured_auth_supersedes_profile(codex_auth, &dest) {
        return;
    }
    if let Err(error) = store::atomic_copy(codex_auth, &dest) {
        eprintln!("warning: failed to capture tokens for profile '{alias}': {error}");
    }
}

pub fn update_meta_plan_from(paths: &Paths, alias: &str, plan: &str) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let Some(_lock) = store::try_lock(paths)? else {
        eprintln!("warning: skipped profile metadata update while the store was busy");
        return Ok(());
    };
    let dir = store::profile_dir(paths, alias)?;
    let meta_path = dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(&meta_path)?;
    let mut meta: Meta = serde_json::from_str(&contents)?;
    meta.alias = alias.to_string();
    meta.plan = Some(plan.to_string());
    let json = serde_json::to_vec_pretty(&meta)?;
    store::atomic_write(&meta_path, &json)
}

pub fn update_meta_plan(alias: &str, plan: &str) -> Result<()> {
    update_meta_plan_from(&config::default_paths()?, alias, plan)
}

// === Default-paths wrappers (used by commands) ===

pub fn list_profiles() -> Result<Vec<Profile>> {
    list_profiles_from(&config::default_paths()?)
}
pub fn get_profile(alias: &str) -> Result<Profile> {
    get_profile_from(&config::default_paths()?, alias)
}
pub fn save_profile(alias: &str, email: Option<&str>, auth_json_src: &Path) -> Result<()> {
    save_profile_to(&config::default_paths()?, alias, email, auth_json_src)
}
pub fn save_profile_and_activate(
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    save_profile_and_activate_to(&config::default_paths()?, alias, email, auth_json_src)
}
pub fn delete_profile(alias: &str) -> Result<()> {
    delete_profile_from(&config::default_paths()?, alias)
}
pub fn get_active() -> Result<Option<String>> {
    get_active_from(&config::default_paths()?)
}
pub fn set_active(alias: &str) -> Result<()> {
    set_active_from(&config::default_paths()?, alias)
}
pub fn switch_to(alias: &str) -> Result<String> {
    switch_to_from(&config::default_paths()?, alias)
}
pub fn switch_to_auth_json(alias: &str, auth_json: &Path) -> Result<String> {
    switch_to_auth_json_from(&config::default_paths()?, alias, auth_json)
}
