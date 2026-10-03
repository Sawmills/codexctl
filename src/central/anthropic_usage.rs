use super::*;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub data: Option<Value>,
    pub observed_at: Option<i64>,
    pub next_retry_at: i64,
    pub stale: bool,
    pub error: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
pub(super) struct UsageState {
    last_request: i64,
    cooldown_until: i64,
    failures: u32,
    accounts: BTreeMap<String, Usage>,
}
impl Engine {
    /// A cached read never acquires credentials or contacts the provider.
    pub async fn usage(&self, user: &str, id: &str, cached: bool) -> Result<Usage> {
        self.selected(user, id).await?;
        let mut state = self.usage_state.lock().await;
        let mut result = state.accounts.get(id).cloned().unwrap_or_default();
        result.stale = result.data.is_none()
            || result.error.is_some()
            || now() >= result.next_retry_at
            || result.observed_at.is_some_and(|t| t > now() + MARGIN);
        if cached || now() < result.next_retry_at || now() < state.cooldown_until {
            result.next_retry_at = result.next_retry_at.max(state.cooldown_until);
            return Ok(result);
        }
        let access = match self.acquire(user, id, None).await {
            Ok(access) => access,
            Err(_) => {
                result.error = Some("login_required".into());
                result.stale = true;
                return Ok(result);
            }
        };
        let wait = (state.last_request + 1000 - now()).max(0) as u64;
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        state.last_request = now();
        let response = self
            .http
            .get(format!("{}/api/oauth/usage", self.endpoints.api))
            .bearer_auth(&access.access_token)
            .header("anthropic-beta", BETA)
            .send()
            .await;
        result.next_retry_at = now() + 300_000;
        result.stale = true;
        match response {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(value) if value.is_object() => {
                        let data: serde_json::Map<String, Value> = value
                            .as_object()
                            .unwrap()
                            .iter()
                            .filter(|(k, _)| {
                                k.as_str() == "five_hour"
                                    || k.starts_with("seven_day")
                                    || matches!(k.as_str(), "limits" | "extra_usage")
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for window in ["five_hour", "seven_day"] {
                            if let Some(reset) = data
                                .get(window)
                                .and_then(|v| v.get("resets_at"))
                                .and_then(Value::as_str)
                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            {
                                let time = reset.timestamp_millis();
                                if time > now() {
                                    result.next_retry_at = result.next_retry_at.min(time);
                                }
                            }
                        }
                        result.data = Some(Value::Object(data));
                        result.observed_at = Some(now());
                        result.error = None;
                        result.stale = false;
                        state.failures = 0;
                    }
                    _ => result.error = Some("invalid_usage".into()),
                }
            }
            Ok(response) => {
                let status = response.status().as_u16();
                result.error = Some(
                    match status {
                        401 => "credential_rejected",
                        403 => "missing_scope",
                        429 => "usage_throttled",
                        _ => "usage_unavailable",
                    }
                    .into(),
                );
                if status == 429 {
                    state.failures = state.failures.saturating_add(1);
                    let delay = (300_000_i64 * (1_i64 << state.failures.saturating_sub(1).min(4)))
                        .min(3_600_000);
                    let retry = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| {
                            s.parse::<i64>()
                                .ok()
                                .and_then(|n| n.checked_mul(1000))
                                .or_else(|| {
                                    chrono::DateTime::parse_from_rfc2822(s)
                                        .ok()
                                        .map(|t| t.timestamp_millis().saturating_sub(now()))
                                })
                        })
                        .unwrap_or(0);
                    state.cooldown_until = now().saturating_add(delay.max(retry));
                    result.next_retry_at = state.cooldown_until;
                }
            }
            Err(_) => result.error = Some("usage_unavailable".into()),
        }
        state.accounts.insert(id.into(), result.clone());
        store::atomic_write(
            &self.state.join("usage.json"),
            &serde_json::to_vec(&*state)?,
        )?;
        Ok(result)
    }
}
