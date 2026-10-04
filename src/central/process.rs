//! Identity of the exact process that owns durable local state.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Clone, Serialize, Deserialize)]
pub struct Process {
    pid: u32,
    incarnation: String,
}
/// All server credential children share process and parent-death isolation.
pub(super) fn isolate(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("credential owner parent exited"));
                }
                Ok(())
            });
        }
    }
}
fn incarnation(pid: u32) -> Result<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .context("invalid process stat")?
            .1
            .split_whitespace()
            .collect();
        if fields.first() == Some(&"Z") {
            return Ok(None);
        }
        let start = fields.get(19).context("missing process start time")?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let namespace = std::fs::read_link(format!("/proc/{pid}/ns/pid"))?;
        Ok(Some(format!(
            "{}:{}:{start}",
            boot.trim(),
            namespace.display()
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()?;
        let start = String::from_utf8(output.stdout)?.trim().to_owned();
        if start.is_empty() {
            return Ok(None);
        }
        if !output.status.success() {
            bail!("cannot identify credential owner");
        }
        Ok(Some(start))
    }
}
impl Process {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn capture(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            incarnation: incarnation(pid)?.context("credential owner already exited")?,
        })
    }
    pub fn alive(&self) -> Result<bool> {
        Ok(incarnation(self.pid)?.is_some_and(|s| s == self.incarnation))
    }
}
/// npm's launcher forks the native owner. Resolve its native executable before spawning.
pub fn owner_binary(binary: &Path) -> Result<PathBuf> {
    let resolved = if binary.components().count() > 1 {
        binary.to_owned()
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|p| p.join(binary))
                    .find(|p| p.is_file())
            })
            .context("Codex executable not found")?
    };
    let resolved = std::fs::canonicalize(resolved)?;
    if resolved.file_name().is_some_and(|s| s == "codex.js") {
        let package = resolved
            .parent()
            .and_then(Path::parent)
            .context("invalid Codex npm package")?;
        let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "x86_64-unknown-linux-musl",
            ("linux", "aarch64") => "aarch64-unknown-linux-musl",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            _ => bail!("unsupported native Codex platform"),
        };
        let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "codex-linux-x64",
            ("linux", "aarch64") => "codex-linux-arm64",
            ("macos", "x86_64") => "codex-darwin-x64",
            ("macos", "aarch64") => "codex-darwin-arm64",
            _ => unreachable!(),
        };
        for vendor in [
            package
                .join("node_modules/@openai")
                .join(platform)
                .join("vendor"),
            package
                .parent()
                .context("invalid npm install")?
                .join(platform)
                .join("vendor"),
            package.join("vendor"),
        ] {
            let native = vendor.join(triple).join("bin/codex");
            if native.is_file() {
                return Ok(std::fs::canonicalize(native)?);
            }
        }
        bail!("native Codex executable missing; reinstall the pinned npm package");
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn when_a_pid_has_a_different_incarnation_then_it_is_not_the_previous_owner() {
        let process = Process {
            pid: std::process::id(),
            incarnation: "old-process-incarnation".into(),
        };
        let alive = process.alive().unwrap();
        assert!(!alive);
    }
    #[test]
    fn when_the_actual_owner_still_exists_then_restart_refuses_it() {
        let process = Process::capture(std::process::id()).unwrap();
        let alive = process.alive().unwrap();
        assert!(alive);
    }
}
