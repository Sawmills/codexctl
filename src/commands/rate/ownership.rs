//! Current process ownership, independent of response-log activity.
use serde::Serialize;
#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize)]
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

pub(super) struct Snapshot {
    pub processes: Vec<LiveProcess>,
    pub warnings: Vec<String>,
}

#[cfg(unix)]
pub(super) fn snapshot(host_account: Option<&str>) -> Snapshot {
    use std::path::Path;
    use std::process::Command;
    let mut warnings = Vec::new();
    let home = match dirs::home_dir() {
        Some(home) => home,
        None => {
            warnings
                .push("live process inventory unavailable: cannot locate home directory".into());
            return Snapshot {
                processes: Vec::new(),
                warnings,
            };
        }
    };
    #[cfg(feature = "central-prototype")]
    let launchers = match codexctl::central::native::launch_owners() {
        Ok(launchers) => launchers,
        Err(error) => {
            warnings.push(format!("live launch inventory unavailable: {error:#}"));
            BTreeMap::new()
        }
    };
    #[cfg(not(feature = "central-prototype"))]
    let launchers: BTreeMap<u32, String> = BTreeMap::new();
    let output = match Command::new("ps")
        .args([
            "-u",
            &unsafe { libc::geteuid() }.to_string(),
            "-o",
            "pid=,ppid=,lstart=,stat=,comm=",
        ])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            warnings.push(format!("live process inventory unavailable: {error}"));
            return Snapshot {
                processes: fallback_processes(host_account, &home, &launchers),
                warnings,
            };
        }
    };
    if !output.status.success() {
        warnings.push("live process inventory unavailable: ps returned a failure".into());
        return Snapshot {
            processes: fallback_processes(host_account, &home, &launchers),
            warnings,
        };
    }
    let mut processes = BTreeMap::new();
    let output_text = match std::str::from_utf8(&output.stdout) {
        Ok(text) => text,
        Err(_) => {
            warnings.push("live process inventory unavailable: ps output was not UTF-8".into());
            return Snapshot {
                processes: fallback_processes(host_account, &home, &launchers),
                warnings,
            };
        }
    };
    let mut malformed = 0usize;
    for line in output_text.lines() {
        let mut fields = line.split_whitespace();
        let Some((pid, parent)) = fields
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .zip(fields.next().and_then(|p| p.parse::<u32>().ok()))
        else {
            malformed += 1;
            continue;
        };
        let start = fields.by_ref().take(5).collect::<Vec<_>>().join(" ");
        let Ok(start) = chrono::NaiveDateTime::parse_from_str(&start, "%a %b %e %T %Y") else {
            malformed += 1;
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
    if malformed > 0 {
        warnings
            .push("live process inventory partially unavailable: incompatible ps output".into());
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
    Snapshot {
        processes: result,
        warnings,
    }
}

#[cfg(target_os = "linux")]
fn fallback_processes(
    host_account: Option<&str>,
    home: &std::path::Path,
    launchers: &BTreeMap<u32, String>,
) -> Vec<LiveProcess> {
    let mut processes = BTreeMap::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.first() == Some(&"Z") {
            continue;
        }
        let Some(parent) = fields.get(1).and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(start_ticks) = fields.get(19).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(started_at) = proc_start_epoch(start_ticks) else {
            continue;
        };
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name == "codex" || name == "codex-app-server")
            })
            .unwrap_or(false);
        let argv_codex = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .is_some_and(|bytes| {
                bytes.split(|byte| *byte == 0).any(|arg| {
                    std::path::Path::new(String::from_utf8_lossy(arg).as_ref())
                        .file_name()
                        .is_some_and(|name| name == "codex" || name == "codex-app-server")
                })
            });
        let codex = executable || argv_codex;
        processes.insert(pid, (parent, started_at, codex));
    }
    let mut result = Vec::new();
    for (&pid, &(parent, started_at, codex)) in &processes {
        let mut ancestor = parent;
        let mut visited = BTreeSet::new();
        let mut alias = None;
        while visited.insert(ancestor) {
            if let Some(value) = launchers.get(&ancestor) {
                alias = Some(value.clone());
                break;
            }
            let Some(&(next, _, _)) = processes.get(&ancestor) else {
                break;
            };
            ancestor = next;
        }
        if let Some(alias) = alias {
            if codex {
                result.push(LiveProcess {
                    pid,
                    started_at,
                    account: Some((alias, Source::Launch)),
                });
            }
        } else if codex && let Some(default_home) = process_home(pid, home) {
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
    result
}

#[cfg(target_os = "linux")]
fn proc_start_epoch(start_ticks: u64) -> Option<i64> {
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<i64>()
        .ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (hz > 0).then(|| btime.checked_add(i64::try_from(start_ticks / hz as u64).ok()?))?
}

#[cfg(all(unix, not(target_os = "linux")))]
fn fallback_processes(
    _host_account: Option<&str>,
    _home: &std::path::Path,
    _launchers: &BTreeMap<u32, String>,
) -> Vec<LiveProcess> {
    // Without a process inventory, a non-Linux fallback cannot identify the
    // Codex child below a launcher. Keep the report successful and let the
    // snapshot warning explain why ownership is unavailable.
    Vec::new()
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
pub(super) fn snapshot(_host_account: Option<&str>) -> Snapshot {
    Snapshot {
        processes: Vec::new(),
        warnings: Vec::new(),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn proc_starttime_is_converted_from_boot_ticks_to_unix_seconds() {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let ticks: u64 = fields[19].parse().unwrap();
        let epoch = super::proc_start_epoch(ticks).unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!(epoch > now - 86_400 && epoch <= now);
    }
}
