use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level pokeball spec, loaded from pokeball.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PokeballSpec {
    pub api_version: String,
    pub metadata: Metadata,
    pub source: Source,
    pub runtime: Runtime,
    pub build: BuildSpec,
    pub provider: Provider,
    pub tools: ToolsSpec,
    pub security: Security,
    pub resources: Resources,
    #[serde(default, skip_serializing_if = "ProjectMeta::is_empty")]
    pub meta: ProjectMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

// ── Source: supports both local paths and git repos ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Git(GitSourceWrapper),
    Local(LocalSourceWrapper),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSourceWrapper {
    pub git: GitSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSource {
    pub url: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSourceWrapper {
    pub local: LocalSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSource {
    pub path: String,
}

impl Source {
    /// Get the local filesystem path regardless of source kind
    pub fn local_path(&self) -> &str {
        match self {
            Source::Git(g) => &g.git.path,
            Source::Local(l) => &l.local.path,
        }
    }

    pub fn new_local(path: impl Into<String>) -> Self {
        Source::Local(LocalSourceWrapper {
            local: LocalSource { path: path.into() },
        })
    }

    pub fn new_git(url: impl Into<String>, path: impl Into<String>) -> Self {
        Source::Git(GitSourceWrapper {
            git: GitSource {
                url: url.into(),
                path: path.into(),
                branch: None,
            },
        })
    }
}

// ── ProjectMeta: enhanced detection results ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectMeta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<ComposeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monorepo: Option<MonorepoInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dockerfiles: Vec<String>,
}

impl ProjectMeta {
    pub fn is_empty(&self) -> bool {
        self.env_vars.is_empty()
            && self.compose.is_none()
            && self.monorepo.is_none()
            && self.dockerfiles.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeInfo {
    pub file: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ComposeService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeService {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonorepoInfo {
    pub tool: String, // "npm-workspaces", "pnpm-workspaces", "yarn-workspaces"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
}

// ── Existing types (unchanged) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Runtime {
    pub base_image: String,
    pub language: String,
    #[serde(default)]
    pub package_manager: Option<String>,
    #[serde(default)]
    pub node_version: Option<String>,
    #[serde(default)]
    pub existing_dockerfile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSpec {
    #[serde(default)]
    pub install_cmd: Option<String>,
    #[serde(default)]
    pub build_cmd: Option<String>,
    #[serde(default)]
    pub system_packages: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
}

fn default_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsSpec {
    pub allow: Vec<ToolEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Security {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
    pub process: ProcessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    pub policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPolicy {
    pub read_only_root: bool,
    pub work_mount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessPolicy {
    pub user: String,
    pub no_new_privileges: bool,
    pub drop_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    pub memory_mb: u64,
    #[serde(default = "default_pids_limit")]
    pub pids_limit: i64,
    #[serde(default = "default_timeout_minutes")]
    pub timeout_minutes: u64,
}

fn default_pids_limit() -> i64 {
    256
}

fn default_timeout_minutes() -> u64 {
    60
}

impl PokeballSpec {
    /// Load a spec from a YAML file
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let spec: Self = serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(spec)
    }

    /// Save the spec to a YAML file
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(path, yaml)?;
        Ok(())
    }

    /// Serialize to YAML string
    pub fn to_yaml(&self) -> anyhow::Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Image tag for this pokeball
    pub fn image_tag(&self) -> String {
        format!("pokeball-{}:latest", self.metadata.name)
    }

    /// Create a default spec with the given name and project path
    pub fn default_for(name: &str, project_path: &str) -> Self {
        Self {
            api_version: "pokeball/v1".to_string(),
            metadata: Metadata {
                name: name.to_string(),
                description: String::new(),
            },
            source: Source::new_local(project_path),
            runtime: Runtime {
                base_image: "debian:bookworm-slim".to_string(),
                language: "unknown".to_string(),
                package_manager: None,
                node_version: None,
                existing_dockerfile: None,
            },
            build: BuildSpec {
                install_cmd: None,
                build_cmd: None,
                system_packages: vec!["git".to_string(), "curl".to_string(), "tini".to_string()],
                exclude: vec![
                    ".git".to_string(),
                    "*.log".to_string(),
                    ".env*".to_string(),
                ],
            },
            provider: Provider {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
            },
            tools: ToolsSpec {
                allow: vec![
                    ToolEntry { name: "bash".to_string() },
                    ToolEntry { name: "file_read".to_string() },
                    ToolEntry { name: "file_write".to_string() },
                    ToolEntry { name: "grep".to_string() },
                    ToolEntry { name: "glob".to_string() },
                ],
            },
            security: Security {
                network: NetworkPolicy {
                    policy: "deny".to_string(),
                },
                filesystem: FilesystemPolicy {
                    read_only_root: true,
                    work_mount: "rw".to_string(),
                },
                process: ProcessPolicy {
                    user: "pokeball".to_string(),
                    no_new_privileges: true,
                    drop_capabilities: vec!["ALL".to_string()],
                },
            },
            resources: Resources {
                memory_mb: 4096,
                pids_limit: 256,
                timeout_minutes: 60,
            },
            meta: ProjectMeta::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_spec_roundtrip() {
        let spec = PokeballSpec::default_for("test-project", "/tmp/test");
        let yaml = spec.to_yaml().unwrap();
        let parsed: PokeballSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.metadata.name, "test-project");
        assert_eq!(parsed.api_version, "pokeball/v1");
        assert_eq!(parsed.runtime.language, "unknown");
    }

    #[test]
    fn test_image_tag() {
        let spec = PokeballSpec::default_for("openclaw", "/some/path");
        assert_eq!(spec.image_tag(), "pokeball-openclaw:latest");
    }

    #[test]
    fn test_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pokeball.yaml");

        let spec = PokeballSpec::default_for("roundtrip", "/test/path");
        spec.save(&path).unwrap();

        let loaded = PokeballSpec::load(&path).unwrap();
        assert_eq!(loaded.metadata.name, "roundtrip");
        assert_eq!(loaded.source.local_path(), "/test/path");
        assert_eq!(loaded.resources.memory_mb, 4096);
    }

    #[test]
    fn test_source_local_path() {
        let local = Source::new_local("/my/project");
        assert_eq!(local.local_path(), "/my/project");

        let git = Source::new_git("https://github.com/user/repo", "/clone/dir");
        assert_eq!(git.local_path(), "/clone/dir");
    }

    #[test]
    fn test_git_source_roundtrip() {
        let mut spec = PokeballSpec::default_for("git-test", "/tmp/clone");
        spec.source = Source::new_git("https://github.com/user/repo", "/tmp/clone");
        let yaml = spec.to_yaml().unwrap();
        let parsed: PokeballSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.source.local_path(), "/tmp/clone");
        match &parsed.source {
            Source::Git(g) => {
                assert_eq!(g.git.url, "https://github.com/user/repo");
            }
            _ => panic!("expected Git source"),
        }
    }

    #[test]
    fn test_project_meta_empty_skipped() {
        let spec = PokeballSpec::default_for("test", "/tmp/test");
        let yaml = spec.to_yaml().unwrap();
        // Empty meta should not appear in YAML
        assert!(!yaml.contains("meta:"));
    }

    #[test]
    fn test_project_meta_serialized() {
        let mut spec = PokeballSpec::default_for("test", "/tmp/test");
        spec.meta.dockerfiles = vec!["Dockerfile".to_string()];
        let yaml = spec.to_yaml().unwrap();
        assert!(yaml.contains("meta:"));
        assert!(yaml.contains("Dockerfile"));
    }
}
