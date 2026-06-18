use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Which AI CLI provider to use inside the container.
/// Open newtype — any name registered in providers/*.toml is valid.
/// Validated against the registry at runtime, not at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Provider(pub String);

impl Default for Provider {
    fn default() -> Self {
        // The ONE deliberate product default (which provider a bare config
        // gets) — not provider logic. Everything else is TOML-driven.
        Provider("codex".to_string())
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Provider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.to_lowercase();
        if name.is_empty() {
            return Err("provider name cannot be empty".to_string());
        }
        // Resolve aliases through the provider registry (single source: the
        // TOMLs' `aliases` lists — openai→codex, google→gemini, agy→antigravity,
        // …). Unknown names pass through so custom providers keep working.
        let resolved = match crate::provider_registry::ProviderRegistry::load().get(&name) {
            Some(def) => def.provider.name.clone(),
            None => name,
        };
        Ok(Provider(resolved))
    }
}

/// Top-level config from .nemesis8.toml
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    /// AI CLI provider — any name/alias from the provider registry
    #[serde(default)]
    pub provider: Provider,

    /// Workspace mount mode: "root" or "named"
    #[serde(default = "default_mount_mode")]
    pub workspace_mount_mode: String,

    /// Base host dir for per-session dedicated `/workspace` roots. Each session
    /// mounts `<base>/<agent-name>` at `/workspace`, with the source nested at
    /// `/workspace/<dirname>` — so projects the agent builds by climbing out of
    /// the source dir land on the host (visible + persistent) instead of the
    /// ephemeral container fs. Empty/unset → `<.nemesis8>/workspaces`; `"off"`
    /// disables it (mount only the source, legacy behavior).
    #[serde(default)]
    pub workspace_root: Option<String>,

    /// Active MCP tool filenames
    #[serde(default)]
    pub mcp_tools: Vec<String>,

    /// AI provider CLIs to install when building the Docker image.
    /// Defaults to all built-ins. Remove any you don't need to speed up builds.
    #[serde(default = "default_providers")]
    pub providers: Vec<String>,

    /// Include the latest ffmpeg static build in the Docker image (default: false).
    #[serde(default)]
    pub ffmpeg: bool,

    /// Bake NVIDIA GPU support (CUDA runtime libs + capability env) into the
    /// image (default: false). Equivalent to `n8 build --gpu`.
    #[serde(default)]
    pub gpu: bool,

    /// Include a C/C++ build toolchain in the image so agents can compile native
    /// code (default: false). Equivalent to `n8 build --native`.
    #[serde(default)]
    pub native: bool,

    /// Codex CLI version: pinned (e.g. "0.115.0") or "latest"
    #[serde(default)]
    pub codex_cli_version: Option<String>,

    /// Commands to run inside the container before launching the CLI
    #[serde(default)]
    pub setup_commands: Vec<String>,

    /// Environment section (contains both static vars and env_imports)
    #[serde(default)]
    pub env: EnvSection,

    /// Extra host-to-container bind mounts
    #[serde(default)]
    pub mounts: Vec<Mount>,

    /// Ports to publish host→container so servers an agent starts inside the
    /// container are reachable from the host (e.g. a dev server on :3000).
    /// Entries: "3000" (same port both sides) or "8080:80" (host:container).
    /// NOTE: the in-container server must bind 0.0.0.0, not 127.0.0.1.
    #[serde(default)]
    pub ports: Vec<String>,

    /// Session tracking (auto-updated)
    #[serde(default)]
    pub last_session: Option<LastSession>,

    /// Bare LAST_SESSION key at top level (legacy compat)
    #[serde(rename = "LAST_SESSION", default)]
    pub last_session_id_bare: Option<String>,

    /// Remote gateway URL (skip local Docker, delegate to remote nemesis8 serve)
    #[serde(default)]
    pub remote: Option<String>,

    /// Auth token for remote gateway
    #[serde(default)]
    pub remote_token: Option<String>,

    /// Integrations — auto-connect to running services
    #[serde(default)]
    pub integrations: Integrations,

    /// Control-plane role + topology (hierarchical fleet). Absent = standalone.
    #[serde(default)]
    pub control_plane: Option<ControlPlane>,
}

/// Hierarchical control-plane configuration.
///
/// - `role = "controller"` (default if section present): this daemon holds the
///   fleet registry; workers register up to it.
/// - `role = "worker"`: this daemon manages local agents and pushes them up to
///   `controller_url`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ControlPlane {
    /// "controller" or "worker".
    #[serde(default = "default_role")]
    pub role: String,

    /// For workers: the controller's base URL (e.g. http://workstation:4000).
    #[serde(default)]
    pub controller_url: Option<String>,

    /// Stable host id. Defaults to the machine hostname when omitted.
    #[serde(default)]
    pub host_id: Option<String>,
}

fn default_role() -> String {
    "controller".to_string()
}

/// Auto-discovery integrations
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct Integrations {
    /// Auto-connect to Hyperia if running (checks port 9800)
    #[serde(default)]
    pub hyperia: Option<bool>,

    /// Ferricula URL to auto-connect
    #[serde(default)]
    pub ferricula: Option<String>,
}

