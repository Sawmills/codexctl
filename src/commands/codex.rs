use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
#[cfg(not(unix))]
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::commands::use_profile;
use crate::config::{self, Paths};
use crate::profile;

const SPEND_CAP_MESSAGE: &str = "You hit your spend cap set by the owner of your workspace. Ask an owner to increase your spend cap to continue.";
const SELF_MANAGED_SPEND_CAP_MESSAGE: &str =
    "You hit your spend cap set in your workspace. Increase your spend cap to continue.";
pub const DEFAULT_RECOVERY_PROMPT: &str = "Continue the previous request.";

pub fn run(args: &[String], recovery_prompt: &str, allow_billing: bool) -> Result<i32> {
    let paths = config::default_paths()?;
    let mut reporter = HerdrAgentReporter::from_env();
    let mut runner = PtyCodexRunner::new(reporter.clone());
    let failed_alias = failed_alias_for_child_auth(&paths);
    let mut switcher = CodexctlProfileSwitcher::new(&paths);
    let mut sessions = FilesystemSessionStore::new(&paths)?;
    let mut consent = InteractiveConsent { allow_billing };
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let options = WrapperOptions::new(args.to_vec(), recovery_prompt.to_string(), cwd);
    // The account that hit the cap is already active; never switch back to it.
    let initial_tried: Vec<String> = failed_alias.into_iter().collect();
    run_with_reporter(
        &options,
        &mut runner,
        &mut switcher,
        &mut sessions,
        &mut reporter,
        &mut consent,
        initial_tried,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexRunOutcome {
    Exited(i32),
    SpendCap { session_id: Option<String> },
}

trait CodexRunner {
    fn run_codex(&mut self, invocation: &CodexInvocation) -> Result<CodexRunOutcome>;
}

trait ProfileSwitcher {
    /// Pick the next recovery account, excluding any `tried` aliases.
    fn find_recovery_candidate(
        &mut self,
        tried: &[String],
    ) -> Result<Option<use_profile::RecoveryCandidate>>;

    /// Switch the child Codex auth to `alias`.
    fn switch_to(&mut self, alias: &str) -> Result<()>;
}

/// Decides whether spend-cap recovery may switch to a credit-billing account
/// (one whose overage is open, so it draws credits past 100%).
trait RecoveryConsent {
    fn allow_billing_account(&mut self, alias: &str) -> bool;
}

trait SessionStore {
    fn discover_latest_session_id(
        &mut self,
        invocation: &CodexInvocation,
    ) -> Result<Option<String>>;

    fn goal_status_for_session_id(&mut self, session_id: &str)
    -> Result<Option<SessionGoalStatus>>;
}

#[derive(Debug, Clone)]
struct WrapperOptions {
    args: Vec<String>,
    recovery_prompt: String,
    cwd: PathBuf,
}

impl WrapperOptions {
    fn new(args: Vec<String>, recovery_prompt: String, cwd: PathBuf) -> Self {
        Self {
            args,
            recovery_prompt,
            cwd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexInvocation {
    args: Vec<String>,
    cwd: PathBuf,
    continue_goal_on_start: bool,
}

impl CodexInvocation {
    fn new(args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            args,
            cwd,
            continue_goal_on_start: false,
        }
    }

    fn from_forwarded_args(args: &[String], cwd: &std::path::Path) -> Self {
        Self::new(invocation_args_for_cwd(args, cwd), cwd.to_path_buf())
    }

    fn with_continue_goal_on_start(mut self) -> Self {
        self.continue_goal_on_start = true;
        self
    }
}

#[cfg(test)]
fn run_with(
    options: &WrapperOptions,
    runner: &mut impl CodexRunner,
    switcher: &mut impl ProfileSwitcher,
    sessions: &mut impl SessionStore,
) -> Result<i32> {
    let mut reporter = None;
    let mut consent = tests::DenyBillingConsent;
    run_with_reporter(
        options,
        runner,
        switcher,
        sessions,
        &mut reporter,
        &mut consent,
        Vec::new(),
    )
}

fn run_with_reporter(
    options: &WrapperOptions,
    runner: &mut impl CodexRunner,
    switcher: &mut impl ProfileSwitcher,
    sessions: &mut impl SessionStore,
    reporter: &mut impl AgentReporter,
    consent: &mut impl RecoveryConsent,
    initial_tried: Vec<String>,
) -> Result<i32> {
    let session_id = session_id_from_codex_args(&options.args);
    if let Some(session_id) = session_id.as_deref() {
        reporter.report_session(session_id);
    }
    reporter.report(HerdrAgentState::Unknown, session_id.as_deref());
    let result = run_with_reporter_inner(
        options,
        runner,
        switcher,
        sessions,
        reporter,
        consent,
        initial_tried,
    );
    reporter.release();
    result
}

fn run_with_reporter_inner(
    options: &WrapperOptions,
    runner: &mut impl CodexRunner,
    switcher: &mut impl ProfileSwitcher,
    sessions: &mut impl SessionStore,
    reporter: &mut impl AgentReporter,
    consent: &mut impl RecoveryConsent,
    mut tried: Vec<String>,
) -> Result<i32> {
    let mut invocation = CodexInvocation::from_forwarded_args(&options.args, &options.cwd);
    // The resume invocation is built once on the first spend cap and reused for
    // every subsequent switch, since each switch resumes the same session.
    let mut recovery: Option<CodexInvocation> = None;

    loop {
        match runner.run_codex(&invocation)? {
            CodexRunOutcome::Exited(code) => return Ok(code),
            CodexRunOutcome::SpendCap { session_id } => {
                if recovery.is_none() {
                    let plan = recovery_plan(options, session_id, sessions)?;
                    let mut rec = CodexInvocation::from_forwarded_args(&plan.args, &options.cwd);
                    let recovery_session_id = session_id_from_codex_args(&rec.args);
                    if let Some(session_id) = recovery_session_id.as_deref() {
                        reporter.report_session(session_id);
                    }
                    reporter.report(HerdrAgentState::Unknown, recovery_session_id.as_deref());
                    if plan.continue_paused_goal_on_prompt {
                        rec = rec.with_continue_goal_on_start();
                    }
                    recovery = Some(rec);
                }

                let Some(candidate) = switcher.find_recovery_candidate(&tried)? else {
                    bail!(
                        "spend cap reached and no alternate rate-limited account is available to switch to"
                    );
                };

                eprintln!();
                if candidate.bills_credits {
                    eprintln!("codexctl: spend cap reached; only credit-billing accounts remain.");
                    if !consent.allow_billing_account(&candidate.alias) {
                        bail!(
                            "spend cap reached; switching to credit-billing account {} was not approved",
                            candidate.alias
                        );
                    }
                } else {
                    eprintln!("codexctl: spend cap detected; switching to a no-overage account");
                }

                switcher.switch_to(&candidate.alias)?;
                tried.push(candidate.alias);

                let next = recovery.clone().expect("recovery invocation set above");
                eprintln!("codexctl: running `codex {}`", next.args.join(" "));
                invocation = next;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HerdrAgentState {
    Unknown,
    Idle,
    Working,
    Blocked,
}

impl HerdrAgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

trait AgentReporter {
    fn report_session(&mut self, session_id: &str);
    fn report(&mut self, state: HerdrAgentState, session_id: Option<&str>);
    fn release(&mut self);
}

impl AgentReporter for Option<HerdrAgentReporter> {
    fn report_session(&mut self, session_id: &str) {
        if let Some(reporter) = self {
            reporter.report_session(session_id);
        }
    }

    fn report(&mut self, state: HerdrAgentState, session_id: Option<&str>) {
        if let Some(reporter) = self {
            reporter.report(state, session_id);
        }
    }

    fn release(&mut self) {
        if let Some(reporter) = self {
            reporter.release();
        }
    }
}

#[derive(Clone)]
struct HerdrAgentReporter {
    socket_path: PathBuf,
    pane_id: String,
}

impl HerdrAgentReporter {
    const SOURCE: &'static str = "codexctl";
    const CODEX_SESSION_SOURCE: &'static str = "herdr:codex";
    const AGENT: &'static str = "codex";

    fn from_env() -> Option<Self> {
        if std::env::var_os("HERDR_ENV").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let socket_path = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from)?;
        let pane_id = std::env::var("HERDR_PANE_ID").ok()?;
        if pane_id.trim().is_empty() {
            return None;
        }
        Some(Self {
            socket_path,
            pane_id,
        })
    }

    fn report_session(&self, session_id: &str) {
        self.send(herdr_report_agent_session_request(
            &self.pane_id,
            session_id,
            herdr_seq(),
        ));
    }

    fn report(&self, state: HerdrAgentState, session_id: Option<&str>) {
        self.send(herdr_report_agent_request(
            &self.pane_id,
            state,
            session_id,
            herdr_seq(),
        ));
    }

    fn release(&self) {
        self.send(herdr_release_agent_request(&self.pane_id, herdr_seq()));
    }

    fn send(&self, request: serde_json::Value) {
        let Ok(payload) = serde_json::to_vec(&request) else {
            return;
        };

        #[cfg(unix)]
        {
            let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket_path) else {
                return;
            };
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            let _ = stream
                .write_all(&payload)
                .and_then(|_| stream.write_all(b"\n"));
        }

        #[cfg(not(unix))]
        let _ = payload;
    }
}

fn herdr_report_agent_request(
    pane_id: &str,
    state: HerdrAgentState,
    session_id: Option<&str>,
    seq: u64,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "pane_id": pane_id,
        "source": HerdrAgentReporter::SOURCE,
        "agent": HerdrAgentReporter::AGENT,
        "state": state.as_str(),
        "seq": seq,
    });
    if let Some(session_id) = session_id {
        params["agent_session_id"] = serde_json::json!(session_id);
    }
    serde_json::json!({
        "id": format!("codexctl:report-agent:{seq}"),
        "method": "pane.report_agent",
        "params": params,
    })
}

fn herdr_report_agent_session_request(
    pane_id: &str,
    session_id: &str,
    seq: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": format!("codexctl:report-agent-session:{seq}"),
        "method": "pane.report_agent_session",
        "params": {
            "pane_id": pane_id,
            "source": HerdrAgentReporter::CODEX_SESSION_SOURCE,
            "agent": HerdrAgentReporter::AGENT,
            "seq": seq,
            "agent_session_id": session_id,
        },
    })
}

fn herdr_release_agent_request(pane_id: &str, seq: u64) -> serde_json::Value {
    serde_json::json!({
        "id": format!("codexctl:release-agent:{seq}"),
        "method": "pane.release_agent",
        "params": {
            "pane_id": pane_id,
            "source": HerdrAgentReporter::SOURCE,
            "agent": HerdrAgentReporter::AGENT,
            "seq": seq,
        },
    })
}

fn herdr_seq() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

struct RecoveryPlan {
    args: Vec<String>,
    continue_paused_goal_on_prompt: bool,
}

fn recovery_plan(
    options: &WrapperOptions,
    detected_session_id: Option<String>,
    sessions: &mut impl SessionStore,
) -> Result<RecoveryPlan> {
    let mut session_id = session_id_from_codex_args(&options.args).or(detected_session_id);
    if session_id.is_none() {
        let invocation = CodexInvocation::from_forwarded_args(&options.args, &options.cwd);
        session_id = sessions.discover_latest_session_id(&invocation)?;
    }

    if let Some(session_id) = session_id {
        let continue_paused_goal_on_prompt =
            should_continue_paused_goal_on_recovery(options, &session_id, sessions)?;
        let recovery_prompt = if continue_paused_goal_on_prompt {
            ""
        } else {
            &options.recovery_prompt
        };
        return Ok(RecoveryPlan {
            args: resume_args_for_session(
                preserved_codex_options(&options.args),
                &session_id,
                recovery_prompt,
            ),
            continue_paused_goal_on_prompt,
        });
    }

    if resume_command_index(&options.args).is_some() {
        return Ok(RecoveryPlan {
            args: resume_args_with_recovery_prompt(&options.args, &options.recovery_prompt),
            continue_paused_goal_on_prompt: false,
        });
    }

    bail!("spend cap detected, but no Codex session id was available to resume")
}

fn should_continue_paused_goal_on_recovery(
    options: &WrapperOptions,
    session_id: &str,
    sessions: &mut impl SessionStore,
) -> Result<bool> {
    if options.recovery_prompt.trim().is_empty() {
        return Ok(false);
    }
    if options.recovery_prompt != DEFAULT_RECOVERY_PROMPT {
        return Ok(false);
    }

    Ok(sessions
        .goal_status_for_session_id(session_id)?
        .is_some_and(SessionGoalStatus::prompts_on_quiet_resume))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl SessionGoalStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "usage_limited" | "usageLimited" => Some(Self::UsageLimited),
            "budget_limited" | "budgetLimited" => Some(Self::BudgetLimited),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }

    fn prompts_on_quiet_resume(self) -> bool {
        matches!(self, Self::Paused | Self::Blocked | Self::UsageLimited)
    }
}

fn resume_args_with_recovery_prompt(args: &[String], recovery_prompt: &str) -> Vec<String> {
    let mut args = args.to_vec();
    if recovery_prompt.trim().is_empty() {
        return args;
    }

    if let Some(prompt_index) = resume_prompt_index(&args) {
        args[prompt_index] = recovery_prompt.to_string();
    } else {
        args.push(recovery_prompt.to_string());
    }
    args
}

fn resume_args_for_session(
    mut preserved_options: Vec<String>,
    session_id: &str,
    recovery_prompt: &str,
) -> Vec<String> {
    let mut args = Vec::with_capacity(preserved_options.len() + 3);
    args.append(&mut preserved_options);
    args.push("resume".to_string());
    args.push(session_id.to_string());
    append_recovery_prompt(&mut args, recovery_prompt);
    args
}

fn append_recovery_prompt(args: &mut Vec<String>, recovery_prompt: &str) {
    if !recovery_prompt.trim().is_empty() {
        args.push(recovery_prompt.to_string());
    }
}

fn invocation_args_for_cwd(args: &[String], cwd: &std::path::Path) -> Vec<String> {
    if codex_args_set_cwd(args) {
        return args.to_vec();
    }

    let mut invocation_args = Vec::with_capacity(args.len() + 2);
    invocation_args.push("--cd".to_string());
    invocation_args.push(cwd.display().to_string());
    invocation_args.extend(args.iter().cloned());
    invocation_args
}

fn codex_args_set_cwd(args: &[String]) -> bool {
    cwd_from_codex_args(args).is_some()
}

fn preserved_codex_options(args: &[String]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if let Some(name) = arg.strip_prefix("--") {
            let option_name = name.split_once('=').map_or(name, |(name, _)| name);
            let long_name = format!("--{option_name}");

            if long_option_takes_value(&long_name) {
                preserved.push(arg.clone());
                if !arg.contains('=')
                    && let Some(value) = args.get(index + 1)
                {
                    preserved.push(value.clone());
                    index += 2;
                    continue;
                }
            } else if long_option_is_preserved_flag(&long_name) {
                preserved.push(arg.clone());
            }

            index += 1;
            continue;
        }

        if short_option_takes_value(arg) {
            preserved.push(arg.clone());
            if let Some(value) = args.get(index + 1) {
                preserved.push(value.clone());
                index += 2;
                continue;
            }
        } else if short_option_is_preserved_flag(arg) {
            preserved.push(arg.clone());
        }

        index += 1;
    }

    preserved
}

