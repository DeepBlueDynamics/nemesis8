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

    // ── Socket server (HTTP/SSE) — set `url`; mutually exclusive with `command` ──
    /// Endpoint, e.g. `http://host.docker.internal:9800/mcp`.
    #[serde(default)]
    pub url: Option<String>,
    /// `auto` (detect from path: `/sse` → sse, else http), `http`, or `sse`.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Name of the env var holding a Bearer token. When set, the agent config
    /// gets `Authorization: Bearer <value>` (value read in-container at
    /// config-gen). The var must be forwarded into the container (build_env
    /// forwards every registry server's bearer_token_env).
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Extra static headers merged into the request (e.g. `X-Region`).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    // ── Stdio server — set `command`; mutually exclusive with `url` ──
    /// Launcher binary, e.g. `uvx` (runs `uvx blender-mcp`). When set, the
    /// server is registered as a stdio subprocess rather than a remote endpoint.
    #[serde(default)]
    pub command: Option<String>,
    /// Args for `command`, e.g. `["blender-mcp"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Literal env for the stdio subprocess (e.g. `BLENDER_HOST`). Values are
    /// written verbatim — don't put secrets here (TOML is committed); use a
    /// socket server + bearer_token_env, or a `.py` tool, for those.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Show pre-enabled in the picker / seed new workspaces. Informational for
    /// now; enablement is still per-workspace via `mcp_tools`.
    #[serde(default)]
    pub enabled_by_default: bool,
}

fn default_transport() -> String {
    "auto".to_string()
}

impl McpServerSpec {
    /// True when this is a stdio (command) server rather than a socket one.
    pub fn is_stdio(&self) -> bool {
        self.command.is_some()
    }

    /// Concrete transport for a socket server: resolves `auto` from the URL path
    /// (`/sse` → sse, else http). Meaningless for stdio servers.
    pub fn resolved_transport(&self) -> &'static str {
        match self.transport.as_str() {
            "sse" => "sse",
            "http" => "http",
            _ => {
                let path = self.url.as_deref().unwrap_or("").trim_end_matches('/');
                if path.ends_with("/sse") || path.contains("/sse?") {
                    "sse"
                } else {
                    "http"
                }
            }
        }
    }
}
