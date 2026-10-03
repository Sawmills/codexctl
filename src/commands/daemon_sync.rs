//! Bring Codex's shared app-server daemon onto the account a switch installed.
//!
//! A switch swaps `auth.json` or rewrites the server provider. A running daemon
//! keeps its startup account, so every TUI attached to it keeps that account too. Applying
//! the switch means restarting the daemon, which stops its running turns; the
//! sessions that stopped, whether on the usage limit or by the restart, get a
//! new turn afterwards so nobody has to resume them one by one.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Result, bail};

use crate::api;
use crate::daemon;

/// Whether a switch may restart the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Restart without asking.
    Always,
    /// Ask on the terminal first.
    Ask,
    /// Leave it running and say how to apply the switch.
    Never,
}

impl Restart {
    /// The policy for a switch the operator asked for: the flag restarts
    /// outright, a terminal gets a question, anything else only a warning.
    pub fn for_switch(restart_daemon: bool) -> Self {
        if restart_daemon {
            Self::Always
        } else if std::io::stdin().is_terminal() {
            Self::Ask
        } else {
            Self::Never
        }
    }
}

/// After `alias` was installed into `auth_json`, restart the daemon that
/// serves that Codex home unless it provably runs that account already, then
/// resume the sessions the restart and the usage limit stopped.
///
/// `exclude` names a session the caller resumes itself. A failed restart is
/// an error; the sessions that did not resume come back as ids, so a caller
/// with its own recovery to finish can still finish it.
pub fn after_switch(
    alias: &str,
    auth_json: &Path,
    policy: Restart,
    exclude: Option<&str>,
    prompt: &str,
) -> Result<Vec<String>> {
    let codex_home = daemon::codex_home_of(auth_json);
    if daemon::shares_daemon(&codex_home) {
        // A pinned exec home links the daemon state of the live home. Its
        // daemon serves every lane, so one lane's switch must not restart it
        // onto that lane's account.
        if daemon::running_pid(&codex_home).is_some() {
            eprintln!(
                "codexctl: not restarting the shared Codex app-server daemon from the pinned home {}.",
                codex_home.display()
            );
        }
        return Ok(Vec::new());
    }
    let Some(inspection) = inspect_or_assume_stale(&codex_home, exclude) else {
        return Ok(Vec::new());
    };
    if daemon_runs_installed(&inspection.account, auth_json) {
        return Ok(Vec::new());
    }

    eprintln!(
        "codexctl: the Codex app-server daemon (pid {}) still uses {}.",
        inspection.pid,
        inspection.account.display()
    );
    let sessions = match policy {
        Restart::Always => inspection.sessions,
        Restart::Never => return Ok(declined(alias)),
        Restart::Ask => {
            print_sessions(&inspection.sessions);
            let approved = dialoguer::Confirm::new()
                .with_prompt("codexctl: restart the daemon to apply the switch?")
                .default(false)
                .interact()?;
            if !approved {
                return Ok(declined(alias));
            }
            // While the question waited, a session may have started a turn,
            // or another switch may have restarted the daemon already. Look
            // again right before the restart; its drain rejects new turns, so
            // nothing can start after this.
            let Some(fresh) = inspect_or_assume_stale(&codex_home, exclude) else {
                return Ok(Vec::new());
            };
            if daemon_runs_installed(&fresh.account, auth_json) {
                eprintln!("codexctl: the daemon runs this account already; not restarting it.");
                return Ok(Vec::new());
            }
            // The answer covers only the sessions it was shown. Resumed turns
            // run with full access, so a session that appeared since needs its
            // own yes; without one it is left to Codex and its own settings.
            let (mut approved, added) = split_approved(fresh.sessions, &inspection.sessions);
            if !added.is_empty() {
                eprint!("{}", added_summary(&added));
                if dialoguer::Confirm::new()
                    .with_prompt("codexctl: resume these sessions too?")
                    .default(false)
                    .interact()?
                {
                    approved.extend(added);
                } else {
                    eprintln!(
                        "codexctl: not resuming the sessions that started after the question."
                    );
                }
            }
            approved
        }
    };
    daemon::restart_and_resume(&codex_home, &sessions, prompt, None, &mut std::io::stderr())
}

/// Apply an explicitly requested server-provider switch to the daemon.
/// The preserved local auth file cannot prove which server account it runs.
#[cfg(feature = "central-prototype")]
pub fn after_server_switch(codex_home: &Path, prompt: &str) -> Result<Vec<String>> {
    let Some(inspection) = inspect_or_assume_stale(codex_home, None) else {
        return Ok(Vec::new());
    };
    daemon::restart_and_resume(
        codex_home,
        &inspection.sessions,
        prompt,
        Some("codexctl-central"),
        &mut std::io::stderr(),
    )
    .map_err(|error| error.context("server account is active; daemon restart failed"))
}

