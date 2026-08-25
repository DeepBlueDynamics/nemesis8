//! n8-managed secret store backed by the OS keychain (Windows Credential
//! Manager / macOS Keychain / Linux Secret Service) via the `keyring` crate.
//!
//! Secrets live encrypted at rest in the OS store — never plaintext in
//! `.nemesis8.toml [env]` or a shell profile. The service namespace is
//! `"nemesis8"` and each entry's key is the ENV VAR NAME it is injected as
//! (`ANTHROPIC_API_KEY`, `DISCORD_BOT_TOKEN`, a custom name). At container
//! launch, `docker.rs::build_env` prefers a keychain value over host env over
//! the `[env]` table.
//!
//! `keyring` exposes no "enumerate all" — a backend can only be queried by a
//! known name — so listing works against a CANDIDATE set (see
//! [`candidate_names`]): the same secrets n8 already forwards.

use crate::config::Config;

/// Keychain service namespace. All n8 secrets are stored under this service; the
/// per-entry "user" is the env var name.
const SERVICE: &str = "nemesis8";

/// A secret's display record: its name, whether it is set, and a masked preview.
#[derive(Debug, Clone)]
pub struct SecretInfo {
    pub name: String,
    pub set: bool,
    /// Masked value (prefix…suffix) when set; `None` when unset.
    pub masked: Option<String>,
}

fn entry(name: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| anyhow::anyhow!("keyring open `{name}`: {e}"))
}

/// Whether a real OS keychain backend is usable on this host. On a keyring-less
/// Linux box (no Secret Service daemon) this is `false`, so callers can degrade
/// to host env / `[env]` with a clear message instead of hard-erroring.
pub fn available() -> bool {
    match keyring::Entry::new(SERVICE, "__n8_probe__") {
        // A working backend answers a lookup with a value or `NoEntry`; a broken
        // or absent one answers with a storage/platform failure.
        Ok(e) => !matches!(
            e.get_password(),
            Err(keyring::Error::NoStorageAccess(_)) | Err(keyring::Error::PlatformFailure(_))
        ),
        Err(_) => false,
    }
}

/// Fetch a secret by name. `Ok(None)` when it is not set.
pub fn get(name: &str) -> anyhow::Result<Option<String>> {
    match entry(name)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring get `{name}`: {e}")),
    }
}

/// Store (or overwrite) a secret.
pub fn set(name: &str, value: &str) -> anyhow::Result<()> {
    entry(name)?
        .set_password(value)
        .map_err(|e| anyhow::anyhow!("keyring set `{name}`: {e}"))
}

/// Remove a secret. `Ok(())` even when it was already absent.
pub fn delete(name: &str) -> anyhow::Result<()> {
    match entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("keyring delete `{name}`: {e}")),
    }
}

/// Mask a secret for display: a short prefix + last 4, middle elided
/// (`sk-a…WxYz`). Values of 8 chars or fewer are fully masked so a short token
/// is never revealed.
pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return "•".repeat(n.max(1));
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[n - 4..].iter().collect();
    format!("{prefix}…{suffix}")
}

/// The secret names a user manages in the store — the union n8 cares about:
/// a static list of integration secrets, every provider's
/// `[provider.api_keys]` chain/target, and every socket-MCP server's
/// bearer-token env. Deduped, order-stable. URLs/non-secret config vars are
/// intentionally excluded (they belong in `[env]`, not the keychain).
pub fn candidate_names(_config: &Config) -> Vec<String> {
    // Known integration secrets (the secret subset of build_env's forward list).
    let mut names: Vec<String> = [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "SERPAPI_API_KEY",
        "ELEVENLABS_API_KEY",
        "HYPERIA_AGENT_TOKEN",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // Every provider's declared API keys (a new provider TOML shows up here for
    // free — same union build_env forwards).
    let providers = crate::provider_registry::ProviderRegistry::load();
    for def in providers.all() {
        for k in def
            .provider
            .api_keys
            .chain
            .iter()
            .chain(def.provider.api_keys.target.iter())
        {
            if !k.is_empty() && !names.contains(k) {
                names.push(k.clone());
            }
        }
    }

    // Each socket-MCP server's bearer-token env var.
    let mcp = crate::mcp_registry::McpRegistry::load();
    for def in mcp.all() {
        if let Some(tok) = &def.server.bearer_token_env {
            if !tok.is_empty() && !names.contains(tok) {
                names.push(tok.clone());
            }
        }
    }

    names
}

/// Look up each candidate name and report set/masked status (for `n8 secrets
/// list` and the control-room Secrets screen). Names that error on read are
/// reported as unset rather than failing the whole listing.
pub fn list(candidates: &[String]) -> Vec<SecretInfo> {
    candidates
        .iter()
        .map(|name| match get(name) {
            Ok(Some(v)) => SecretInfo {
                name: name.clone(),
                set: true,
                masked: Some(mask(&v)),
            },
            _ => SecretInfo {
                name: name.clone(),
                set: false,
                masked: None,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask() {
        assert_eq!(mask(""), "•");
        assert_eq!(mask("short"), "•••••");
        assert_eq!(mask("12345678"), "••••••••"); // 8 → fully masked
        assert_eq!(mask("sk-ant-api03-XYWxYz"), "sk-a…WxYz");
    }

    #[test]
    fn test_candidate_names_includes_known_secrets() {
        let names = candidate_names(&Config::default());
        assert!(names.iter().any(|n| n == "ANTHROPIC_API_KEY"));
        // Deduped: no name appears twice.
        let mut sorted = names.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "candidate_names must be deduped");
    }
}
