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
    pub binary: String,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub install_package: Option<String>,

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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigDirSpec {
    pub path: String,
    pub format: String,
    pub filename: String,
    #[serde(default = "default_mcp_key")]
    pub mcp_key: String,
}

fn default_mcp_key() -> String {
    "mcpServers".to_string()
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
}

impl Default for SystemPromptSpec {
    fn default() -> Self {
        Self {
            env_var: None,
            source_file: default_source_file(),
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelSpec {
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default = "default_model_env")]
    pub env_source: String,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            flag: None,
            env_source: default_model_env(),
        }
    }
}

fn default_model_env() -> String {
    "CODEX_DEFAULT_MODEL".to_string()
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
    fn test_parse_gemini_provider() {
        let def = load_provider("gemini");
        assert_eq!(def.provider.name, "gemini");
        assert_eq!(def.provider.danger.flag.as_deref(), Some("-y"));
        assert_eq!(def.provider.env_overrides.get("HOME").map(|s| s.as_str()), Some("/opt/codex-home"));
        assert!(!def.provider.hooks.auth_files_sync.is_empty());
        assert!(!def.provider.hooks.extra_config_files.is_empty());
    }

    #[test]
    fn test_parse_claude_provider() {
        let def = load_provider("claude");
        assert_eq!(def.provider.name, "claude");
        assert_eq!(def.provider.danger.flag.as_deref(), Some("--permission-mode bypassPermissions"));
        assert_eq!(def.provider.prompt.flag.as_deref(), Some("-p"));
    }

    #[test]
    fn test_parse_openclaw_provider() {
        let def = load_provider("openclaw");
        assert_eq!(def.provider.name, "openclaw");
        assert_eq!(def.provider.prompt.interactive_subcommand.as_deref(), Some("tui"));
        assert_eq!(def.provider.prompt.exec_subcommand.as_deref(), Some("agent"));
        assert_eq!(def.provider.prompt.exec_prompt_flag.as_deref(), Some("--message"));
        assert!(def.provider.danger.flag.is_none());
    }

    #[test]
    fn test_parse_ollama_provider() {
        let def = load_provider("ollama");
        assert_eq!(def.provider.name, "ollama");
        assert_eq!(def.provider.binary, "codex");
        assert_eq!(def.provider.env_overrides.get("OPENAI_BASE_URL").map(|s| s.as_str()),
            Some("http://host.docker.internal:11434/v1"));
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
}
