use std::{collections::VecDeque, future::Future, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

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
    pending: VecDeque<Value>,
}

impl Rpc {
    pub async fn start(binary: &Path, home: &Path) -> Result<Self> {
        let mut rpc = Self::spawn(binary, home, false)?;
        rpc.initialize().await?;
        Ok(rpc)
    }

    pub fn spawn(binary: &Path, home: &Path, isolate_signals: bool) -> Result<Self> {
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
            .env("CODEX_HOME", home)
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEXCTL_PINNED_ALIAS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        if isolate_signals {
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .context("could not start Codex app-server")?;
        let input = child.stdin.take().context("missing app-server stdin")?;
        let output = BufReader::new(child.stdout.take().context("missing app-server stdout")?);
        Ok(Self {
            _child: child,
            input: Some(input),
            output,
            next_id: 0,
            healthy: true,
            pending: VecDeque::new(),
        })
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.call("initialize", json!({"clientInfo":{"name":"codexctl_central_prototype","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await?;
        self.send(json!({"method":"initialized"})).await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        drop(self.input.take());
        let status = timeout(Duration::from_secs(30), self._child.wait())
            .await
            .context("owner did not exit; runtime retained")??;
        if !status.success() {
            bail!("owner exited unsuccessfully; runtime retained");
        }
        Ok(())
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
        let operation = async {
            self.send(json!({"id":id,"method":method,"params":params}))
                .await?;
            loop {
                let response = self.read_wire().await?;
                if response.get("id") == Some(&json!(id)) && response.get("method").is_none() {
                    if response.get("error").is_some() {
                        bail!("app-server rejected {method}");
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
                if result.is_err() {
                    self.healthy = false;
                }
                result
            }
            Err(_) => {
                self.healthy = false;
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
        assert!(rpc._child.try_wait().unwrap().is_none());
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
