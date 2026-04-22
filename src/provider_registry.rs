use crate::provider_def::ProviderDef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Directory inside the Docker image where provider TOMLs are stored.
const BUILTIN_PROVIDERS_DIR: &str = "/opt/defaults/providers";

/// Registry of all known providers (builtins + user-defined).
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderDef>,
    aliases: HashMap<String, String>,
}

impl ProviderRegistry {
    /// Build the registry from the providers directory, then overlay user-defined ones.
    pub fn load() -> Self {
        let mut reg = Self {
            providers: HashMap::new(),
            aliases: HashMap::new(),
        };

        // Load builtins from /opt/defaults/providers/ (set at runtime; inside Docker image)
        let builtin_dir = builtin_providers_dir();
        if builtin_dir.is_dir() {
            reg.load_providers_from_dir(&builtin_dir);
        }

        // Load user-defined providers from ~/.nemesis8/providers/*.toml (override builtins)
        reg.load_user_providers();

        reg
    }

    fn load_providers_from_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[nemesis8] warning: could not read providers dir {}: {e}", dir.display());
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(true, |e| e != "toml") {
                continue;
            }
            self.load_provider_file(&path);
        }
    }

    fn load_provider_file(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<ProviderDef>(&content) {
                Ok(def) => {
                    let name = def.provider.name.to_lowercase();
                    for alias in &def.provider.aliases {
                        self.aliases.insert(alias.to_lowercase(), name.clone());
                    }
                    self.providers.insert(name, def);
                }
                Err(e) => {
                    eprintln!(
                        "[nemesis8] warning: failed to parse {}: {e}",
                        path.display()
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "[nemesis8] warning: could not read {}: {e}",
                    path.display()
                );
            }
        }
    }

    fn load_user_providers(&mut self) {
        let user_dir = dirs::home_dir()
            .map(|h| h.join(".nemesis8").join("providers"))
            .unwrap_or_default();

        if !user_dir.is_dir() {
            return;
        }

        let entries = match std::fs::read_dir(&user_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(true, |e| e != "toml") {
                continue;
            }
            self.load_provider_file(&path);
        }
    }

    /// Look up a provider by name or alias.
    pub fn get(&self, name: &str) -> Option<&ProviderDef> {
        let key = name.to_lowercase();
        self.providers
            .get(&key)
            .or_else(|| self.aliases.get(&key).and_then(|n| self.providers.get(n)))
    }

    /// Resolve a provider name, returning a helpful error if not found.
    pub fn resolve(&self, name: &str) -> Result<&ProviderDef, String> {
        self.get(name).ok_or_else(|| {
            let mut available: Vec<&str> = self.providers.keys().map(|s| s.as_str()).collect();
            available.sort();
            format!(
                "unknown provider '{}'. Available: {}",
                name,
                available.join(", ")
            )
        })
    }

    /// List all registered provider names.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.providers.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }
}

/// Returns the builtin providers directory.
/// Checks NEMESIS8_PROVIDERS_DIR env var first (used in tests), then the Docker image path.
fn builtin_providers_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NEMESIS8_PROVIDERS_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(BUILTIN_PROVIDERS_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_test_registry() -> ProviderRegistry {
        let providers_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("providers");
        std::env::set_var("NEMESIS8_PROVIDERS_DIR", &providers_dir);
        ProviderRegistry::load()
    }

    #[test]
    fn test_load_builtins() {
        let reg = load_test_registry();
        assert!(reg.get("codex").is_some());
        assert!(reg.get("gemini").is_some());
        assert!(reg.get("claude").is_some());
        assert!(reg.get("openclaw").is_some());
        assert!(reg.get("ollama").is_some());
    }

    #[test]
    fn test_aliases() {
        let reg = load_test_registry();
        assert!(reg.get("openai").is_some());
        assert!(reg.get("google").is_some());
        assert!(reg.get("anthropic").is_some());
        assert!(reg.get("claw").is_some());
    }

    #[test]
    fn test_resolve_error() {
        let reg = load_test_registry();
        let err = reg.resolve("nonexistent").unwrap_err();
        assert!(err.contains("unknown provider"));
        assert!(err.contains("codex"));
    }

    #[test]
    fn test_names() {
        let reg = load_test_registry();
        let names = reg.names();
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"gemini"));
        assert!(names.len() >= 5);
    }

    #[test]
    fn test_case_insensitive() {
        let reg = load_test_registry();
        assert!(reg.get("Codex").is_some());
        assert!(reg.get("GEMINI").is_some());
        assert!(reg.get("Claude").is_some());
    }
}
