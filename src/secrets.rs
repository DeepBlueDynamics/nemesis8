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
//! There is deliberately NO enumeration: `keyring` cannot list what's stored,
//! and n8 does not keep a shadow inventory. Secrets are addressed BY NAME —
//! `set`/`get`/`delete` a name you know. `status` reports the backend's health,
//! not its contents.

/// Keychain service namespace. All n8 secrets are stored under this service; the
/// per-entry "user" is the env var name.
const SERVICE: &str = "nemesis8";

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

/// Human-readable name of the OS keychain backend for this platform (for
/// `status` output).
pub fn backend() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows Credential Manager"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS Keychain"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux Secret Service"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "OS keychain"
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
    fn test_backend_nonempty() {
        assert!(!backend().is_empty());
    }
}