/// The [env] section: static key=value vars plus env_imports list
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct EnvSection {
    /// Host env var names to import into the container
    #[serde(default)]
    pub env_imports: Vec<String>,

    /// Static environment variables (all other keys in [env])
    #[serde(flatten)]
    pub vars: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Mount {
    pub host: String,
    pub container: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LastSession {
    pub last_session_id: Option<String>,
    pub last_session_file: Option<String>,
    pub last_session_updated: Option<String>,
    pub last_session_when: Option<String>,
}

fn default_mount_mode() -> String {
    "root".to_string()
}

fn default_providers() -> Vec<String> {
    // Every builtin provider TOML — adding a provider file adds it to the
    // default image build with no code change.
    crate::provider_registry::ProviderRegistry::builtin_names()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            workspace_mount_mode: "root".to_string(),
            workspace_root: None,
            mcp_tools: Vec::new(),
            providers: default_providers(),
            ffmpeg: false,
            gpu: false,
            native: false,
            codex_cli_version: None,
            setup_commands: Vec::new(),
            env: EnvSection::default(),
            mounts: Vec::new(),
            ports: Vec::new(),
            last_session: None,
            last_session_id_bare: None,
            remote: None,
            remote_token: None,
            integrations: Integrations::default(),
            control_plane: None,
        }
    }
}

impl Config {
    /// Load config from a .nemesis8.toml file
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| "parsing .nemesis8.toml")?;
        Ok(config)
    }

    /// Load config or return defaults if file doesn't exist
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config load failed (using defaults): {e}");
                Self::default()
            }
        }
    }

    /// Find the config file, searching upward from the given directory
    pub fn find(start: &Path) -> Option<PathBuf> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join(".nemesis8.toml");
            if candidate.is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
        None
    }

    /// Resolve the effective last session ID
    pub fn last_session_id(&self) -> Option<&str> {
        self.last_session
            .as_ref()
            .and_then(|s| s.last_session_id.as_deref())
            .or(self.last_session_id_bare.as_deref())
    }

    /// Build the full environment map for a container run.
    /// Merges static env, imported host vars, and overrides.
    pub fn container_env(&self) -> Vec<String> {
        let mut env_vec: Vec<String> = Vec::new();

        // Static env from [env] table
        for (k, v) in &self.env.vars {
            env_vec.push(format!("{k}={v}"));
        }

        // Import host env vars
        for key in &self.env.env_imports {
            if let Ok(val) = std::env::var(key) {
                env_vec.push(format!("{key}={val}"));
            }
        }

        env_vec
    }

    /// Build Docker build args for the provider install script.
    pub fn docker_build_args(&self) -> std::collections::HashMap<String, String> {
        let mut args = std::collections::HashMap::new();
        args.insert("INSTALL_PROVIDERS".to_string(), self.providers.join(","));
        args.insert(
            "INCLUDE_FFMPEG".to_string(),
            if self.ffmpeg { "true" } else { "false" }.to_string(),
        );
        args.insert(
            "INCLUDE_GPU".to_string(),
            if self.gpu { "true" } else { "false" }.to_string(),
        );
        args.insert(
            "INCLUDE_NATIVE".to_string(),
            if self.native { "true" } else { "false" }.to_string(),
        );
        args
    }

    pub fn docker_build_args_with_flags(
        &self,
        ffmpeg: bool,
        gpu: bool,
        native: bool,
    ) -> std::collections::HashMap<String, String> {
        let mut args = self.docker_build_args();
        if ffmpeg {
            args.insert("INCLUDE_FFMPEG".to_string(), "true".to_string());
        }
        if gpu {
            args.insert("INCLUDE_GPU".to_string(), "true".to_string());
        }
        if native {
            args.insert("INCLUDE_NATIVE".to_string(), "true".to_string());
        }
        args
    }

    /// The default `.nemesis8.toml` scaffold — used by `n8 init` and the control
    /// room's Config → Init/Reset. Curated `mcp_tools` (only tools the current
    /// image ships); deliberately NOT the retired gnosis-*/ask.py set.
    pub fn scaffold_template(dir_name: &str) -> String {
        format!(
            r#"# nemesis8 config for: {dir_name}

# MCP tools (leave empty to discover all available).
# Built-in binary servers are always on, no entry needed: `nuts-files`
# (read/write/edit/search/diff — replaced gnosis-files-*), `shivvr` (embeddings),
# and `ask` (one-shot second opinion from Claude/Gemini/OpenAI — replaced ask.py).
mcp_tools = [
    "grub-crawler.py",
    "serpapi-search.py",
    "calculate.py",
    "time-tool.py",
    "tool-manager.py",
    "nemesis8-orchestrator.py",
    "hyperia-mcp.py",
]

[env]
# env_imports = ["SERPAPI_API_KEY"]
HYPERIA_URL = "http://host.docker.internal:9800"

[integrations]
hyperia = true
# ferricula = "http://nemesis:8764"

# [[mounts]]
# host = "C:/Users/you/data"
# container = "/workspace/data"
"#
        )
    }

    /// Resolve the per-session workspace-root base dir, or None when disabled.
    /// `"off"`/empty → None; an explicit path → that path; unset → the n8 config
    /// root's `workspaces/` (sibling of the HOME volume, so it isn't double-
    /// mounted through `/opt/nemesis8`).
    pub fn workspace_root_base(&self) -> Option<PathBuf> {
        match self.workspace_root.as_deref() {
            Some("off") | Some("") => None,
            Some(p) => Some(PathBuf::from(p)),
            None => crate::paths::data_home()
                .parent()
                .map(|p| p.join("workspaces"))
                .or_else(|| dirs::home_dir().map(|h| h.join(".nemesis8").join("workspaces"))),
        }
    }

    /// Build Docker bind mounts from the mounts config.
    /// Skips entries whose host path does not exist on this machine so that
    /// Windows paths in a shared config don't crash Linux runs (and vice versa).
    pub fn docker_binds(&self) -> Vec<String> {
        self.mounts
            .iter()
            .filter(|m| {
                let exists = std::path::Path::new(&m.host).exists();
                if !exists {
                    eprintln!(
                        "[nemesis8] skipping mount '{}' — host path not found on this machine",
                        m.host
                    );
                }
                exists
            })
            .map(|m| {
                let mode = m.mode.as_deref().unwrap_or("rw");
                format!("{}:{}:{}", m.host, m.container, mode)
            })
            .collect()
    }

    /// Update the last_session tracking in the TOML file
    pub fn update_last_session(path: &Path, session_id: &str) -> Result<()> {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc = content
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| "parsing TOML for update")?;

        doc["LAST_SESSION"] = toml_edit::value(session_id);

        let table = doc["last_session"]
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .context("last_session must be a table")?;

        table["last_session_id"] = toml_edit::value(session_id);
        table["last_session_updated"] =
            toml_edit::value(chrono::Utc::now().to_rfc3339());
        table["last_session_when"] = toml_edit::value("exit");

        std::fs::write(path, doc.to_string())
            .with_context(|| "writing updated config")?;

        Ok(())
    }
}

