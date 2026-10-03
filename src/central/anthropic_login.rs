use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

const REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
#[derive(Serialize, Deserialize)]
struct Flow {
    user: String,
    machine: String,
    alias: String,
    verifier: String,
    state: String,
    expires_at: i64,
    expected: Option<Identity>,
    exchanging: bool,
}
#[derive(Serialize)]
pub struct Login {
    pub id: String,
    pub authorize_url: String,
    pub expires_at: i64,
}
impl Engine {
    pub async fn start_login(
        &self,
        user: &str,
        machine: &str,
        alias: &str,
        renew: bool,
    ) -> Result<Login> {
        if self.read_only {
            bail!("server is read-only");
        }
        let alias = store::validate_alias(alias)?;
        let existing = self
            .accounts(user)
            .await
            .into_iter()
            .find(|a| a.alias.eq_ignore_ascii_case(alias));
        if existing.is_some() != renew {
            bail!("use login renewal for an existing alias, or a new alias for login");
        }
        let flow = Flow {
            user: user.into(),
            machine: machine.into(),
            alias: alias.into(),
            verifier: URL_SAFE_NO_PAD.encode(enrollment::random_bytes()),
            state: revision(),
            expires_at: now() + 300_000,
            expected: existing.map(|a| a.identity),
            exchanging: false,
        };
        let id = revision();
        let directory = self.state.join("logins");
        store::ensure_private_dir(&directory)?;
        vault::seal(&directory.join(format!("{id}.enc")), &self.key, &flow)?;
        let mut url = reqwest::Url::parse("https://claude.ai/oauth/authorize")?;
        url.query_pairs_mut().extend_pairs([
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT),
            ("scope", "org:create_api_key user:profile user:inference"),
            (
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes())),
            ),
            ("code_challenge_method", "S256"),
            ("state", &flow.state),
        ]);
        Ok(Login {
            id,
            authorize_url: url.to_string(),
            expires_at: flow.expires_at,
        })
    }
    pub async fn finish_login(
        &self,
        user: &str,
        machine: &str,
        id: &str,
        pasted: &str,
    ) -> Result<Receipt> {
        if self.read_only {
            bail!("server is read-only");
        }
        if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid login identifier");
        }
        if let Some(receipt) = self.receipt(user, id).await? {
            return Ok(receipt);
        }
        let path = self.state.join("logins").join(format!("{id}.enc"));
        #[derive(Serialize, Deserialize)]
        struct Retained {
            received_at: i64,
            body: Vec<u8>,
        }
        let retained = self.state.join("logins").join(format!("{id}-grant.enc"));
        // A retry can verify a retained result, but can never replay an uncertain exchange.
        let (flow, resume): (Flow, bool) = {
            let _lock = self.admissions.lock().await;
            let mut flow: Flow = vault::unseal(&path, &self.key)?;
            if flow.user != user || flow.machine != machine {
                bail!("login does not belong to this user and machine");
            }
            let resume = flow.exchanging;
            if resume {
                if !retained.try_exists()? {
                    bail!("login exchange outcome uncertain; start a new login");
                }
            } else {
                if flow.expires_at < now() {
                    bail!("login expired");
                }
                let (code, state) = pasted
                    .trim()
                    .split_once('#')
                    .context("paste code#state from the Claude sign-in page")?;
                if code.is_empty() || state != flow.state {
                    bail!("login state mismatch");
                }
                flow.exchanging = true;
                vault::seal(&path, &self.key, &flow)?;
            }
            (flow, resume)
        };
        let saved: Retained =
            if resume {
                vault::unseal(&retained, &self.key)?
            } else {
                let (code, _) = pasted
                    .trim()
                    .split_once('#')
                    .context("invalid login response")?;
                let response = self.http.post(&self.endpoints.token).json(&json!({
                "grant_type":"authorization_code", "code":code, "state":flow.state,
                "client_id":CLIENT_ID, "redirect_uri":REDIRECT, "code_verifier":flow.verifier
            })).send().await.map_err(|_| anyhow::anyhow!("login exchange outcome uncertain"))?;
                if !response.status().is_success() {
                    bail!("Claude rejected the login exchange");
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| anyhow::anyhow!("login response incomplete"))?;
                let saved = Retained {
                    received_at: now(),
                    body: bytes.to_vec(),
                };
                vault::seal(&retained, &self.key, &saved)?;
                saved
            };
        #[derive(Deserialize)]
        struct Response {
            access_token: String,
            refresh_token: String,
            expires_in: i64,
            scope: String,
        }
        let token: Response = serde_json::from_slice(&saved.body)
            .map_err(|_| anyhow::anyhow!("login response invalid; acquired response retained"))?;
        let expiry = token
            .expires_in
            .checked_mul(1000)
            .and_then(|t| saved.received_at.checked_add(t))
            .filter(|_| token.expires_in > 0)
            .context("invalid login expiry; acquired response retained")?;
        let grant = Grant {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: expiry,
            scopes: token.scope.split_whitespace().map(str::to_owned).collect(),
        };
        let receipt = self
            .admit(user, &flow.alias, id, grant, flow.expected.as_ref())
            .await?;
        for file in [path, retained] {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        store::sync_directory(&self.state.join("logins"))?;
        Ok(receipt)
    }
}