/// Inspect the daemon, or `None` when none runs.
///
/// The switch has already happened when this runs, so a daemon that cannot
/// be read must not undo it or stop `codexctl codex` recovery. It is treated
/// as running an unknown account with no known sessions: the policy still
/// decides on a restart, and the warning says what could not be resumed.
fn inspect_or_assume_stale(codex_home: &Path, exclude: Option<&str>) -> Option<daemon::Inspection> {
    let error = match daemon::inspect(codex_home, exclude) {
        Ok(inspection) => {
            for thread in inspection.iter().flat_map(|i| &i.unreadable) {
                eprintln!(
                    "codexctl: warning: could not read session {thread}; it will not be resumed"
                );
            }
            return inspection;
        }
        Err(error) => error,
    };
    let pid = daemon::running_pid(codex_home)?;
    eprintln!(
        "codexctl: warning: could not list the daemon's sessions ({error:#}); a restart resumes none of them"
    );
    Some(daemon::Inspection {
        pid,
        account: daemon::DaemonAccount::unreadable(&error),
        sessions: Vec::new(),
        unreadable: Vec::new(),
    })
}

/// Fail when sessions the restart stopped did not resume.
pub fn require_resumed(unresumed: Vec<String>) -> Result<()> {
    if unresumed.is_empty() {
        return Ok(());
    }
    bail!(
        "{} {} did not resume; run `codex resume <id>` for: {}",
        unresumed.len(),
        if unresumed.len() == 1 {
            "session"
        } else {
            "sessions"
        },
        unresumed.join(", ")
    )
}

fn declined(alias: &str) -> Vec<String> {
    eprintln!(
        "codexctl: Codex sessions keep the old account until the daemon restarts. Run `codexctl use {alias} --restart-daemon` to apply it."
    );
    Vec::new()
}

/// Whether the daemon provably runs the account in `auth_json`.
///
/// A workspace holds many logins, so the workspace id alone cannot prove it.
/// Both the workspace and the login's email must agree; anything the daemon
/// or the token does not state counts as disagreement, and the daemon restarts.
fn daemon_runs_installed(account: &daemon::DaemonAccount, auth_json: &Path) -> bool {
    let Ok(auth) = api::read_auth_json(auth_json) else {
        return false;
    };
    let identity = api::token_identity(&auth.access_token).unwrap_or_default();
    match (auth.account_id.or(identity.account_id), identity.email) {
        (Some(account_id), Some(email)) => account.is(&account_id, &email),
        _ => false,
    }
}

/// Split a fresh listing into the sessions the operator was shown and those
/// that appeared since. Only the first may be resumed on the first answer.
fn split_approved(
    fresh: Vec<daemon::StoppedSession>,
    shown: &[daemon::StoppedSession],
) -> (Vec<daemon::StoppedSession>, Vec<daemon::StoppedSession>) {
    fresh
        .into_iter()
        .partition(|session| shown.iter().any(|seen| seen.thread_id == session.thread_id))
}

fn added_summary(added: &[daemon::StoppedSession]) -> String {
    let mut summary = format!(
        "codexctl: {} started after the question and would also resume with {}:\n",
        if added.len() == 1 {
            "1 session".to_string()
        } else {
            format!("{} sessions", added.len())
        },
        daemon::RESUME_PERMISSIONS
    );
    for session in added {
        summary.push_str(&session_row(session));
    }
    summary
}

fn session_row(session: &daemon::StoppedSession) -> String {
    format!(
        "  {}  {}  ({})\n",
        session.thread_id,
        session.title,
        session.reason.describe()
    )
}

fn print_sessions(sessions: &[daemon::StoppedSession]) {
    eprint!("{}", sessions_summary(sessions));
}

/// What a restart would do to the daemon's sessions, stated before the
/// operator approves it, including the permissions resumed turns get.
fn sessions_summary(sessions: &[daemon::StoppedSession]) -> String {
    if sessions.is_empty() {
        return "codexctl: no session runs or waits on the usage limit.\n".to_string();
    }
    let one = sessions.len() == 1;
    let mut summary = format!(
        "codexctl: {} {} and {} resumed on the new account with {}:\n",
        sessions.len(),
        if one {
            "session stops"
        } else {
            "sessions stop"
        },
        if one { "is" } else { "are" },
        daemon::RESUME_PERMISSIONS
    );
    for session in sessions {
        summary.push_str(&session_row(session));
    }
    summary
}