/// Find `.nemesis8.toml` files that can shadow sessions besides the active
/// workspace one — the classic leak being a stray config in the home root, which
/// every session launched from a home subdir walks up and inherits. Scans cwd's
/// ancestors + the home root + the downloaded project clone, returns existing
/// files (deduped, excluding `active`). Drives Config → Validate / Reset.
pub fn scan_stray_configs(cwd: &Path, active: Option<&Path>) -> Vec<PathBuf> {
    let active_canon = active.and_then(|p| std::fs::canonicalize(p).ok());
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        candidates.push(d.join(".nemesis8.toml"));
        dir = d.parent();
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".nemesis8.toml"));
        candidates.push(home.join(".nemesis8").join("project").join(".nemesis8.toml"));
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in candidates {
        if !p.is_file() {
            continue;
        }
        let canon = std::fs::canonicalize(&p).ok();
        if canon.is_some() && canon == active_canon {
            continue;
        }
        let key = canon.unwrap_or_else(|| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

/// Read the `mcp_tools` array from a `.nemesis8.toml` without requiring the rest
/// of the config to be valid. Returns an empty vec when the file or key is
/// absent. Used by the control-room tools picker to seed the enabled set for an
/// arbitrary workspace (e.g. the session being resumed), which may not parse
/// into a full `Config`.
pub fn read_mcp_tools(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    doc.get("mcp_tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Write the `mcp_tools` array into a `.nemesis8.toml`, preserving the rest of
/// the file (toml_edit). Creates the file (and parent dirs) with the array when
/// absent, so a fresh workspace can enable tools straight from the picker.
pub fn write_mcp_tools(path: &Path, tools: &[String]) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {} for tool update", path.display()))?;
    let mut arr = toml_edit::Array::new();
    for t in tools {
        arr.push(t.as_str());
    }
    doc["mcp_tools"] = toml_edit::value(arr);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Generate Gemini settings.json content with MCP tool registrations
fn is_mcp_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn url_to_server_name(url: &str) -> String {
    // Use full path so http://x:1/a and http://x:1/b get distinct names.
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .replace('.', "-")
        .replace(':', "-")
        .replace('/', "-")
}

/// Guess the MCP transport from the URL path.
/// Paths ending in `/sse` are SSE; everything else is Streamable HTTP.
fn detect_url_transport(url: &str) -> &'static str {
    let path = url.trim_end_matches('/');
    if path.ends_with("/sse") || path.contains("/sse?") {
        "sse"
    } else {
        "http"
    }
}

/// Env vars forwarded into every spawned MCP server's `env`. Agents (Codex,
/// Gemini, …) sanitize the subprocess environment to ONLY what's declared in the
/// server's config `env`, so a tool like hyperia-mcp otherwise can't see
/// HYPERIA_AGENT_TOKEN/HYPERIA_URL (→ "No identity on this request"). Mirrors the
/// set build_env forwards into the container. We write each var's ACTUAL VALUE
/// (not `${VAR}`): Codex does NOT interpolate `${VAR}` in mcp_servers.env — it
/// passes the literal string, which broke auth. entry.rs runs in-container where
/// these are set, so the value is available; it lands in the per-session config
/// in the HOME volume (container-only, regenerated each launch).
pub const MCP_FORWARD_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "HYPERIA_URL",
    "HYPERIA_AGENT_TOKEN",
    "HYPERIA_PANE",
    "SERPAPI_API_KEY",
    "ELEVENLABS_API_KEY",
    "TRANSCRIPTION_SERVICE_URL",
    "FERRICULA_URL",
];

/// A resolved socket (HTTP/SSE) MCP server ready to emit into an agent config.
struct SocketServer {
    name: String,
    url: String,
    transport: &'static str, // "http" | "sse"
    headers: std::collections::BTreeMap<String, String>,
}

/// A resolved stdio (command) MCP server, e.g. `uvx blender-mcp`.
struct StdioServer {
    name: String,
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
}

/// A `mcp_tools` entry resolved against the registry: either a remote socket
/// server or a stdio subprocess. `None` (from [`resolve_server`]) means it's a
/// `.py` tool / binary, which keep their existing stdio path.
enum ResolvedServer {
    Socket(SocketServer),
    Stdio(StdioServer),
}

/// Authorization + static headers for a socket server, reading the bearer-token
/// VALUE from the (in-container) env. Empty when there's no auth/headers. We
/// emit the literal value (codex doesn't interpolate `${VAR}`, and a literal
/// header works uniformly across codex/gemini/claude); the token must be present
/// in the container env (forwarded by build_env from the registry's token vars).
pub fn socket_headers(spec: &crate::mcp_def::McpServerSpec) -> std::collections::BTreeMap<String, String> {
    let mut h = spec.headers.clone();
    if let Some(env_name) = &spec.bearer_token_env {
        if let Ok(v) = std::env::var(env_name) {
            let v = v.trim();
            if !v.is_empty() {
                h.insert("Authorization".to_string(), format!("Bearer {v}"));
            }
        }
    }
    h
}

/// Classify a `mcp_tools` entry against the registry: a raw `http(s)://` URL
/// (socket, no auth), or a registry NAME — which may be a socket server (with
/// auth/headers) or a stdio command server (`uvx blender-mcp`). Returns None for
/// `.py`/binary tools, which keep their existing stdio path.
fn resolve_server(tool: &str, reg: &crate::mcp_registry::McpRegistry) -> Option<ResolvedServer> {
    if is_mcp_url(tool) {
        return Some(ResolvedServer::Socket(SocketServer {
            name: url_to_server_name(tool),
            url: tool.to_string(),
            transport: detect_url_transport(tool),
            headers: std::collections::BTreeMap::new(),
        }));
    }
    let def = reg.get(tool)?;
    if def.server.is_stdio() {
        Some(ResolvedServer::Stdio(StdioServer {
            name: def.server.name.clone(),
            command: def.server.command.clone().unwrap_or_default(),
            args: def.server.args.clone(),
            env: def.server.env.clone(),
        }))
    } else {
        Some(ResolvedServer::Socket(SocketServer {
            name: def.server.name.clone(),
            url: def.server.url.clone().unwrap_or_default(),
            transport: def.server.resolved_transport(),
            headers: socket_headers(&def.server),
        }))
    }
}

/// JSON-config dialects. Codex uses TOML (separate fn); these two are the
/// JSON-`mcpServers` agents, which disagree on the remote-server shape:
/// Gemini wants `httpUrl` (+ `url` for SSE); Claude wants `type` + `url`.
#[derive(Clone, Copy)]
enum JsonFlavor {
    Gemini,
    Claude,
}

fn generate_json_config(tools: &[String], python_cmd: &str, flavor: JsonFlavor) -> String {
    use serde_json::{json, Map, Value};
    use std::collections::BTreeMap;

    let registry = crate::mcp_registry::McpRegistry::load();
    let mut servers: Map<String, Value> = Map::new();

    for tool in tools {
        match resolve_server(tool, &registry) {
            Some(ResolvedServer::Socket(s)) => {
                let mut entry = Map::new();
                match flavor {
                    JsonFlavor::Gemini => {
                        // httpUrl => StreamableHTTP transport; url => SSE.
                        let key = if s.transport == "sse" { "url" } else { "httpUrl" };
                        entry.insert(key.to_string(), json!(s.url));
                    }
                    JsonFlavor::Claude => {
                        entry.insert("type".to_string(), json!(s.transport));
                        entry.insert("url".to_string(), json!(s.url));
                    }
                }
                if !s.headers.is_empty() {
                    entry.insert("headers".to_string(), json!(s.headers));
                }
                servers.insert(s.name, Value::Object(entry));
                continue;
            }
            Some(ResolvedServer::Stdio(s)) => {
                let mut entry = Map::new();
                entry.insert("command".to_string(), json!(s.command));
                entry.insert("args".to_string(), json!(s.args));
                if !s.env.is_empty() {
                    entry.insert("env".to_string(), json!(s.env));
                }
                servers.insert(s.name, Value::Object(entry));
                continue;
            }
            None => {}
        }

        // stdio python tool
        let name = tool.trim_end_matches(".py").to_string();
        let mut m = BTreeMap::new();
        for k in MCP_FORWARD_ENV {
            if let Ok(v) = std::env::var(k) {
                m.insert((*k).to_string(), v);
            }
        }
        let mut entry = Map::new();
        entry.insert("command".to_string(), json!(python_cmd));
        entry.insert("args".to_string(), json!(["-u", format!("/opt/nemesis8/mcp/{tool}")]));
        if !m.is_empty() {
            entry.insert("env".to_string(), json!(m));
        }
        servers.insert(name, Value::Object(entry));
    }

    // Built-in binary MCP servers (e.g. nuts-files) — registered directly, no python.
    for (name, cmd) in BINARY_MCP_SERVERS {
        let mut entry = Map::new();
        entry.insert("command".to_string(), json!(cmd));
        entry.insert("args".to_string(), json!([] as [&str; 0]));
        servers.insert((*name).to_string(), Value::Object(entry));
    }

    serde_json::to_string_pretty(&json!({ "mcpServers": Value::Object(servers) }))
        .unwrap_or_else(|_| "{}".to_string())
}

pub fn generate_gemini_config(tools: &[String], python_cmd: &str) -> String {
    generate_json_config(tools, python_cmd, JsonFlavor::Gemini)
}

/// JSON-config generator selected by the provider's `mcp_http_style`:
/// `opencode` → OpenCode's distinct `mcp` schema; `claude` → type+url; anything
/// else → gemini's httpUrl. Used by entry.rs, which dispatches by config format
/// and can't otherwise tell the dialects apart.
pub fn generate_json_config_styled(tools: &[String], python_cmd: &str, style: &str) -> String {
    match style {
        "opencode" => generate_opencode_mcp(tools, python_cmd),
        "claude" => generate_json_config(tools, python_cmd, JsonFlavor::Claude),
        _ => generate_json_config(tools, python_cmd, JsonFlavor::Gemini),
    }
}

/// OpenCode's MCP schema differs from `mcpServers`: the top key is `mcp`, and
/// each server is `{type:"local", command:[cmd, …args], environment, enabled}`
/// (stdio: `.py` tools, binary servers, registry stdio servers) or
/// `{type:"remote", url, headers, enabled}` (registry/URL socket servers).
fn generate_opencode_mcp(tools: &[String], python_cmd: &str) -> String {
    use serde_json::{json, Map, Value};
    use std::collections::BTreeMap;

    let registry = crate::mcp_registry::McpRegistry::load();
    let mut mcp: Map<String, Value> = Map::new();

    for tool in tools {
        match resolve_server(tool, &registry) {
            Some(ResolvedServer::Socket(s)) => {
                let mut e = Map::new();
                e.insert("type".to_string(), json!("remote"));
                e.insert("url".to_string(), json!(s.url));
                e.insert("enabled".to_string(), json!(true));
                if !s.headers.is_empty() {
                    e.insert("headers".to_string(), json!(s.headers));
                }
                mcp.insert(s.name, Value::Object(e));
            }
            Some(ResolvedServer::Stdio(s)) => {
                let mut command = vec![s.command];
                command.extend(s.args);
                let mut e = Map::new();
                e.insert("type".to_string(), json!("local"));
                e.insert("command".to_string(), json!(command));
                e.insert("enabled".to_string(), json!(true));
                if !s.env.is_empty() {
                    e.insert("environment".to_string(), json!(s.env));
                }
                mcp.insert(s.name, Value::Object(e));
            }
            None => {
                let name = tool.trim_end_matches(".py").to_string();
                let mut env = BTreeMap::new();
                for k in MCP_FORWARD_ENV {
                    if let Ok(v) = std::env::var(k) {
                        env.insert((*k).to_string(), v);
                    }
                }
                let command = vec![
                    python_cmd.to_string(),
                    "-u".to_string(),
                    format!("/opt/nemesis8/mcp/{tool}"),
                ];
                let mut e = Map::new();
                e.insert("type".to_string(), json!("local"));
                e.insert("command".to_string(), json!(command));
                e.insert("enabled".to_string(), json!(true));
                if !env.is_empty() {
                    e.insert("environment".to_string(), json!(env));
                }
                mcp.insert(name, Value::Object(e));
            }
        }
    }

    // Built-in binary MCP servers (nuts-files, …) as local stdio commands.
    for (name, cmd) in BINARY_MCP_SERVERS {
        let mut e = Map::new();
        e.insert("type".to_string(), json!("local"));
        e.insert("command".to_string(), json!([cmd]));
        e.insert("enabled".to_string(), json!(true));
        mcp.insert((*name).to_string(), Value::Object(e));
    }

    serde_json::to_string_pretty(&json!({ "mcp": Value::Object(mcp) }))
        .unwrap_or_else(|_| "{}".to_string())
}

/// MCP servers shipped as native binaries (not Python tools), installed on
/// PATH in the image. Always registered alongside the discovered .py tools.
/// (name, absolute command path)
const BINARY_MCP_SERVERS: &[(&str, &str)] = &[
    ("nuts-files", "/usr/local/bin/nuts-files"),
    ("shivvr", "/usr/local/bin/shivvr"),
    ("ask", "/usr/local/bin/ask"),
];

/// True if `name` (a server name, i.e. a tool filename with `.py` stripped) is a
/// built-in binary MCP server. Used to stop a stray same-named `.py` (e.g. a
/// leftover `ask.py` in the volume) from shadowing the canonical binary.
pub fn is_binary_server(name: &str) -> bool {
    let stem = name.strip_suffix(".py").unwrap_or(name);
    BINARY_MCP_SERVERS.iter().any(|(n, _)| *n == stem)
}

/// Generate Claude Code config (JSON with mcpServers). Remote servers use
/// `type` + `url` + `headers` (differs from Gemini's `httpUrl`).
pub fn generate_claude_config(tools: &[String], python_cmd: &str) -> String {
    generate_json_config(tools, python_cmd, JsonFlavor::Claude)
}

/// Generate Qwen Code config (Gemini-family fork — same `httpUrl` shape).
pub fn generate_qwen_config(tools: &[String], python_cmd: &str) -> String {
    generate_json_config(tools, python_cmd, JsonFlavor::Gemini)
}

/// Generate Codex config.toml content with MCP tool registrations
pub fn generate_codex_config(tools: &[String], python_cmd: &str) -> String {
    let mut doc = toml_edit::DocumentMut::new();

    let registry = crate::mcp_registry::McpRegistry::load();

    let servers = doc["mcp_servers"]
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .expect("mcp_servers must be a table");

    for tool in tools {
        match resolve_server(tool, &registry) {
            Some(ResolvedServer::Socket(s)) => {
                let mut entry = toml_edit::Table::new();
                entry["type"] = toml_edit::value(s.transport);
                entry["url"] = toml_edit::value(s.url.as_str());
                if !s.headers.is_empty() {
                    let mut h = toml_edit::Table::new();
                    h.set_implicit(false);
                    for (k, v) in &s.headers {
                        h[k] = toml_edit::value(v.as_str());
                    }
                    entry["http_headers"] = toml_edit::Item::Table(h);
                }
                servers[&s.name] = toml_edit::Item::Table(entry);
                continue;
            }
            Some(ResolvedServer::Stdio(s)) => {
                let mut entry = toml_edit::Table::new();
                entry["command"] = toml_edit::value(s.command.as_str());
                let mut args = toml_edit::Array::new();
                for a in &s.args {
                    args.push(a.as_str());
                }
                entry["args"] = toml_edit::value(args);
                if !s.env.is_empty() {
                    let mut e = toml_edit::Table::new();
                    e.set_implicit(false);
                    for (k, v) in &s.env {
                        e[k] = toml_edit::value(v.as_str());
                    }
                    entry["env"] = toml_edit::Item::Table(e);
                }
                servers[&s.name] = toml_edit::Item::Table(entry);
                continue;
            }
            None => {}
        }

        let name = tool.trim_end_matches(".py");
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(python_cmd);

        let mut args = toml_edit::Array::new();
        args.push("-u");
        args.push(format!("/opt/nemesis8/mcp/{tool}"));
        entry["args"] = toml_edit::value(args);

        let mut env_table = toml_edit::Table::new();
        for k in MCP_FORWARD_ENV {
            if let Ok(v) = std::env::var(k) {
                env_table[*k] = toml_edit::value(v);
            }
        }
        if !env_table.is_empty() {
            entry["env"] = toml_edit::Item::Table(env_table);
        }

        servers[name] = toml_edit::Item::Table(entry);
    }

    // Built-in binary MCP servers (e.g. nuts-files) — registered directly, no python.
    for (name, cmd) in BINARY_MCP_SERVERS {
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(*cmd);
        entry["args"] = toml_edit::value(toml_edit::Array::new());
        servers[*name] = toml_edit::Item::Table(entry);
    }

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.workspace_mount_mode, "root");
        assert!(config.mcp_tools.is_empty());
    }

    #[test]
    fn test_is_binary_server_blocks_shadow() {
        // A leftover ask.py / the binary name both resolve to the binary server,
        // so they get filtered (the binary wins). Real .py tools do not.
        assert!(is_binary_server("ask"));
        assert!(is_binary_server("ask.py"));
        assert!(is_binary_server("nuts-files"));
        assert!(is_binary_server("shivvr.py"));
        assert!(!is_binary_server("grub-crawler.py"));
        assert!(!is_binary_server("open-meteo.py"));
    }

    #[test]
    fn test_mcp_tools_round_trip_preserves_other_keys() {
        let dir = std::env::temp_dir().join(format!("n8-tools-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".nemesis8.toml");

        // Seed a config with an unrelated key + an [env] table.
        std::fs::write(
            &path,
            "workspace_mount_mode = \"named\"\nmcp_tools = [\"a.py\"]\n\n[env]\nFOO = \"bar\"\n",
        )
        .unwrap();

        // Absent file → empty; seeded file → its list.
        assert!(read_mcp_tools(&dir.join("nope.toml")).is_empty());
        assert_eq!(read_mcp_tools(&path), vec!["a.py".to_string()]);

        // Rewrite the list; the other key + table must survive.
        write_mcp_tools(&path, &["b.py".to_string(), "https://x/mcp".to_string()]).unwrap();
        let back = read_mcp_tools(&path);
        assert_eq!(back, vec!["b.py".to_string(), "https://x/mcp".to_string()]);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("workspace_mount_mode = \"named\""));
        assert!(raw.contains("FOO = \"bar\""));

        // Writing to a fresh path creates the file with just the array.
        let fresh = dir.join("fresh/.nemesis8.toml");
        write_mcp_tools(&fresh, &["c.py".to_string()]).unwrap();
        assert_eq!(read_mcp_tools(&fresh), vec!["c.py".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_config() {
        let toml_str = r#"
workspace_mount_mode = "named"
mcp_tools = ["agent-chat.py", "gnosis-crawl.py"]

[env]
FOO = "bar"

env_imports = ["MY_KEY"]

[[mounts]]
host = "C:/Users/kord/Code/gnosis/myoo"
container = "/workspace/myoo"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.workspace_mount_mode, "named");
        assert_eq!(config.mcp_tools.len(), 2);
        assert_eq!(config.env.vars.get("FOO").unwrap(), "bar");
        assert_eq!(config.env.env_imports, vec!["MY_KEY"]);
        assert_eq!(config.mounts.len(), 1);
        assert_eq!(config.mounts[0].container, "/workspace/myoo");
    }

    #[test]
    fn test_generate_codex_config() {
        let tools = vec!["agent-chat.py".to_string(), "gnosis-crawl.py".to_string()];
        let output = generate_codex_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(output.contains("[mcp_servers.agent-chat]"));
        assert!(output.contains("[mcp_servers.gnosis-crawl]"));
        assert!(output.contains("/opt/nemesis8/mcp/agent-chat.py"));
    }

    #[test]
    fn test_scaffold_template_is_clean() {
        let t = Config::scaffold_template("myproj");
        assert!(t.contains("nemesis8 config for: myproj"));
        assert!(t.contains("mcp_tools"));
        assert!(t.contains("hyperia-mcp.py"));
        // It must parse, and the retired tools must NOT be in the actual
        // mcp_tools list (the comment may still mention them as "replaced by …").
        let cfg: Config = toml::from_str(&t).expect("scaffold parses");
        assert!(
            !cfg.mcp_tools.iter().any(|x| x.contains("gnosis") || x == "ask.py"),
            "scaffold must not list retired tools: {:?}",
            cfg.mcp_tools
        );
    }

    #[test]
    fn test_scan_stray_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // root/.nemesis8.toml (a stray ancestor) and root/ws/.nemesis8.toml (active)
        std::fs::write(root.join(".nemesis8.toml"), "mcp_tools = []\n").unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let active = ws.join(".nemesis8.toml");
        std::fs::write(&active, "mcp_tools = []\n").unwrap();

        let strays = scan_stray_configs(&ws, Some(&active));
        // The active one is excluded; the ancestor stray is found.
        assert!(strays.iter().any(|p| p.ends_with("ws/../.nemesis8.toml")
            || p == &root.join(".nemesis8.toml")));
        assert!(!strays.iter().any(|p| std::fs::canonicalize(p).ok()
            == std::fs::canonicalize(&active).ok()));
    }

    #[test]
    fn test_workspace_root_base() {
        let mut c = Config::default();
        // unset → defaults to a .../workspaces dir
        let base = c.workspace_root_base().expect("default base");
        assert!(base.ends_with("workspaces"), "default: {}", base.display());
        // "off"/empty → disabled
        c.workspace_root = Some("off".to_string());
        assert!(c.workspace_root_base().is_none());
        c.workspace_root = Some(String::new());
        assert!(c.workspace_root_base().is_none());
        // explicit path → used verbatim
        c.workspace_root = Some("/srv/ws".to_string());
        assert_eq!(c.workspace_root_base(), Some(PathBuf::from("/srv/ws")));
    }

    #[test]
    fn test_raw_url_socket_server_no_auth() {
        // A bare http(s) URL in mcp_tools registers as a remote server with no
        // headers (the simple, no-auth quick-add path).
        let tools = vec!["https://example.test/mcp".to_string()];
        let codex = generate_codex_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(codex.contains("https://example.test/mcp"), "codex: {codex}");
        assert!(codex.contains("type = \"http\""));
        assert!(!codex.contains("http_headers"), "no auth expected: {codex}");

        let gemini = generate_gemini_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(gemini.contains("\"httpUrl\""), "gemini uses httpUrl: {gemini}");

        let claude = generate_claude_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(claude.contains("\"type\""), "claude uses type+url: {claude}");
        assert!(claude.contains("\"url\""));
    }

    #[test]
    fn test_registry_name_resolves_with_auth() {
        // A bare NAME resolves against the MCP registry (embedded hyperia) and,
        // with the bearer-token env present, emits an Authorization header.
        unsafe {
            std::env::set_var("HYPERIA_AGENT_TOKEN", "hyp_test_tok_123");
        }
        let tools = vec!["hyperia".to_string()];

        let codex = generate_codex_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(codex.contains("[mcp_servers.hyperia]"), "codex: {codex}");
        assert!(codex.contains(":9800/mcp"));
        assert!(codex.contains("[mcp_servers.hyperia.http_headers]"), "headers table: {codex}");
        assert!(codex.contains("Bearer hyp_test_tok_123"), "auth value: {codex}");

        let gemini = generate_gemini_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(gemini.contains("\"httpUrl\""));
        assert!(gemini.contains("Bearer hyp_test_tok_123"), "gemini headers: {gemini}");

        let claude = generate_claude_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(claude.contains("\"headers\""));
        assert!(claude.contains("Bearer hyp_test_tok_123"));

        unsafe {
            std::env::remove_var("HYPERIA_AGENT_TOKEN");
        }
    }

    #[test]
    fn test_registry_stdio_server_blender() {
        // A registry NAME pointing at a stdio command server (blender → uvx
        // blender-mcp) emits command+args+env, not a url.
        let tools = vec!["blender".to_string()];

        let codex = generate_codex_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(codex.contains("[mcp_servers.blender]"), "codex: {codex}");
        assert!(codex.contains("command = \"uvx\""));
        assert!(codex.contains("blender-mcp"));
        assert!(codex.contains("BLENDER_HOST"));
        assert!(!codex.contains("url ="), "stdio server must not emit a url: {codex}");

        let gemini = generate_gemini_config(&tools, "/opt/mcp-venv/bin/python3");
        assert!(gemini.contains("\"command\""));
        assert!(gemini.contains("uvx"));
        assert!(gemini.contains("blender-mcp"));
        assert!(gemini.contains("BLENDER_HOST"));
    }

    #[test]
    fn test_opencode_mcp_schema() {
        // OpenCode's distinct schema: top `mcp` key; local = type+command-array,
        // remote = type+url. Covers a .py tool, a registry stdio server (blender),
        // and a registry socket server (hyperia).
        let tools = vec![
            "calculate.py".to_string(),
            "blender".to_string(),
            "hyperia".to_string(),
        ];
        let out = generate_json_config_styled(&tools, "/opt/mcp-venv/bin/python3", "opencode");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let mcp = v.get("mcp").expect("top-level mcp key");

        assert_eq!(mcp["calculate"]["type"], "local");
        assert_eq!(mcp["calculate"]["command"][0], "/opt/mcp-venv/bin/python3");
        assert!(mcp["calculate"]["command"][2].as_str().unwrap().contains("calculate.py"));
        assert_eq!(mcp["calculate"]["enabled"], true);

        assert_eq!(mcp["blender"]["type"], "local");
        assert_eq!(mcp["blender"]["command"][0], "uvx");
        assert_eq!(mcp["blender"]["command"][1], "blender-mcp");
        assert!(mcp["blender"]["environment"]["BLENDER_HOST"].is_string());

        assert_eq!(mcp["hyperia"]["type"], "remote");
        assert!(mcp["hyperia"]["url"].as_str().unwrap().contains("/mcp"));

        assert_eq!(mcp["nuts-files"]["type"], "local");
    }

    #[test]
    fn test_nuts_files_binary_server_registered() {
        // The built-in nuts-files binary server must appear in BOTH config
        // shapes (codex toml + gemini json), with no empty tool list needed.
        let codex = generate_codex_config(&[], "/opt/mcp-venv/bin/python3");
        assert!(codex.contains("[mcp_servers.nuts-files]"), "codex: {codex}");
        assert!(codex.contains("/usr/local/bin/nuts-files"));
        let gemini = generate_gemini_config(&[], "/opt/mcp-venv/bin/python3");
        assert!(gemini.contains("nuts-files"));
        assert!(gemini.contains("/usr/local/bin/nuts-files"));
    }

    #[test]
    fn test_docker_binds() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let host_path = dir.path().to_str().unwrap().to_string();
        let config = Config {
            mounts: vec![
                Mount {
                    host: "C:/nonexistent/path/that/does/not/exist".to_string(),
                    container: "/workspace/foo".to_string(),
                    mode: None,
                },
                Mount {
                    host: host_path.clone(),
                    container: "/container/bar".to_string(),
                    mode: Some("ro".to_string()),
                },
            ],
            ..Config::default()
        };
        let binds = config.docker_binds();
        // Non-existent host path is silently skipped
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0], format!("{}:/container/bar:ro", host_path));
    }

    #[test]
    fn test_container_env_static_vars() {
        let mut vars = HashMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        vars.insert("BAZ".to_string(), "qux".to_string());
        let config = Config {
            env: EnvSection {
                vars,
                env_imports: vec![],
            },
            ..Config::default()
        };
        let env = config.container_env();
        assert!(env.contains(&"FOO=bar".to_string()));
        assert!(env.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn test_last_session_id_from_table() {
        let config = Config {
            last_session: Some(LastSession {
                last_session_id: Some("abc-123".to_string()),
                last_session_file: None,
                last_session_updated: None,
                last_session_when: None,
            }),
            ..Config::default()
        };
        assert_eq!(config.last_session_id(), Some("abc-123"));
    }

    #[test]
    fn test_last_session_id_from_bare() {
        let config = Config {
            last_session_id_bare: Some("bare-456".to_string()),
            ..Config::default()
        };
        assert_eq!(config.last_session_id(), Some("bare-456"));
    }

    #[test]
    fn test_last_session_id_table_takes_precedence() {
        let config = Config {
            last_session: Some(LastSession {
                last_session_id: Some("table-id".to_string()),
                last_session_file: None,
                last_session_updated: None,
                last_session_when: None,
            }),
            last_session_id_bare: Some("bare-id".to_string()),
            ..Config::default()
        };
        assert_eq!(config.last_session_id(), Some("table-id"));
    }

    #[test]
    fn test_empty_toml_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.workspace_mount_mode, "root");
        assert!(config.mcp_tools.is_empty());
        assert!(config.mounts.is_empty());
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".nemesis8.toml");
        std::fs::write(
            &path,
            r#"
workspace_mount_mode = "named"
mcp_tools = ["calculate.py"]
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.workspace_mount_mode, "named");
        assert_eq!(config.mcp_tools, vec!["calculate.py"]);
    }

    #[test]
    fn test_load_or_default_missing_file() {
        let config = Config::load_or_default(Path::new("/nonexistent/.nemesis8.toml"));
        assert_eq!(config.workspace_mount_mode, "root");
    }

    #[test]
    fn test_find_config() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("sub/deep");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(dir.path().join(".nemesis8.toml"), "").unwrap();

        let found = Config::find(&child);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), dir.path().join(".nemesis8.toml"));
    }

    #[test]
    fn test_update_last_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "workspace_mount_mode = \"root\"\n").unwrap();

        Config::update_last_session(&path, "test-session-id").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("test-session-id"));
        assert!(content.contains("[last_session]"));
    }

    #[test]
    fn test_parse_full_real_config() {
        let toml_str = r#"
workspace_mount_mode = "named"
mcp_tools = ["agent-chat.py", "gnosis-crawl.py", "calculate.py"]

[env]
BLENDER_BRIDGE_URL = "http://host.docker.internal:8787"
CODEX_GATEWAY_SESSION_DIRS = "/opt/nemesis8/.codex/sessions"
env_imports = ["SERVICE_ENGINE_URL", "MOLTBOOK_API_KEY"]

[[mounts]]
host = "C:/Users/kord/Code/gnosis/myoo"
container = "/workspace/myoo"

[[mounts]]
host = "C:/Users/kord/Code/gnosis/meditation"
container = "/workspace/meditation"

LAST_SESSION = "019c7d80-f629-7452-b38c-ac4ab228d44d"

[last_session]
last_session_id = "019c7d80-f629-7452-b38c-ac4ab228d44d"
last_session_updated = "2026-02-26T06:39:20Z"
last_session_when = "exit"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcp_tools.len(), 3);
        assert_eq!(
            config.env.vars.get("BLENDER_BRIDGE_URL").unwrap(),
            "http://host.docker.internal:8787"
        );
        assert_eq!(config.env.env_imports.len(), 2);
        assert_eq!(config.mounts.len(), 2);
        assert_eq!(
            config.last_session_id(),
            Some("019c7d80-f629-7452-b38c-ac4ab228d44d")
        );
    }

    #[test]
    fn test_generate_codex_config_empty_tools() {
        let output = generate_codex_config(&[], "/opt/mcp-venv/bin/python3");
        assert!(output.contains("mcp_servers"));
        // Should still be valid TOML, just with no sub-entries
    }
}