fn long_option_is_preserved_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--strict-config"
            | "--oss"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-bypass-hook-trust"
            | "--search"
            | "--no-alt-screen"
            | "--full-auto"
            | "--skip-git-repo-check"
    )
}

fn short_option_is_preserved_flag(_arg: &str) -> bool {
    false
}

fn session_id_from_codex_args(args: &[String]) -> Option<String> {
    let mut index = resume_command_index(args)? + 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            index += 1;
            continue;
        }
        if arg.starts_with("--") {
            if !arg.contains('=') && long_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if arg.starts_with('-') {
            if short_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        return is_session_id(arg).then(|| arg.clone());
    }

    None
}

fn resume_command_index(args: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if arg.starts_with("--") {
            if !arg.contains('=') && long_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if arg.starts_with('-') {
            if short_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        return (arg == "resume").then_some(index);
    }

    None
}

fn resume_prompt_index(args: &[String]) -> Option<usize> {
    let shape = resume_args_shape(args)?;
    if shape.has_last {
        shape.positionals.first().copied()
    } else {
        shape.positionals.get(1).copied()
    }
}

struct ResumeArgsShape {
    has_last: bool,
    positionals: Vec<usize>,
}

fn resume_args_shape(args: &[String]) -> Option<ResumeArgsShape> {
    let mut index = resume_command_index(args)? + 1;
    let mut has_last = false;
    let mut positionals = Vec::new();
    let mut after_terminator = false;

    while index < args.len() {
        let arg = &args[index];
        if !after_terminator {
            if arg == "--" {
                after_terminator = true;
                index += 1;
                continue;
            }
            if arg == "--last" {
                has_last = true;
                index += 1;
                continue;
            }
            if arg.starts_with("--") {
                if !arg.contains('=') && long_option_takes_value(arg) {
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            if arg.starts_with('-') {
                if short_option_takes_value(arg) {
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
        }

        positionals.push(index);
        index += 1;
    }

    Some(ResumeArgsShape {
        has_last,
        positionals,
    })
}

fn long_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--config"
            | "--remote"
            | "--remote-auth-token-env"
            | "--image"
            | "--model"
            | "--local-provider"
            | "--profile"
            | "--sandbox"
            | "--cd"
            | "--add-dir"
            | "--ask-for-approval"
            | "--enable"
            | "--disable"
    )
}

fn short_option_takes_value(arg: &str) -> bool {
    matches!(arg, "-c" | "-i" | "-m" | "-p" | "-s" | "-C" | "-a")
}

fn is_session_id(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|i| bytes[*i] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| [8, 13, 18, 23].contains(&i) || b.is_ascii_hexdigit())
}

struct CodexctlProfileSwitcher {
    auth_json: PathBuf,
}

impl CodexctlProfileSwitcher {
    fn new(paths: &Paths) -> Self {
        Self {
            auth_json: codex_auth_json_for_child(paths),
        }
    }
}

impl ProfileSwitcher for CodexctlProfileSwitcher {
    fn find_recovery_candidate(
        &mut self,
        tried: &[String],
    ) -> Result<Option<use_profile::RecoveryCandidate>> {
        use_profile::find_recovery_candidate(tried)
    }

    fn switch_to(&mut self, alias: &str) -> Result<()> {
        let email = profile::switch_to_auth_json(alias, &self.auth_json)?;
        eprintln!("codexctl: switched to {alias} ({email})");
        Ok(())
    }
}

struct InteractiveConsent {
    /// Set by `--allow-billing`: approve credit-billing accounts without asking,
    /// for unattended runs.
    allow_billing: bool,
}

impl RecoveryConsent for InteractiveConsent {
    fn allow_billing_account(&mut self, alias: &str) -> bool {
        if self.allow_billing {
            eprintln!("codexctl: --allow-billing set; switching to {alias} (may use credits)");
            return true;
        }
        // Only ask when a human can answer; otherwise refuse so recovery never
        // bills credits unattended.
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "codexctl: not switching to {alias} (no terminal to approve credit billing; pass --allow-billing to allow)"
            );
            return false;
        }
        eprint!("codexctl: switch to {alias} and allow it to use credits? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

struct FilesystemSessionStore {
    sessions_dir: PathBuf,
    seen: HashSet<PathBuf>,
}

impl FilesystemSessionStore {
    fn new(paths: &Paths) -> Result<Self> {
        let codex_home = codex_home_for_child(paths);
        let sessions_dir = codex_home.join("sessions");
        let seen = session_files(&sessions_dir)?;
        Ok(Self { sessions_dir, seen })
    }
}

impl SessionStore for FilesystemSessionStore {
    fn discover_latest_session_id(
        &mut self,
        invocation: &CodexInvocation,
    ) -> Result<Option<String>> {
        let expected_cwd = invocation_session_cwd(invocation);
        let mut newest: Option<(SystemTime, String)> = None;
        for path in session_files(&self.sessions_dir)? {
            if self.seen.contains(&path) {
                continue;
            }
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let meta = match session_meta_from_file(&path) {
                Ok(Some(meta)) => meta,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!(
                        "warning: failed to inspect codex session file {}: {e:#}",
                        path.display()
                    );
                    continue;
                }
            };
            if meta.is_subagent {
                continue;
            }
            if !session_cwd_matches(meta.cwd.as_deref(), &expected_cwd) {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|(newest_modified, _)| modified > *newest_modified)
            {
                newest = Some((modified, meta.id));
            }
        }

        Ok(newest.map(|(_, session_id)| session_id))
    }

    fn goal_status_for_session_id(
        &mut self,
        session_id: &str,
    ) -> Result<Option<SessionGoalStatus>> {
        let mut candidates = Vec::new();
        for path in session_files(&self.sessions_dir)? {
            let meta = match session_meta_from_file(&path) {
                Ok(Some(meta)) => meta,
                Ok(None) => continue,
                Err(_) => continue,
            };
            if meta.is_subagent {
                continue;
            }
            if meta.thread_ids.contains(session_id) {
                let modified = std::fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((modified, path));
            }
        }

        candidates.sort_by(|(left, _), (right, _)| right.cmp(left));
        for (_, path) in candidates {
            match session_goal_state_from_file(&path, session_id)? {
                SessionGoalState::Status(status) => return Ok(Some(status)),
                SessionGoalState::Cleared | SessionGoalState::MatchedGoalWithoutStatus => {
                    return Ok(None);
                }
                SessionGoalState::Unknown => continue,
            }
        }

        Ok(None)
    }
}

fn codex_auth_json_for_child(paths: &Paths) -> PathBuf {
    codex_home_for_child(paths).join("auth.json")
}

fn codex_home_for_child(paths: &Paths) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.home.join(".codex"))
}

