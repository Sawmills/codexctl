use super::{
    rpc::{RequestHandler, Rpc},
    server::{TokenRequest, TokenResponse},
    vault,
};
use crate::store;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

async fn fetch(
    http: &reqwest::Client,
    url: &str,
    device_token: &str,
    request: TokenRequest,
) -> Result<TokenResponse> {
    let response = http
        .post(format!("{url}/v1/token"))
        .bearer_auth(device_token)
        .json(&request)
        .send()
        .await
        .context("broker request failed")?;
    if !response.status().is_success() {
        bail!("broker rejected token request (HTTP {})", response.status());
    }
    response
        .json()
        .await
        .context("invalid broker token response")
}

struct TokenHandler {
    http: reqwest::Client,
    url: String,
    device_token: String,
    current: TokenResponse,
}

impl RequestHandler for TokenHandler {
    async fn handle(&mut self, request: Value) -> Result<Value> {
        if request.get("method").and_then(Value::as_str)
            != Some("account/chatgptAuthTokens/refresh")
        {
            return Ok(
                json!({"error":{"code":-32601,"message":"interactive requests unavailable in prototype"}}),
            );
        }
        if request
            .pointer("/params/previousAccountId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != self.current.chatgpt_account_id)
        {
            bail!("app-server requested refresh for another account");
        }
        let updated = fetch(
            &self.http,
            &self.url,
            &self.device_token,
            TokenRequest {
                previous_revision: Some(self.current.revision.clone()),
                account_id: Some(self.current.chatgpt_account_id.clone()),
                billing: false,
                alias: None,
            },
        )
        .await?;
        if updated.chatgpt_account_id != self.current.chatgpt_account_id {
            bail!("broker changed account during refresh");
        }
        self.current = updated;
        Ok(json!({"result":self.current.refresh()}))
    }
}

pub async fn run_client(
    url: &str,
    token_file: &Path,
    binary: &Path,
    cwd: &Path,
    model: &str,
    prompt: &str,
) -> Result<String> {
    super::transport::origin(url)?;
    let device_token = String::from_utf8(vault::private_read(token_file)?)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    let tokens = fetch(
        &http,
        url.trim_end_matches('/'),
        &device_token,
        TokenRequest::default(),
    )
    .await?;
    let home = tempfile::Builder::new()
        .prefix("codexctl-central-client-")
        .tempdir()?;
    store::ensure_private_dir(home.path())?;
    let mut rpc = Rpc::start(binary, home.path()).await?;
    let mut handler = TokenHandler {
        http,
        url: url.trim_end_matches('/').into(),
        device_token,
        current: tokens,
    };
    let deadline = Duration::from_secs(180);
    rpc.call_handled(
        "account/login/start",
        handler.current.login(),
        &mut handler,
        deadline,
    )
    .await?;
    let thread = rpc.call_handled("thread/start", json!({"cwd":cwd,"model":model,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}), &mut handler, deadline).await?;
    let thread_id = thread
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("missing thread id")?;
    let turn = rpc
        .call_handled(
            "turn/start",
            json!({"threadId":thread_id,"input":[{"type":"text","text":prompt}]}),
            &mut handler,
            deadline,
        )
        .await?;
    let turn_id = turn
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .context("missing turn id")?;
    let mut output = String::new();
    loop {
        let message = rpc.receive().await?;
        match message.get("method").and_then(Value::as_str) {
            Some("account/chatgptAuthTokens/refresh") => {
                let mut reply = handler.handle(message.clone()).await?;
                reply["id"] = message["id"].clone();
                rpc.send(reply).await?;
            }
            Some("item/completed")
                if message.pointer("/params/threadId").and_then(Value::as_str)
                    == Some(thread_id) =>
            {
                if message.pointer("/params/item/type").and_then(Value::as_str)
                    == Some("agentMessage")
                    && let Some(text) = message.pointer("/params/item/text").and_then(Value::as_str)
                {
                    output.push_str(text);
                }
            }
            Some("turn/completed")
                if message.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id) =>
            {
                if message
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    != Some("completed")
                {
                    bail!("Codex turn did not complete successfully");
                }
                break;
            }
            _ if message.get("id").is_some() && message.get("method").is_some() => {
                rpc.send(json!({"id":message["id"],"error":{"code":-32601,"message":"interactive requests unavailable in prototype"}})).await?;
            }
            _ => {}
        }
    }
    if home.path().join("auth.json").exists() {
        bail!("external-auth client unexpectedly persisted credentials");
    }
    if output.is_empty() {
        bail!("Codex completed without an agent response");
    }
    Ok(output)
}
