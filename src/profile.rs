use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api;
use crate::config::{self, Paths};

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
    for entry in std::fs::read_dir(&profiles_dir)
        .with_context(|| format!("failed to read {}", profiles_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let meta: Meta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        profiles.push(Profile { meta, dir: path });
    }
    profiles.sort_by(|a, b| a.meta.alias.cmp(&b.meta.alias));
    Ok(profiles)
}

pub fn get_profile_from(paths: &Paths, alias: &str) -> Result<Profile> {
    let dir = paths.profiles_dir().join(alias);
    if !dir.exists() {
        anyhow::bail!("profile '{}' not found", alias);
    }
    let meta_path = dir.join("meta.json");
    let contents = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let meta: Meta = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;
    Ok(Profile { meta, dir })
}

pub fn save_profile_to(
    paths: &Paths,
    alias: &str,
    email: Option<&str>,
    auth_json_src: &std::path::Path,
) -> Result<()> {
    let dir = paths.profiles_dir().join(alias);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let dest = dir.join("auth.json");
    std::fs::copy(auth_json_src, &dest)
        .with_context(|| format!("failed to copy auth.json to {}", dest.display()))?;

    let meta = Meta {
        alias: alias.to_string(),
        email: email.map(|s| s.to_string()),
        plan: None,
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    let meta_json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(dir.join("meta.json"), meta_json)?;
    Ok(())
}

pub fn delete_profile_from(paths: &Paths, alias: &str) -> Result<()> {
    let dir = paths.profiles_dir().join(alias);
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
    let contents = std::fs::read_to_string(&active_file)?;
    let alias = contents.trim().to_string();
    if alias.is_empty() {
        return Ok(None);
    }
    Ok(Some(alias))
}

pub fn set_active_from(paths: &Paths, alias: &str) -> Result<()> {
    let active_file = paths.active_file();
    std::fs::write(&active_file, alias)?;
    Ok(())
}

pub fn switch_to_from(paths: &Paths, alias: &str) -> Result<String> {
    switch_to_auth_json_from(paths, alias, &paths.codex_auth_json())
}

pub fn switch_to_auth_json_from(
    paths: &Paths,
    alias: &str,
    codex_auth: &std::path::Path,
) -> Result<String> {
    let profile = get_profile_from(paths, alias)?;

    // Capture the outgoing active profile's live tokens before we overwrite
    // ~/.codex/auth.json. OpenAI uses single-use rotating refresh tokens, so the
    // Codex CLI may have rotated this profile's tokens since it was saved; if we
    // don't fold them back into the store they're lost and the profile later
    // looks "expired". See capture_active_profile_tokens for the safety guard.
    capture_auth_file_profile_tokens(paths, codex_auth);

    if let Some(parent) = codex_auth.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::copy(profile.auth_json_path(), codex_auth)
        .with_context(|| format!("failed to copy auth.json to {}", codex_auth.display()))?;
    if codex_auth == paths.codex_auth_json() {
        set_active_from(paths, alias)?;
    }
    Ok(profile.meta.email.unwrap_or_else(|| "unknown".to_string()))
}

pub fn alias_for_auth_json_from(
    paths: &Paths,
    auth_json: &std::path::Path,
) -> Result<Option<String>> {
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

    for (profile, profile_auth) in profile_auths {
        let profile_sub = api::token_subject(&profile_auth.access_token);
        if target_sub.is_some() && target_sub == profile_sub {
            return Ok(Some(profile.meta.alias));
        }
    }
    Ok(None)
}

/// Best-effort: copy the live Codex auth file back into the saved profile that
/// owns that auth, but only when the token matches by exact value or seat (`sub`).
/// Failures only warn — they must not block a switch.
fn capture_auth_file_profile_tokens(paths: &Paths, codex_auth: &std::path::Path) {
    if !codex_auth.exists() {
        return;
    }
    let Ok(Some(alias)) = alias_for_auth_json_from(paths, codex_auth) else {
        return;
    };
    let dest = paths.profiles_dir().join(&alias).join("auth.json");
    if let Err(e) = std::fs::copy(codex_auth, &dest) {
        eprintln!("warning: failed to capture tokens for profile '{alias}': {e}");
    }
}

pub fn update_meta_plan(alias: &str, plan: &str) -> Result<()> {
    let dir = config::profiles_dir()?.join(alias);
    let meta_path = dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(&meta_path)?;
    let mut meta: Meta = serde_json::from_str(&contents)?;
    meta.plan = Some(plan.to_string());
    let json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(&meta_path, json)?;
    Ok(())
}

// === Default-paths wrappers (used by commands) ===

pub fn list_profiles() -> Result<Vec<Profile>> {
    list_profiles_from(&config::default_paths()?)
}
pub fn get_profile(alias: &str) -> Result<Profile> {
    get_profile_from(&config::default_paths()?, alias)
}
pub fn save_profile(
    alias: &str,
    email: Option<&str>,
    auth_json_src: &std::path::Path,
) -> Result<()> {
    save_profile_to(&config::default_paths()?, alias, email, auth_json_src)
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
pub fn switch_to_auth_json(alias: &str, auth_json: &std::path::Path) -> Result<String> {
    switch_to_auth_json_from(&config::default_paths()?, alias, auth_json)
}
