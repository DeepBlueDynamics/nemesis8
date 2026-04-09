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
        // Resolve built-in aliases so they round-trip cleanly
        let resolved = match name.as_str() {
            "openai"    => "codex",
            "google"    => "gemini",
            "anthropic" => "claude",
            "claw"      => "openclaw",
            "qwen-code" => "qwen",
            "local"     => "ala",
            other       => other,
        };
        Ok(Provider(resolved.to_string()))
    }
}

/// Top-level config from .nemesis8.toml
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    /// AI CLI provider: "codex" or "gemini"
    #[serde(default)]
    pub provider: Provider,

    /// Workspace mount mode: "root" or "named"
    #[serde(default = "default_mount_mode")]
    pub workspace_mount_mode: String,

    /// Active MCP tool filenames
    #[serde(default)]
    pub mcp_tools: Vec<String>,

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

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            workspace_mount_mode: "root".to_string(),
            mcp_tools: Vec::new(),
            codex_cli_version: None,
            setup_commands: Vec::new(),
            env: EnvSection::default(),
            mounts: Vec::new(),
            last_session: None,
            last_session_id_bare: None,
            remote: None,
            remote_token: None,
            integrations: Integrations::default(),
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

    /// Build Docker bind mounts from the mounts config
    pub fn docker_binds(&self) -> Vec<String> {
        self.mounts
            .iter()
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

/// Generate Gemini settings.json content with MCP tool registrations
pub fn generate_gemini_config(tools: &[String], python_cmd: &str) -> String {
    use std::collections::BTreeMap;

    #[derive(serde::Serialize)]
    struct McpEntry {
        command: String,
        args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    }

    #[derive(serde::Serialize)]
    struct GeminiSettings {
        #[serde(rename = "mcpServers")]
        mcp_servers: BTreeMap<String, McpEntry>,
    }

    let mut mcp_servers = BTreeMap::new();
    for tool in tools {
        let name = tool.trim_end_matches(".py").to_string();
        let mut entry = McpEntry {
            command: python_cmd.to_string(),
            args: vec!["-u".to_string(), format!("/opt/codex-home/mcp/{tool}")],
            env: None,
        };

        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            let mut env = BTreeMap::new();
            env.insert(
                "ANTHROPIC_API_KEY".to_string(),
                "${ANTHROPIC_API_KEY}".to_string(),
            );
            entry.env = Some(env);
        }

        mcp_servers.insert(name, entry);
    }

    let settings = GeminiSettings { mcp_servers };
    serde_json::to_string_pretty(&settings).unwrap_or_else(|_| "{}".to_string())
}

/// Generate Claude Code config (JSON with mcpServers)
pub fn generate_claude_config(tools: &[String], python_cmd: &str) -> String {
    generate_gemini_config(tools, python_cmd)
}

/// Generate OpenClaw config (JSON with mcpServers, same shape as Gemini)
pub fn generate_openclaw_config(tools: &[String], python_cmd: &str) -> String {
    // OpenClaw uses the same JSON mcpServers format
    generate_gemini_config(tools, python_cmd)
}

/// Generate Qwen Code config (JSON with mcpServers, same shape as Gemini/Claude)
pub fn generate_qwen_config(tools: &[String], python_cmd: &str) -> String {
    generate_gemini_config(tools, python_cmd)
}

/// Generate Codex config.toml content with MCP tool registrations
pub fn generate_codex_config(tools: &[String], python_cmd: &str) -> String {
    let mut doc = toml_edit::DocumentMut::new();

    let servers = doc["mcp_servers"]
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .expect("mcp_servers must be a table");

    for tool in tools {
        let name = tool.trim_end_matches(".py");
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(python_cmd);

        let mut args = toml_edit::Array::new();
        args.push("-u");
        args.push(format!("/opt/codex-home/mcp/{tool}"));
        entry["args"] = toml_edit::value(args);

        // Pass through ANTHROPIC_API_KEY if available
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            let mut env_table = toml_edit::Table::new();
            env_table["ANTHROPIC_API_KEY"] =
                toml_edit::value("${ANTHROPIC_API_KEY}");
            entry["env"] = toml_edit::Item::Table(env_table);
        }

        servers[name] = toml_edit::Item::Table(entry);
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
        assert!(output.contains("/opt/codex-home/mcp/agent-chat.py"));
    }

    #[test]
    fn test_docker_binds() {
        let config = Config {
            mounts: vec![
                Mount {
                    host: "C:/foo".to_string(),
                    container: "/workspace/foo".to_string(),
                    mode: None,
                },
                Mount {
                    host: "/host/bar".to_string(),
                    container: "/container/bar".to_string(),
                    mode: Some("ro".to_string()),
                },
            ],
            ..Config::default()
        };
        let binds = config.docker_binds();
        assert_eq!(binds[0], "C:/foo:/workspace/foo:rw");
        assert_eq!(binds[1], "/host/bar:/container/bar:ro");
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
CODEX_GATEWAY_SESSION_DIRS = "/opt/codex-home/.codex/sessions"
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
