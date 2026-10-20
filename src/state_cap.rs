//! Safe, holder-aware maintenance for the local Codex state directory.

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const INDEX_FILE: &str = "state-cap-archive-index.tsv";

#[derive(Debug, Clone)]
pub struct StateCapConfig {
    pub root: PathBuf,
    pub max_bytes: u64,
    pub older_than: Duration,
    pub min_free_bytes: u64,
    pub reserve_bytes: u64,
    pub apply: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StateCapReport {
    pub total_bytes: u64,
    pub final_bytes: u64,
    pub max_bytes: u64,
    pub eligible_bytes: u64,
    pub reclaimed_bytes: u64,
    pub holder_skips: u64,
    pub errors: u64,
    pub free_bytes: u64,
    pub free_space_ok: bool,
    pub cap_met: bool,
}

impl StateCapReport {
    pub fn print(&self, apply: bool) {
        println!(
            "Codex state cap: mode={} total_bytes={} final_bytes={} max_bytes={} eligible_bytes={} reclaimed_bytes={} holder_skips={} errors={} free_bytes={} free_space_ok={} cap_met={}",
            if apply { "apply" } else { "dry-run" },
            self.total_bytes,
            self.final_bytes,
            self.max_bytes,
            self.eligible_bytes,
            self.reclaimed_bytes,
            self.holder_skips,
            self.errors,
            self.free_bytes,
            self.free_space_ok,
            self.cap_met
        );
    }
}

/// Apply or report the local Codex state policy. Mutation is opt-in through
/// `config.apply`; all holder and free-space checks fail closed.
pub fn run(config: &StateCapConfig) -> Result<StateCapReport> {
    if !config.root.is_dir() {
        bail!(
            "Codex state root {} is not a directory",
            config.root.display()
        );
    }
    let _lock = config
        .apply
        .then(|| StateCapLock::acquire(&config.root))
        .transpose()?;
    let mut report = StateCapReport {
        max_bytes: config.max_bytes,
        ..StateCapReport::default()
    };
    report.total_bytes = state_bytes(&config.root)?;
    report.free_bytes = free_bytes(&config.root)?;
    report.free_space_ok = report.free_bytes >= config.min_free_bytes;
    if !report.free_space_ok {
        report.errors += 1;
        eprintln!("state-cap: free space is below the configured floor; apply will skip mutation");
    }

    checkpoint_databases(config, &mut report)?;
    let candidates = rollout_candidates(&config.root, config.older_than)?;
    for path in candidates {
        if config.apply
            && report.total_bytes.saturating_sub(report.reclaimed_bytes) <= config.max_bytes
        {
            break;
        }
        let source_bytes = match eligible_rollout(&path, config, &mut report) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(error) => {
                report.errors += 1;
                eprintln!("state-cap: skipped {}: {error:#}", path.display());
                continue;
            }
        };
        report.eligible_bytes = report.eligible_bytes.saturating_add(source_bytes);
        if config.apply {
            match archive_rollout(&path, &config.root) {
                Ok(bytes) => report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(bytes),
                Err(error) => {
                    report.errors += 1;
                    eprintln!("state-cap: skipped {}: {error:#}", path.display());
                }
            }
        }
    }
    report.final_bytes = state_bytes(&config.root)?;
    report.free_bytes = free_bytes(&config.root)?;
    report.cap_met =
        report.final_bytes <= report.max_bytes && report.free_bytes >= config.min_free_bytes;
    Ok(report)
}

fn checkpoint_databases(config: &StateCapConfig, report: &mut StateCapReport) -> Result<()> {
    if config.apply && free_bytes(&config.root)? < config.min_free_bytes {
        return Ok(());
    }
    for name in ["logs_2.sqlite", "thread_history_1.sqlite"] {
        let db = config.root.join(name);
        if !db.is_file() {
            continue;
        }
        if !all_unheld(
            &[db.clone(), sidecar(&db, "-wal"), sidecar(&db, "-shm")],
            report,
        )? {
            continue;
        }
        if config.apply {
            let status = Command::new("sqlite3")
                .arg(&db)
                .arg("PRAGMA wal_checkpoint(TRUNCATE);")
                .status()
                .with_context(|| format!("cannot run sqlite3 for {}", db.display()))?;
            if !status.success() {
                report.errors += 1;
            }
        }
    }
    Ok(())
}