fn failed_alias_for_child_auth(paths: &Paths) -> Option<String> {
    let auth_json = codex_auth_json_for_child(paths);
    profile::alias_for_auth_json_from(paths, &auth_json)
        .ok()
        .flatten()
        .or_else(|| {
            if auth_json == paths.codex_auth_json() {
                profile::get_active_from(paths).ok().flatten()
            } else {
                None
            }
        })
}

fn invocation_session_cwd(invocation: &CodexInvocation) -> PathBuf {
    let cwd = cwd_from_codex_args(&invocation.args).unwrap_or_else(|| invocation.cwd.clone());
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        invocation.cwd.join(cwd)
    };
    normalize_cwd(cwd)
}

fn cwd_from_codex_args(args: &[String]) -> Option<PathBuf> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--cd=") {
            return Some(PathBuf::from(value));
        }
        if arg == "--cd" || arg == "-C" {
            return args.get(index + 1).map(PathBuf::from);
        }
        if arg.starts_with("--") {
            if !arg.contains('=') && long_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if arg.starts_with('-') {
            if short_option_takes_value(arg) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        index += 1;
    }

    None
}

fn normalize_cwd(cwd: PathBuf) -> PathBuf {
    std::fs::canonicalize(&cwd).unwrap_or(cwd)
}

fn session_cwd_matches(session_cwd: Option<&Path>, expected_cwd: &Path) -> bool {
    session_cwd
        .map(|cwd| normalize_cwd(cwd.to_path_buf()) == expected_cwd)
        .unwrap_or(false)
}

struct SessionMeta {
    id: String,
    thread_ids: HashSet<String>,
    cwd: Option<PathBuf>,
    is_subagent: bool,
}

fn session_files(root: &std::path::Path) -> Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();
    if !root.exists() {
        return Ok(files);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                files.insert(path);
            }
        }
    }
    Ok(files)
}

fn session_meta_from_file(path: &std::path::Path) -> Result<Option<SessionMeta>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open session file {}", path.display()))?;
    let mut lines = std::io::BufReader::new(file).lines();
    let Some(line) = lines.next().transpose()? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&line)
        .with_context(|| format!("failed to parse session file {}", path.display()))?;
    let Some(payload) = value.get("payload") else {
        return Ok(None);
    };
    let Some(id) = payload
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let thread_ids = session_thread_ids_from_payload(payload);
    let cwd = payload
        .get("cwd")
        .and_then(|cwd| cwd.as_str())
        .map(PathBuf::from);
    let is_subagent = payload
        .get("thread_source")
        .and_then(|thread_source| thread_source.as_str())
        .is_some_and(|thread_source| thread_source == "subagent")
        || payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .is_some();

    Ok(Some(SessionMeta {
        id,
        thread_ids,
        cwd,
        is_subagent,
    }))
}

#[cfg(test)]
fn session_goal_status_from_file(
    path: &std::path::Path,
    session_id: &str,
) -> Result<Option<SessionGoalStatus>> {
    match session_goal_state_from_file(path, session_id)? {
        SessionGoalState::Status(status) => Ok(Some(status)),
        SessionGoalState::Cleared
        | SessionGoalState::MatchedGoalWithoutStatus
        | SessionGoalState::Unknown => Ok(None),
    }
}

enum SessionGoalState {
    Unknown,
    Cleared,
    MatchedGoalWithoutStatus,
    Status(SessionGoalStatus),
}

fn session_goal_state_from_file(
    path: &std::path::Path,
    session_id: &str,
) -> Result<SessionGoalState> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open session file {}", path.display()))?;
    let mut accepted_thread_ids = HashSet::from([session_id.to_string()]);
    let mut latest = SessionGoalState::Unknown;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if value
            .get("type")
            .and_then(|event_type| event_type.as_str())
            .is_some_and(|event_type| event_type == "session_meta")
        {
            add_session_goal_thread_aliases(payload, session_id, &mut accepted_thread_ids);
        }
        if goal_clear_matches_session(payload, &accepted_thread_ids) {
            latest = SessionGoalState::Cleared;
            continue;
        }
        let Some(goal) = payload.get("goal") else {
            continue;
        };
        let matches_session = payload
            .get("threadId")
            .and_then(|thread_id| thread_id.as_str())
            .is_some_and(|thread_id| accepted_thread_ids.contains(thread_id))
            || goal
                .get("threadId")
                .and_then(|thread_id| thread_id.as_str())
                .is_some_and(|thread_id| accepted_thread_ids.contains(thread_id));
        if !matches_session {
            continue;
        }

        latest = SessionGoalState::MatchedGoalWithoutStatus;
        let Some(status) = goal
            .get("status")
            .and_then(|status| status.as_str())
            .and_then(SessionGoalStatus::parse)
        else {
            continue;
        };
        latest = SessionGoalState::Status(status);
    }

    Ok(latest)
}

fn goal_clear_matches_session(
    payload: &serde_json::Value,
    accepted_thread_ids: &HashSet<String>,
) -> bool {
    let is_clear = [
        &["type"][..],
        &["method"][..],
        &["notification", "type"][..],
        &["notification", "method"][..],
    ]
    .into_iter()
    .filter_map(|path| string_at_path(payload, path))
    .any(is_goal_clear_marker);
    if !is_clear {
        return false;
    }

    [
        &["threadId"][..],
        &["thread_id"][..],
        &["params", "threadId"][..],
        &["params", "thread_id"][..],
        &["notification", "threadId"][..],
        &["notification", "thread_id"][..],
        &["notification", "params", "threadId"][..],
        &["notification", "params", "thread_id"][..],
    ]
    .into_iter()
    .filter_map(|path| string_at_path(payload, path))
    .any(|thread_id| accepted_thread_ids.contains(thread_id))
}

fn is_goal_clear_marker(value: &str) -> bool {
    matches!(
        value,
        "thread_goal_cleared" | "thread/goal/cleared" | "ThreadGoalCleared"
    )
}

fn add_session_goal_thread_aliases(
    payload: &serde_json::Value,
    session_id: &str,
    accepted_thread_ids: &mut HashSet<String>,
) {
    let thread_ids = session_thread_ids_from_payload(payload);
    if !thread_ids.contains(session_id) {
        return;
    }
    accepted_thread_ids.extend(thread_ids);
}

fn session_thread_ids_from_payload(payload: &serde_json::Value) -> HashSet<String> {
    let mut thread_ids = HashSet::new();
    for path in [
        &["id"][..],
        &["forked_from_id"][..],
        &["parent_thread_id"][..],
        &["source", "subagent", "thread_spawn", "parent_thread_id"][..],
        &["source", "thread_spawn", "parent_thread_id"][..],
    ] {
        if let Some(thread_id) = string_at_path(payload, path) {
            thread_ids.insert(thread_id.to_string());
        }
    }
    thread_ids
}

fn string_at_path<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

struct PtyCodexRunner {
    state_reporter: Option<HerdrAgentReporter>,
}

impl PtyCodexRunner {
    fn new(state_reporter: Option<HerdrAgentReporter>) -> Self {
        Self { state_reporter }
    }
}

type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

impl CodexRunner for PtyCodexRunner {
    fn run_codex(&mut self, invocation: &CodexInvocation) -> Result<CodexRunOutcome> {
        run_codex_in_pty(invocation, self.state_reporter.clone())
    }
}

