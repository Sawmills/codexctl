//! Byte-preserving repair of the first session_meta record in Codex rollouts.
use super::native::{PROVIDER, SessionProviderAction};
use crate::store;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{self, File, FileTimes, Metadata, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

const RECENT_WINDOW: Duration = Duration::from_secs(60 * 60);
const MAX_HEADER: u64 = 8 * 1024 * 1024;
const BACKUPS: &str = ".codexctl-session-provider-backups";
type BackupIndex = HashMap<OsString, Vec<PathBuf>>;

#[derive(Default)]
struct Summary {
    scanned: usize,
    changed: usize,
    unchanged: usize,
    open: usize,
    unsafe_path: usize,
    no_backup: usize,
    recent: usize,
    errors: usize,
}
impl Summary {
    fn print(&self, action: SessionProviderAction) {
        let label = match action {
            SessionProviderAction::DryRun => "would-change",
            SessionProviderAction::Rewrite => "rewritten",
            SessionProviderAction::Restore => "restored",
        };
        println!(
            "Session providers: scanned={} {label}={} unchanged={} skipped-open={} skipped-path={} skipped-no-backup={} skipped-recent={} errors={}",
            self.scanned,
            self.changed,
            self.unchanged,
            self.open,
            self.unsafe_path,
            self.no_backup,
            self.recent,
            self.errors
        );
    }
}

pub(super) fn run(home: &Path, action: SessionProviderAction) -> Result<()> {
    let home = fs::canonicalize(home)?;
    let mut summary = Summary::default();
    let result = (|| {
        let quote = |path: &Path| -> Result<String> {
            Ok(format!(
                "'{}'",
                path.to_str()
                    .context("restore path must be UTF-8")?
                    .replace('\'', "'\\''")
            ))
        };
        println!("Session provider backups: {}", home.join(BACKUPS).display());
        println!(
            "Restore command: CODEX_HOME={} {} session-provider restore",
            quote(&home)?,
            quote(&std::env::current_exe()?)?
        );
        let open = if home.join("sessions").try_exists()?
            || home.join("archived_sessions").try_exists()?
        {
            open_files(&home)?
        } else {
            HashSet::new()
        };
        let mut backups = BackupIndex::new();
        if matches!(action, SessionProviderAction::Restore) {
            index_backups(&home.join(BACKUPS), &mut backups)?;
        }
        for directory in ["sessions", "archived_sessions"] {
            visit(
                &home,
                &home.join(directory),
                action,
                &open,
                &backups,
                &mut summary,
            )?;
        }
        Ok(())
    })();
    // Partial progress remains visible even when a later file fails.
    summary.errors = usize::from(result.is_err());
    summary.print(action);
    result
}

// Local deactivation removes the central provider. Require explicit restoration
// first, including rollouts that restore skipped because they are open or recent.
pub(super) fn require_restored(home: &Path) -> Result<()> {
    let mut backups = BackupIndex::new();
    index_backups(&home.join(BACKUPS), &mut backups)?;
    if backups.is_empty() {
        return Ok(());
    }
    let mut rollouts = BackupIndex::new();
    for directory in ["sessions", "archived_sessions"] {
        index_backups(&home.join(directory), &mut rollouts)?;
    }
    for (name, paths) in rollouts {
        if !backups.contains_key(&name) {
            continue;
        }
        for path in paths {
            let (current, _) = header(regular(&path)?)?;
            let exact = home.join(BACKUPS).join(path.strip_prefix(home)?);
            if let Some(saved) = restore_backup(&exact, &path, &current, &backups)?
                && current != saved
            {
                bail!(
                    "repaired session {} still needs the server provider; close sessions, wait until rollouts have no modifications for 60 minutes, then run codexctl session-provider restore before switching to local mode",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn visit(
    home: &Path,
    path: &Path,
    action: SessionProviderAction,
    open: &HashSet<(u64, u64)>,
    backups: &BackupIndex,
    summary: &mut Summary,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        summary.unsafe_path += 1;
        println!("skipped-path {}", path.display());
    } else if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            visit(home, &entry.path(), action, open, backups, summary)?;
        }
    } else if metadata.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
        summary.scanned += 1;
        if metadata.nlink() != 1 {
            summary.unsafe_path += 1;
            println!("skipped-path {}", path.display());
        } else if open.contains(&(metadata.dev(), metadata.ino())) {
            summary.open += 1;
            println!("skipped-open {}", path.display());
        } else if skip_recent(path, &metadata, summary)? {
            return Ok(());
        } else {
            repair(home, path, action, &metadata, backups, summary).with_context(|| {
                format!("session provider repair failed for {}", path.display())
            })?;
        }
    }
    Ok(())
}

fn skip_recent(path: &Path, metadata: &Metadata, summary: &mut Summary) -> Result<bool> {
    // Future timestamps also stay protected. An idle session can close its file
    // between appends, so an open-file snapshot alone is insufficient.
    if SystemTime::now()
        .duration_since(metadata.modified()?)
        .unwrap_or_default()
        < RECENT_WINDOW
    {
        summary.recent += 1;
        println!("skipped-recent {}", path.display());
        return Ok(true);
    }
    Ok(false)
}

#[derive(Deserialize)]
struct Record<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
}
#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(borrow)]
    model_provider: Option<&'a RawValue>,
}

