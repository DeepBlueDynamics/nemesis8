use serde::{Deserialize, Serialize};

/// Declarative template for an **app** — a foreground, non-AI tool nemesis8 runs
/// in a container TTY the user watches (e.g. `glint`, a terminal dashboard).
/// Loaded from `apps/*.toml`, mirroring `service_def::ServiceDef` /
/// `provider_def::ProviderDef`.
///
/// Apps are the non-AI sibling of providers: providers are AI agents (foreground
/// TTY), services are background containers (no TTY), apps are foreground TTY
/// tools. The launch half (mounts → `run_it`) lives in `main.rs`; this is purely
/// *what* to run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppDef {
    pub app: AppSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppSpec {
    /// App id (lowercase) — how it's addressed in the picker / registry.
    pub name: String,
    /// Glyph shown next to the app in the New-session modal's App list.
    #[serde(default)]
    pub emoji: Option<String>,
    /// In-container binary to exec (must be installed in the image).
    pub binary: String,
    /// Extra args appended after the binary.
    #[serde(default)]
    pub args: Vec<String>,
    /// One-line description shown in the picker.
    #[serde(default)]
    pub description: Option<String>,
    /// Host→container config-dir mounts (e.g. `~/.config/glint`). Created on the
    /// host if missing so the app's config/credentials persist on the host.
    #[serde(default)]
    pub config_mounts: Vec<AppMount>,
    /// Host env var names to forward into the container (API keys, etc.).
    #[serde(default)]
    pub env_imports: Vec<String>,
}

/// A host→container bind for an app's config/credentials.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppMount {
    /// Host path; a leading `~` is expanded to the host home.
    pub host: String,
    /// Container path. Relative paths are resolved under the container HOME.
    pub container: String,
    /// Bind mode (`rw` / `ro`). Defaults to `rw`.
    #[serde(default)]
    pub mode: Option<String>,
}

impl AppSpec {
    /// Validate the template: name + binary must be non-empty.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("app template has an empty `name`".to_string());
        }
        if self.binary.trim().is_empty() {
            return Err(format!("app '{}' has an empty `binary`", self.name));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn apps_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("apps")
    }

    #[test]
    fn test_parse_glint_app() {
        let path = apps_dir().join("glint.toml");
        let toml_str = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
        let def: AppDef = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("failed to parse glint.toml: {e}"));
        assert_eq!(def.app.name, "glint");
        assert_eq!(def.app.binary, "glint");
        assert!(!def.app.config_mounts.is_empty());
        def.app.validate().expect("glint template must validate");
    }

    #[test]
    fn test_all_apps_parse_and_validate() {
        let dir = apps_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("could not read apps dir: {e}"))
            .flatten()
            .filter(|e| e.path().extension().map_or(false, |x| x == "toml"))
            .collect();
        assert!(!entries.is_empty(), "no app TOMLs found");
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy();
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
            let def: AppDef = toml::from_str(&content)
                .unwrap_or_else(|e| panic!("failed to parse {name}.toml: {e}"));
            assert_eq!(def.app.name, name.as_ref());
            def.app
                .validate()
                .unwrap_or_else(|e| panic!("{name}.toml invalid: {e}"));
        }
    }
}