fn eligible_rollout(
    path: &Path,
    config: &StateCapConfig,
    report: &mut StateCapReport,
) -> Result<Option<u64>> {
    if is_held(path)? {
        report.holder_skips += 1;
        return Ok(None);
    }
    let archive = PathBuf::from(format!("{}.gz", path.display()));
    if archive.exists() {
        let indexed = read_index(&config.root)?
            .iter()
            .any(|entry| entry.archive == archive);
        if !indexed {
            bail!("orphan archive exists beside {}", path.display());
        }
        return Ok(None);
    }
    let size = fs::metadata(path)?.len();
    let free = free_bytes(&config.root)?;
    if free < config.min_free_bytes {
        return Ok(None);
    }
    if free < size.saturating_add(config.reserve_bytes) {
        return Ok(None);
    }
    Ok(Some(size))
}

fn archive_rollout(path: &Path, root: &Path) -> Result<u64> {
    let archive = PathBuf::from(format!("{}.gz", path.display()));
    if archive.exists() {
        return Ok(0);
    }
    let original_size = fs::metadata(path)?.len();
    let original_mtime = fs::metadata(path)?.modified()?;
    let original_sha = sha256(path)?;
    let session_id = session_id_from_path(path)?;
    if is_held(path)? {
        bail!("rollout became held before archiving {}", path.display());
    }
    let temp = PathBuf::from(format!("{}.gz.tmp.{}", path.display(), std::process::id()));
    if temp.exists() {
        bail!("archive temporary path already exists: {}", temp.display());
    }
    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    output_options.mode(0o600);
    let mut output = output_options.open(&temp)?;
    let mut temp_guard = TempGuard::new(temp.clone());
    let status = Command::new("gzip")
        .args(["-n", "-c"])
        .arg(path)
        .stdout(Stdio::from(output.try_clone()?))
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(_) => bail!("gzip failed while archiving {}", path.display()),
        Err(error) => return Err(error).context("failed to run gzip while archiving rollout"),
    }
    output.flush()?;
    output.sync_all()?;
    drop(output);
    let valid = Command::new("gzip")
        .args(["-t"])
        .arg(&temp)
        .status()?
        .success();
    let decompressed_sha = if valid {
        Some(gzip_sha256(&temp)?)
    } else {
        None
    };
    let unchanged = !is_held(path)?
        && fs::metadata(path)?.len() == original_size
        && fs::metadata(path)?.modified()? == original_mtime
        && sha256(path)? == original_sha
        && decompressed_sha.as_deref() == Some(original_sha.as_str());
    if !valid {
        bail!("gzip validation failed for {}", path.display());
    }
    if !unchanged {
        bail!(
            "rollout changed or became held while archiving {}",
            path.display()
        );
    }
    fs::hard_link(&temp, &archive)
        .with_context(|| format!("cannot publish archive {}", archive.display()))?;
    fs::remove_file(&temp)?;
    temp_guard.disarm();
    if let Err(error) = append_index(
        root,
        &ArchiveEntry {
            session_id: session_id.clone(),
            original: path.to_path_buf(),
            archive: archive.clone(),
            _bytes: original_size,
            _mtime: original_mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            sha256: original_sha.clone(),
        },
    ) {
        let _ = fs::remove_file(&archive);
        return Err(error);
    }
    if is_held(path)? || fs::metadata(path)?.len() != original_size || sha256(path)? != original_sha
    {
        let _ = fs::remove_file(&archive);
        let _ = remove_index_entry(root, &session_id);
        return Ok(0);
    }
    fs::remove_file(path)?;
    let archived_size = fs::metadata(&archive)?.len();
    Ok(original_size.saturating_sub(archived_size))
}

