//! Codex's shared app-server daemon.
//!
//! With `features.daemon_auto_start`, every `codex` TUI attaches to one
//! long-lived app server per Codex home. That server reads `auth.json` once at
//! startup and ignores a later swap to another account: its token refresh
//! re-reads the file only while the account id still matches. A switch reaches
//! the daemon only through a restart, and a restart stops every turn it runs.
//!
//! This module finds the daemon, restarts it, and resumes the sessions that a
//! usage limit or the restart itself stopped.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

const CONTROL_SOCKET: &str = "app-server-control/app-server-control.sock";
const PID_FILE: &str = "app-server-daemon/daemon.pid";
/// The daemon speaks WebSocket over its Unix socket; the URL only names the
/// handshake target, exactly as Codex's own client sends it.
const HANDSHAKE_URL: &str = "ws://localhost/rpc";
/// How long the client waits for one response.
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    request: Duration,
    /// Loading a long session from its rollout can take well over a minute.
    resume: Duration,
}

const TIMEOUTS: Timeouts = Timeouts {
    request: Duration::from_secs(20),
    resume: Duration::from_secs(180),
};
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The pid of the daemon that serves `codex_home`, if one is alive.
///
/// The daemon removes its pid file on a clean stop, so a file that names a
/// dead process is a crash leftover and does not count.
pub fn running_pid(codex_home: &Path) -> Option<i32> {
    let contents = std::fs::read_to_string(codex_home.join(PID_FILE)).ok()?;
    let pid = serde_json::from_str::<Value>(&contents)
        .ok()?
        .get("pid")?
        .as_i64()?;
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    process_alive(pid).then_some(pid)
}

/// Whether `codex_home` links its daemon state from another home, as a
/// pinned exec home does. Its daemon then belongs to that other home.
pub fn shares_daemon(codex_home: &Path) -> bool {
    std::fs::symlink_metadata(codex_home.join("app-server-daemon"))
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn process_alive(pid: i32) -> bool {
    // Signal 0 checks existence without delivering anything. EPERM still
    // means the process exists, just under another user.
    let delivered = unsafe { libc::kill(pid, 0) } == 0;
    delivered || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Restart the daemon that serves `codex_home` through the Codex CLI.
pub fn restart(codex_home: &Path) -> Result<()> {
    restart_with(Path::new("codex"), codex_home)
}

fn restart_with(codex: &Path, codex_home: &Path) -> Result<()> {
    let output = Command::new(codex)
        .args(["app-server", "daemon", "restart"])
        .env("CODEX_HOME", codex_home)
        .output()
        .with_context(|| {
            format!(
                "failed to run `{} app-server daemon restart`",
                codex.display()
            )
        })?;
    let status = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|report| report.get("status")?.as_str().map(str::to_string));
    if output.status.success() && matches!(status.as_deref(), Some("restarted" | "started")) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "`codex app-server daemon restart` failed ({}): {}",
        output.status,
        stderr.trim()
    )
}

/// The account a running daemon authenticates as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonAccount {
    pub account_id: Option<String>,
    pub email: Option<String>,
    /// Why `account/read` failed, when it did. The daemon still runs on some
    /// account, so a failed read must not hide that a restart may be due.
    pub unreadable: Option<String>,
}

impl DaemonAccount {
    fn from_account_read(result: &Value) -> Self {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
        Self {
            account_id: text(result.pointer("/workspaceRouting/chatgptAccountId")),
            email: text(result.pointer("/account/email")),
            unreadable: None,
        }
    }

    fn unreadable(error: &anyhow::Error) -> Self {
        Self {
            account_id: None,
            email: None,
            unreadable: Some(format!("{error:#}")),
        }
    }

    /// Whether this is the login `email` in the workspace `account_id`.
    pub fn is(&self, account_id: &str, email: &str) -> bool {
        self.account_id.as_deref() == Some(account_id)
            && self
                .email
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(email))
    }

    pub fn display(&self) -> String {
        if let Some(name) = self.email.as_deref().or(self.account_id.as_deref()) {
            return name.to_string();
        }
        match &self.unreadable {
            Some(error) => format!("an account codexctl could not read ({error})"),
            None => "an unknown account".to_string(),
        }
    }
}

