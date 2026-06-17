use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Declarative definition of a *socket* (HTTP/SSE) MCP server nemesis8 can wire
/// into any agent's config. Loaded from `mcp/*.toml`, mirroring
/// `provider_def::ProviderDef` and `service_def::ServiceDef`.
///
/// Unlike a `.py` tool (a stdio subprocess shipped in the image), a socket
/// server lives at a URL and is registered directly with the agent — codex via
/// `url`+`bearer_token_env_var`/`http_headers`, gemini/claude via `url`+`headers`.
/// A `mcp_tools` entry that is a bare *name* resolves against this registry, so
/// adding a server is a TOML drop with no image rebuild.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerDef {
    pub server: McpServerSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerSpec {
    /// Registry key + the server name written into the agent config.
    pub name: String,
    /// Alternate names that resolve to this server.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Endpoint, e.g. `http://host.docker.internal:9800/mcp`.
    pub url: String,
    /// `auto` (detect from path: `/sse` → sse, else http), `http`, or `sse`.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Name of the env var holding a Bearer token. When set, the agent config
    /// gets `Authorization: Bearer <value>` (value read in-container at
    /// config-gen). The var must be forwarded into the container (see
    /// `config::mcp_token_envs`).
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Extra static headers merged into the request (e.g. `X-Region`).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Show pre-enabled in the picker / seed new workspaces. Informational for
    /// now; enablement is still per-workspace via `mcp_tools`.
    #[serde(default)]
    pub enabled_by_default: bool,
}

fn default_transport() -> String {
    "auto".to_string()
}

impl McpServerSpec {
    /// Concrete transport: resolves `auto` from the URL path (`/sse` → sse).
    pub fn resolved_transport(&self) -> &'static str {
        match self.transport.as_str() {
            "sse" => "sse",
            "http" => "http",
            _ => {
                let path = self.url.trim_end_matches('/');
                if path.ends_with("/sse") || path.contains("/sse?") {
                    "sse"
                } else {
                    "http"
                }
            }
        }
    }
}