/// Warn when the daemon for the live Codex home runs on another account than
/// the active profile. Silent when no daemon runs.
pub fn warn_if_stale(alias: &str, auth_json: &Path) {
    let codex_home = daemon::codex_home_of(auth_json);
    if daemon::running_pid(&codex_home).is_none() {
        return;
    }
    let account = daemon::Client::connect(&codex_home).and_then(|mut client| client.account());
    match account {
        Ok(account) if !daemon_runs_installed(&account, auth_json) => eprintln!(
            "warning: the Codex app-server daemon still uses {}. Run `codexctl use {alias} --restart-daemon` to apply the switch.",
            account.display()
        ),
        Ok(_) => {}
        Err(error) => {
            eprintln!("warning: could not read the Codex app-server daemon account: {error:#}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn auth_file(dir: &Path, account_id: &str, email: Option<&str>) -> std::path::PathBuf {
        let mut claims = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        });
        if let Some(email) = email {
            claims["https://api.openai.com/profile"] = serde_json::json!({"email": email});
        }
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!("h.{}.s", engine.encode(claims.to_string()));
        let path = dir.join("auth.json");
        let auth = serde_json::json!({"tokens": {"access_token": token, "account_id": account_id}});
        std::fs::write(&path, auth.to_string()).unwrap();
        path
    }

    fn daemon_on(account_id: &str, email: &str) -> daemon::DaemonAccount {
        daemon::DaemonAccount {
            account_id: Some(account_id.to_string()),
            email: Some(email.to_string()),
            unreadable: None,
        }
    }

    #[test]
    fn skips_the_restart_only_on_the_same_workspace_and_login() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_file(dir.path(), "ws", Some("a@test"));

        assert!(daemon_runs_installed(&daemon_on("ws", "a@test"), &auth));
        // Two logins in one team workspace share the workspace id.
        assert!(!daemon_runs_installed(&daemon_on("ws", "b@test"), &auth));
        assert!(!daemon_runs_installed(&daemon_on("other", "a@test"), &auth));
    }

    #[test]
    fn restarts_when_the_token_does_not_state_its_login() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_file(dir.path(), "ws", None);
        assert!(!daemon_runs_installed(&daemon_on("ws", "a@test"), &auth));
        assert!(!daemon_runs_installed(
            &daemon_on("ws", "a@test"),
            &dir.path().join("missing.json")
        ));
    }

    #[test]
    fn require_resumed_names_every_session_left_stopped() {
        assert!(require_resumed(Vec::new()).is_ok());
        let error = require_resumed(vec!["t1".into(), "t2".into()]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "2 sessions did not resume; run `codex resume <id>` for: t1, t2"
        );
    }

    /// A daemon that runs but cannot be read still gets a restart decision,
    /// so neither `use` nor `codex` recovery fails after the swap.
    #[test]
    fn an_unreadable_daemon_is_assumed_stale() {
        #[cfg(unix)]
        let root = std::path::PathBuf::from("/tmp");
        #[cfg(windows)]
        let root = std::env::temp_dir();
        let home = tempfile::Builder::new()
            .prefix("cxs")
            .tempdir_in(root)
            .unwrap();
        assert!(inspect_or_assume_stale(home.path(), None).is_none());

        std::fs::create_dir_all(home.path().join("app-server-daemon")).unwrap();
        let own = std::process::id();
        std::fs::write(
            home.path().join("app-server-daemon/daemon.pid"),
            format!(r#"{{"pid":{own}}}"#),
        )
        .unwrap();
        let inspection = inspect_or_assume_stale(home.path(), None).unwrap();
        assert_eq!(inspection.pid, own as i32);
        assert!(inspection.sessions.is_empty());
        assert!(inspection.account.unreadable.is_some());
        let auth = auth_file(home.path(), "ws", Some("a@test"));
        assert!(!daemon_runs_installed(&inspection.account, &auth));
    }

    /// The question must say what a "yes" grants the resumed sessions.
    #[test]
    fn the_restart_question_discloses_the_resume_permissions() {
        let sessions = [daemon::StoppedSession {
            thread_id: "t1".into(),
            title: "fix auth".into(),
            reason: daemon::StopReason::UsageLimit,
        }];
        let summary = sessions_summary(&sessions);
        assert!(
            summary.contains("no sandbox and no approval prompts"),
            "{summary}"
        );
        assert!(summary.contains("t1  fix auth"), "{summary}");
        assert!(!sessions_summary(&[]).contains("sandbox"));
    }

    fn session(thread_id: &str, reason: daemon::StopReason) -> daemon::StoppedSession {
        daemon::StoppedSession {
            thread_id: thread_id.into(),
            title: format!("session {thread_id}"),
            reason,
        }
    }

    /// A "yes" covers the sessions it was shown, never one that appeared
    /// while the question waited.
    #[test]
    fn only_shown_sessions_are_approved() {
        use daemon::StopReason::{Interrupted, UsageLimit};
        let shown = [session("a", UsageLimit), session("gone", Interrupted)];
        let fresh = vec![session("a", UsageLimit), session("new", Interrupted)];

        let (approved, added) = split_approved(fresh, &shown);
        let ids = |s: &[daemon::StoppedSession]| -> Vec<String> {
            s.iter().map(|s| s.thread_id.clone()).collect()
        };
        assert_eq!(ids(&approved), ["a"]);
        assert_eq!(ids(&added), ["new"]);

        let summary = added_summary(&added);
        assert!(
            summary.contains("1 session started after the question"),
            "{summary}"
        );
        assert!(
            summary.contains("no sandbox and no approval prompts"),
            "{summary}"
        );
        assert!(summary.contains("new  session new"), "{summary}");
    }
}
