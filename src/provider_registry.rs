use crate::provider_def::ProviderDef;
use std::collections::HashMap;

/// Built-in provider TOML files, embedded at compile time.
const BUILTIN_PROVIDERS: &[(&str, &str)] = &[
    ("codex", include_str!("../providers/codex.toml")),
    ("gemini", include_str!("../providers/gemini.toml")),
    ("claude", include_str!("../providers/claude.toml")),
    ("openclaw", include_str!("../providers/openclaw.toml")),
    ("qwen", include_str!("../providers/qwen.toml")),
];

/// Registry of all known providers (builtins + user-defined).
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderDef>,
    aliases: HashMap<String, String>,
}

impl ProviderRegistry {
    /// Build the registry from embedded providers, then overlay user-defined ones.
    pub fn load() -> Self {
        let mut reg = Self {
            providers: HashMap::new(),
            aliases: HashMap::new(),
        };

        // Load builtins
        for (name, toml_str) in BUILTIN_PROVIDERS {
            match toml::from_str::<ProviderDef>(toml_str) {
                Ok(def) => {
                    for alias in &def.provider.aliases {
                        reg.aliases.insert(alias.to_lowercase(), name.to_string());
                    }
                    reg.providers.insert(name.to_string(), def);
                }
                Err(e) => {
                    eprintln!("[nemesis8] warning: failed to parse builtin provider {name}: {e}");
                }
            }
        }

        // Load user-defined providers from ~/.nemesis8/providers/*.toml
        reg.load_user_providers();

        reg
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

            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<ProviderDef>(&content) {
                    Ok(def) => {
                        let name = def.provider.name.to_lowercase();
                        for alias in &def.provider.aliases {
                            self.aliases.insert(alias.to_lowercase(), name.clone());
                        }
                        // User providers override builtins
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
            let available: Vec<&str> = self.providers.keys().map(|s| s.as_str()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_builtins() {
        let reg = ProviderRegistry::load();
        assert!(reg.get("codex").is_some());
        assert!(reg.get("gemini").is_some());
        assert!(reg.get("claude").is_some());
        assert!(reg.get("openclaw").is_some());
        assert!(reg.get("qwen").is_some());
    }

    #[test]
    fn test_aliases() {
        let reg = ProviderRegistry::load();
        assert!(reg.get("openai").is_some());
        assert!(reg.get("google").is_some());
        assert!(reg.get("anthropic").is_some());
        assert!(reg.get("claw").is_some());
    }

    #[test]
    fn test_resolve_error() {
        let reg = ProviderRegistry::load();
        let err = reg.resolve("nonexistent").unwrap_err();
        assert!(err.contains("unknown provider"));
        assert!(err.contains("codex"));
    }

    #[test]
    fn test_names() {
        let reg = ProviderRegistry::load();
        let names = reg.names();
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"gemini"));
        assert!(names.len() >= 5);
    }

    #[test]
    fn test_case_insensitive() {
        let reg = ProviderRegistry::load();
        assert!(reg.get("Codex").is_some());
        assert!(reg.get("GEMINI").is_some());
        assert!(reg.get("Claude").is_some());
    }
}