fn run_codex_in_pty(
    invocation: &CodexInvocation,
    state_reporter: Option<HerdrAgentReporter>,
) -> Result<CodexRunOutcome> {
    let interactive = std::io::stdin().is_terminal();
    let _raw_mode = RawModeGuard::enable(interactive)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(current_pty_size())
        .context("failed to open pty")?;
    let mut command = CommandBuilder::new("codex");
    for arg in &invocation.args {
        command.arg(arg);
    }
    command.cwd(invocation.cwd.as_os_str());
    if std::env::var_os("TERM").is_none() {
        command.env("TERM", "xterm-256color");
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to run `codex`")?;
    drop(pair.slave);

    let stop_input = Arc::new(AtomicBool::new(false));
    let spend_cap = Arc::new(AtomicBool::new(false));
    let session_id = Arc::new(Mutex::new(None));

    let mut master = Some(pair.master);
    let reader = master.as_ref().unwrap().try_clone_reader()?;
    let writer = Arc::new(Mutex::new(master.as_ref().unwrap().take_writer()?));
    let mut killer = child.clone_killer();

    let reader_thread = spawn_output_thread(
        reader,
        Arc::clone(&spend_cap),
        Arc::clone(&session_id),
        Arc::clone(&stop_input),
        Arc::clone(&writer),
        invocation.continue_goal_on_start,
        state_reporter,
        move || {
            let _ = killer.kill();
        },
    );
    let input_thread = if interactive {
        Some(InputThread {
            handle: spawn_input_thread(writer, Arc::clone(&stop_input), master.take().unwrap()),
            join_on_stop: true,
        })
    } else {
        Some(InputThread {
            handle: spawn_pipe_input_thread(writer, Arc::clone(&stop_input)),
            join_on_stop: pipe_input_thread_is_stop_aware(),
        })
    };

    let status = child.wait().context("failed to wait for codex")?;
    stop_input.store(true, Ordering::SeqCst);
    if let Some(input_thread) = input_thread
        && input_thread.join_on_stop
    {
        let _ = input_thread.handle.join();
    }
    let _ = reader_thread.join();

    if spend_cap.load(Ordering::SeqCst) {
        let session_id = session_id.lock().ok().and_then(|guard| guard.clone());
        Ok(CodexRunOutcome::SpendCap { session_id })
    } else {
        Ok(CodexRunOutcome::Exited(status.exit_code() as i32))
    }
}

struct InputThread {
    handle: std::thread::JoinHandle<()>,
    join_on_stop: bool,
}

fn current_pty_size() -> PtySize {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable(interactive: bool) -> Result<Self> {
        if interactive {
            terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;
        }
        Ok(Self {
            enabled: interactive,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = terminal::disable_raw_mode();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_output_thread(
    mut reader: Box<dyn Read + Send>,
    spend_cap: Arc<AtomicBool>,
    session_id: Arc<Mutex<Option<String>>>,
    stop_input: Arc<AtomicBool>,
    continue_goal_writer: SharedPtyWriter,
    continue_goal_on_start: bool,
    state_reporter: Option<HerdrAgentReporter>,
    mut kill_child: impl FnMut() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        let mut buffer = [0; 8192];
        let mut recent = String::new();
        let mut continue_goal_auto_enter = ContinueGoalAutoEnter::default();
        let mut state_watcher = state_reporter.as_ref().map(|_| CodexStateWatcher::new());

        loop {
            let bytes_read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk = &buffer[..bytes_read];
            let _ = stdout.write_all(chunk);
            let _ = stdout.flush();

            let chunk_text = String::from_utf8_lossy(chunk);
            push_bounded_recent(&mut recent, &chunk_text);

            if spend_cap_seen(&recent) {
                spend_cap.store(true, Ordering::SeqCst);
                if let Some(id) = find_resume_hint_session_id(&recent)
                    && let Ok(mut guard) = session_id.lock()
                {
                    *guard = Some(id);
                }
                stop_input.store(true, Ordering::SeqCst);
                kill_child();
                break;
            }

            if continue_goal_on_start && continue_goal_auto_enter.observe_output(&chunk_text) {
                let _ = write_continue_goal_enter_to_pty(&continue_goal_writer);
            }

            if let (Some(reporter), Some(watcher)) =
                (state_reporter.as_ref(), state_watcher.as_mut())
                && let Some(state) = watcher.observe(chunk)
            {
                reporter.report(state, None);
            }
        }
    })
}

#[derive(Default)]
struct ContinueGoalAutoEnter {
    recent: String,
}

impl ContinueGoalAutoEnter {
    fn observe_output(&mut self, output: &str) -> bool {
        push_bounded_recent(&mut self.recent, output);
        if resume_paused_goal_prompt_seen(&self.recent) {
            self.recent.clear();
            return true;
        }
        false
    }
}

fn push_bounded_recent(recent: &mut String, output: &str) {
    recent.push_str(output);
    if recent.len() > 16_384 {
        let keep_from = recent.len().saturating_sub(8_192);
        let drain_to = recent
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= keep_from)
            .unwrap_or(0);
        recent.drain(..drain_to);
    }
}

#[cfg(unix)]
fn pipe_input_thread_is_stop_aware() -> bool {
    true
}

#[cfg(not(unix))]
fn pipe_input_thread_is_stop_aware() -> bool {
    false
}

#[cfg(unix)]
fn spawn_pipe_input_thread(
    writer: SharedPtyWriter,
    stop_input: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let stdin_fd = stdin.as_raw_fd();
        let mut buffer = [0; 8192];
        while !stop_input.load(Ordering::SeqCst) {
            let mut pollfd = libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut pollfd, 1, 50) };
            if result < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if result == 0 {
                continue;
            }
            if pollfd.revents & libc::POLLIN == 0 {
                if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    break;
                }
                continue;
            }

            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if !write_to_pty(&writer, &buffer[..n]) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

#[cfg(not(unix))]
fn spawn_pipe_input_thread(
    writer: SharedPtyWriter,
    stop_input: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0; 8192];
        while !stop_input.load(Ordering::SeqCst) {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if !write_to_pty(&writer, &buffer[..n]) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

#[cfg(unix)]
fn spawn_input_thread(
    writer: SharedPtyWriter,
    stop_input: Arc<AtomicBool>,
    master: Box<dyn portable_pty::MasterPty + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let stdin_fd = stdin.as_raw_fd();
        let mut stdin = stdin.lock();
        let mut buffer = [0; 8192];
        let mut last_size = terminal::size().ok();

        while !stop_input.load(Ordering::SeqCst) {
            resize_child_if_needed(master.as_ref(), &mut last_size);

            let mut pollfd = libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut pollfd, 1, 50) };
            if result < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if result == 0 {
                continue;
            }
            if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                break;
            }
            if pollfd.revents & libc::POLLIN == 0 {
                continue;
            }

            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if !write_to_pty(&writer, &buffer[..n]) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

#[cfg(not(unix))]
fn spawn_input_thread(
    writer: SharedPtyWriter,
    stop_input: Arc<AtomicBool>,
    master: Box<dyn portable_pty::MasterPty + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop_input.load(Ordering::SeqCst) {
            match event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        if let Some(bytes) = key_event_bytes(key) {
                            if !write_to_pty(&writer, &bytes) {
                                break;
                            }
                        }
                    }
                    Ok(Event::Paste(text)) => {
                        if !write_to_pty(&writer, &paste_event_bytes(&text)) {
                            break;
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    })
}

fn write_continue_goal_enter(writer: &mut dyn Write) -> bool {
    writer.write_all(b"\r").and_then(|_| writer.flush()).is_ok()
}

fn write_continue_goal_enter_to_pty(writer: &SharedPtyWriter) -> bool {
    writer
        .lock()
        .map(|mut writer| write_continue_goal_enter(writer.as_mut()))
        .unwrap_or(false)
}

fn write_to_pty(writer: &SharedPtyWriter, bytes: &[u8]) -> bool {
    writer
        .lock()
        .map(|mut writer| writer.write_all(bytes).and_then(|_| writer.flush()).is_ok())
        .unwrap_or(false)
}

fn resume_paused_goal_prompt_seen(output: &str) -> bool {
    output.contains("Resume paused goal?")
        && output.contains("Resume goal")
        && output.contains("Mark it active and continue when idle")
        && output.contains("Press enter to confirm or esc to go back")
}

#[cfg(not(unix))]
fn paste_event_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[cfg(not(unix))]
fn key_event_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let mut bytes = Vec::new();
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.push(0x1b);
    }

    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            bytes.push(control_byte(c)?);
        }
        KeyCode::Char(c) => {
            let mut encoded = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => bytes.extend_from_slice(function_key_bytes(n)?),
        _ => return None,
    }

    Some(bytes)
}

#[cfg(not(unix))]
fn control_byte(c: char) -> Option<u8> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_alphabetic() {
        Some((lower as u8) - b'a' + 1)
    } else {
        match c {
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            _ => None,
        }
    }
}

#[cfg(not(unix))]
fn function_key_bytes(n: u8) -> Option<&'static [u8]> {
    match n {
        1 => Some(b"\x1bOP"),
        2 => Some(b"\x1bOQ"),
        3 => Some(b"\x1bOR"),
        4 => Some(b"\x1bOS"),
        5 => Some(b"\x1b[15~"),
        6 => Some(b"\x1b[17~"),
        7 => Some(b"\x1b[18~"),
        8 => Some(b"\x1b[19~"),
        9 => Some(b"\x1b[20~"),
        10 => Some(b"\x1b[21~"),
        11 => Some(b"\x1b[23~"),
        12 => Some(b"\x1b[24~"),
        _ => None,
    }
}