/// Why a session needs a new turn after the restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Its last turn failed because the account ran out of usage.
    UsageLimit,
    /// Its turn is still running, and the restart will stop it.
    Interrupted,
}

impl StopReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::UsageLimit => "stopped at the usage limit",
            Self::Interrupted => "interrupted by the restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedSession {
    pub thread_id: String,
    pub title: String,
    pub reason: StopReason,
}

/// What resuming one session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumed {
    /// A new turn runs with the prompt.
    Started,
    /// The turn finished while the old daemon drained, so nothing was sent.
    AlreadyFinished,
}

/// Decide whether a loaded thread needs a turn after the restart.
///
/// Subagent threads are left to the parent that drives them, and ephemeral
/// threads have no rollout on disk to resume from.
fn stop_reason(thread: &Value, last_turn: Option<&Value>) -> Option<StopReason> {
    if thread.get("parentThreadId").is_some_and(|id| !id.is_null())
        || thread.get("ephemeral").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let turn = last_turn?;
    match turn.get("status").and_then(Value::as_str)? {
        "inProgress" => Some(StopReason::Interrupted),
        "failed" if turn_failed_on_usage_limit(turn) => Some(StopReason::UsageLimit),
        _ => None,
    }
}

fn turn_failed_on_usage_limit(turn: &Value) -> bool {
    let Some(error) = turn.get("error") else {
        return false;
    };
    if error.get("codexErrorInfo").and_then(Value::as_str) == Some("usageLimitExceeded") {
        return true;
    }
    // A workspace spend cap reaches the client as a plain message.
    error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("spend cap"))
}

fn session_title(thread: &Value) -> String {
    let text = |key: &str| {
        thread
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty())
    };
    let title = text("name")
        .or_else(|| text("preview"))
        .unwrap_or_else(|| "(untitled)".to_string());
    if title.chars().count() > 60 {
        format!("{}…", title.chars().take(59).collect::<String>())
    } else {
        title
    }
}

/// A JSON-RPC client on the daemon's control socket.
pub struct Client {
    socket: WebSocket<UnixStream>,
    next_id: u64,
    /// Notifications that arrived while a response was awaited.
    notifications: Vec<Value>,
    timeouts: Timeouts,
}

impl Client {
    pub fn connect(codex_home: &Path) -> Result<Self> {
        Self::connect_socket(&codex_home.join(CONTROL_SOCKET))
    }

    pub fn connect_socket(socket_path: &Path) -> Result<Self> {
        Self::connect_socket_with(socket_path, TIMEOUTS)
    }

