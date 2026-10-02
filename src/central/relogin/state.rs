use super::*;

pub(super) const URL: &str = "https://auth.openai.com/codex/device";
pub(super) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Starting,
    Pending,
    Committing,
    Promoted,
    Retiring,
    Completed,
    Failed,
    Canceled,
}
impl Phase {
    pub(super) fn commit_started(&self) -> bool {
        matches!(self, Self::Committing | Self::Promoted | Self::Retiring)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "process", rename_all = "snake_case")]
pub(super) enum Child {
    NotStarted,
    Spawning,
    Running(process::Process),
    Exited,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub sequence: u64,
    pub id: String,
    pub user: String,
    pub device: String,
    pub alias: String,
    pub broker: process::Process,
    pub child: Child,
    // Durable verifier spawn intent, separate from the device-login child.
    #[serde(default)]
    pub verifier_broker: Option<process::Process>,
    pub original_revision: String,
    pub candidate_revision: Option<String>,
    pub candidate: Option<Value>,
    pub phase: Phase,
    pub code: Option<String>,
    pub error: Option<String>,
    pub retired: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::central) struct Request {
    pub alias: String,
    pub id: String,
}
pub(super) fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("invalid login operation");
    }
    Ok(())
}
pub(super) fn directory(state: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(state.join("relogin").join(id))
}
pub(super) fn save(state: &Path, record: &Record) -> Result<()> {
    store::atomic_write(
        &directory(state, &record.id)?.join("record.json"),
        &serde_json::to_vec(record)?,
    )
}
pub(super) fn publish(state: &Path, record: &Record) -> Result<()> {
    let root = state.join("relogin");
    store::ensure_private_dir(&root)?;
    let prepared = tempfile::Builder::new()
        .prefix(".pending-login-")
        .tempdir_in(state)?;
    store::ensure_private_dir(&prepared.path().join("home"))?;
    // This marker is only fallback proof for an unreadable record. Remove and sync
    // it before any spawn attempt. The record governs every normal transition.
    store::atomic_write(&prepared.path().join("home/spawn-failed"), b"not-started")?;
    store::atomic_write(
        &prepared.path().join("record.json"),
        &serde_json::to_vec(record)?,
    )?;
    store::sync_directory(prepared.path())?;
    std::fs::rename(prepared.path(), directory(state, &record.id)?)?;
    store::sync_directory(&root)
}
pub(super) fn load(state: &Path, id: &str) -> Result<Record> {
    let record: Record = serde_json::from_slice(&vault::private_read(
        &directory(state, id)?.join("record.json"),
    )?)?;
    if record.id != id {
        bail!("login operation identity changed");
    }
    Ok(record)
}
fn inventory(state: &Path) -> Result<(Vec<Record>, bool)> {
    let root = state.join("relogin");
    if !root.try_exists()? {
        return Ok((Vec::new(), false));
    }
    let mut result = Vec::new();
    let mut corrupt = false;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid login directory"))?;
        match load(state, &id) {
            Ok(record) => result.push(record),
            Err(_)
                if vault::private_read(&entry.path().join("home/spawn-failed"))
                    .is_ok_and(|b| b == b"not-started") =>
            {
                corrupt = true
            }
            Err(error) => return Err(error),
        }
    }
    result.sort_by_key(|r| r.sequence);
    if result
        .windows(2)
        .any(|pair| pair[0].sequence == pair[1].sequence)
    {
        bail!("ambiguous login operation order");
    }
    Ok((result, corrupt))
}
pub(super) fn records(state: &Path) -> Result<Vec<Record>> {
    Ok(inventory(state)?.0)
}
pub(super) fn current(state: &Path) -> Result<Option<Record>> {
    let (mut records, corrupt) = inventory(state)?;
    if corrupt {
        bail!("account login record requires repair");
    }
    Ok(records.pop())
}
pub(super) fn local_corruption(state: &Path) -> Result<bool> {
    Ok(inventory(state)?.1)
}
pub(super) fn candidate(state: &Path, record: &Record) -> Result<Option<Value>> {
    if let Some(auth) = &record.candidate {
        vault::validate_auth(auth)?;
        return Ok(Some(auth.clone()));
    }
    let path = directory(state, &record.id)?.join("home/auth.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let auth = serde_json::from_slice(&vault::private_read(&path)?)?;
    vault::validate_auth(&auth)?;
    Ok(Some(auth))
}
/// Unusable native output has no known refresh identity after proven process exit.
/// Preserve those bytes but do not make unrelated operations parse them.
pub(super) fn reservation(state: &Path, record: &Record) -> Result<Option<Value>> {
    if record.retired || record.phase == Phase::Completed {
        return Ok(None);
    }
    if record.candidate.is_some() {
        return candidate(state, record);
    }
    if stopped(state, record).is_err() {
        // A live worker has not published a candidate yet. Its selected account
        // remains reserved separately. Startup explicitly audits process exit.
        return Ok(None);
    }
    Ok(candidate(state, record).ok().flatten())
}
pub(super) fn stopped(_state: &Path, record: &Record) -> Result<()> {
    if matches!(record.child, Child::NotStarted | Child::Exited) {
        return Ok(());
    }
    // All production login children spawn on the broker's long-lived main thread
    // with PR_SET_PDEATHSIG. Its recorded incarnation exiting proves child death,
    // even if the broker crashed before persisting the child's PID.
    #[cfg(target_os = "linux")]
    if !record.broker.alive()? {
        return Ok(());
    }
    if let Child::Running(process) = &record.child
        && !process.alive()?
    {
        return Ok(());
    }
    bail!("login child is live or cannot be identified")
}
pub(super) fn job_key(state: &Path, id: &str) -> String {
    format!("{}\0{id}", state.display())
}
pub(super) fn response(record: &Record) -> Response {
    let status = match record.phase {
        Phase::Starting => "starting",
        Phase::Pending => "pending",
        Phase::Committing | Phase::Promoted | Phase::Retiring => "verifying",
        Phase::Completed => "completed",
        Phase::Failed => "failed",
        Phase::Canceled => "canceled",
    };
    (
        [("cache-control", "no-store")],
        Json(json!({
            "id":record.id, "alias":record.alias.trim(), "userId":record.user, "status":status,
            "verificationUrl":if record.phase == Phase::Pending { Some(URL) } else { None },
            "userCode":if record.phase == Phase::Pending { record.code.as_deref() } else { None },
            "error":record.error,
        })),
    )
        .into_response()
}
