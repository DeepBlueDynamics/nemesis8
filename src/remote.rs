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
}
