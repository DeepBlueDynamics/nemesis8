use anyhow::{Context, Result};
use std::path::PathBuf;

/// Manages the ~/.nemisis8/pokeballs/ registry
pub struct PokeballStore {
    root: PathBuf,
}

/// Summary info about a stored pokeball
#[derive(Debug)]
pub struct PokeballInfo {
    pub name: String,
    pub path: PathBuf,
    pub image_tag: Option<String>,
    pub has_spec: bool,
}

impl PokeballStore {
    /// Create a store rooted at ~/.nemisis8/pokeballs/
    pub fn open() -> Result<Self> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        let root = home.join(".nemisis8").join("pokeballs");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating store at {}", root.display()))?;
        Ok(Self { root })
    }

    /// Get the directory for a specific pokeball
    pub fn pokeball_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Check if a pokeball exists
    pub fn exists(&self, name: &str) -> bool {
        self.pokeball_dir(name).join("pokeball.yaml").is_file()
    }

    /// Get the spec path for a pokeball
    pub fn spec_path(&self, name: &str) -> PathBuf {
        self.pokeball_dir(name).join("pokeball.yaml")
    }

    /// Get the source directory for a pokeball (used for git clones)
    pub fn source_dir(&self, name: &str) -> PathBuf {
        self.pokeball_dir(name).join("source")
    }

    /// Get the comms directory for a pokeball
    pub fn comms_dir(&self, name: &str) -> PathBuf {
        self.pokeball_dir(name).join("comms")
    }

    /// Ensure comms directories exist
    pub fn ensure_comms(&self, name: &str) -> Result<()> {
        let comms = self.comms_dir(name);
        std::fs::create_dir_all(comms.join("inbox"))?;
        std::fs::create_dir_all(comms.join("outbox"))?;
        std::fs::create_dir_all(comms.join("status"))?;
        Ok(())
    }

    /// List all pokeballs
    pub fn list(&self) -> Result<Vec<PokeballInfo>> {
        let mut results = Vec::new();

        if !self.root.is_dir() {
            return Ok(results);
        }

        for entry in std::fs::read_dir(&self.root).context("reading pokeball store")? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = entry
                .file_name()
                .to_string_lossy()
                .to_string();

            let has_spec = path.join("pokeball.yaml").is_file();

            let image_tag = std::fs::read_to_string(path.join("image_tag"))
                .ok()
                .map(|s| s.trim().to_string());

            results.push(PokeballInfo {
                name,
                path,
                image_tag,
                has_spec,
            });
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(results)
    }

    /// Remove a pokeball from the store
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.pokeball_dir(name);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing pokeball '{name}'"))?;
        }
        Ok(())
    }

    /// Find a pokeball by name and return its spec
    pub fn load_spec(&self, name: &str) -> Result<super::spec::PokeballSpec> {
        let path = self.spec_path(name);
        if !path.is_file() {
            anyhow::bail!("pokeball '{name}' not found in store");
        }
        super::spec::PokeballSpec::load(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, PokeballStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pokeballs");
        std::fs::create_dir_all(&root).unwrap();
        (dir, PokeballStore { root })
    }

    #[test]
    fn test_pokeball_dir() {
        let (_dir, store) = temp_store();
        let path = store.pokeball_dir("test");
        assert!(path.ends_with("pokeballs/test") || path.ends_with("pokeballs\\test"));
    }

    #[test]
    fn test_exists_false() {
        let (_dir, store) = temp_store();
        assert!(!store.exists("nonexistent"));
    }

    #[test]
    fn test_list_empty() {
        let (_dir, store) = temp_store();
        let list = store.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_ensure_comms() {
        let (_dir, store) = temp_store();
        store.ensure_comms("test").unwrap();
        let comms = store.comms_dir("test");
        assert!(comms.join("inbox").is_dir());
        assert!(comms.join("outbox").is_dir());
        assert!(comms.join("status").is_dir());
    }

    #[test]
    fn test_list_with_pokeballs() {
        let (_dir, store) = temp_store();

        // Create a pokeball
        let pb_dir = store.pokeball_dir("alpha");
        std::fs::create_dir_all(&pb_dir).unwrap();
        std::fs::write(pb_dir.join("pokeball.yaml"), "test: true").unwrap();
        std::fs::write(pb_dir.join("image_tag"), "pokeball-alpha:latest").unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "alpha");
        assert!(list[0].has_spec);
        assert_eq!(
            list[0].image_tag.as_deref(),
            Some("pokeball-alpha:latest")
        );
    }

    #[test]
    fn test_remove() {
        let (_dir, store) = temp_store();
        let pb_dir = store.pokeball_dir("todelete");
        std::fs::create_dir_all(&pb_dir).unwrap();
        std::fs::write(pb_dir.join("pokeball.yaml"), "test: true").unwrap();

        assert!(store.exists("todelete"));
        store.remove("todelete").unwrap();
        assert!(!store.exists("todelete"));
    }
}