fn append_index(root: &Path, entry: &ArchiveEntry) -> Result<()> {
    let path = root.join(INDEX_FILE);
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path)?;
    let original = index_path(root, &entry.original)?;
    let archive = index_path(root, &entry.archive)?;
    writeln!(
        file,
        "{}\t{}\t{}\t{}\t{}\t{}",
        entry.session_id, original, archive, entry._bytes, entry._mtime, entry.sha256
    )?;
    file.sync_all()?;
    Ok(())
}

fn remove_index_entry(root: &Path, session_id: &str) -> Result<()> {
    let path = root.join(INDEX_FILE);
    let entries = read_index(root)?;
    let remaining: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.session_id != session_id)
        .collect();
    let temp = root.join(format!("{}.tmp.{}", INDEX_FILE, std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temp)?;
    let mut temp_guard = TempGuard::new(temp.clone());
    for entry in remaining {
        let original = index_path(root, &entry.original)?;
        let archive = index_path(root, &entry.archive)?;
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}",
            entry.session_id, original, archive, entry._bytes, entry._mtime, entry.sha256
        )?;
    }
    file.sync_all()?;
    fs::rename(&temp, &path)?;
    temp_guard.disarm();
    File::open(root)?.sync_all()?;
    Ok(())
}

fn session_id_from_path(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    let record: serde_json::Value =
        serde_json::from_str(&line).context("invalid rollout metadata")?;
    record
        .get("payload")
        .and_then(|payload| payload.get("session_id").or_else(|| payload.get("id")))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("rollout metadata has no session id")
}

fn gzip_sha256(path: &Path) -> Result<String> {
    let mut gzip = Command::new("gzip")
        .args(["-cd"])
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot start gzip reader")?;
    let stdout = gzip.stdout.take().context("gzip reader had no stdout")?;
    let hash = Command::new("shasum")
        .args(["-a", "256"])
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot start SHA-256 reader")?
        .wait_with_output()?;
    let gzip_status = gzip.wait()?;
    if !gzip_status.success() || !hash.status.success() {
        bail!("compressed rollout hash failed");
    }
    String::from_utf8(hash.stdout)
        .context("SHA-256 output was not UTF-8")?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("SHA-256 output was empty")
}

fn rollout_candidates(root: &Path, age: Duration) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_rollouts(&root.join("sessions"), age, &mut paths)?;
    Ok(paths)
}

fn collect_rollouts(path: &Path, age: Duration, output: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_rollouts(&entry?.path(), age, output)?;
        }
    } else if metadata.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        && SystemTime::now()
            .duration_since(metadata.modified()?)
            .unwrap_or_default()
            >= age
    {
        output.push(path.to_path_buf());
    }
    Ok(())
}

fn state_bytes(root: &Path) -> Result<u64> {
    let mut total: u64 = 0;
    for path in [
        root.join("thread_history_1.sqlite"),
        root.join("thread_history_1.sqlite-wal"),
        root.join("thread_history_1.sqlite-shm"),
        root.join("logs_2.sqlite"),
        root.join("logs_2.sqlite-wal"),
        root.join("logs_2.sqlite-shm"),
    ] {
        total = total.saturating_add(path_bytes(&path)?);
    }
    total = total.saturating_add(path_bytes(&root.join("sessions"))?);
    Ok(total)
}

fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut total: u64 = 0;
    for entry in fs::read_dir(path)? {
        total = total.saturating_add(path_bytes(&entry?.path())?);
    }
    Ok(total)
}

fn free_bytes(path: &Path) -> Result<u64> {
    let output = Command::new("df")
        .args(["-Pk"])
        .arg(path)
        .output()
        .context("cannot run df")?;
    if !output.status.success() {
        bail!("df failed for {}", path.display());
    }
    let output = String::from_utf8(output.stdout)?;
    let line = output
        .lines()
        .nth(1)
        .context("df output was missing a filesystem row")?;
    line.split_whitespace()
        .nth(3)
        .context("df output was missing free blocks")?
        .parse::<u64>()
        .map(|blocks| blocks * 1024)
        .context("df free blocks were not numeric")
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), suffix))
}

