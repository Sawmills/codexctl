use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api;
use crate::config::{self, Paths};
use crate::store;

#[derive(Serialize, Deserialize, Clone)]
pub struct Meta {
    pub alias: String,
    pub email: Option<String>,
    pub plan: Option<String>,
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
/// home. A re-login of the already-active alias deliberately skips capturing
/// the old live token so it cannot overwrite the new login.
pub fn save_profile_and_activate_to(
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &Path,
) -> Result<()> {
    let alias = store::validate_alias(alias)?;
    let _lock = store::lock(paths)?;
    let was_active = get_active_from(paths)?.as_deref() == Some(alias);
    save_profile_unlocked(paths, alias, email, auth_json_src)?;

    let live_auth = paths.codex_auth_json();
    if auth_json_src != live_auth {
        if !was_active {
            capture_auth_file_profile_tokens(paths, &live_auth);
        }
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

    let meta = Meta {
        alias: alias.to_string(),
        email: email.map(str::to_string),
        plan: None,
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_json = serde_json::to_vec_pretty(&meta)?;
    store::atomic_write(&dir.join("meta.json"), &meta_json)?;
    Ok(())
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
    // from overwriting an unrelated saved profile.
    capture_auth_file_profile_tokens(paths, codex_auth);

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
fn alias_for_exact_token_from(paths: &Paths, auth_json: &Path) -> Option<String> {
    let target = api::read_auth_json(auth_json).ok()?;
    list_profiles_from(paths)
        .ok()?
        .into_iter()
        .find_map(|profile| {
            let stored = api::read_auth_json(&profile.auth_json_path()).ok()?;
            (stored.access_token == target.access_token).then_some(profile.meta.alias)
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
        && auth_files_have_same_access_token(auth_json, &profile.auth_json_path())
    {
        return Some(profile.meta.alias.clone());
    }
    if let Some(alias) = alias_for_exact_token_from(paths, auth_json) {
        return Some(alias);
    }
    if let Some(profile) = hinted
        && auth_files_have_same_owner(auth_json, &profile.auth_json_path())
    {
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
    if candidate.access_token == current.access_token
        && candidate.refresh_token == current.refresh_token
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
    if auth_files_have_same_owner(&live, &profile.auth_json_path()) {
        live
    } else {
        profile.auth_json_path()
    }
}

fn auth_files_have_same_access_token(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (api::read_auth_json(left), api::read_auth_json(right)) else {
        return false;
    };
    left.access_token == right.access_token
}

fn auth_files_have_same_owner(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (api::read_auth_json(left), api::read_auth_json(right)) else {
        return false;
    };
    if left.access_token == right.access_token {
        return true;
    }
    let left_subject = api::token_subject(&left.access_token);
    left_subject.is_some() && left_subject == api::token_subject(&right.access_token)
}

pub fn alias_for_auth_json_from(paths: &Paths, auth_json: &Path) -> Result<Option<String>> {
    let Ok(target_auth) = api::read_auth_json(auth_json) else {
        return Ok(None);
    };
    let target_sub = api::token_subject(&target_auth.access_token);
    let mut profile_auths = Vec::new();
    for profile in list_profiles_from(paths)? {
        let Ok(profile_auth) = api::read_auth_json(&profile.auth_json_path()) else {
            continue;
        };
        profile_auths.push((profile, profile_auth));
    }

    for (profile, profile_auth) in &profile_auths {
        if profile_auth.access_token == target_auth.access_token {
            return Ok(Some(profile.meta.alias.clone()));
        }
    }

    let mut sub_matches = profile_auths
        .into_iter()
        .filter_map(|(profile, profile_auth)| {
            let profile_sub = api::token_subject(&profile_auth.access_token);
            (target_sub.is_some() && target_sub == profile_sub).then_some(profile.meta.alias)
        });
    if let Some(alias) = sub_matches.next()
        && sub_matches.next().is_none()
    {
        return Ok(Some(alias));
    }
    Ok(None)
}

/// Best-effort: fold a live Codex auth file into the saved profile that owns it.
/// Failures only warn because token capture must not block a requested switch.
fn capture_auth_file_profile_tokens(paths: &Paths, codex_auth: &Path) {
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
