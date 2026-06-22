use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Complete definition of an AI CLI provider, loaded from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderDef {
    pub provider: ProviderSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderSpec {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Glyph shown next to the provider in the entry banner / UI lists.
    #[serde(default)]
    pub emoji: Option<String>,
    pub binary: String,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub install_package: Option<String>,
    /// Flag that tells the CLI which directory is the workspace, passed as
    /// `<flag> <workspace_root>` at launch. Needed for agents that otherwise
    /// sandbox file writes to their own session dir instead of the mounted
    /// project (e.g. antigravity's `--add-dir`). Omit when the CLI already uses
    /// its cwd as the workspace.
    #[serde(default)]
    pub workspace_flag: Option<String>,

    /// One-line hint shown under the model picker in the new-session modal —
    /// data-driven UI text fed from the provider's config. e.g. opencode's
    /// "listed models are local Ollama; pick any other inside opencode". Omit
    /// for no hint.
    #[serde(default)]
    pub picker_hint: Option<String>,

    pub config_dir: ConfigDirSpec,
    #[serde(default)]
    pub prompt: PromptSpec,
    #[serde(default)]
    pub system_prompt: SystemPromptSpec,
    #[serde(default)]
    pub danger: DangerSpec,
    #[serde(default)]
    pub model: ModelSpec,
    #[serde(default)]
    pub api_keys: ApiKeySpec,
    #[serde(default)]
    pub validation: ValidationSpec,
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    #[serde(default)]
    pub hooks: HooksSpec,
    #[serde(default)]
    pub login: LoginSpec,
    /// Settings merged into the provider's generated config EVERY session (not
    /// just danger mode). Used to pin provider-level defaults — e.g. pi's
    /// `defaultProvider` so a multi-backend agent doesn't fall back to a
    /// backend whose API is unavailable (pi was defaulting to Anthropic). JSON
    /// object, shallow-merged over the generated config.
    #[serde(default)]
    pub config_defaults: Option<serde_json::Value>,
    /// Generic local-daemon model enumeration into the agent's config (see
    /// LocalModelsSpec). Pairs with model.local_daemon_env (the daemon URL).
    #[serde(default)]
    pub local_models: Option<LocalModelsSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigDirSpec {
    pub path: String,
    pub format: String,
    pub filename: String,
    #[serde(default = "default_mcp_key")]
    pub mcp_key: String,
    /// Remote (socket) MCP server shape for JSON-config agents, since they
    /// disagree: `gemini` (default) emits `httpUrl`+`headers` (gemini-cli,
    /// qwen, antigravity); `claude` emits `type`+`url`+`headers` (claude-code).
    /// Ignored for the TOML path (codex), which always uses `type`+`url`+`http_headers`.
    #[serde(default = "default_mcp_http_style")]
    pub mcp_http_style: String,
    /// Merge the MCP servers table into an existing config file instead of
    /// overwriting it. Needed when the CLI keeps its OWN state in the same file
    /// (grok: [cli]/[marketplace]). Default false — codex regenerates a
    /// clean config each session, which keeps them immune to the CLI persisting
    /// a value its next version can't parse (the codex `model_availability_nux`
    /// schema-drift breakage). Only opt in when the file is co-owned.
    #[serde(default)]
    pub merge: bool,
    /// The agent's MCP client can't handle HTTP/socket servers (no `httpUrl`
    /// support). When true, native HTTP registry servers (e.g. the `hyperia`
    /// registry server) and raw URL servers are dropped from this agent's
    /// generated config — they'd emit as `httpUrl` and the agent rejects them
    /// ("no connector can handle spec"). Stdio servers (incl. the hyperia-mcp
    /// shim) and `.py`/binary tools are kept. Set for antigravity, whose Gemini-
    /// inherited config schema includes `httpUrl` but whose connector doesn't
    /// implement it. Flip back to false if the agent gains HTTP support.
    #[serde(default)]
    pub http_mcp_unsupported: bool,
    /// Per-server schema-cache subdir under this config dir (e.g. antigravity's
    /// `<path>/mcp/<server>/`). When a tool is deleted, n8 purges
    /// `<path>/<cache_subdir>/<server>` so stale cached tool-schemas don't
    /// linger as ghosts. Empty (default) = the provider has no such cache.
    #[serde(default)]
    pub cache_subdir: String,
    /// Config locations this provider WROTE in a past version but no longer
    /// uses, relative to the container HOME. config-gen deletes them each
    /// session so a path migration doesn't strand an orphan the agent still
    /// reads/merges (e.g. antigravity's old `.gemini/config/mcp_config.json`
    /// after its config moved to `.gemini/antigravity-cli/`). Covers the common
    /// "provider moved its files" case declaratively — no per-provider Rust.
    #[serde(default)]
    pub legacy_paths: Vec<String>,
    /// Additional locations (relative to the container HOME) the SAME generated
    /// config is mirrored to after it's written. Unlike `legacy_paths` (deleted),
    /// these are kept in sync with the live config. Needed for antigravity, which
    /// still MERGES `~/.gemini/config/mcp_config.json` (the gemini-global path) in
    /// addition to its own `.gemini/antigravity-cli/` dir — and on Windows its
    /// execution sandbox reads ONLY that mirrored copy, so deleting it leaves tools
    /// registered in the UI but invisible to the sandbox. Mirroring the CURRENT
    /// config (rather than sweeping) keeps the sandbox in sync and can't strand
    /// stale servers (the write truncates). Empty (default) = no mirroring.
    #[serde(default)]
    pub mirror_paths: Vec<String>,
    /// File (in the config dir) where this agent reads a per-tool MCP permission
    /// **allowlist** at startup. When set, config-gen pre-fills it with one entry
    /// per MCP server so tools are callable from the first token. Needed for agy,
    /// which loads `permissions.allow` once at launch — without pre-population,
    /// connected MCP tools error "not enabled for server" and mid-session grants
    /// don't reload. Empty (default) = the agent doesn't gate MCP this way.
    #[serde(default)]
    pub mcp_allowlist_file: String,
    /// JSON pointer to the allowlist array inside `mcp_allowlist_file`.
    #[serde(default = "default_mcp_allowlist_pointer")]
    pub mcp_allowlist_pointer: String,
    /// Template for each allowlist entry; `{server}` is replaced with the MCP
    /// server name (e.g. agy's `mcp({server}/*)` = allow all tools on a server).
    #[serde(default = "default_mcp_allowlist_entry")]
    pub mcp_allowlist_entry: String,
}

fn default_mcp_allowlist_pointer() -> String {
    "/permissions/allow".to_string()
}

fn default_mcp_allowlist_entry() -> String {
    "mcp({server}/*)".to_string()
}

fn default_mcp_key() -> String {
    "mcpServers".to_string()
}

fn default_mcp_http_style() -> String {
    "gemini".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PromptSpec {
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default)]
    pub exec_subcommand: Option<String>,
    #[serde(default)]
    pub exec_prompt_flag: Option<String>,
    #[serde(default)]
    pub interactive_subcommand: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPromptSpec {
    #[serde(default)]
    pub env_var: Option<String>,
    #[serde(default = "default_source_file")]
    pub source_file: String,
    #[serde(default)]
    pub write_to_file: Option<String>,
    /// The agent-specific identity line, prepended to the embedded agent-agnostic
    /// BASE prompt at injection (e.g. "You are Codex, OpenAI's coding agent…").
    /// Lives here so the base stays shared and the per-agent bit is data-driven —
    /// no "You are Codex" baked into a prompt every agent receives.
    #[serde(default)]
    pub persona: Option<String>,
}

impl Default for SystemPromptSpec {
    fn default() -> Self {
        Self {
            env_var: None,
            source_file: default_source_file(),
            write_to_file: None,
            persona: None,
        }
    }
}

fn default_source_file() -> String {
    "PROMPT.md".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DangerSpec {
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default)]
    pub env_vars: Vec<String>,
    #[serde(default)]
    pub config_merge: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelSpec {
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default = "default_model_env")]
    pub env_source: String,
    /// Default model id for this provider — used by entry.rs as the fallback
    /// when no model is picked (env_source unset). Must NOT be duplicated into
    /// env_overrides, which would clobber an explicit pick (issue #65).
    #[serde(default)]
    pub default: Option<String>,
    /// Env var holding a local model daemon's base URL (e.g. OLLAMA_HOST for
    /// Ollama). When set, n8 lists the daemon's downloaded models in the
    /// new-session modal — local models first — and entry.rs enumerates them
    /// into the provider's generated config (see local_daemon_models_key).
    /// General mechanism: any provider that fronts a local OpenAI-compatible
    /// daemon (e.g. opencode → ollama) can opt in via data, no code changes.
    #[serde(default)]
    pub local_daemon_env: Option<String>,
    /// Fallback base URL when local_daemon_env is unset (e.g. http://localhost:11434).
    #[serde(default)]
    pub local_daemon_default_url: Option<String>,
    /// Prefix applied to local model ids in the modal dropdown / the value
    /// passed as --model (e.g. "ollama/" so opencode gets `ollama/<model>`).
    #[serde(default)]
    pub local_daemon_model_prefix: Option<String>,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            flag: None,
            env_source: default_model_env(),
            default: None,
            local_daemon_env: None,
            local_daemon_default_url: None,
            local_daemon_model_prefix: None,
        }
    }
}