fn resize_child_if_needed(
    master: &(dyn portable_pty::MasterPty + Send),
    last_size: &mut Option<(u16, u16)>,
) {
    let Ok((cols, rows)) = terminal::size() else {
        return;
    };
    if last_size.is_some_and(|size| size == (cols, rows)) {
        return;
    }
    *last_size = Some((cols, rows));
    let _ = master.resize(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
}

fn spend_cap_seen(output: &str) -> bool {
    // Codex renders the error inside a bordered box, so the message is wrapped
    // across lines, padded, and interleaved with box glyphs and ANSI styling.
    // Flatten all of that and match the `■` marker immediately followed by the
    // message, so detection is independent of terminal width / wrapping.
    let normalized = normalize_spend_cap_text(output);
    [SPEND_CAP_MESSAGE, SELF_MANAGED_SPEND_CAP_MESSAGE]
        .iter()
        .any(|message| {
            let needle = format!("\u{25a0}{}", normalize_spend_cap_text(message));
            normalized.contains(&needle)
        })
}

/// Flatten Codex's bordered error box for matching: drop ANSI escapes,
/// box-drawing/block glyphs, and all whitespace, while keeping the `■` error
/// marker (U+25A0, just past the block-element range).
fn normalize_spend_cap_text(text: &str) -> String {
    strip_ansi_escapes(text)
        .chars()
        .filter(|c| !c.is_whitespace() && !is_box_drawing(*c))
        .collect()
}

fn is_box_drawing(c: char) -> bool {
    // Box Drawing (U+2500–U+257F) and Block Elements (U+2580–U+259F), e.g. the
    // `▕` right border. The `■` marker is U+25A0, deliberately outside this range.
    ('\u{2500}'..='\u{259f}').contains(&c)
}

/// Remove ANSI escape sequences (CSI, OSC, and lone two-byte escapes) so styled,
/// cursor-positioned terminal output can be matched as plain text.
fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: parameters/intermediates until a final byte 0x40–0x7E.
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: until BEL or the ST terminator (ESC \).
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\u{07}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

fn find_resume_hint_session_id(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .filter_map(|line| {
            line.find("codex resume")
                .map(|index| &line[index + "codex resume".len()..])
        })
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_hexdigit() || c == '-')))
        .find(|token| is_session_id(token))
        .map(str::to_string)
}

/// Renders Codex's raw PTY output into a terminal screen and derives the agent
/// state from the *current* screen. Codexctl runs Codex inside its own PTY, so
/// herdr sees `codexctl` as the foreground process and can't run its own
/// detection; this watcher lets codexctl report Codex's real state instead of a
/// constant `unknown`. Working on the live screen (rather than the accumulated
/// byte stream) avoids reporting a stale `Working` after a past frame scrolls
/// off.
struct CodexStateWatcher {
    parser: vt100::Parser,
    last: HerdrAgentState,
}

impl CodexStateWatcher {
    fn new() -> Self {
        let size = current_pty_size();
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, 0),
            last: HerdrAgentState::Unknown,
        }
    }

    /// Feed a chunk of Codex output and return the new state only when it
    /// changed, so callers report transitions instead of every frame.
    fn observe(&mut self, chunk: &[u8]) -> Option<HerdrAgentState> {
        if let Ok((cols, rows)) = terminal::size()
            && self.parser.screen().size() != (rows, cols)
        {
            self.parser.screen_mut().set_size(rows, cols);
        }
        self.parser.process(chunk);
        let state = detect_codex_state(&self.parser.screen().contents());
        (state != self.last).then(|| {
            self.last = state;
            state
        })
    }
}

/// Classify Codex's current screen. Mirrors herdr's own `detect_codex` so a
/// wrapped Codex reports the same Working/Idle/Blocked states herdr would
/// detect for an unwrapped one.
fn detect_codex_state(content: &str) -> HerdrAgentState {
    let lower = content.to_lowercase();

    if lower.contains("press enter to confirm or esc to cancel")
        || lower.contains("enter to submit answer")
        || lower.contains("enter to submit all")
        || lower.contains("allow command?")
        || lower.contains("[y/n]")
        || lower.contains("yes (y)")
        || codex_confirmation_prompt(&lower)
    {
        return HerdrAgentState::Blocked;
    }

    if codex_interrupt_pattern(&lower) || codex_working_header(content) {
        return HerdrAgentState::Working;
    }

    HerdrAgentState::Idle
}

fn codex_confirmation_prompt(lower_content: &str) -> bool {
    if let Some(pos) = lower_content
        .find("do you want")
        .or_else(|| lower_content.find("would you like"))
    {
        let after = &lower_content[pos..];
        return after.contains("yes") || after.contains('❯');
    }
    false
}

fn codex_interrupt_pattern(lower_content: &str) -> bool {
    lower_content.contains("esc to interrupt")
        || lower_content.contains("ctrl+c to interrupt")
        || (lower_content.contains("esc") && lower_content.contains("interrupt"))
}

