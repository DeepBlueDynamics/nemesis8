//! Host-side nemesis8 data locations.
//!
//! The container home volume lives at `~/.nemesis8/home` (mounted at
//! `/opt/nemesis8` inside agent containers). It sits as a subdir of
//! `~/.nemesis8` — next to `env`, `providers/`, `session-workspaces.json` —
//! rather than being `~/.nemesis8` itself, so the env file (key material)
//! is never bind-mounted into agent containers.
//!
//! History: this dir was `~/.codex-service` from when the project wrapped
//! only Codex (issue #39). `ensure_data_home()` ports an existing legacy
//! dir forward by copying, so logins and session history survive the rename.

use std::path::PathBuf;

/// `~/.nemesis8` — nemesis8's root config dir on the host.
pub fn nemesis_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".nemesis8")
}

/// `~/.nemesis8/home` — the container HOME volume (mounted at /opt/nemesis8).
/// Auth tokens, provider session stores, MCP state, trigger store, daemon
/// pid/log all live here.
pub fn data_home() -> PathBuf {
    nemesis_root().join("home")
}

/// `~/.nemesis8/config.toml` — the GLOBAL config layer (under the n8 dir, not the
/// bare home dir). `load_layered` merges this beneath the per-workspace
/// `<cwd>/.nemesis8.toml` (local wins).
pub fn global_config_path() -> PathBuf {
    nemesis_root().join("config.toml")
}

/// `~/.nemesis8.toml` — the LEGACY global-config location (bare home dir). Read as
/// a fallback and one-time-copied to `global_config_path()`; left in place so
/// nothing is lost. Users can delete it once migrated.
pub fn legacy_global_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".nemesis8.toml")
}

/// `~/.codex-service` — the legacy name for `data_home()`.
pub fn legacy_data_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex-service")
}

/// Make sure `data_home()` exists, porting a legacy `~/.codex-service` forward
/// the first time. The legacy dir is COPIED (not moved) and left in place —
/// containers that are already running keep their bind mount on the old path
/// until restarted, and an old binary pointed at it keeps working. Safe to
/// call every startup: once the new dir exists this is a no-op.
pub fn ensure_data_home() -> PathBuf {
    let new = data_home();
    if new.is_dir() {
        return new;
    }
    let old = legacy_data_home();
    if old.is_dir() {
        eprintln!(
            "[nemesis8] one-time migration: copying {} -> {} (logins + sessions come along)...",
            old.display(),
            new.display()
        );
        match copy_tree(&old, &new) {
            Ok(n) => {
                eprintln!(
                    "[nemesis8] migrated {n} files. The old dir was left in place; \
                     remove it once you're satisfied (restart running agents so new \
                     sessions land in the new home)."
                );
            }
            Err(e) => {
                eprintln!("[nemesis8] migration failed ({e}); continuing with a fresh data home");
            }
        }
    }
    std::fs::create_dir_all(&new).ok();
    new
}

/// Recursive copy; skips entries that fail (locked files from a running
/// container shouldn't abort the whole port). Returns files copied.
fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut copied = 0u64;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {
                copied += copy_tree(&from, &to).unwrap_or(0);
            }
            Ok(ft) if ft.is_file() => {
                if std::fs::copy(&from, &to).is_ok() {
                    copied += 1;
                }
            }
            _ => {} // symlinks/unknown: skip
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_home_is_under_nemesis_root() {
        assert_eq!(data_home(), nemesis_root().join("home"));
        assert!(data_home().to_string_lossy().contains(".nemesis8"));
    }

    #[test]
    fn test_copy_tree_ports_nested_files() {
        let src = std::env::temp_dir().join(format!("n8-mig-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("n8-mig-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(src.join(".gemini/antigravity-cli/antigravity-oauth-token"), b"tok").unwrap();
        std::fs::write(src.join("agents.json"), b"{}").unwrap();
        let n = copy_tree(&src, &dst).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            std::fs::read(dst.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
            b"tok"
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
