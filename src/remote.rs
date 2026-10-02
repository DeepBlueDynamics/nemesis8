use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;

/// HTTP client for delegating commands to a remote nemesis8 gateway.
pub struct RemoteClient {
    base_url: String,
    token: Option<String>,
    client: Client,
}

#[derive(Serialize)]
struct CompletionBody {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

impl RemoteClient {
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.map(|t| t.to_string()),
            client: Client::new(),
        }
    }

    /// The gateway's base URL (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The bearer token this client sends, if any.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Build a request with optional auth header.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.client.request(method, &url);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }
        req
    }

    /// Handle HTTP error responses with friendly messages.
    async fn check_response(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        match status.as_u16() {
            401 | 403 => {
                anyhow::bail!("Authentication failed. Check --token.");
            }
            _ => {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Remote gateway returned {status}: {body}");
            }
        }
    }

    /// Send the request and handle connection errors.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                anyhow::anyhow!(
                    "Cannot reach gateway at {}. Is 'nemesis8 serve' running?",
                    self.base_url
                )
            } else {
                anyhow::anyhow!("{e}")
            }
        })?;
        self.check_response(resp).await
    }

    pub async fn health(&self) -> Result<serde_json::Value> {
        let req = self.request(reqwest::Method::GET, "/health");
        let resp = self.send(req).await?;
        resp.json().await.context("parsing health response")
    }

    pub async fn status(&self) -> Result<serde_json::Value> {
        let req = self.request(reqwest::Method::GET, "/status");
        let resp = self.send(req).await?;
        resp.json().await.context("parsing status response")
    }

    pub async fn run_prompt(
        &self,
        prompt: &str,
        model: Option<&str>,
        danger: bool,
        session_id: Option<&str>,
    ) -> Result<String> {
        let _ = danger; // danger is a server-side config, not sent per-request
        eprintln!("Running on remote gateway at {}...", self.base_url);

        let body = CompletionBody {
            prompt: prompt.to_string(),
            model: model.map(|m| m.to_string()),
            session_id: session_id.map(|s| s.to_string()),
        };

        let req = self
            .request(reqwest::Method::POST, "/completion")
            .json(&body);
        let resp = self.send(req).await?;
        let json: serde_json::Value = resp.json().await.context("parsing completion response")?;

        Ok(json["output"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    pub async fn list_sessions(&self) -> Result<Vec<serde_json::Value>> {
        let req = self.request(reqwest::Method::GET, "/sessions");
        let resp = self.send(req).await?;
        resp.json().await.context("parsing sessions response")
    }

    pub async fn get_session(&self, id: &str) -> Result<serde_json::Value> {
        let path = format!("/sessions/{id}");
        let req = self.request(reqwest::Method::GET, &path);
        let resp = self.send(req).await?;
        resp.json().await.context("parsing session response")
    }

    /// GET /agents — the fleet's agent list (controller merges local + workers).
    pub async fn list_agents(&self) -> Result<Vec<crate::registry::AgentRecord>> {
        let req = self.request(reqwest::Method::GET, "/agents");
        let resp = self.send(req).await?;
        resp.json().await.context("parsing agents response")
    }

    /// POST /agents/{id}/kill — kill an agent (id may be local_id, global, or prefix).
    pub async fn kill_agent(&self, id: &str) -> Result<crate::registry::AgentRecord> {
        let path = format!("/agents/{id}/kill");
        let req = self.request(reqwest::Method::POST, &path);
        let resp = self.send(req).await?;
        resp.json().await.context("parsing kill response")
    }

    /// POST /agents/spawn — launch a new agent on the gateway.
    pub async fn spawn_agent(&self, prompt: &str, provider: Option<&str>) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "prompt": prompt, "provider": provider });
        let req = self.request(reqwest::Method::POST, "/agents/spawn").json(&body);
        let resp = self.send(req).await?;
        resp.json().await.context("parsing spawn response")
    }

    /// GET /health → the gateway's version string.
    pub async fn version(&self) -> Result<String> {
        let h = self.health().await?;
        Ok(h["version"].as_str().unwrap_or("?").to_string())
    }

    /// GET /sessions as typed rows (older gateways answer the same shape).
    pub async fn sessions(&self) -> Result<Vec<crate::session::SessionInfo>> {
        let req = self.request(reqwest::Method::GET, "/sessions");
        let resp = self.send(req).await?;
        resp.json().await.context("parsing sessions response")
    }

    /// GET /providers → the names of providers the gateway's image can run
    /// (`installed` true, or every registered one when the image predates the
    /// label). Empty on a gateway older than 0.26.4, which has no such route.
    pub async fn provider_names(&self) -> Vec<String> {
        let req = self.request(reqwest::Method::GET, "/providers");
        let Ok(resp) = self.send(req).await else {
            return Vec::new();
        };
        let Ok(v) = resp.json::<serde_json::Value>().await else {
            return Vec::new();
        };
        let Some(list) = v["providers"].as_array() else {
            return Vec::new();
        };
        let installed: Vec<String> = list
            .iter()
            .filter(|p| p["installed"].as_bool() == Some(true))
            .filter_map(|p| p["name"].as_str().map(str::to_string))
            .collect();
        if !installed.is_empty() {
            return installed;
        }
        list.iter().filter_map(|p| p["name"].as_str().map(str::to_string)).collect()
    }

    /// POST /agents/spawn with `interactive: true` — start an agent with a TTY
    /// on the gateway and return its agent id, ready for a PTY attach
    /// (`pty_client::run(.., PtyMode::Attach)`). `workspace` is a path ON THE
    /// GATEWAY'S machine; `session_id` resumes that provider session there.
    pub async fn spawn_interactive(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        danger: Option<bool>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String> {
        let body = serde_json::json!({
            "interactive": true,
            "provider": provider,
            "model": model,
            "danger": danger,
            "workspace": workspace,
            "session_id": session_id,
        });
        let req = self.request(reqwest::Method::POST, "/agents/spawn").json(&body);
        let resp = self.send(req).await?;
        let v: serde_json::Value = resp.json().await.context("parsing spawn response")?;
        v["agent_id"]
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the gateway did not return an agent id (needs n8 >= 0.26.7 there): {v}"
                )
            })
    }
}