fn all_unheld(paths: &[PathBuf], report: &mut StateCapReport) -> Result<bool> {
    for path in paths {
        if path.exists() && is_held(path)? {
            report.holder_skips += 1;
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveEntry {
    session_id: String,
    original: PathBuf,
    archive: PathBuf,
    _bytes: u64,
    _mtime: i64,
    sha256: String,
}

struct TempGuard {
    path: Option<PathBuf>,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

struct StateCapLock {
    path: PathBuf,
    file: File,
}

impl StateCapLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(".state-cap.lock");
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .with_context(|| format!("cannot open state-cap lock {}", path.display()))?;
        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                bail!(
                    "state-cap lock is held; retry after the maintenance run: {}",
                    path.display()
                );
            }
        }
        Ok(Self { path, file })
    }
}

impl Drop for StateCapLock {
    fn drop(&mut self) {
        let _ = &self.path;
        let _ = &self.file;
        #[cfg(not(unix))]
        let _ = fs::remove_file(&self.path);
    }
}

/// Restore an indexed rollout before native `codex resume` starts.
///
/// Returns `true` when an archive was restored and `false` when the session is
/// not in the state-cap index. Existing files are never overwritten silently.
pub fn restore_indexed_rollout(root: &Path, session_id: &str) -> Result<bool> {
    let Some(entry) = read_index(root)?
        .into_iter()
        .find(|entry| entry.session_id == session_id)
    else {
        return Ok(false);
    };
    let _lock = StateCapLock::acquire(root)?;

    if entry.original.exists() {
        if is_held(&entry.original)? {
            bail!("cannot restore session {session_id}: destination is held by a process");
        }
        let current = sha256(&entry.original)?;
        if current == entry.sha256 {
            let _ = remove_index_entry(root, session_id);
            let _ = fs::remove_file(&entry.archive);
            return Ok(false);
        }
        bail!("cannot restore session {session_id}: destination exists with a different SHA-256");
    }
    if !entry.archive.is_file() {
        bail!(
            "cannot restore session {session_id}: archive {} is missing",
            entry.archive.display()
        );
    }

    let parent = entry
        .original
        .parent()
        .context("indexed rollout has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.restore.{}.tmp",
        session_id,
        std::process::id()
    ));
    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    output_options.mode(0o600);
    let mut output = output_options
        .open(&temp)
        .with_context(|| format!("cannot create restore temporary file {}", temp.display()))?;
    let mut temp_guard = TempGuard::new(temp.clone());

    let status = Command::new("gzip")
        .args(["-cd"])
        .arg(&entry.archive)
        .stdout(Stdio::from(output.try_clone()?))
        .status()
        .context("failed to run gzip for session restore")?;
    if !status.success() {
        let _ = fs::remove_file(&temp);
        bail!("cannot restore session {session_id}: gzip failed");
    }
    output.flush()?;
    output.sync_all()?;
    drop(output);

    if is_held(&temp)? || sha256(&temp)? != entry.sha256 {
        let _ = fs::remove_file(&temp);
        bail!("cannot restore session {session_id}: archive hash verification failed");
    }
    if entry.original.exists() {
        let held = is_held(&entry.original)?;
        let _ = fs::remove_file(&temp);
        if held {
            bail!("cannot restore session {session_id}: destination became held during restore");
        }
        bail!("cannot restore session {session_id}: destination changed during restore");
    }
    fs::hard_link(&temp, &entry.original).with_context(|| {
        format!(
            "cannot publish restored session {}",
            entry.original.display()
        )
    })?;
    fs::remove_file(&temp)?;
    temp_guard.disarm();
    File::open(parent)?.sync_all()?;
    remove_index_entry(root, session_id)?;
    let _ = fs::remove_file(&entry.archive);
    Ok(true)
}