fn replacement(line: &[u8]) -> Result<Option<Vec<u8>>> {
    if line.is_empty() {
        return Ok(None);
    }
    let record: Record<'_> = serde_json::from_slice(line).context("invalid rollout metadata")?;
    if record.kind.as_deref() != Some("session_meta") {
        return Ok(None);
    }
    let Some(payload) = record.payload else {
        return Ok(None);
    };
    let payload: Payload<'_> = serde_json::from_str(payload.get())?;
    let Some(provider) = payload.model_provider else {
        return Ok(None);
    };
    if serde_json::from_str::<String>(provider.get())? != "openai" {
        return Ok(None);
    }
    // RawValue borrows the exact token, including its original escapes. No JSON
    // serialization touches the surrounding metadata or any subsequent byte.
    let start = provider.get().as_ptr() as usize - line.as_ptr() as usize;
    let mut output = Vec::with_capacity(line.len() + PROVIDER.len());
    output.extend_from_slice(&line[..start]);
    output.extend_from_slice(serde_json::to_string(PROVIDER)?.as_bytes());
    output.extend_from_slice(&line[start + provider.get().len()..]);
    Ok(Some(output))
}

fn header(file: File) -> Result<(Vec<u8>, BufReader<File>)> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(MAX_HEADER + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() as u64 > MAX_HEADER {
        bail!("session metadata exceeds 8 MiB; file unchanged");
    }
    Ok((line, reader))
}

fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("rollout must be a regular file");
    }
    Ok(file)
}

fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn backup_path(home: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(home)?;
    let result = home.join(BACKUPS).join(relative);
    // Never follow a backup-directory symlink, including intermediate directories.
    let mut parent = home.to_path_buf();
    for part in Path::new(BACKUPS)
        .join(relative)
        .parent()
        .context("missing backup parent")?
        .components()
    {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(m) if !m.is_dir() || m.file_type().is_symlink() => bail!("unsafe backup directory"),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}

fn repair(
    home: &Path,
    path: &Path,
    action: SessionProviderAction,
    before: &Metadata,
    backups: &BackupIndex,
    summary: &mut Summary,
) -> Result<()> {
    let file = regular(path)?;
    let inspected = file.metadata()?;
    if skip_recent(path, &inspected, summary)? {
        return Ok(());
    }
    if !same(before, &inspected) {
        bail!("rollout changed during inspection; retry");
    }
    let (original, mut reader) = header(file)?;
    let backup = backup_path(home, path)?;
    let updated = match action {
        SessionProviderAction::Restore => {
            let Some(saved) = restore_backup(&backup, path, &original, backups)? else {
                summary.no_backup += 1;
                println!("skipped-no-backup {}", path.display());
                return Ok(());
            };
            if saved == original {
                summary.unchanged += 1;
                return Ok(());
            }
            if replacement(&saved)?.as_ref() != Some(&original) {
                bail!("current metadata differs from its backup; refusing restore");
            }
            saved
        }
        _ => match replacement(&original)? {
            Some(updated) => updated,
            None => {
                summary.unchanged += 1;
                return Ok(());
            }
        },
    };
    if matches!(action, SessionProviderAction::DryRun) {
        summary.changed += 1;
        println!("would-change {}", path.display());
        return Ok(());
    }
    let parent = path.parent().context("missing rollout parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&updated)?;
    std::io::copy(&mut reader, temp.as_file_mut())?;
    let copied = reader.get_ref().metadata()?;
    drop(reader);
    if skip_recent(path, &copied, summary)? {
        return Ok(());
    }
    if !same(before, &copied) {
        bail!("rollout changed during copy; file unchanged");
    }
    if matches!(action, SessionProviderAction::Rewrite) {
        save_backup(&backup, &original)?;
    }
    temp.as_file().set_permissions(before.permissions())?;
    temp.as_file().set_times(
        FileTimes::new()
            .set_accessed(before.accessed()?)
            .set_modified(before.modified()?),
    )?;
    temp.as_file().sync_all()?;
    // Include every process, including this CLI's parent. Close our own input
    // before asking the OS, then recheck the inode and timestamps before rename.
    if is_open(path, home)? {
        summary.open += 1;
        println!("skipped-open {}", path.display());
        return Ok(());
    }
    let latest = fs::symlink_metadata(path)?;
    if skip_recent(path, &latest, summary)? {
        return Ok(());
    }
    if !same(before, &latest) {
        bail!("rollout changed during copy; file unchanged");
    }
    temp.persist(path).map_err(|e| e.error)?;
    summary.changed += 1;
    File::open(parent)?
        .sync_all()
        .context("rollout replaced but directory sync failed")?;
    Ok(())
}

fn save_backup(path: &Path, line: &[u8]) -> Result<()> {
    if path.try_exists()? {
        if read_backup(path)? != line {
            bail!("original metadata backup differs; refusing replacement");
        }
        return Ok(());
    }
    let parent = path.parent().context("missing backup parent")?;
    // Create each component privately, including the backup root.
    let mut missing = Vec::new();
    let mut current = parent;
    while !current.try_exists()? {
        missing.push(current);
        current = current.parent().context("missing backup ancestor")?;
    }
    for directory in missing.into_iter().rev() {
        store::ensure_private_dir(directory)?;
        File::open(directory.parent().unwrap())?.sync_all()?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(line)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn lsof(args: &[&std::ffi::OsStr], home: &Path) -> Result<std::process::Output> {
    let mut command = Command::new("lsof");
    command.arg("-nP");
    #[cfg(target_os = "linux")]
    {
        // Unprivileged lsof cannot stat tracefs on some Linux hosts. These
        // kernel tracing files cannot hold rollouts. Exempt only confirmed
        // tracefs mounts, retaining diagnostics for every session filesystem.
        let mounts = fs::read_to_string("/proc/self/mountinfo")
            .context("cannot inspect mounts for OS open-file check")?;
        for path in ["/sys/kernel/debug/tracing", "/sys/kernel/tracing"] {
            if mounts.lines().any(|line| {
                line.split_once(" - ").is_some_and(|(mount, filesystem)| {
                    mount.split_whitespace().nth(4) == Some(path)
                        && filesystem.split_whitespace().next() == Some("tracefs")
                })
            }) {
                command.args(["-e", path]);
            }
        }
    }
    let output = command
        .args(args)
        .output()
        .context("cannot run lsof; no session was rewritten")?;
    let no_matches = output.status.code() == Some(1) && output.stdout.is_empty();
    let mut after_warning = false;
    let has_real_diagnostic = output.stderr.split(|byte| *byte == b'\n').any(|line| {
        if line.is_empty() {
            return false;
        }
        if harmless_lsof_warning(line, home) {
            after_warning = true;
            return false;
        }
        let continuation = after_warning
            && String::from_utf8_lossy(line).trim() == "Output information may be incomplete.";
        after_warning = false;
        !continuation
    });
    if has_real_diagnostic || !(output.status.success() || no_matches) {
        bail!("OS open-file check failed; no further session will be rewritten");
    }
    Ok(output)
}
fn harmless_lsof_warning(line: &[u8], home: &Path) -> bool {
    let line = String::from_utf8_lossy(line);
    line.contains("WARNING: can't stat() nsfs file system /run/docker/netns/")
        || line.contains("WARNING: can't stat() tracefs file system /sys/kernel/debug/tracing")
        || line.contains("WARNING: can't stat() tracefs file system /sys/kernel/tracing")
        || (line.contains("WARNING: can't stat() overlay file system /var/lib/docker/")
            && overlay_warning_is_unrelated(&line, home))
}
#[cfg(target_os = "linux")]
fn overlay_warning_is_unrelated(line: &str, home: &Path) -> bool {
    // Docker's nested overlay mounts are safe to ignore only when mountinfo
    // proves the selected Codex home is outside the affected mount. If mount
    // information is unavailable or the home is inside it, fail closed.
    let Some(warning_path) = line
        .split_once("overlay file system ")
        .and_then(|(_, path)| path.split_whitespace().next())
        .map(Path::new)
    else {
        return false;
    };
    if home.starts_with(warning_path) {
        return false;
    }
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let mounts = mountinfo
        .lines()
        .filter_map(parse_mountinfo)
        .collect::<Vec<_>>();
    let Some(warned_mount) =
        visible_mount(&mounts, warning_path).filter(|mount| mount.filesystem == "overlay")
    else {
        // A warning without a matching mount entry is ambiguous. Keep the
        // inventory fail-closed rather than trusting a pathname alone.
        return false;
    };
    let Some(home_mount) = mounts
        .iter()
        .filter(|mount| home.starts_with(&mount.mountpoint))
        .max_by_key(|mount| mount.mountpoint.components().count())
        .and_then(|mount| visible_mount(&mounts, &mount.mountpoint))
    else {
        return false;
    };
    warned_mount.id != home_mount.id
        && (warned_mount.device != home_mount.device
            || roots_are_disjoint(&warned_mount.root, &home_mount.root))
        && !home.starts_with(&warned_mount.mountpoint)
}
#[cfg(not(target_os = "linux"))]
fn overlay_warning_is_unrelated(_line: &str, _home: &Path) -> bool {
    false
}
#[cfg(target_os = "linux")]
struct MountInfo {
    id: u64,
    parent: u64,
    device: String,
    root: String,
    mountpoint: PathBuf,
    filesystem: String,
}
#[cfg(target_os = "linux")]
fn parse_mountinfo(line: &str) -> Option<MountInfo> {
    let (mount, filesystem) = line.split_once(" - ")?;
    let fields = mount.split_whitespace().collect::<Vec<_>>();
    Some(MountInfo {
        id: fields.first()?.parse().ok()?,
        parent: fields.get(1)?.parse().ok()?,
        device: fields.get(2)?.to_owned().to_string(),
        root: unescape_mountinfo(fields.get(3)?),
        mountpoint: PathBuf::from(unescape_mountinfo(fields.get(4)?)),
        filesystem: filesystem.split_whitespace().next()?.to_owned(),
    })
}
#[cfg(target_os = "linux")]
fn visible_mount<'a>(mounts: &'a [MountInfo], mountpoint: &Path) -> Option<&'a MountInfo> {
    let candidates = mounts
        .iter()
        .filter(|mount| mount.mountpoint == mountpoint)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    let parent = mountpoint
        .parent()
        .and_then(|parent| {
            mounts
                .iter()
                .filter(|mount| {
                    mount.mountpoint != *mountpoint && parent.starts_with(&mount.mountpoint)
                })
                .max_by_key(|mount| (mount.mountpoint.components().count(), mount.id))
        })
        .map(|mount| mount.id);
    candidates
        .iter()
        .copied()
        .filter(|mount| parent.is_none_or(|parent| mount.parent == parent))
        .max_by_key(|mount| mount.id)
        .or_else(|| candidates.into_iter().max_by_key(|mount| mount.id))
}
#[cfg(target_os = "linux")]
fn unescape_mountinfo(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}
#[cfg(target_os = "linux")]
fn roots_are_disjoint(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    !left.starts_with(right) && !right.starts_with(left)
}
fn is_open(path: &Path, home: &Path) -> Result<bool> {
    let output = lsof(
        &["-F".as_ref(), "p".as_ref(), "--".as_ref(), path.as_os_str()],
        home,
    )?;
    match (output.status.code(), output.stdout.is_empty()) {
        (Some(0), false) => Ok(true),
        (Some(1), true) => Ok(false),
        _ => bail!("ambiguous OS open-file result; file unchanged"),
    }
}
fn open_files(home: &Path) -> Result<HashSet<(u64, u64)>> {
    let output = lsof(&["-F".as_ref(), "pDi".as_ref()], home)?;
    let mut files = HashSet::new();
    let mut device = None;
    for line in output.stdout.split(|&b| b == b'\n') {
        match line.first() {
            Some(b'f' | b'p') => device = None,
            Some(b'D') => {
                device = Some(u64::from_str_radix(
                    std::str::from_utf8(&line[1..])?.trim_start_matches("0x"),
                    16,
                )?)
            }
            Some(b'i') => {
                if let Some(device) = device {
                    files.insert((device, std::str::from_utf8(&line[1..])?.parse()?));
                }
            }
            _ => (),
        }
    }
    Ok(files)
}

// Codex moves rollouts between sessions/date/... and archived_sessions while
// preserving the filename. Index names once; verify the entire metadata line
// before accepting a backup from a different relative path.
fn index_backups(path: &Path, index: &mut BackupIndex) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if metadata.file_type().is_symlink() {
        bail!("unsafe backup symlink");
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            index_backups(&entry?.path(), index)?;
        }
    } else if metadata.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
        index
            .entry(
                path.file_name()
                    .context("missing backup filename")?
                    .to_owned(),
            )
            .or_default()
            .push(path.to_owned());
    }
    Ok(())
}

