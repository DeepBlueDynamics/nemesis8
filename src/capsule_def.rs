use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Declarative template for a **capsule**: a narrow, single-purpose, hardened
/// artifact nemesis8 *emits* (rather than runs) for an air-gapped / Iron Bank
/// target. Loaded from `capsules/*.toml`, mirroring `service_def::ServiceDef`.
///
/// Unlike a service (which n8 pulls/runs), a capsule is a *build recipe* the
/// emitter turns into a Repo One–submittable bundle: pinned source, vendored
/// deps, a hardened Dockerfile rebased onto an Iron Bank base, a
/// `hardening_manifest.yaml`, and an exported image tarball. The emit pipeline
/// lives in `capsule.rs`; this is purely *what* to emit.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CapsuleDef {
    pub capsule: CapsuleSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CapsuleSpec {
    /// Capsule name — the bundle dir + local image tag stem.
    pub name: String,
    /// Where the source comes from (a pinned git rev, or a local checkout).
    pub source: SourceSpec,
    /// Which binary the runtime image ships (e.g. `sigil-mcp`).
    pub binary: String,
    /// Cargo features to build with (e.g. `["mcp-server"]`).
    #[serde(default)]
    pub build_features: Vec<String>,
    /// Iron Bank *runtime* base image ref, e.g.
    /// `registry1.dso.mil/ironbank/redhat/ubi/ubi9-minimal:9.4`. Parameterized —
    /// local verification swaps in a stand-in hardened base.
    pub base_image: String,
    /// Iron Bank *builder* base image ref (a rust toolchain image).
    pub builder_image: String,
    /// Non-root UID the runtime image runs as. Default `1000`.
    #[serde(default = "default_run_user")]
    pub run_user: String,
    /// Runtime env baked/documented into the image, `KEY=value`.
    #[serde(default)]
    pub env: Vec<String>,
    /// hardening_manifest maintainers.
    #[serde(default)]
    pub maintainers: Vec<Maintainer>,
    /// hardening_manifest image tags (e.g. `sigil-capsule/0.1.0`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// OCI labels emitted into both the Dockerfile and the hardening manifest.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Optional dev-loop config: the connected agent image n8 launches for
    /// `n8 capsule dev` (the "start it, dev on it" step, before you freeze).
    #[serde(default)]
    pub dev: Option<DevSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DevSpec {
    /// The connected dev agent image (e.g. a Sigil self-hosted image with the
    /// toolchain + source). Launched interactively, mounted on the source.
    pub image: String,
    /// Override the image ENTRYPOINT (e.g. `bash` for an interactive dev shell).
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Command/args to run in the dev container (empty → the image's entrypoint).
    #[serde(default)]
    pub command: Vec<String>,
    /// Default model for the dev session (cloud OK — this is the low side).
    #[serde(default)]
    pub model: Option<String>,
    /// Extra `KEY=value` env for the dev session.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceSpec {
    /// Git remote to clone (used when no local `path` override is given).
    #[serde(default)]
    pub git: Option<String>,
    /// Pinned commit SHA or tag — required with `git` (Iron Bank pins source).
    #[serde(default)]
    pub rev: Option<String>,
    /// Local checkout to build from instead of cloning — dev/offline convenience.
    /// The `--source` CLI flag overrides this.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Maintainer {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub username: String,
}

fn default_run_user() -> String {
    "1000".to_string()
}

impl CapsuleSpec {
    /// Validate the template: a source (git+rev or a local path), and the base
    /// images + binary must be set.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("capsule has an empty `name`".into());
        }
        match (&self.source.git, &self.source.path) {
            (None, None) => {
                return Err(format!(
                    "capsule '{}' sets neither `source.git` nor `source.path`",
                    self.name
                ));
            }
            (Some(_), _) if self.source.rev.as_deref().unwrap_or("").trim().is_empty() => {
                return Err(format!(
                    "capsule '{}' sets `source.git` but no `source.rev` (Iron Bank pins source)",
                    self.name
                ));
            }
            _ => {}
        }
        if self.binary.trim().is_empty() {
            return Err(format!("capsule '{}' has an empty `binary`", self.name));
        }
        if self.base_image.trim().is_empty() {
            return Err(format!("capsule '{}' has an empty `base_image`", self.name));
        }
        if self.builder_image.trim().is_empty() {
            return Err(format!("capsule '{}' has an empty `builder_image`", self.name));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn capsules_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("capsules")
    }

    #[test]
    fn test_parse_sigil_capsule() {
        let path = capsules_dir().join("sigil.toml");
        let toml_str = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
        let def: CapsuleDef = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("failed to parse sigil.toml: {e}"));
        assert_eq!(def.capsule.name, "sigil");
        assert!(!def.capsule.binary.is_empty());
        def.capsule.validate().expect("sigil capsule must validate");
    }

    #[test]
    fn test_all_capsules_parse_and_validate() {
        let dir = capsules_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("could not read capsules dir: {e}"))
            .flatten()
            .filter(|e| e.path().extension().map_or(false, |x| x == "toml"))
            .collect();
        assert!(!entries.is_empty(), "no capsule TOMLs found");
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy();
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
            let def: CapsuleDef = toml::from_str(&content)
                .unwrap_or_else(|e| panic!("failed to parse {name}.toml: {e}"));
            assert_eq!(def.capsule.name, name.as_ref());
            def.capsule
                .validate()
                .unwrap_or_else(|e| panic!("{name}.toml invalid: {e}"));
        }
    }

    #[test]
    fn test_validate_requires_rev_with_git() {
        let spec = CapsuleSpec {
            name: "x".into(),
            source: SourceSpec { git: Some("https://example/x".into()), rev: None, path: None },
            binary: "x".into(),
            build_features: vec![],
            base_image: "base:1".into(),
            builder_image: "builder:1".into(),
            run_user: default_run_user(),
            env: vec![],
            maintainers: vec![],
            tags: vec![],
            labels: BTreeMap::new(),
            dev: None,
        };
        assert!(spec.validate().is_err());
    }
}