fn codex_working_header(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('•') && trimmed.contains("Working (")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "019e8489-aa28-7071-ab90-16b81c7cfd1d";
    const JWT_HDR: &str = "eyJhbGciOiJub25lIn0";

    #[test]
    fn session_id_from_resume_args_reads_explicit_session() {
        let args = vec!["resume".to_string(), SESSION_ID.to_string()];

        assert_eq!(
            session_id_from_codex_args(&args).as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn session_id_from_resume_args_skips_resume_flags() {
        let args = vec![
            "resume".to_string(),
            "--all".to_string(),
            SESSION_ID.to_string(),
        ];

        assert_eq!(
            session_id_from_codex_args(&args).as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn session_id_from_resume_args_allows_global_codex_flags() {
        let args = vec![
            "-C".to_string(),
            "/Users/amirjakoby/Code/codexctl".to_string(),
            "resume".to_string(),
            SESSION_ID.to_string(),
        ];

        assert_eq!(
            session_id_from_codex_args(&args).as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn invocation_args_add_current_cwd_when_not_supplied() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");

        assert_eq!(
            invocation_args_for_cwd(&["resume".to_string(), SESSION_ID.to_string()], &cwd),
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
            ]
        );
    }

    #[test]
    fn invocation_args_preserve_explicit_cwd() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");

        assert_eq!(
            invocation_args_for_cwd(
                &[
                    "--cd".to_string(),
                    "/tmp/other".to_string(),
                    "resume".to_string(),
                    SESSION_ID.to_string(),
                ],
                &cwd,
            ),
            vec![
                "--cd".to_string(),
                "/tmp/other".to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
            ]
        );
    }

    #[test]
    fn invocation_args_ignore_cwd_after_codex_terminator() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");

        assert_eq!(
            invocation_args_for_cwd(
                &[
                    "--".to_string(),
                    "--cd".to_string(),
                    "/tmp/prompt-text".to_string(),
                    "explain".to_string(),
                ],
                &cwd,
            ),
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "--".to_string(),
                "--cd".to_string(),
                "/tmp/prompt-text".to_string(),
                "explain".to_string(),
            ]
        );
    }

    #[test]
    fn preserved_codex_options_stop_at_codex_terminator() {
        assert_eq!(
            preserved_codex_options(&[
                "--".to_string(),
                "--cd".to_string(),
                "/tmp/prompt-text".to_string(),
            ]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn resume_command_index_ignores_prompt_after_codex_terminator() {
        assert_eq!(
            resume_command_index(&[
                "--".to_string(),
                "resume".to_string(),
                SESSION_ID.to_string()
            ]),
            None
        );
    }

    #[test]
    fn alias_for_auth_json_matches_custom_child_auth_by_subject() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let seat_a_old = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.old");
        let seat_a_live = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QSJ9.live");
        let seat_b = format!("{JWT_HDR}.eyJzdWIiOiJzZWF0QiJ9.sig");
        write_profile_auth(&paths, "failed@test", &seat_a_old);
        write_profile_auth(&paths, "next@test", &seat_b);
        let custom_auth = tmp.path().join("custom-codex-home").join("auth.json");
        std::fs::create_dir_all(custom_auth.parent().unwrap()).unwrap();
        std::fs::write(
            &custom_auth,
            format!(r#"{{"access_token":"{seat_a_live}"}}"#),
        )
        .unwrap();

        assert_eq!(
            profile::alias_for_auth_json_from(&paths, &custom_auth)
                .unwrap()
                .as_deref(),
            Some("failed@test")
        );
    }

    #[test]
    fn resume_hint_session_id_ignores_unrelated_screen_uuid() {
        let output = format!("run id: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n{SPEND_CAP_MESSAGE}");

        assert_eq!(find_resume_hint_session_id(&output), None);
    }

    #[test]
    fn resume_hint_session_id_reads_codex_resume_hint() {
        let output = format!("To continue, run codex resume {SESSION_ID}\n{SPEND_CAP_MESSAGE}");

        assert_eq!(
            find_resume_hint_session_id(&output).as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn spend_cap_seen_requires_codex_error_marker() {
        let output = format!("assistant quoted: {SPEND_CAP_MESSAGE}");

        assert!(!spend_cap_seen(&output));
        assert!(spend_cap_seen(&format!("\u{25a0} {SPEND_CAP_MESSAGE}")));
        assert!(spend_cap_seen(&format!(
            "\u{25a0} {SELF_MANAGED_SPEND_CAP_MESSAGE}"
        )));
    }

    #[test]
    fn spend_cap_seen_detects_wrapped_bordered_box() {
        // Codex wraps the message inside a narrow bordered box, splitting it
        // across lines with right borders and padding.
        let boxed = concat!(
            "\u{25a0} You hit your spend cap set by the owner of \u{2595}\n",
            "your workspace. Ask an owner to increase your\u{2595}\n",
            "spend cap to continue.                       \u{2595}\n",
        );

        assert!(spend_cap_seen(boxed));
    }

    #[test]
    fn spend_cap_seen_ignores_ansi_styling_around_box() {
        // Same box, but with SGR color codes and a cursor move between rows.
        let styled = concat!(
            "\u{1b}[1;31m\u{25a0}\u{1b}[0m You hit your spend cap set by the owner of \u{2595}",
            "\u{1b}[2;1Hyour workspace. Ask an owner to increase your\u{2595}",
            "\u{1b}[3;1Hspend cap to continue.                       \u{2595}",
        );

        assert!(spend_cap_seen(styled));
    }

    #[test]
    fn spend_cap_seen_still_requires_message_after_marker() {
        // A bare marker with unrelated text must not trigger recovery.
        assert!(!spend_cap_seen("\u{25a0} build finished with warnings"));
    }

    #[test]
    fn detect_codex_state_reports_idle_for_composer_prompt() {
        let content = concat!(
            "  Built the spend-cap recovery wrapper.\n\n",
            "▌ Ask Codex to do something\n",
            "  ⏎ send   ⌃J newline   /help\n"
        );

        assert_eq!(detect_codex_state(content), HerdrAgentState::Idle);
    }

    #[test]
    fn detect_codex_state_reports_working_on_interrupt_hint() {
        assert_eq!(
            detect_codex_state("• Working (12s • Esc to interrupt)"),
            HerdrAgentState::Working
        );
        assert_eq!(
            detect_codex_state("thinking…   Ctrl+C to interrupt"),
            HerdrAgentState::Working
        );
    }

    #[test]
    fn detect_codex_state_reports_blocked_on_approval_prompts() {
        assert_eq!(
            detect_codex_state("Allow command?  cargo test"),
            HerdrAgentState::Blocked
        );
        assert_eq!(
            detect_codex_state("Run `rm`?\n  Press enter to confirm or Esc to cancel"),
            HerdrAgentState::Blocked
        );
        assert_eq!(
            detect_codex_state("Do you want to apply this change?\n  ❯ Yes   No"),
            HerdrAgentState::Blocked
        );
    }

    #[test]
    fn detect_codex_state_keeps_resume_paused_goal_prompt_non_blocked() {
        let prompt = concat!(
            "  Resume paused goal?\n",
            "› 1. Resume goal   Mark it active and continue when idle\n",
            "  Press enter to confirm or esc to go back\n"
        );

        // Codex's own "go back" goal prompt is not a tool-approval blocker, and
        // codexctl auto-confirms it during recovery; match herdr by not flagging
        // it as Blocked.
        assert_eq!(detect_codex_state(prompt), HerdrAgentState::Idle);
    }

    #[test]
    fn state_watcher_reports_transitions_on_the_rendered_screen() {
        let mut watcher = CodexStateWatcher::new();

        // Working frame ("•" = e2 80 a2, used by Codex's working header).
        assert_eq!(
            watcher.observe(
                b"\x1b[2J\x1b[H\xe2\x80\xa2 Working (3s \xe2\x80\xa2 Esc to interrupt)\r\n"
            ),
            Some(HerdrAgentState::Working)
        );

        // A fresh screen ("\x1b[2J" clears it) that no longer shows the interrupt
        // hint must report Idle, never a stale Working. This is the whole reason
        // detection runs on the rendered screen instead of the byte stream.
        assert_eq!(
            watcher.observe(b"\x1b[2J\x1b[H\xe2\x96\x8c Ask Codex to do something\r\n"),
            Some(HerdrAgentState::Idle)
        );

        // Unchanged screen -> no transition reported.
        assert_eq!(watcher.observe(b"\r\n"), None);

        // Tool-approval prompt -> Blocked.
        assert_eq!(
            watcher.observe(
                b"\x1b[2J\x1b[HAllow command?\r\n  Press enter to confirm or Esc to cancel\r\n"
            ),
            Some(HerdrAgentState::Blocked)
        );
    }

    #[test]
    fn wrapper_reports_codex_agent_to_herdr_while_child_runs() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![CodexRunOutcome::Exited(0)]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let mut reporter = FakeAgentReporter::default();
        let mut consent = DenyBillingConsent;
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            "Continue the previous request.".to_string(),
            cwd,
        );

        let exit = run_with_reporter(
            &options,
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut reporter,
            &mut consent,
            Vec::new(),
        )
        .unwrap();

        assert_eq!(exit, 0);
        assert_eq!(
            reporter.events,
            vec![
                AgentReportEvent::ReportSession {
                    session_id: SESSION_ID.to_string(),
                },
                AgentReportEvent::Report {
                    state: HerdrAgentState::Unknown,
                    session_id: Some(SESSION_ID.to_string()),
                },
                AgentReportEvent::Release,
            ]
        );
    }

    #[test]
    fn herdr_session_reports_use_codex_hook_source() {
        let request = herdr_report_agent_session_request("pane-1", SESSION_ID, 42);

        assert_eq!(request["method"], "pane.report_agent_session");
        assert_eq!(request["params"]["source"], "herdr:codex");
        assert_eq!(request["params"]["agent"], "codex");
        assert_eq!(request["params"]["agent_session_id"], SESSION_ID);
    }

    #[test]
    fn continue_goal_enter_writes_carriage_return() {
        let mut bytes = Vec::new();

        assert!(write_continue_goal_enter(&mut bytes));

        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn resume_paused_goal_prompt_requires_default_resume_action() {
        let prompt = concat!(
            "  Resume paused goal?\n",
            "  Goal: Keep improving the bare goal command.\n\n",
            "› 1. Resume goal   Mark it active and continue when idle\n",
            "  2. Leave paused  Keep it paused; use /goal resume later\n\n",
            "  Press enter to confirm or esc to go back\n"
        );

        assert!(resume_paused_goal_prompt_seen(prompt));
        assert!(!resume_paused_goal_prompt_seen("Resume goal"));
    }

    #[test]
    fn continue_goal_auto_enter_can_retry_after_replayed_prompt_text() {
        let prompt = concat!(
            "  Resume paused goal?\n",
            "  Goal: Keep improving the bare goal command.\n\n",
            "› 1. Resume goal   Mark it active and continue when idle\n",
            "  2. Leave paused  Keep it paused; use /goal resume later\n\n",
            "  Press enter to confirm or esc to go back\n"
        );
        let mut auto_enter = ContinueGoalAutoEnter::default();

        assert!(auto_enter.observe_output(prompt));
        assert!(!auto_enter.observe_output("restored transcript continues\n"));
        assert!(auto_enter.observe_output(prompt));
    }

    #[test]
    fn spend_cap_recovery_switches_profile_and_resumes_session() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            "resume".to_string(),
            cwd.clone(),
        );

        let exit = run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(exit, 0);
        assert_eq!(switcher.switched, vec!["next@test".to_string()]);
        assert!(!runner.calls[0].continue_goal_on_start);
        assert!(!runner.calls[1].continue_goal_on_start);
        assert_eq!(
            runner.calls,
            vec![
                CodexInvocation::new(
                    vec![
                        "--cd".to_string(),
                        cwd.display().to_string(),
                        "resume".to_string(),
                        SESSION_ID.to_string(),
                    ],
                    cwd.clone()
                ),
                CodexInvocation::new(
                    vec![
                        "--cd".to_string(),
                        cwd.display().to_string(),
                        "resume".to_string(),
                        SESSION_ID.to_string(),
                        "resume".to_string()
                    ],
                    cwd,
                )
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_quiet_resumes_and_auto_confirms_paused_goal() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore {
            goal_status: Some(SessionGoalStatus::Paused),
            ..Default::default()
        };
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1],
            CodexInvocation::new(
                vec![
                    "--cd".to_string(),
                    cwd.display().to_string(),
                    "resume".to_string(),
                    SESSION_ID.to_string(),
                ],
                cwd,
            )
            .with_continue_goal_on_start()
        );
    }

    #[test]
    fn spend_cap_recovery_discovers_session_for_new_run() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap { session_id: None },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore {
            discovered: Some(SESSION_ID.to_string()),
            ..Default::default()
        };
        let options = WrapperOptions::new(
            vec!["finish this".to_string()],
            "resume".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1],
            CodexInvocation::new(
                vec![
                    "--cd".to_string(),
                    cwd.display().to_string(),
                    "resume".to_string(),
                    SESSION_ID.to_string(),
                    "resume".to_string()
                ],
                cwd,
            )
        );
    }

    #[test]
    fn spend_cap_recovery_does_not_auto_continue_when_recovery_prompt_is_disabled() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore {
            goal_status: Some(SessionGoalStatus::Paused),
            ..Default::default()
        };
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            String::new(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert!(!runner.calls[1].continue_goal_on_start);
        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_preserves_custom_prompt_for_paused_goal() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore {
            goal_status: Some(SessionGoalStatus::Paused),
            ..Default::default()
        };
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            "do X after switching".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert!(!runner.calls[1].continue_goal_on_start);
        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
                "do X after switching".to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_preserves_default_prompt_for_budget_limited_goal() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore {
            goal_status: Some(SessionGoalStatus::BudgetLimited),
            ..Default::default()
        };
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            DEFAULT_RECOVERY_PROMPT.to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert!(!runner.calls[1].continue_goal_on_start);
        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
                DEFAULT_RECOVERY_PROMPT.to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_prefers_explicit_resume_id_over_detected_uuid() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let unrelated = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(unrelated.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert!(runner.calls[1].args.contains(&SESSION_ID.to_string()));
        assert!(!runner.calls[1].args.contains(&unrelated.to_string()));
    }

    #[test]
    fn spend_cap_recovery_replaces_resume_last_prompt() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap { session_id: None },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec![
                "resume".to_string(),
                "--last".to_string(),
                "fix tests".to_string(),
            ],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                "--last".to_string(),
                "Continue the previous request.".to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_appends_resume_last_prompt_when_missing() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap { session_id: None },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec!["resume".to_string(), "--last".to_string()],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                "--last".to_string(),
                "Continue the previous request.".to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_replaces_named_resume_prompt() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap { session_id: None },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec![
                "resume".to_string(),
                "release-lane".to_string(),
                "fix tests".to_string(),
            ],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "resume".to_string(),
                "release-lane".to_string(),
                "Continue the previous request.".to_string(),
            ]
        );
    }

    #[test]
    fn session_discovery_ignores_new_files_for_other_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let mut store = FilesystemSessionStore::new(&paths).unwrap();
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        write_session_file(
            &sessions.join("rollout-other.jsonl"),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "/tmp/other",
        );
        write_session_file(
            &sessions.join("rollout-current.jsonl"),
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );
        let invocation = CodexInvocation::new(
            vec![
                "--cd".to_string(),
                "/Users/amirjakoby/Code/codexctl".to_string(),
            ],
            PathBuf::from("/Users/amirjakoby/Code/codexctl"),
        );

        assert_eq!(
            store
                .discover_latest_session_id(&invocation)
                .unwrap()
                .as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn session_discovery_ignores_new_subagent_files_for_same_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let mut store = FilesystemSessionStore::new(&paths).unwrap();
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        write_session_file(
            &sessions.join("rollout-current.jsonl"),
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_subagent_session_file(
            &sessions.join("rollout-subagent.jsonl"),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "/Users/amirjakoby/Code/codexctl",
        );
        let invocation = CodexInvocation::new(
            vec![
                "--cd".to_string(),
                "/Users/amirjakoby/Code/codexctl".to_string(),
            ],
            PathBuf::from("/Users/amirjakoby/Code/codexctl"),
        );

        assert_eq!(
            store
                .discover_latest_session_id(&invocation)
                .unwrap()
                .as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn session_discovery_skips_invalid_new_session_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let mut store = FilesystemSessionStore::new(&paths).unwrap();
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("rollout-bad.jsonl"), "{not-json\n").unwrap();
        write_session_file(
            &sessions.join("rollout-current.jsonl"),
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );
        let invocation = CodexInvocation::new(
            vec![
                "--cd".to_string(),
                "/Users/amirjakoby/Code/codexctl".to_string(),
            ],
            PathBuf::from("/Users/amirjakoby/Code/codexctl"),
        );

        assert_eq!(
            store
                .discover_latest_session_id(&invocation)
                .unwrap()
                .as_deref(),
            Some(SESSION_ID)
        );
    }

    #[test]
    fn session_goal_status_reads_latest_thread_goal_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout-current.jsonl");
        write_session_file(&path, SESSION_ID, "/Users/amirjakoby/Code/codexctl");
        append_goal_status_line(&path, "paused");
        append_goal_status_line(&path, "usageLimited");

        assert_eq!(
            session_goal_status_from_file(&path, SESSION_ID).unwrap(),
            Some(SessionGoalStatus::UsageLimited)
        );
        assert_eq!(
            session_goal_status_from_file(&path, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap(),
            None
        );
    }

    #[test]
    fn session_goal_status_treats_goal_clear_as_no_goal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout-current.jsonl");
        write_session_file(&path, SESSION_ID, "/Users/amirjakoby/Code/codexctl");
        append_goal_status_line(&path, "paused");
        append_goal_clear_line_for_thread(&path, SESSION_ID);

        assert_eq!(
            session_goal_status_from_file(&path, SESSION_ID).unwrap(),
            None
        );
    }

    #[test]
    fn session_goal_status_accepts_parent_thread_goal_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout-current.jsonl");
        let parent_thread_id = "019e7766-2198-7071-ab90-16b81c7cfd1d";
        write_resumed_session_file(
            &path,
            SESSION_ID,
            parent_thread_id,
            "/Users/amirjakoby/Code/codexctl",
        );
        append_goal_status_line_for_thread(&path, parent_thread_id, "usageLimited");
        append_goal_status_line_for_thread(&path, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "paused");

        assert_eq!(
            session_goal_status_from_file(&path, SESSION_ID).unwrap(),
            Some(SessionGoalStatus::UsageLimited)
        );
        assert_eq!(
            session_goal_status_from_file(&path, "bbbbbbbb-cccc-dddd-eeee-ffffffffffff").unwrap(),
            None
        );
    }

    #[test]
    fn session_store_goal_status_reads_resumed_rollout_for_explicit_parent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        let resumed_session_id = "019e9480-aaaa-7071-ab90-16b81c7cfd1d";
        let path = sessions.join("rollout-resumed.jsonl");
        write_resumed_session_file(
            &path,
            resumed_session_id,
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );
        append_goal_status_line_for_thread(&path, SESSION_ID, "paused");

        let mut store = FilesystemSessionStore::new(&paths).unwrap();

        assert_eq!(
            store.goal_status_for_session_id(SESSION_ID).unwrap(),
            Some(SessionGoalStatus::Paused)
        );
    }

    #[test]
    fn session_store_goal_status_skips_newer_rollouts_without_goal_events() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        let older_path = sessions.join("rollout-parent.jsonl");
        write_session_file(&older_path, SESSION_ID, "/Users/amirjakoby/Code/codexctl");
        append_goal_status_line_for_thread(&older_path, SESSION_ID, "paused");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let newer_path = sessions.join("rollout-resumed-empty.jsonl");
        write_resumed_session_file(
            &newer_path,
            "019e9480-cccc-7071-ab90-16b81c7cfd1d",
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );

        let mut store = FilesystemSessionStore::new(&paths).unwrap();

        assert_eq!(
            store.goal_status_for_session_id(SESSION_ID).unwrap(),
            Some(SessionGoalStatus::Paused)
        );
    }

    #[test]
    fn session_store_goal_status_stops_at_newer_unparseable_goal_event() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        let older_path = sessions.join("rollout-parent.jsonl");
        write_session_file(&older_path, SESSION_ID, "/Users/amirjakoby/Code/codexctl");
        append_goal_status_line_for_thread(&older_path, SESSION_ID, "paused");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let newer_path = sessions.join("rollout-resumed-new-status.jsonl");
        write_resumed_session_file(
            &newer_path,
            "019e9480-dddd-7071-ab90-16b81c7cfd1d",
            SESSION_ID,
            "/Users/amirjakoby/Code/codexctl",
        );
        append_goal_status_line_for_thread(&newer_path, SESSION_ID, "newTerminalStatus");

        let mut store = FilesystemSessionStore::new(&paths).unwrap();

        assert_eq!(store.goal_status_for_session_id(SESSION_ID).unwrap(), None);
    }

    #[test]
    fn session_store_goal_status_ignores_subagent_rollouts_for_parent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(tmp.path().to_path_buf());
        let sessions = paths
            .home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("04");
        std::fs::create_dir_all(&sessions).unwrap();
        let parent_path = sessions.join("rollout-parent.jsonl");
        write_session_file(&parent_path, SESSION_ID, "/Users/amirjakoby/Code/codexctl");
        append_goal_status_line_for_thread(&parent_path, SESSION_ID, "paused");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let subagent_session_id = "019e9480-bbbb-7071-ab90-16b81c7cfd1d";
        let subagent_path = sessions.join("rollout-subagent.jsonl");
        write_subagent_session_file(
            &subagent_path,
            subagent_session_id,
            "/Users/amirjakoby/Code/codexctl",
        );
        append_goal_status_line_for_thread(&subagent_path, subagent_session_id, "complete");

        let mut store = FilesystemSessionStore::new(&paths).unwrap();

        assert_eq!(
            store.goal_status_for_session_id(SESSION_ID).unwrap(),
            Some(SessionGoalStatus::Paused)
        );
    }

    fn no_bill(alias: &str) -> use_profile::RecoveryCandidate {
        use_profile::RecoveryCandidate {
            alias: alias.to_string(),
            bills_credits: false,
        }
    }

    fn billing(alias: &str) -> use_profile::RecoveryCandidate {
        use_profile::RecoveryCandidate {
            alias: alias.to_string(),
            bills_credits: true,
        }
    }

    fn run_with_consent(
        options: &WrapperOptions,
        runner: &mut impl CodexRunner,
        switcher: &mut impl ProfileSwitcher,
        sessions: &mut impl SessionStore,
        consent: &mut impl RecoveryConsent,
    ) -> Result<i32> {
        let mut reporter = None;
        run_with_reporter(
            options,
            runner,
            switcher,
            sessions,
            &mut reporter,
            consent,
            Vec::new(),
        )
    }

    fn resume_options() -> WrapperOptions {
        WrapperOptions::new(
            vec!["resume".to_string(), SESSION_ID.to_string()],
            DEFAULT_RECOVERY_PROMPT.to_string(),
            PathBuf::from("/Users/amirjakoby/Code/codexctl"),
        )
    }

    #[test]
    fn recovery_loops_through_no_bill_accounts_in_turn() {
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::new(vec![no_bill("amir+2"), no_bill("amir+3")]);
        let mut sessions = FakeSessionStore::default();
        let mut consent = DenyBillingConsent;

        let exit = run_with_consent(
            &resume_options(),
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut consent,
        )
        .unwrap();

        assert_eq!(exit, 0);
        assert_eq!(
            switcher.switched,
            vec!["amir+2".to_string(), "amir+3".to_string()]
        );
    }

    #[test]
    fn recovery_stops_when_only_billing_account_and_consent_declined() {
        let mut runner = FakeCodexRunner::new(vec![CodexRunOutcome::SpendCap {
            session_id: Some(SESSION_ID.to_string()),
        }]);
        let mut switcher = FakeProfileSwitcher::new(vec![billing("amir@sawmills.ai")]);
        let mut sessions = FakeSessionStore::default();
        let mut consent = RecordingConsent {
            allow: false,
            asked: Vec::new(),
        };

        let result = run_with_consent(
            &resume_options(),
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut consent,
        );

        assert!(
            result.is_err(),
            "declined billing switch must stop recovery"
        );
        assert_eq!(consent.asked, vec!["amir@sawmills.ai".to_string()]);
        assert!(
            switcher.switched.is_empty(),
            "must not switch to a billing account without consent"
        );
    }

    #[test]
    fn recovery_switches_to_billing_account_when_consent_granted() {
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::new(vec![billing("amir@sawmills.ai")]);
        let mut sessions = FakeSessionStore::default();
        let mut consent = RecordingConsent {
            allow: true,
            asked: Vec::new(),
        };

        let exit = run_with_consent(
            &resume_options(),
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut consent,
        )
        .unwrap();

        assert_eq!(exit, 0);
        assert_eq!(consent.asked, vec!["amir@sawmills.ai".to_string()]);
        assert_eq!(switcher.switched, vec!["amir@sawmills.ai".to_string()]);
    }

    #[test]
    fn recovery_prefers_no_bill_before_asking_for_billing_account() {
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        // No-bill candidate must be used first; the billing seat only after it.
        let mut switcher =
            FakeProfileSwitcher::new(vec![no_bill("amir+2"), billing("amir@sawmills.ai")]);
        let mut sessions = FakeSessionStore::default();
        let mut consent = RecordingConsent {
            allow: true,
            asked: Vec::new(),
        };

        let exit = run_with_consent(
            &resume_options(),
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut consent,
        )
        .unwrap();

        assert_eq!(exit, 0);
        // amir+2 (no-bill) switched silently; amir@ asked for only after it.
        assert_eq!(consent.asked, vec!["amir@sawmills.ai".to_string()]);
        assert_eq!(
            switcher.switched,
            vec!["amir+2".to_string(), "amir@sawmills.ai".to_string()]
        );
    }

    #[test]
    fn recovery_bails_when_no_alternate_account_available() {
        let mut runner = FakeCodexRunner::new(vec![CodexRunOutcome::SpendCap {
            session_id: Some(SESSION_ID.to_string()),
        }]);
        let mut switcher = FakeProfileSwitcher::new(Vec::new());
        let mut sessions = FakeSessionStore::default();
        let mut consent = DenyBillingConsent;

        let result = run_with_consent(
            &resume_options(),
            &mut runner,
            &mut switcher,
            &mut sessions,
            &mut consent,
        );

        assert!(result.is_err(), "no usable account must stop recovery");
        assert!(switcher.switched.is_empty());
    }

    #[test]
    fn allow_billing_flag_approves_without_prompting() {
        // With --allow-billing the consent approves up front, so it works even
        // with no terminal to prompt on (unattended runs).
        let mut consent = InteractiveConsent {
            allow_billing: true,
        };
        assert!(consent.allow_billing_account("amir@sawmills.ai"));
    }

    fn write_session_file(path: &std::path::Path, session_id: &str, cwd: &str) {
        let line = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "cwd": cwd,
            }
        });
        std::fs::write(path, format!("{line}\n")).unwrap();
    }

    fn append_goal_status_line(path: &std::path::Path, status: &str) {
        append_goal_status_line_for_thread(path, SESSION_ID, status);
    }

    fn append_goal_status_line_for_thread(path: &std::path::Path, thread_id: &str, status: &str) {
        let line = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "thread_goal_updated",
                "threadId": thread_id,
                "goal": {
                    "threadId": thread_id,
                    "status": status,
                    "objective": "finish the task"
                }
            }
        });
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn append_goal_clear_line_for_thread(path: &std::path::Path, thread_id: &str) {
        let line = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "thread_goal_cleared",
                "threadId": thread_id
            }
        });
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn write_resumed_session_file(
        path: &std::path::Path,
        session_id: &str,
        parent_thread_id: &str,
        cwd: &str,
    ) {
        let line = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "cwd": cwd,
                "forked_from_id": parent_thread_id,
                "parent_thread_id": parent_thread_id
            }
        });
        std::fs::write(path, format!("{line}\n")).unwrap();
    }

    fn write_subagent_session_file(path: &std::path::Path, session_id: &str, cwd: &str) {
        let line = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "cwd": cwd,
                "thread_source": "subagent",
                "source": {
                    "subagent": {
                        "thread_spawn": {
                            "parent_thread_id": SESSION_ID
                        }
                    }
                }
            }
        });
        std::fs::write(path, format!("{line}\n")).unwrap();
    }

    fn write_profile_auth(paths: &Paths, alias: &str, access_token: &str) {
        let dir = paths.profiles_dir().join(alias);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            format!(r#"{{"access_token":"{access_token}"}}"#),
        )
        .unwrap();
        let meta = profile::Meta {
            alias: alias.to_string(),
            email: None,
            plan: None,
            saved_at: "2026-01-01T00:00:00Z".to_string(),
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn spend_cap_recovery_preserves_explicit_cwd_and_model() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let explicit_cwd = "/tmp/other-repo".to_string();
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec![
                "--cd".to_string(),
                explicit_cwd.clone(),
                "-m".to_string(),
                "gpt-5".to_string(),
                "finish this".to_string(),
            ],
            "Continue the previous request.".to_string(),
            cwd,
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                explicit_cwd,
                "-m".to_string(),
                "gpt-5".to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
                "Continue the previous request.".to_string(),
            ]
        );
    }

    #[test]
    fn spend_cap_recovery_preserves_resume_relevant_codex_flags() {
        let cwd = PathBuf::from("/Users/amirjakoby/Code/codexctl");
        let mut runner = FakeCodexRunner::new(vec![
            CodexRunOutcome::SpendCap {
                session_id: Some(SESSION_ID.to_string()),
            },
            CodexRunOutcome::Exited(0),
        ]);
        let mut switcher = FakeProfileSwitcher::default();
        let mut sessions = FakeSessionStore::default();
        let options = WrapperOptions::new(
            vec![
                "--full-auto".to_string(),
                "--skip-git-repo-check".to_string(),
                "--local-provider".to_string(),
                "ollama".to_string(),
                "finish this".to_string(),
            ],
            "Continue the previous request.".to_string(),
            cwd.clone(),
        );

        run_with(&options, &mut runner, &mut switcher, &mut sessions).unwrap();

        assert_eq!(
            runner.calls[1].args,
            vec![
                "--cd".to_string(),
                cwd.display().to_string(),
                "--full-auto".to_string(),
                "--skip-git-repo-check".to_string(),
                "--local-provider".to_string(),
                "ollama".to_string(),
                "resume".to_string(),
                SESSION_ID.to_string(),
                "Continue the previous request.".to_string(),
            ]
        );
    }

    struct FakeProfileSwitcher {
        candidates: Vec<use_profile::RecoveryCandidate>,
        switched: Vec<String>,
    }

    impl FakeProfileSwitcher {
        fn new(candidates: Vec<use_profile::RecoveryCandidate>) -> Self {
            Self {
                candidates,
                switched: Vec::new(),
            }
        }
    }

    impl Default for FakeProfileSwitcher {
        fn default() -> Self {
            Self::new(vec![use_profile::RecoveryCandidate {
                alias: "next@test".to_string(),
                bills_credits: false,
            }])
        }
    }

    impl ProfileSwitcher for FakeProfileSwitcher {
        fn find_recovery_candidate(
            &mut self,
            tried: &[String],
        ) -> anyhow::Result<Option<use_profile::RecoveryCandidate>> {
            Ok(self
                .candidates
                .iter()
                .find(|c| !tried.iter().any(|t| t == &c.alias))
                .cloned())
        }

        fn switch_to(&mut self, alias: &str) -> anyhow::Result<()> {
            self.switched.push(alias.to_string());
            Ok(())
        }
    }

    pub(super) struct DenyBillingConsent;

    impl RecoveryConsent for DenyBillingConsent {
        fn allow_billing_account(&mut self, _alias: &str) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct RecordingConsent {
        allow: bool,
        asked: Vec<String>,
    }

    impl RecoveryConsent for RecordingConsent {
        fn allow_billing_account(&mut self, alias: &str) -> bool {
            self.asked.push(alias.to_string());
            self.allow
        }
    }

    #[derive(Default)]
    struct FakeSessionStore {
        discovered: Option<String>,
        goal_status: Option<SessionGoalStatus>,
    }

    impl SessionStore for FakeSessionStore {
        fn discover_latest_session_id(
            &mut self,
            _invocation: &CodexInvocation,
        ) -> anyhow::Result<Option<String>> {
            Ok(self.discovered.clone())
        }

        fn goal_status_for_session_id(
            &mut self,
            _session_id: &str,
        ) -> anyhow::Result<Option<SessionGoalStatus>> {
            Ok(self.goal_status)
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum AgentReportEvent {
        ReportSession {
            session_id: String,
        },
        Report {
            state: HerdrAgentState,
            session_id: Option<String>,
        },
        Release,
    }

    #[derive(Default)]
    struct FakeAgentReporter {
        events: Vec<AgentReportEvent>,
    }

    impl AgentReporter for FakeAgentReporter {
        fn report_session(&mut self, session_id: &str) {
            self.events.push(AgentReportEvent::ReportSession {
                session_id: session_id.to_string(),
            });
        }

        fn report(&mut self, state: HerdrAgentState, session_id: Option<&str>) {
            self.events.push(AgentReportEvent::Report {
                state,
                session_id: session_id.map(str::to_string),
            });
        }

        fn release(&mut self) {
            self.events.push(AgentReportEvent::Release);
        }
    }

    struct FakeCodexRunner {
        calls: Vec<CodexInvocation>,
        outcomes: Vec<CodexRunOutcome>,
    }

    impl FakeCodexRunner {
        fn new(outcomes: Vec<CodexRunOutcome>) -> Self {
            Self {
                calls: Vec::new(),
                outcomes,
            }
        }
    }

    impl CodexRunner for FakeCodexRunner {
        fn run_codex(&mut self, invocation: &CodexInvocation) -> anyhow::Result<CodexRunOutcome> {
            self.calls.push(invocation.clone());
            Ok(self.outcomes.remove(0))
        }
    }
}
