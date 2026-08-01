use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

use crate::config::Paths;

const MAX_ALIAS_BYTES: usize = 128;

/// Validate one profile-store path component.
///
/// Email addresses and other existing aliases stay valid. Path syntax,
/// control characters, and dot-prefixed staging names do not.
pub fn validate_alias(alias: &str) -> Result<&str> {
    let alias = alias.trim();
    if alias.is_empty() {
        bail!("profile alias cannot be empty");
    }
    if alias.len() > MAX_ALIAS_BYTES {
        bail!("profile alias cannot exceed {MAX_ALIAS_BYTES} bytes");
    }
    if !alias.is_ascii() {
        bail!("profile alias must contain only ASCII characters");
    }
    if alias.starts_with('.') {
        bail!("profile alias cannot start with '.'");
    }
    if alias.chars().any(|c| c.is_control()) {
        bail!("profile alias cannot contain control characters");
    }
    if alias.contains('/') || alias.contains('\\') {
        bail!("profile alias cannot contain path separators");
    }

    let mut components = Path::new(alias).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("profile alias must be one path component");
    }
    Ok(alias)
}

/// Return a checked profile directory below the profile-store root.
pub fn profile_dir(paths: &Paths, alias: &str) -> Result<PathBuf> {
    let alias = validate_alias(alias)?;
    checked_child(&paths.profiles_dir(), alias)
}

/// Return a checked isolated login home below the login-home root.
pub fn login_home(paths: &Paths, alias: &str) -> Result<PathBuf> {
    let alias = validate_alias(alias)?;
    checked_child(&paths.login_homes_dir(), alias)
}

fn checked_child(root: &Path, name: &str) -> Result<PathBuf> {
    if root.exists() {
        for entry in
            std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let Some(existing) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if existing != name && existing.eq_ignore_ascii_case(name) {
                bail!(
                    "profile alias conflicts with existing alias '{}' by letter case",
                    existing
                );
            }
        }
    }
    let child = root.join(name);
    let relative = child
        .strip_prefix(root)
        .context("profile path escaped its store root")?;
    let mut components = relative.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("profile path escaped its store root");
    }
    if std::fs::symlink_metadata(&child).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("profile path cannot be a symbolic link");
    }
    Ok(child)
}

/// An exclusive lock for profile-store mutations and live auth switches.
pub struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub fn lock(paths: &Paths) -> Result<StoreLock> {
    ensure_private_dir(&paths.codexctl_dir())?;
    let lock_path = paths.codexctl_dir().join("store.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    set_private_file_permissions(&lock_path)?;
    file.lock_exclusive()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(StoreLock { file })
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    set_private_dir_permissions(path)
}

/// Replace a file atomically with bytes from `source`.
pub fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
    let mut input =
        File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
    atomic_replace(destination, |output| {
        std::io::copy(&mut input, output)
            .with_context(|| format!("failed to copy {}", source.display()))?;
        Ok(())
    })
}

/// Replace a file atomically with the supplied bytes.
pub fn atomic_write(destination: &Path, contents: &[u8]) -> Result<()> {
    atomic_replace(destination, |output| {
        output
            .write_all(contents)
            .with_context(|| format!("failed to write {}", destination.display()))
    })
}

fn atomic_replace(destination: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let parent = destination
        .parent()
        .with_context(|| format!("{} has no parent directory", destination.display()))?;
    ensure_private_dir(parent)?;

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("destination file name is not valid UTF-8")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut temp_path = None;
    let mut temp_file = None;
    for attempt in 0..16 {
        let candidate = parent.join(format!(
            ".{file_name}.codexctl-{}-{nonce}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create temporary file in {}", parent.display())
                });
            }
        }
    }

    let temp_path = temp_path.context("failed to allocate a unique temporary file")?;
    let mut temp_file = temp_file.context("failed to open a temporary file")?;
    let result = (|| {
        write(&mut temp_file)?;
        temp_file
            .sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        set_private_file_permissions(&temp_path)?;
        drop(temp_file);
        std::fs::rename(&temp_path, destination)
            .with_context(|| format!("failed to atomically replace {}", destination.display()))?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set private permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set private permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("failed to sync {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_validation_accepts_email_aliases() {
        assert_eq!(
            validate_alias("  amir+8@sawmills.ai  ").unwrap(),
            "amir+8@sawmills.ai"
        );
    }

    #[test]
    fn alias_validation_rejects_path_syntax() {
        for alias in [
            "../escape",
            "a/b",
            "a\\b",
            ".",
            "..",
            ".hidden",
            "/tmp/x",
            "amír@example.com",
        ] {
            assert!(validate_alias(alias).is_err(), "accepted {alias:?}");
        }
    }

    #[test]
    fn profile_path_is_one_child_below_store() {
        let paths = Paths::from_home(PathBuf::from("/tmp/codexctl-store-test"));
        assert_eq!(
            profile_dir(&paths, "a@example.com").unwrap(),
            paths.profiles_dir().join("a@example.com")
        );
    }

    #[test]
    fn profile_path_rejects_case_fold_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(temp.path().to_path_buf());
        ensure_private_dir(&paths.profiles_dir()).unwrap();
        ensure_private_dir(&paths.profiles_dir().join("Work")).unwrap();

        assert!(profile_dir(&paths, "work").is_err());
        assert!(profile_dir(&paths, "Work").is_ok());
    }
}
