//! TUI confirmation uses the native completion record, independent of screen size/redraws.
use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use super::diagnostics::{self, Reason};
use super::{Action, MAX_LINE, MESSAGE, Receipt, Turn, identity, valid_id};
use anyhow::{Context, Result};

pub(super) struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    pub(super) fn start(
        paths: &crate::config::Paths,
        listen: SocketAddr,
        launch: String,
    ) -> Result<Self> {
        let root = paths.codexctl_dir();
        let home = paths.codex_home();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::Builder::new()
            .name("terminal-capacity".into())
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_millis(100))
                    .timeout(Duration::from_millis(250))
                    .build();
                let Ok(client) = client else {
                    diagnostics::record(&root, Reason::ClientUnavailable);
                    return;
                };
                let send = |receipt: &Receipt| -> Result<reqwest::blocking::Response> {
                    let body = serde_json::to_vec(receipt)?;
                    let signature = identity::receipt_signature(&root, listen, &body)?;
                    Ok(client
                        .post(format!("http://{listen}/v1/terminal-capacity"))
                        .header("content-type", "application/json")
                        .header("x-codexctl-receipt", signature)
                        .body(body)
                        .send()?
                        .error_for_status()?)
                };
                let mut warned = false;
                let mut state_warned: BTreeSet<(String, String)> = BTreeSet::new();
                loop {
                    let closing = done.load(Ordering::SeqCst);
                    let mut poll = || -> Result<()> {
                        let response = send(&Receipt {
                            thread_id: None,
                            turn_id: None,
                            launch_id: Some(launch.clone()),
                            action: Action::Inspect,
                        })?;
                        let mut bytes = Vec::new();
                        response.take(4097).read_to_end(&mut bytes)?;
                        anyhow::ensure!(bytes.len() <= 4096, "pending receipt response too large");
                        let turns: Vec<Turn> = serde_json::from_slice(&bytes)?;
                        state_warned.retain(|(thread, id)| {
                            turns
                                .iter()
                                .any(|turn| thread == &turn.thread_id && id == &turn.turn_id)
                        });
                        for turn in turns.into_iter().take(8) {
                            if !valid_id(&turn.thread_id) || !valid_id(&turn.turn_id) {
                                continue;
                            }
                            let action = match completed_capacity(&home, &turn) {
                                Ok(true) => Action::Confirm,
                                Ok(false) => continue,
                                Err(_) => {
                                    if state_warned
                                        .insert((turn.thread_id.clone(), turn.turn_id.clone()))
                                    {
                                        diagnostics::record(&root, Reason::NativeStateUnavailable);
                                    }
                                    // A live turn may still be retrying while its
                                    // native state is temporarily unreadable.
                                    // Defer the unconfirmed receipt until the
                                    // launch closes so a later confirmed result
                                    // wins and the pending failure stays intact.
                                    if !closing {
                                        continue;
                                    }
                                    Action::NativeStateUnavailable
                                }
                            };
                            send(&Receipt {
                                thread_id: Some(turn.thread_id),
                                turn_id: Some(turn.turn_id),
                                launch_id: Some(launch.clone()),
                                action,
                            })?;
                        }
                        Ok(())
                    };
                    if poll().is_err() && !warned {
                        diagnostics::record(&root, Reason::NativeCompletionUnavailable);
                        warned = true;
                    }
                    if closing {
                        break;
                    }
                    std::thread::park_timeout(Duration::from_millis(200));
                }
            })
            .context("could not start terminal receipt watcher")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn completed_capacity(home: &Path, turn: &Turn) -> Result<bool> {
    // Codex 0.163's indexed lookup avoids scanning unrelated session logs.
    let db = rusqlite::Connection::open_with_flags(
        home.join("state_5.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(25))?;
    let path: String = db.query_row(
        "SELECT rollout_path FROM threads WHERE id = ?1",
        [&turn.thread_id],
        |row| row.get(0),
    )?;
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path.starts_with(home.join("sessions"))
            && !path.components().any(|c| c == Component::ParentDir),
        "native rollout outside sessions"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "native rollout is not regular");
    let start = metadata.len().saturating_sub(MAX_LINE as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.take(MAX_LINE as u64).read_to_end(&mut tail)?;
    for (index, line) in tail.split(|b| *b == b'\n').enumerate() {
        if index == 0 && start != 0 {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
            continue;
        }
        let payload = &event["payload"];
        if payload["type"] == "task_complete" {
            let id = payload["turn_id"]
                .as_str()
                .context("native completion schema has no turn id")?;
            if id == turn.turn_id && !payload["error"].is_null() {
                let error = &payload["error"];
                let message = error["message"]
                    .as_str()
                    .context("native completion schema has no message")?;
                let code = error
                    .get("codex_error_info")
                    .context("native completion schema has no error class")?;
                return Ok(code == "server_overloaded" && message == MESSAGE);
            }
        }
    }
    Ok(false)
}
