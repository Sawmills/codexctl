//! Best-effort launcher diagnostics must never borrow the child's terminal.
use std::{
    fs::{OpenOptions, Permissions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use serde::Serialize;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    WatcherStartFailed,
    CapacityPolicyStopped,
    DeliveryFailed,
    ClientUnavailable,
    NativeStateUnavailable,
    NativeCompletionUnavailable,
}

pub(super) fn record(root: &Path, reason: Reason) {
    // Logging failure is optional evidence, never a reason to interrupt a child.
    let _ = (|| -> anyhow::Result<()> {
        crate::store::ensure_private_dir(root)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("terminal-capacity.jsonl"))?;
        anyhow::ensure!(file.metadata()?.is_file(), "diagnostic log is not regular");
        file.set_permissions(Permissions::from_mode(0o600))?;
        let mut line = serde_json::to_vec(&serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "reason": reason,
        }))?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    })();
}