fn read_index(root: &Path) -> Result<Vec<ArchiveEntry>> {
    let path = root.join(INDEX_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let parsed = (|| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 6 {
                bail!("invalid state-cap index line");
            }
            let session_id = fields[0].to_owned();
            if !is_safe_session_id(&session_id) {
                bail!("invalid indexed session id");
            }
            Ok(ArchiveEntry {
                session_id,
                original: resolve_index_path(root, fields[1])?,
                archive: resolve_index_path(root, fields[2])?,
                _bytes: fields[3].parse().context("invalid indexed byte count")?,
                _mtime: fields[4].parse().context("invalid indexed mtime")?,
                sha256: fields[5].to_owned(),
            })
        })();
        match parsed {
            Ok(entry) => entries.push(entry),
            Err(error) => eprintln!("state-cap: ignoring malformed archive index line: {error:#}"),
        }
    }
    Ok(entries)
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id != "."
        && session_id != ".."
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
}

fn index_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("index path {} is outside state root", path.display()))?;
    Ok(relative.to_string_lossy().into_owned())
}

fn resolve_index_path(root: &Path, value: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(value);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("indexed path {} is outside state root", path.display()))?;
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("indexed path {} contains traversal", value);
    }
    Ok(path)
}

fn sha256(path: &Path) -> Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .with_context(|| format!("cannot hash {}", path.display()))?;
    if !output.status.success() {
        bail!("SHA-256 failed for {}", path.display());
    }
    let text = String::from_utf8(output.stdout).context("SHA-256 output was not UTF-8")?;
    text.split_whitespace()
        .next()
        .map(str::to_owned)
        .context("SHA-256 output was empty")
}