fn default_model_env() -> String {
    "CODEX_DEFAULT_MODEL".to_string()
}

/// How entry.rs writes the local daemon's models into the in-container agent's
/// config. Fully data-driven: the file, JSON path, list shape, and the static
/// provider wrapper all come from the provider TOML — entry.rs names no
/// provider. opencode (map shape, main config file) and pi (array shape, a
/// separate models.json, compat flags) differ ONLY in these fields.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LocalModelsSpec {
    /// File (relative to config_dir.path) to write into. Empty = the provider's
    /// main config file (config_dir.filename).
    #[serde(default)]
    pub file: Option<String>,
    /// Dotted JSON path within `file` where the enumerated model list is placed
    /// (e.g. "provider.ollama.models" for opencode, "ollama.models" for pi).
    pub models_key: String,
    /// List shape: "map" → `{<id>: {name, tools:true}}` (opencode) or "array" →
    /// `[{id}]` (pi). Defaults to "map".
    #[serde(default = "default_models_shape")]
    pub shape: String,
    /// Static JSON merged into `file` before the models are injected — e.g. pi's
    /// ollama provider block (baseUrl/api/apiKey/compat). Omit when the wrapper
    /// is already supplied via config_defaults (opencode's main-file case).
    #[serde(default)]
    pub wrapper: Option<serde_json::Value>,
}

