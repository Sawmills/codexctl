//! Current process ownership, independent of response-log activity.
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Context, bail};
use serde::Serialize;
#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Source {
    #[cfg(unix)]
    Launch,
    #[cfg(unix)]
    HostDefault,
    Log,
}

pub(super) struct LiveProcess {
    pub pid: u32,
    pub started_at: i64,
    pub account: Option<(String, Source)>,
}

#[cfg(unix)]
pub(super) fn snapshot(host_account: Option<&str>) -> Result<Vec<LiveProcess>> {
    use std::path::Path;
    use std::process::Command;
    let home = dirs::home_dir().context("cannot locate home directory")?;
    #[cfg(feature = "central-prototype")]
    let launchers = codexctl::central::native::launch_owners()?;
    #[cfg(not(feature = "central-prototype"))]
    let launchers: BTreeMap<u32, String> = BTreeMap::new();
    let output = Command::new("ps")
        .args([
            "-u",
            &unsafe { libc::geteuid() }.to_string(),
            "-o",
            "pid=,ppid=,lstart=,stat=,comm=",
        ])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .context("cannot inspect live Codex processes")?;
    if !output.status.success() {
        bail!("cannot inspect live Codex processes");
    }
    let mut processes = BTreeMap::new();
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        let mut fields = line.split_whitespace();
        let Some((pid, parent)) = fields
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .zip(fields.next().and_then(|p| p.parse::<u32>().ok()))
        else {
            continue;
        };
        let start = fields.by_ref().take(5).collect::<Vec<_>>().join(" ");
        let Ok(start) = chrono::NaiveDateTime::parse_from_str(&start, "%a %b %e %T %Y") else {
            continue;
        };
        let started_at = start.and_utc().timestamp();
        if fields.next().is_none_or(|state| state.starts_with('Z')) {
            continue;
        }
        let command = fields.collect::<Vec<_>>().join(" ");
        // Linux comm truncates names at 15 bytes (including codex-app-server).
        #[cfg(target_os = "linux")]
        let command = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|path| {
                path.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_owned()
            })
            .unwrap_or(command);
        let codex = Path::new(&command)
            .file_name()
            .is_some_and(|name| name == "codex" || name == "codex-app-server");
        processes.insert(pid, (parent, started_at, codex));
    }
    let mut result = Vec::new();
    for (&pid, &(parent, started_at, codex)) in &processes {
        let mut ancestor = parent;
        let mut visited = BTreeSet::new();
        let mut launch = None;
        while visited.insert(ancestor) {
            if let Some(alias) = launchers.get(&ancestor) {
                // Include the direct child even when an npm/script launcher
                // has not yet handed off to the native Codex binary.
                if codex || ancestor == parent {
                    launch = Some(alias.clone());
                }
                break;
            }
            let Some(&(next, _, _)) = processes.get(&ancestor) else {
                break;
            };
            ancestor = next;
        }
        if let Some(alias) = launch {
            result.push(LiveProcess {
                pid,
                started_at,
                account: Some((alias, Source::Launch)),
            });
        } else if codex && let Some(default_home) = process_home(pid, &home) {
            result.push(LiveProcess {
                pid,
                started_at,
                account: default_home
                    .then_some(host_account)
                    .flatten()
                    .map(|alias| (alias.to_owned(), Source::HostDefault)),
            });
        }
    }
    Ok(result)
}

/// None means another home or unreadable evidence. False means an isolated or
/// pinned home: log evidence may identify it, but the host pointer cannot.
#[cfg(unix)]
fn process_home(pid: u32, home: &std::path::Path) -> Option<bool> {
    let home = home.to_str()?;
    let bytes = process_environment(pid)?;
    let value = |key: &[u8]| {
        bytes
            .split(|b| *b == 0)
            .find_map(|part| part.strip_prefix(key))
    };
    if value(b"HOME=")? != home.as_bytes() {
        return None;
    }
    Some(
        value(b"CODEXCTL_PINNED_ALIAS=").is_none()
            && value(b"CODEX_HOME=").is_none_or(|v| v == format!("{home}/.codex").as_bytes()),
    )
}

#[cfg(target_os = "linux")]
fn process_environment(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

#[cfg(target_os = "macos")]
fn process_environment(pid: u32) -> Option<Vec<u8>> {
    // KERN_PROCARGS2 preserves NUL boundaries. `ps eww` flattens argv and env,
    // so a prompt containing HOME= could otherwise masquerade as ownership.
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        i32::try_from(pid).ok()?,
    ];
    let mut bytes = vec![0_u8; 1024 * 1024];
    let mut length = bytes.len();
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    bytes.truncate(length);
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    let mut cursor = 4;
    // Executable path, padding, then exactly argc NUL-terminated arguments.
    cursor += bytes.get(cursor..)?.iter().position(|b| *b == 0)? + 1;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    for _ in 0..argc {
        cursor += bytes.get(cursor..)?.iter().position(|b| *b == 0)? + 1;
    }
    Some(bytes.get(cursor..)?.to_vec())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_environment(_pid: u32) -> Option<Vec<u8>> {
    None
}

#[cfg(not(unix))]
pub(super) fn snapshot(_host_account: Option<&str>) -> Result<Vec<LiveProcess>> {
    Ok(Vec::new())
}
