use std::{collections::VecDeque, future::Future, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

#[derive(Debug)]
pub(super) struct RoutingPolicyError;

impl std::fmt::Display for RoutingPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native workspace routing is unsupported")
    }
}
impl std::error::Error for RoutingPolicyError {}

#[derive(Debug)]
struct AppServerError;
impl std::fmt::Display for AppServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("app-server rejected request")
    }
}
impl std::error::Error for AppServerError {}

#[derive(Debug)]
pub(super) struct RetryableUsageRead;
impl std::fmt::Display for RetryableUsageRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("app-server usage read rejected with an explicit retry marker")
    }
}
impl std::error::Error for RetryableUsageRead {}

pub(super) trait RequestHandler {
    fn keep_notifications(&self) -> bool {
        true
    }
    fn handle(&mut self, request: Value) -> impl Future<Output = Result<Value>> + Send;
}

struct RejectRequests;
impl RequestHandler for RejectRequests {
    fn keep_notifications(&self) -> bool {
        false
    }
    async fn handle(&mut self, _request: Value) -> Result<Value> {
        Ok(json!({"error":{"code":-32601,"message":"request unavailable in this prototype"}}))
    }
}

/// A private stdio connection. Never include protocol payloads in errors or logs.
pub struct Rpc {
    _child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
    healthy: bool,
    retryable_failure: bool,
    timed_out: bool,
    outstanding: Option<(u64, bool)>,
    verified_login: bool,
    rejected_login: bool,
    exportable_login: bool,
    pending: VecDeque<Value>,
}

impl Rpc {
    pub(super) fn completion_pending(&self) -> bool {
        self.outstanding.is_some()
    }

    pub(super) fn process_exited(&mut self) -> bool {
        self._child.try_wait().ok().flatten().is_some()
    }

    pub(super) async fn terminate(mut self) {
        let _ = self.input.take();
        // A failed protocol read can race with the child finishing its
        // credential journal write. Give that write a bounded grace period
        // before forcing termination.
        if timeout(Duration::from_secs(1), self._child.wait())
            .await
            .is_ok()
        {
            return;
        }
        let _ = self._child.start_kill();
        let _ = timeout(Duration::from_secs(5), self._child.wait()).await;
    }

    pub(super) fn retryable_failure(&self) -> bool {
        self.retryable_failure
    }

    pub(super) fn retryable_or_timed_out(&self) -> bool {
        self.retryable_failure() || self.timed_out
    }

    pub(super) fn timed_out(&self) -> bool {
        self.timed_out
    }

    pub async fn start(binary: &Path, home: &Path) -> Result<Self> {
        let mut rpc = Self::spawn(binary, home, false)?;
        rpc.initialize().await?;
        Ok(rpc)
    }

    pub(super) fn spawn_refresh(
        binary: &Path,
        home: &Path,
        proof: super::relogin::ClearedIdentity<'_>,
    ) -> Result<Self> {
        proof.validate_home(home)?;
        Self::spawn(binary, home, true)
    }