fn default_models_shape() -> String {
    "map".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ApiKeySpec {
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub chain: Vec<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub write_to_config: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ValidationSpec {
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub danger_flags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksSpec {
    #[serde(default)]
    pub requires_git_init: bool,
    #[serde(default)]
    pub supports_sessions: bool,
    /// How to pass a session ID to the provider binary.
    /// "--resume" → `<bin> --resume <id>` (flag)
    /// None → `<bin> resume <id>` (subcommand, Codex style)
    #[serde(default)]
    pub resume_flag: Option<String>,
    /// Session storage dirs relative to the data home (~/.nemesis8/home).
    /// Supports a single `*` wildcard for one path component.
    /// e.g. [".codex/sessions"] or [".gemini/tmp/*/chats"]
    #[serde(default)]
    pub session_dirs: Vec<String>,
    #[serde(default)]
    pub auth_files_sync: Vec<String>,
    #[serde(default)]
    pub extra_config_files: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LoginSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub env_vars: Vec<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    /// Host-side auth check run before interactive sessions: if `env_fallback`
    /// isn't set and `file` (relative to the host home dir) is missing, bail
    /// with `hint` instead of failing inside the container.
    #[serde(default)]
    pub preflight: Option<PreflightSpec>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PreflightSpec {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub env_fallback: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn providers_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("providers")
    }

    fn load_provider(name: &str) -> ProviderDef {
        let path = providers_dir().join(format!("{name}.toml"));
        let toml_str = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
        toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("failed to parse {name}.toml: {e}"))
    }

    #[test]
    fn test_parse_codex_provider() {
        let def = load_provider("codex");
        assert_eq!(def.provider.name, "codex");
        assert_eq!(def.provider.binary, "codex");
        assert_eq!(def.provider.config_dir.format, "toml");
        assert_eq!(def.provider.config_dir.mcp_key, "mcp_servers");
        assert!(def.provider.hooks.requires_git_init);
        assert!(def.provider.hooks.supports_sessions);
    }

    #[test]
    fn test_parse_claude_provider() {
        let def = load_provider("claude");
        assert_eq!(def.provider.name, "claude");
        assert_eq!(def.provider.danger.flag.as_deref(), Some("--permission-mode bypassPermissions"));
        assert_eq!(def.provider.prompt.flag.as_deref(), Some("-p"));
    }

    #[test]
    fn test_all_providers_parse() {
        let dir = providers_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("could not read providers dir: {e}"))
            .flatten()
            .filter(|e| e.path().extension().map_or(false, |x| x == "toml"))
            .collect();
        assert!(!entries.is_empty(), "no provider TOMLs found");
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy();
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
            let def: ProviderDef = toml::from_str(&content)
                .unwrap_or_else(|e| panic!("failed to parse {name}.toml: {e}"));
            assert_eq!(def.provider.name, name.as_ref());
        }
    }

    #[test]
    fn test_parse_pi_provider() {
        let def = load_provider("pi");
        assert_eq!(def.provider.name, "pi");
        assert_eq!(def.provider.danger.flag.as_deref(), Some("--approve"));
        assert_eq!(def.provider.config_dir.mcp_key, "");
        assert_eq!(def.provider.config_dir.format, "json");
        assert!(def.provider.danger.config_merge.is_some());
    }
}