fn read_backup(path: &Path) -> Result<Vec<u8>> {
    let mut file = regular(path)?;
    if file.metadata()?.mode() & 0o077 != 0 {
        bail!("metadata backup must be private (0600)");
    }
    let mut line = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_HEADER + 1)
        .read_to_end(&mut line)?;
    if line.len() as u64 > MAX_HEADER {
        bail!("metadata backup exceeds 8 MiB");
    }
    Ok(line)
}

fn restore_backup(
    exact: &Path,
    rollout: &Path,
    current: &[u8],
    index: &BackupIndex,
) -> Result<Option<Vec<u8>>> {
    if exact.try_exists()? {
        return Ok(Some(read_backup(exact)?));
    }
    let mut found = None;
    if let Some(candidates) = rollout.file_name().and_then(|name| index.get(name)) {
        for candidate in candidates {
            let saved = read_backup(candidate)?;
            if saved == current || replacement(&saved)?.as_deref() == Some(current) {
                if found.is_some() {
                    bail!(
                        "multiple metadata backups match the moved rollout; reconcile backups before restore"
                    );
                }
                found = Some(saved);
            }
        }
        if found.is_none() {
            bail!("metadata differs from backups for this rollout; refusing restore");
        }
    }
    Ok(found)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        MountInfo, harmless_lsof_warning, overlay_warning_is_unrelated, parse_mountinfo,
        roots_are_disjoint, visible_mount,
    };
    use std::fs;
    use std::path::Path;

    #[test]
    fn docker_overlay_warning_is_ignored_outside_the_selected_mount() {
        let Some(mount) = fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .filter_map(parse_mountinfo)
                    .find(|mount: &MountInfo| {
                        mount.filesystem == "overlay"
                            && mount.mountpoint.starts_with("/var/lib/docker/")
                    })
            })
        else {
            return;
        };
        let line = format!(
            "lsof: WARNING: can't stat() overlay file system {}",
            mount.mountpoint.display()
        );
        assert!(overlay_warning_is_unrelated(
            &line,
            Path::new("/tmp/codexctl-home")
        ));
        assert!(harmless_lsof_warning(
            line.as_bytes(),
            Path::new("/tmp/codexctl-home")
        ));
    }

    #[test]
    fn docker_overlay_warning_covering_the_home_is_not_ignored() {
        let line = b"lsof: WARNING: can't stat() overlay file system /var/lib/docker/rootfs/overlayfs/test";
        assert!(!overlay_warning_is_unrelated(
            std::str::from_utf8(line).unwrap(),
            Path::new("/var/lib/docker/rootfs/overlayfs/test/codex")
        ));
        assert!(!harmless_lsof_warning(
            line,
            Path::new("/var/lib/docker/rootfs/overlayfs/test/codex")
        ));
    }

    #[test]
    fn same_device_bind_alias_roots_are_not_assumed_disjoint() {
        assert!(!roots_are_disjoint("/", "/workspace"));
        assert!(!roots_are_disjoint("/workspace", "/workspace/codex"));
        assert!(roots_are_disjoint("/workspace", "/other"));
    }

    #[test]
    fn stacked_mounts_choose_the_visible_entry() {
        let mounts = [
            parse_mountinfo("1 0 8:1 / / rw - ext4 /dev/root rw").unwrap(),
            parse_mountinfo("2 1 8:1 / /var rw - ext4 /dev/root rw").unwrap(),
            parse_mountinfo("3 2 0:1 / /var/lib/docker/rootfs rw - overlay overlay rw").unwrap(),
            parse_mountinfo("4 2 0:2 / /var/lib/docker/rootfs rw - tmpfs tmpfs rw").unwrap(),
        ];
        assert_eq!(
            visible_mount(&mounts, Path::new("/var/lib/docker/rootfs"))
                .unwrap()
                .id,
            4
        );
    }
}
