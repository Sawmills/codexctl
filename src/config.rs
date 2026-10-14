use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::store;

/// Resolve a shared Codex configuration without replacing its symlink.
pub(crate) fn codex_config_path(home: &Path) -> Result<PathBuf> {
    let path = home.join("config.toml");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        Ok(std::fs::canonicalize(path)?)
    } else {
        Ok(path)
    }
}

/// Atomically project configuration while preserving its existing permissions.
pub(crate) fn write_codex_config(destination: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = destination
        .parent()
        .context("configuration has no parent")?;
    let permissions = match std::fs::metadata(destination) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions)?;
    }
    file.as_file().sync_all()?;
    file.persist(destination).map_err(|e| e.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

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

    pub fn login_homes_dir(&self) -> PathBuf {
        self.codexctl_dir().join("login-homes")
    }

    /// Root of the per-alias Codex homes that `codexctl exec` pins a child to.
    pub fn exec_homes_dir(&self) -> PathBuf {
        self.codexctl_dir().join("exec-homes")
    }

    pub fn active_file(&self) -> PathBuf {
        self.codexctl_dir().join("active")
    }

    pub fn codex_home(&self) -> PathBuf {
        self.home.join(".codex")
    }

    pub fn codex_auth_json(&self) -> PathBuf {
        self.codex_home().join("auth.json")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        store::ensure_private_dir(&self.codexctl_dir())?;
        let profiles = self.profiles_dir();
        store::ensure_private_dir(&profiles)
            .with_context(|| format!("failed to initialize {}", profiles.display()))?;
        Ok(())
    }
}

/// Default paths using real home directory.
pub fn default_paths() -> Result<Paths> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(Paths::from_home(home))
}

// Convenience functions that delegate to default_paths()
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(default_paths()?.profiles_dir())
}
pub fn active_file() -> Result<PathBuf> {
    Ok(default_paths()?.active_file())
}
pub fn codex_auth_json() -> Result<PathBuf> {
    Ok(default_paths()?.codex_auth_json())
}
pub fn ensure_dirs() -> Result<()> {
    default_paths()?.ensure_dirs()
}