fn is_held(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let mut command = Command::new("lsof");
        command.arg("-nP");
        #[cfg(target_os = "linux")]
        {
            let mounts = fs::read_to_string("/proc/self/mountinfo")
                .context("cannot inspect mounts for OS open-file check")?;
            for excluded in ["/sys/kernel/debug/tracing", "/sys/kernel/tracing"] {
                if mounts.lines().any(|line| {
                    line.split_once(" - ").is_some_and(|(mount, filesystem)| {
                        mount.split_whitespace().nth(4) == Some(excluded)
                            && filesystem.split_whitespace().next() == Some("tracefs")
                    })
                }) {
                    command.args(["-e", excluded]);
                }
            }
        }
        let output = command
            .args(["-t", "--"])
            .arg(path)
            .output()
            .context("cannot run lsof; refusing state mutation")?;
        if !output.stderr.is_empty() {
            bail!("lsof reported an error; refusing state mutation");
        }
        match output.status.code() {
            Some(0) => Ok(!output.stdout.is_empty()),
            Some(1) if output.stdout.is_empty() => Ok(false),
            _ => bail!("ambiguous lsof result; refusing state mutation"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        bail!("holder checks are supported only on Unix hosts");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn restores_an_indexed_rollout_before_resume() {
        let root = std::env::temp_dir().join(format!(
            "codexctl-state-cap-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = root.join("sessions/2026/10/19");
        fs::create_dir_all(&sessions).unwrap();
        let original = sessions.join("rollout-test.jsonl");
        let archive = sessions.join("rollout-test.jsonl.gz");
        let source = b"{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"test\"}}\n";
        fs::write(&original, source).unwrap();
        let status = Command::new("gzip")
            .args(["-n", "-c"])
            .arg(&original)
            .stdout(File::create(&archive).unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(&original).unwrap();
        let digest = sha256(&archive).unwrap();
        let source_digest = {
            let temp = sessions.join("source.jsonl");
            fs::write(&temp, source).unwrap();
            let result = sha256(&temp).unwrap();
            fs::remove_file(temp).unwrap();
            result
        };
        fs::write(
            root.join(INDEX_FILE),
            format!(
                "test\t{}\t{}\t{}\t0\t{}\n",
                original.display(),
                archive.display(),
                source.len(),
                source_digest
            ),
        )
        .unwrap();

        assert!(restore_indexed_rollout(&root, "test").unwrap());
        assert_eq!(fs::read(&original).unwrap(), source);
        assert_eq!(digest.len(), 64);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_does_not_create_an_archive_index() {
        let root = std::env::temp_dir().join(format!(
            "codexctl-state-cap-dry-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("sessions")).unwrap();
        let report = run(&StateCapConfig {
            root: root.clone(),
            max_bytes: 1,
            older_than: Duration::from_secs(7 * 24 * 60 * 60),
            min_free_bytes: 1,
            reserve_bytes: 1,
            apply: false,
        })
        .unwrap();
        assert!(report.cap_met);
        assert_eq!(report.reclaimed_bytes, 0);
        assert!(!root.join(INDEX_FILE).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_archives_an_old_unheld_rollout_and_records_restore_metadata() {
        let root = std::env::temp_dir().join(format!(
            "codexctl-state-cap-apply-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = root.join("sessions/2026/10/19");
        fs::create_dir_all(&sessions).unwrap();
        let original = sessions.join("rollout-test-apply.jsonl");
        let source = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"test-apply\",\"pad\":\"{}\"}}}}\n",
            "x".repeat(4096)
        )
        .into_bytes();
        let mut file = File::create(&original).unwrap();
        file.write_all(&source).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60)),
        )
        .unwrap();
        drop(file);
        let report = run(&StateCapConfig {
            root: root.clone(),
            max_bytes: 0,
            older_than: Duration::from_secs(7 * 24 * 60 * 60),
            min_free_bytes: 1,
            reserve_bytes: 1,
            apply: true,
        })
        .unwrap();
        assert!(report.reclaimed_bytes > 0);
        assert!(!original.exists());
        assert!(original.with_extension("jsonl.gz").exists());
        assert!(
            fs::read_to_string(root.join(INDEX_FILE))
                .unwrap()
                .starts_with("test-apply\t")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restore_refuses_a_conflicting_destination_and_keeps_archive() {
        let root = std::env::temp_dir().join(format!(
            "codexctl-state-cap-conflict-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = root.join("sessions/2026/10/19");
        fs::create_dir_all(&sessions).unwrap();
        let original = sessions.join("rollout-conflict.jsonl");
        let archive = sessions.join("rollout-conflict.jsonl.gz");
        let source = b"{\"payload\":{\"session_id\":\"conflict\"}}\n";
        let conflicting = b"{\"payload\":{\"session_id\":\"other\"}}\n";
        let source_path = sessions.join("source.jsonl");
        fs::write(&source_path, source).unwrap();
        let digest = sha256(&source_path).unwrap();
        Command::new("gzip")
            .args(["-n", "-c"])
            .arg(&source_path)
            .stdout(File::create(&archive).unwrap())
            .status()
            .unwrap();
        fs::remove_file(source_path).unwrap();
        fs::write(&original, conflicting).unwrap();
        fs::write(
            root.join(INDEX_FILE),
            format!(
                "conflict\t{}\t{}\t{}\t0\t{}\n",
                original.display(),
                archive.display(),
                source.len(),
                digest
            ),
        )
        .unwrap();

        let error = restore_indexed_rollout(&root, "conflict").unwrap_err();
        assert!(error.to_string().contains("different SHA-256"));
        assert_eq!(fs::read(&original).unwrap(), conflicting);
        assert!(archive.exists());
        assert!(root.join(INDEX_FILE).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_respects_the_free_space_floor() {
        let root = std::env::temp_dir().join(format!(
            "codexctl-state-cap-floor-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sessions = root.join("sessions/2026/10/19");
        fs::create_dir_all(&sessions).unwrap();
        let original = sessions.join("rollout-floor.jsonl");
        fs::write(&original, b"{\"payload\":{\"session_id\":\"floor\"}}\n").unwrap();
        let file = File::options().append(true).open(&original).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60)),
        )
        .unwrap();
        drop(file);
        let report = run(&StateCapConfig {
            root: root.clone(),
            max_bytes: 0,
            older_than: Duration::from_secs(7 * 24 * 60 * 60),
            min_free_bytes: u64::MAX,
            reserve_bytes: 0,
            apply: true,
        })
        .unwrap();
        assert_eq!(report.reclaimed_bytes, 0);
        assert!(original.exists());
        assert!(!original.with_extension("jsonl.gz").exists());
        let _ = fs::remove_dir_all(root);
    }
}