    fn connect_socket_with(socket_path: &Path, timeouts: Timeouts) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).with_context(|| {
            format!(
                "failed to connect to the Codex app-server daemon at {}",
                socket_path.display()
            )
        })?;
        stream.set_read_timeout(Some(timeouts.request))?;
        stream.set_write_timeout(Some(timeouts.request))?;
        let (socket, _) = tungstenite::client(HANDSHAKE_URL, stream)
            .map_err(|error| anyhow::anyhow!("daemon websocket handshake failed: {error}"))?;
        let mut client = Self {
            socket,
            next_id: 0,
            notifications: Vec::new(),
            timeouts,
        };
        client.request(
            "initialize",
            json!({"clientInfo": {"name": "codexctl", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        client.send(&json!({"method": "initialized"}))?;
        Ok(client)
    }

    /// Connect to a daemon that is still coming up after a restart.
    fn connect_when_ready(codex_home: &Path) -> Result<Self> {
        let deadline = Instant::now() + RECONNECT_TIMEOUT;
        loop {
            match Self::connect(codex_home) {
                Ok(client) => return Ok(client),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    pub fn account(&mut self) -> Result<DaemonAccount> {
        let result = self.request("account/read", json!({"refreshToken": false}))?;
        Ok(DaemonAccount::from_account_read(&result))
    }

    /// Loaded sessions that need a turn once the daemon comes back.
    pub fn stopped_sessions(&mut self) -> Result<Vec<StoppedSession>> {
        let mut stopped = Vec::new();
        for thread_id in self.loaded_thread_ids()? {
            let thread = self.request("thread/read", json!({"threadId": thread_id}))?;
            let turns = self.request(
                "thread/turns/list",
                json!({"threadId": thread_id, "limit": 1, "itemsView": "notLoaded"}),
            )?;
            let thread = thread.get("thread").unwrap_or(&Value::Null);
            let last_turn = turns
                .get("data")
                .and_then(Value::as_array)
                .and_then(|t| t.first());
            if let Some(reason) = stop_reason(thread, last_turn) {
                stopped.push(StoppedSession {
                    thread_id,
                    title: session_title(thread),
                    reason,
                });
            }
        }
        Ok(stopped)
    }

    fn loaded_thread_ids(&mut self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let page = self.request("thread/loaded/list", json!({"cursor": cursor}))?;
            ids.extend(
                page.get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string),
            );
            match page.get("nextCursor") {
                Some(next) if !next.is_null() => cursor = next.clone(),
                _ => return Ok(ids),
            }
        }
    }

    /// Load `thread_id` and start a turn that sends `prompt`, with no sandbox
    /// and no approval prompts.
    ///
    /// A thread whose turn a forced restart killed comes back with the daemon
    /// already continuing it under the session's old permissions. Input sent
    /// then would join that turn and drop the overrides, so the turn is
    /// interrupted first. A thread whose turn finished while the old daemon
    /// drained needs nothing.
    ///
    /// Returns once the new turn runs. The daemon never unloads a running
    /// thread, so the turn outlives this connection.
    pub fn resume(&mut self, thread_id: &str, prompt: &str) -> Result<Resumed> {
        self.request_within(
            self.timeouts.resume,
            "thread/resume",
            json!({
                "threadId": thread_id,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
                "excludeTurns": true,
            }),
        )?;
        if let Some(turn) = self.last_turn(thread_id)? {
            match turn.get("status").and_then(Value::as_str) {
                Some("completed") => return Ok(Resumed::AlreadyFinished),
                Some("inProgress") => {
                    let turn_id = turn.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.request(
                        "turn/interrupt",
                        json!({"threadId": thread_id, "turnId": turn_id}),
                    )?;
                    self.wait_for_turn_event("turn/completed", thread_id, turn_id)?;
                }
                _ => {}
            }
        }
        let started = self.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": prompt, "text_elements": []}],
                "approvalPolicy": "never",
                "sandboxPolicy": {"type": "dangerFullAccess"},
            }),
        )?;
        let turn_id = started
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("turn/start returned no turn id")?
            .to_string();
        self.wait_for_turn_event("turn/started", thread_id, &turn_id)?;
        Ok(Resumed::Started)
    }

    fn last_turn(&mut self, thread_id: &str) -> Result<Option<Value>> {
        let turns = self.request(
            "thread/turns/list",
            json!({"threadId": thread_id, "limit": 1, "itemsView": "notLoaded"}),
        )?;
        Ok(turns
            .get("data")
            .and_then(Value::as_array)
            .and_then(|turns| turns.first())
            .cloned())
    }

    fn wait_for_turn_event(&mut self, method: &str, thread_id: &str, turn_id: &str) -> Result<()> {
        let matches = |message: &Value| {
            message.get("method").and_then(Value::as_str) == Some(method)
                && message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
                && message.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id)
        };
        if let Some(index) = self.notifications.iter().position(matches) {
            self.notifications.remove(index);
            return Ok(());
        }
        loop {
            let message = self.receive()?;
            if matches(&message) {
                return Ok(());
            }
            self.answer_server_request(&message)?;
        }
    }

    fn request_within(&mut self, timeout: Duration, method: &str, params: Value) -> Result<Value> {
        self.socket.get_ref().set_read_timeout(Some(timeout))?;
        let result = self.request(method, params);
        self.socket
            .get_ref()
            .set_read_timeout(Some(self.timeouts.request))?;
        result
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            let message = self.receive()?;
            if message.get("method").is_some() {
                self.answer_server_request(&message)?;
                if message.get("id").is_none() {
                    self.notifications.push(message);
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("daemon rejected {method}: {detail}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Refuse a request the daemon sends to this client, so it does not wait
    /// on an answer that never comes. With approvals off, none are expected.
    fn answer_server_request(&mut self, message: &Value) -> Result<()> {
        let (Some(id), Some(method)) = (message.get("id"), message.get("method")) else {
            return Ok(());
        };
        let reply = json!({
            "id": id,
            "error": {"code": -32601, "message": format!("codexctl does not handle {method}")},
        });
        self.send(&reply)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        self.socket
            .send(Message::text(message.to_string()))
            .map_err(|error| anyhow::anyhow!("failed to write to the daemon: {error}"))
    }

    fn receive(&mut self) -> Result<Value> {
        loop {
            let message = self
                .socket
                .read()
                .map_err(|error| anyhow::anyhow!("failed to read from the daemon: {error}"))?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Message::Close(_) => bail!("the daemon closed the connection"),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            return serde_json::from_str(&text).context("the daemon sent invalid JSON");
        }
    }
}

/// A running daemon and what a restart would stop.
pub struct Inspection {
    pub pid: i32,
    pub account: DaemonAccount,
    pub sessions: Vec<StoppedSession>,
}

/// Inspect the daemon that serves `codex_home`, or `None` when none runs.
///
/// `exclude` names a session the caller resumes itself.
pub fn inspect(codex_home: &Path, exclude: Option<&str>) -> Result<Option<Inspection>> {
    let Some(pid) = running_pid(codex_home) else {
        return Ok(None);
    };
    let mut client = Client::connect(codex_home)?;
    let account = client
        .account()
        .unwrap_or_else(|error| DaemonAccount::unreadable(&error));
    let mut sessions = client.stopped_sessions()?;
    sessions.retain(|session| Some(session.thread_id.as_str()) != exclude);
    Ok(Some(Inspection {
        pid,
        account,
        sessions,
    }))
}

/// Restart the daemon, then resume every session in `sessions` with `prompt`.
///
/// Each resume is reported as it lands. A failed resume does not stop the
/// others. A failed restart is an error; the sessions that did not resume are
/// returned by id.
pub fn restart_and_resume(
    codex_home: &Path,
    sessions: &[StoppedSession],
    prompt: &str,
    out: &mut impl Write,
) -> Result<Vec<String>> {
    restart_and_resume_with(Path::new("codex"), codex_home, sessions, prompt, out)
}

fn restart_and_resume_with(
    codex: &Path,
    codex_home: &Path,
    sessions: &[StoppedSession],
    prompt: &str,
    out: &mut impl Write,
) -> Result<Vec<String>> {
    if sessions
        .iter()
        .any(|session| session.reason == StopReason::Interrupted)
    {
        let _ = writeln!(
            out,
            "codexctl: restarting the Codex app-server daemon; running turns get up to 60 s to finish"
        );
    }
    restart_with(codex, codex_home)?;
    let new_pid = running_pid(codex_home)
        .map(|pid| format!(" (pid {pid})"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "codexctl: restarted the Codex app-server daemon{new_pid}"
    );
    if sessions.is_empty() {
        return Ok(Vec::new());
    }

    let mut client = match Client::connect_when_ready(codex_home) {
        Ok(client) => client,
        Err(error) => {
            let _ = writeln!(
                out,
                "codexctl: could not reach the restarted daemon to resume sessions: {error:#}"
            );
            return Ok(sessions.iter().map(|s| s.thread_id.clone()).collect());
        }
    };
    let mut failed = Vec::new();
    for session in sessions {
        match client.resume(&session.thread_id, prompt) {
            Ok(Resumed::Started) => {
                let _ = writeln!(
                    out,
                    "codexctl: resumed {} {} ({})",
                    session.thread_id,
                    session.title,
                    session.reason.describe()
                );
            }
            Ok(Resumed::AlreadyFinished) => {
                let _ = writeln!(
                    out,
                    "codexctl: {} {} finished before the restart; not resumed",
                    session.thread_id, session.title
                );
            }
            Err(error) => {
                let _ = writeln!(
                    out,
                    "codexctl: failed to resume {} {}: {error:#}",
                    session.thread_id, session.title
                );
                failed.push(session.thread_id.clone());
            }
        }
    }
    Ok(failed)
}

/// The Codex home the live `auth.json` belongs to.
pub fn codex_home_of(auth_json: &Path) -> PathBuf {
    auth_json
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn short_tempdir() -> tempfile::TempDir {
        // Unix socket paths must stay under ~104 bytes, which the default
        // macOS temp root can exceed.
        tempfile::Builder::new()
            .prefix("cxd")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn running_pid_requires_a_live_process() {
        let home = short_tempdir();
        assert_eq!(running_pid(home.path()), None);

        std::fs::create_dir_all(home.path().join("app-server-daemon")).unwrap();
        let pid_file = home.path().join(PID_FILE);
        let own = std::process::id() as i32;
        std::fs::write(
            &pid_file,
            format!(r#"{{"pid":{own},"processStartTime":"x"}}"#),
        )
        .unwrap();
        assert_eq!(running_pid(home.path()), Some(own));

        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id() as i32;
        child.wait().unwrap();
        std::fs::write(&pid_file, format!(r#"{{"pid":{dead}}}"#)).unwrap();
        assert_eq!(running_pid(home.path()), None);

        std::fs::write(&pid_file, "not json").unwrap();
        assert_eq!(running_pid(home.path()), None);
    }

    #[test]
    fn shares_daemon_only_for_a_linked_daemon_directory() {
        let live = short_tempdir();
        std::fs::create_dir_all(live.path().join("app-server-daemon")).unwrap();
        assert!(!shares_daemon(live.path()));

        let pinned = short_tempdir();
        std::os::unix::fs::symlink(
            live.path().join("app-server-daemon"),
            pinned.path().join("app-server-daemon"),
        )
        .unwrap();
        assert!(shares_daemon(pinned.path()));

        let empty = short_tempdir();
        assert!(!shares_daemon(empty.path()));
    }

    #[test]
    fn stop_reason_picks_usage_limit_failures_and_running_turns() {
        let thread = json!({"id": "t", "parentThreadId": null, "ephemeral": false});
        let limit = json!({"status": "failed", "error": {"message": "x", "codexErrorInfo": "usageLimitExceeded"}});
        let spend_cap = json!({"status": "failed", "error": {"message": "You hit your spend cap set in your workspace."}});
        let running = json!({"status": "inProgress"});
        let overloaded = json!({"status": "failed", "error": {"message": "x", "codexErrorInfo": "serverOverloaded"}});
        let done = json!({"status": "completed"});

        assert_eq!(
            stop_reason(&thread, Some(&limit)),
            Some(StopReason::UsageLimit)
        );
        assert_eq!(
            stop_reason(&thread, Some(&spend_cap)),
            Some(StopReason::UsageLimit)
        );
        assert_eq!(
            stop_reason(&thread, Some(&running)),
            Some(StopReason::Interrupted)
        );
        assert_eq!(stop_reason(&thread, Some(&overloaded)), None);
        assert_eq!(stop_reason(&thread, Some(&done)), None);
        assert_eq!(stop_reason(&thread, None), None);

        let subagent = json!({"id": "t", "parentThreadId": "p"});
        assert_eq!(stop_reason(&subagent, Some(&limit)), None);
        let ephemeral = json!({"id": "t", "ephemeral": true});
        assert_eq!(stop_reason(&ephemeral, Some(&running)), None);
    }

    #[test]
    fn session_title_prefers_the_name_and_stays_on_one_line() {
        assert_eq!(
            session_title(&json!({"name": "fix auth", "preview": "p"})),
            "fix auth"
        );
        assert_eq!(
            session_title(&json!({"name": null, "preview": "a\n  b"})),
            "a b"
        );
        assert_eq!(session_title(&json!({})), "(untitled)");
        let long = "x".repeat(80);
        assert_eq!(session_title(&json!({"preview": long})).chars().count(), 60);
    }

    #[test]
    fn restart_requires_a_restarted_report() {
        let dir = short_tempdir();
        let fake = dir.path().join("codex");
        let write_fake = |body: &str| {
            std::fs::write(&fake, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        };

        write_fake(
            r#"[ "$*" = "app-server daemon restart" ] && [ -n "$CODEX_HOME" ] && echo '{"status":"restarted","pid":7}'"#,
        );
        restart_with(&fake, dir.path()).unwrap();

        write_fake(r#"echo 'boom' >&2; exit 1"#);
        let error = restart_with(&fake, dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("boom"), "{error:#}");
    }

    /// A scripted daemon: answers each request by method and emits
    /// `turn/started` after `turn/start`, with a notification in between to
    /// prove the client skips it.
    fn fake_daemon(listener: UnixListener) -> std::thread::JoinHandle<Vec<Value>> {
        fake_daemon_with_delay(listener, "", Duration::ZERO)
    }

    /// `fake_daemon`, but it answers `slow_method` only after `delay`.
    fn fake_daemon_with_delay(
        listener: UnixListener,
        slow_method: &'static str,
        delay: Duration,
    ) -> std::thread::JoinHandle<Vec<Value>> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut seen = Vec::new();
            loop {
                let Ok(message) = socket.read() else { break };
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").cloned() else {
                    continue;
                };
                let method = request["method"].as_str().unwrap().to_string();
                let reply = |result: Value| json!({"id": id, "result": result}).to_string();
                socket
                    .send(Message::text(
                        json!({"method": "thread/status/changed", "params": {}}).to_string(),
                    ))
                    .unwrap();
                let result = match method.as_str() {
                    "initialize" => json!({}),
                    "account/read" => json!({
                        "account": {"type": "chatgpt", "email": "a@test"},
                        "workspaceRouting": {"chatgptAccountId": "acct-a"},
                    }),
                    "thread/loaded/list" => json!({"data": ["t1", "t2", "t3"], "nextCursor": null}),
                    "thread/read" => {
                        let thread = request["params"]["threadId"].as_str().unwrap();
                        json!({"thread": {"id": thread, "name": format!("session {thread}")}})
                    }
                    "thread/turns/list" => match request["params"]["threadId"].as_str() {
                        Some("t1") => {
                            json!({"data": [{"id": "u", "status": "failed", "error": {"message": "limit", "codexErrorInfo": "usageLimitExceeded"}}]})
                        }
                        Some("t2") => json!({"data": [{"id": "u", "status": "completed"}]}),
                        _ => json!({"data": [{"id": "u", "status": "inProgress"}]}),
                    },
                    "thread/resume" if request["params"]["threadId"] == "bad" => {
                        seen.push(request.clone());
                        let error =
                            json!({"id": id, "error": {"code": -32600, "message": "no rollout"}});
                        if socket.send(Message::text(error.to_string())).is_err() {
                            break;
                        }
                        continue;
                    }
                    "thread/resume" => json!({"thread": {}}),
                    "turn/interrupt" => json!({}),
                    "turn/start" => json!({"turn": {"id": "new", "status": "inProgress"}}),
                    other => panic!("unexpected method {other}"),
                };
                seen.push(request.clone());
                if method == slow_method {
                    std::thread::sleep(delay);
                }
                // A client that gave up has closed the socket.
                if socket.send(Message::text(reply(result))).is_err() {
                    break;
                }
                let thread = request["params"]["threadId"].clone();
                let event = match method.as_str() {
                    "turn/start" => Some(("turn/started", "new")),
                    "turn/interrupt" => Some(("turn/completed", "u")),
                    _ => None,
                };
                if let Some((event, turn)) = event {
                    socket
                        .send(Message::text(
                            json!({"method": event, "params": {"threadId": thread, "turn": {"id": turn}}})
                                .to_string(),
                        ))
                        .unwrap();
                }
            }
            seen
        })
    }

    #[test]
    fn client_reads_account_finds_stopped_sessions_and_resumes_with_full_access() {
        let dir = short_tempdir();
        let socket_path = dir.path().join("s.sock");
        let daemon = fake_daemon(UnixListener::bind(&socket_path).unwrap());

        let mut client = Client::connect_socket(&socket_path).unwrap();
        let account = client.account().unwrap();
        assert_eq!(account.account_id.as_deref(), Some("acct-a"));
        assert_eq!(account.display(), "a@test");

        let stopped = client.stopped_sessions().unwrap();
        let summary: Vec<_> = stopped
            .iter()
            .map(|s| (s.thread_id.as_str(), s.title.as_str(), s.reason))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("t1", "session t1", StopReason::UsageLimit),
                ("t3", "session t3", StopReason::Interrupted),
            ]
        );

        client
            .resume("t1", "Continue the previous request.")
            .unwrap();
        drop(client);

        let seen = daemon.join().unwrap();
        let resume = seen
            .iter()
            .find(|r| r["method"] == "thread/resume")
            .unwrap();
        assert_eq!(resume["params"]["approvalPolicy"], "never");
        assert_eq!(resume["params"]["sandbox"], "danger-full-access");
        let turn = seen.iter().find(|r| r["method"] == "turn/start").unwrap();
        assert_eq!(turn["params"]["threadId"], "t1");
        assert_eq!(turn["params"]["approvalPolicy"], "never");
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(
            turn["params"]["input"][0]["text"],
            "Continue the previous request."
        );
    }

    /// A long session loads slowly, so `thread/resume` waits longer than any
    /// other request, and the ordinary limit still applies afterwards.
    #[test]
    fn resume_waits_longer_than_other_requests() {
        let timeouts = Timeouts {
            request: Duration::from_millis(300),
            resume: Duration::from_secs(5),
        };
        let dir = short_tempdir();

        let slow_resume = dir.path().join("r.sock");
        let daemon = fake_daemon_with_delay(
            UnixListener::bind(&slow_resume).unwrap(),
            "thread/resume",
            Duration::from_millis(900),
        );
        let mut client = Client::connect_socket_with(&slow_resume, timeouts).unwrap();
        assert_eq!(client.resume("t1", "go").unwrap(), Resumed::Started);
        drop(client);
        daemon.join().unwrap();

        let slow_read = dir.path().join("a.sock");
        let daemon = fake_daemon_with_delay(
            UnixListener::bind(&slow_read).unwrap(),
            "account/read",
            Duration::from_millis(900),
        );
        let mut client = Client::connect_socket_with(&slow_read, timeouts).unwrap();
        assert!(client.account().is_err());
        drop(client);
        daemon.join().unwrap();
    }

    fn fake_codex(dir: &Path, body: &str) -> PathBuf {
        let fake = dir.join("codex");
        std::fs::write(&fake, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&fake).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&fake, perms).unwrap();
        fake
    }

    fn stopped(thread_id: &str, reason: StopReason) -> StoppedSession {
        StoppedSession {
            thread_id: thread_id.to_string(),
            title: format!("session {thread_id}"),
            reason,
        }
    }

    /// One session that cannot resume must not turn a successful restart into
    /// an error: `codexctl codex` still has its own session to relaunch.
    #[test]
    fn a_failed_resume_is_reported_not_raised() {
        let home = short_tempdir();
        std::fs::create_dir_all(home.path().join("app-server-control")).unwrap();
        let daemon = fake_daemon(UnixListener::bind(home.path().join(CONTROL_SOCKET)).unwrap());
        let codex = fake_codex(home.path(), r#"echo '{"status":"restarted"}'"#);
        let sessions = [
            stopped("bad", StopReason::UsageLimit),
            stopped("t1", StopReason::UsageLimit),
        ];

        let mut out = Vec::new();
        let unresumed =
            restart_and_resume_with(&codex, home.path(), &sessions, "go", &mut out).unwrap();
        daemon.join().unwrap();

        assert_eq!(unresumed, vec!["bad".to_string()]);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("failed to resume bad"), "{out}");
        assert!(out.contains("resumed t1 session t1"), "{out}");
    }

    #[test]
    fn a_failed_restart_is_an_error() {
        let home = short_tempdir();
        let codex = fake_codex(home.path(), "echo boom >&2; exit 1");
        let sessions = [stopped("t1", StopReason::UsageLimit)];
        let error = restart_and_resume_with(&codex, home.path(), &sessions, "go", &mut Vec::new())
            .unwrap_err();
        assert!(format!("{error:#}").contains("boom"), "{error:#}");
    }

    #[test]
    fn daemon_account_needs_both_the_workspace_and_the_login() {
        let account = DaemonAccount {
            account_id: Some("ws".into()),
            email: Some("A@Test".into()),
            unreadable: None,
        };
        assert!(account.is("ws", "a@test"));
        assert!(!account.is("ws", "b@test"), "same workspace, another login");
        assert!(
            !account.is("other", "a@test"),
            "same login, another workspace"
        );

        let no_email = DaemonAccount {
            email: None,
            ..account.clone()
        };
        assert!(
            !no_email.is("ws", "a@test"),
            "an unstated login never agrees"
        );
    }
}
