use std::path::PathBuf;

use anyhow::{Context, Result};

/// All paths codexctl uses. Testable: construct with a custom root.
#[derive(Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn from_home(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn codexctl_dir(&self) -> PathBuf {
        self.home.join(".codexctl")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.codexctl_dir().join("profiles")
    }

    pub fn active_file(&self) -> PathBuf {
        self.codexctl_dir().join("active")
    }

    pub fn codex_auth_json(&self) -> PathBuf {
        self.home.join(".codex").join("auth.json")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        let profiles = self.profiles_dir();
        std::fs::create_dir_all(&profiles)
            .with_context(|| format!("failed to create {}", profiles.display()))?;
        Ok(())
    }
}

/// Default paths using real home directory.
pub fn default_paths() -> Result<Paths> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(Paths::from_home(home))
}

// Convenience functions that delegate to default_paths()
pub fn codexctl_dir() -> Result<PathBuf> { Ok(default_paths()?.codexctl_dir()) }
pub fn profiles_dir() -> Result<PathBuf> { Ok(default_paths()?.profiles_dir()) }
pub fn active_file() -> Result<PathBuf> { Ok(default_paths()?.active_file()) }
pub fn codex_auth_json() -> Result<PathBuf> { Ok(default_paths()?.codex_auth_json()) }
pub fn ensure_dirs() -> Result<()> { default_paths()?.ensure_dirs() }