    fn spawn(binary: &Path, home: &Path, isolate_signals: bool) -> Result<Self> {
        let home = if isolate_signals {
            std::fs::canonicalize(home).context("could not resolve credential owner home")?
        } else {
            home.to_owned()
        };
        let binary = if isolate_signals {
            super::process::owner_binary(binary)?
        } else {
            binary.to_path_buf()
        };
        let mut command = Command::new(binary);
        command
            .args([
                "app-server",
                "--stdio",
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "-c",
                "features.daemon_auto_start=false",
            ])
            .env("CODEX_HOME", &home)
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if isolate_signals {
            command.current_dir(&home).args([
                "-c",
                "model_provider=\"openai\"",
                "-c",
                "forced_login_method=\"chatgpt\"",
            ]);
        }
        if isolate_signals {
            super::process::isolate(&mut command);
        }
        let mut child = command
            .spawn()
            .context("could not start Codex app-server")?;
        let input = child
            .stdin
            .take()
            .expect("configured piped app-server stdin");
        let output = BufReader::new(
            child
                .stdout
                .take()
                .expect("configured piped app-server stdout"),
        );
        Ok(Self {
            _child: child,
            input: Some(input),
            output,
            next_id: 0,
            healthy: true,
            retryable_failure: false,
            timed_out: false,
            outstanding: None,
            verified_login: false,
            rejected_login: false,
            exportable_login: false,
            pending: VecDeque::new(),
        })
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.call("initialize", json!({"clientInfo":{"name":"codexctl_central_prototype","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await?;
        self.send(json!({"method":"initialized"})).await?;
        Ok(())
    }

    pub fn pid(&self) -> Result<u32> {
        self._child.id().context("credential owner already exited")
    }

    pub fn verified_login(&self) -> bool {
        self.verified_login
    }

    pub fn rejected_login(&self) -> bool {
        self.rejected_login
    }
    fn observe_login(&mut self, verifies: bool, response: &Value) {
        if !verifies {
            return;
        }
        if response.get("error").is_none()
            && response
                .pointer("/result/account/type")
                .and_then(Value::as_str)
                == Some("chatgpt")
            && response
                .get("result")
                .and_then(super::server::supported_native_routing)
                .is_some()
        {
            self.verified_login = true;
        }
    }

    pub(super) async fn remember_exportable_login(&mut self) -> Result<String> {
        let status = self
            .call_handled(
                "getAuthStatus",
                json!({"includeToken":true,"refreshToken":false}),
                &mut RejectRequests,
                Duration::from_secs(10),
            )
            .await?;
        let native_chatgpt = status.get("authMethod").and_then(Value::as_str) == Some("chatgpt")
            && status.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true);
        // Pinned file-backed ChatGPT auth suppresses export on permanent refresh
        // failure, including a proactive refresh before our first status call.
        // Empty/unreadable tokens and other auth kinds report a different method.
        self.rejected_login = native_chatgpt && status.get("authToken") == Some(&Value::Null);
        self.exportable_login = native_chatgpt
            && status
                .get("authToken")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty());
        if !self.exportable_login {
            bail!("native owner did not load the supplied cached ChatGPT login");
        }
        Ok(status["authToken"]
            .as_str()
            .context("missing exported login")?
            .to_owned())
    }

    pub(super) async fn inspect_rejection(&mut self) {
        // A completed RPC error can mean configuration or routing failure. Never
        // infer rejection from its code or message. An unfinished call stays fenced.
        if !self.exportable_login || self.outstanding.is_some() {
            return;
        }
        let healthy_before_inspection = self.healthy;
        self.healthy = true;
        let status = self
            .call_handled(
                "getAuthStatus",
                json!({"includeToken":true,"refreshToken":false}),
                &mut RejectRequests,
                Duration::from_secs(10),
            )
            .await;
        // Require an exportable cached ChatGPT token before the force attempt.
        // Its later suppression is pinned Codex's permanent-failure evidence.
        self.rejected_login = status.as_ref().is_ok_and(|s| {
            s.get("authMethod").and_then(Value::as_str) == Some("chatgpt")
                && s.get("authToken") == Some(&Value::Null)
                && s.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true)
        });
        self.healthy = healthy_before_inspection && status.is_ok() && !self.rejected_login;
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if !self.settle_and_stop().await?.success() {
            bail!("owner exited unsuccessfully; runtime retained");
        }
        Ok(())
    }

    pub(super) async fn settle_and_stop(&mut self) -> Result<std::process::ExitStatus> {
        // Do not close stdin while a timed-out request can still rotate credentials.
        if let Some((id, verifies)) = self.outstanding {
            timeout(Duration::from_secs(180), async {
                loop {
                    let response = self.read_wire().await?;
                    if response.get("id") == Some(&json!(id)) && response.get("method").is_none() {
                        self.outstanding = None;
                        self.observe_login(verifies, &response);
                        return Ok::<(), anyhow::Error>(());
                    }
                    if response.get("id").is_some() && response.get("method").is_some() {
                        self.send(json!({"id":response["id"],"error":{"code":-32601,"message":"server stopping"}})).await?;
                    }
                }
            }).await.context("owner request did not settle; runtime retained")??;
        }
        drop(self.input.take());
        timeout(Duration::from_secs(30), self._child.wait())
            .await
            .context("owner did not exit; runtime retained")?
            .context("could not confirm owner exit; runtime retained")
    }

    /// After an EOF/protocol failure, close stdin and wait without killing the
    /// child. This establishes the dead-child condition before its journal is
    /// used for recovery.
    pub(super) async fn wait_after_eof(&mut self) -> bool {
        drop(self.input.take());
        matches!(
            timeout(Duration::from_secs(30), self._child.wait()).await,
            Ok(Ok(_))
        )
    }

    pub async fn send(&mut self, value: Value) -> Result<()> {
        let input = self.input.as_mut().context("app-server input is closed")?;
        input
            .write_all(serde_json::to_string(&value)?.as_bytes())
            .await?;
        input.write_all(b"\n").await?;
        input.flush().await?;
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<Value> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        self.read_wire().await
    }

    async fn read_wire(&mut self) -> Result<Value> {
        let mut bytes = Vec::new();
        let count = timeout(
            Duration::from_secs(180),
            self.output.read_until(b'\n', &mut bytes),
        )
        .await
        .context("app-server notification timed out")??;
        if count == 0 {
            bail!("app-server closed its connection");
        }
        serde_json::from_slice(&bytes).context("invalid app-server protocol message")
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.call_handled(method, params, &mut RejectRequests, Duration::from_secs(90))
            .await
    }

    pub async fn call_handled(
        &mut self,
        method: &str,
        params: Value,
        handler: &mut impl RequestHandler,
        deadline: Duration,
    ) -> Result<Value> {
        if !self.healthy {
            bail!("app-server connection is unavailable after a protocol failure");
        }
        self.next_id += 1;
        let id = self.next_id;
        let verifies = method == "account/read" && params.get("refreshToken") == Some(&json!(true));
        if verifies {
            self.rejected_login = false;
        }
        self.outstanding = Some((id, verifies));
        let operation = async {
            self.send(json!({"id":id,"method":method,"params":params}))
                .await?;
            loop {
                let response = self.read_wire().await?;
                if response.get("id") == Some(&json!(id)) && response.get("method").is_none() {
                    self.outstanding = None;
                    self.observe_login(verifies, &response);
                    if response.get("error").is_some() {
                        // Only these completed, pinned policy errors are safe
                        // refusals. Never log or retain native protocol payloads.
                        if method == "account/read"
                            && response.pointer("/error/code").and_then(Value::as_i64)
                                == Some(-32603)
                            && matches!(
                                response.pointer("/error/message").and_then(Value::as_str),
                                Some(
                                    "workspace routing discovery missing backend origin"
                                        | "workspace routing discovery has invalid account routing override"
                                )
                            )
                        {
                            return Err(RoutingPolicyError.into());
                        }
                        if method == "account/rateLimits/read"
                            && response.pointer("/error/code").and_then(Value::as_i64)
                                == Some(-32000)
                            && response.pointer("/error/data/retryable") == Some(&json!(true))
                        {
                            return Err(RetryableUsageRead.into());
                        }
                        return Err(AppServerError.into());
                    }
                    return response
                        .get("result")
                        .cloned()
                        .context("app-server response has no result");
                }
                if response.get("id").is_none() {
                    if !handler.keep_notifications() {
                        continue;
                    }
                    if self.pending.len() >= 1024 {
                        bail!("too many app-server notifications");
                    }
                    self.pending.push_back(response);
                    continue;
                }
                if response.get("id").is_some() && response.get("method").is_some() {
                    let mut reply = handler.handle(response.clone()).await?;
                    reply["id"] = response["id"].clone();
                    self.send(reply).await?;
                }
            }
        };
        match timeout(deadline, operation).await {
            Ok(result) => {
                if result.as_ref().is_err_and(|error| {
                    !error.is::<RoutingPolicyError>() && !error.is::<AppServerError>()
                }) {
                    self.healthy = false;
                    self.retryable_failure = true;
                }
                result
            }
            Err(_) => {
                self.healthy = false;
                self.timed_out = true;
                bail!("app-server request timed out; completion is unknown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    #[tokio::test]
    async fn when_a_refresh_times_out_then_the_process_stays_alive_to_finish_its_write() {
        let root = tempfile::tempdir().unwrap();
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"synthetic-user"})).unwrap());
        store::atomic_write(&root.path().join("auth.json"), &serde_json::to_vec(&json!({"tokens":{"access_token":format!("header.{payload}."),"refresh_token":"synthetic"}})).unwrap()).unwrap();
        store::atomic_write(&root.path().join("mode"), b"slow").unwrap();
        store::atomic_write(&root.path().join("count"), b"0").unwrap();
        // Each fixture process gets its own environment through an executable wrapper.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        let wrapper = root.path().join("codex");
        let script = format!(
            "#!/usr/bin/env python3\nimport os, runpy\nos.environ['CENTRAL_TEST_MODE_FILE'] = {}\nos.environ['CENTRAL_TEST_REFRESH_COUNTER'] = {}\nrunpy.run_path({}, run_name='__main__')\n",
            serde_json::to_string(&root.path().join("mode")).unwrap(),
            serde_json::to_string(&root.path().join("count")).unwrap(),
            serde_json::to_string(&fixture).unwrap(),
        );
        store::atomic_write(&wrapper, script.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut rpc = Rpc::start(&wrapper, root.path()).await.unwrap();

        let result = rpc
            .call_handled(
                "account/read",
                json!({"refreshToken":true}),
                &mut RejectRequests,
                Duration::from_millis(30),
            )
            .await;

        assert!(result.is_err());
        assert!(!rpc.retryable_failure());
        assert!(rpc.timed_out());
        assert!(rpc._child.try_wait().unwrap().is_none());
        rpc.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("count")).unwrap(),
            "1"
        );
        let auth: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("auth.json")).unwrap()).unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "synthetic-rotated-refresh");
    }
    #[tokio::test]
    async fn when_a_client_child_starts_then_it_stays_in_the_terminal_process_group() {
        let root = tempfile::tempdir().unwrap();

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/central_codex.py");
        let rpc = Rpc::start(&fixture, root.path()).await.unwrap();
        let child_group = unsafe { libc::getpgid(rpc._child.id().unwrap() as i32) };

        assert_eq!(child_group, unsafe { libc::getpgrp() });
    }
}
