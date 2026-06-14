use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Declarative template for a dependency service nemesis8 can spawn and
/// supervise (Ferricula, the Hyperia sidecar, shivvr, opensearch, …). Loaded
/// from `services/*.toml`, mirroring `provider_def::ProviderDef`. The container
/// engine half (pull/build → create+start → health) lives in `docker.rs`; this
/// is purely *what* to start.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceDef {
    pub service: ServiceSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceSpec {
    /// Container name + `gnosis-network` DNS name (how agents reach it).
    pub name: String,
    /// Registry image ref (e.g. `deepbluedynamics/ferricula:0.10.3`). Mutually
    /// exclusive with `build`; exactly one must be set (see [`validate`]).
    #[serde(default)]
    pub image: Option<String>,
    /// Build-from-source for dev, instead of pulling a published image.
    #[serde(default)]
    pub build: Option<BuildSpec>,
    /// Shared bridge network agents also join. Default `gnosis-network`.
    #[serde(default = "default_network")]
    pub network: String,
    /// Docker restart policy: `no` | `on-failure` | `always` | `unless-stopped`.
    #[serde(default = "default_restart")]
    pub restart: String,
    /// Published ports, `"host:cont"` / `"ip:host:cont"` / `"port"`. Defaults to
    /// a 127.0.0.1 binding (this machine only) unless an ip is given.
    #[serde(default)]
    pub ports: Vec<String>,
    /// Named or bind volumes, `"name:/path"` / `"/host:/cont[:mode]"`.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Plain `KEY=value` env. Secret *refs* arrive in M4 (broker plan).
    #[serde(default)]
    pub env: Vec<String>,
    /// Health gate — when set, `ensure_service` waits for it before "up".
    #[serde(default)]
    pub health: Option<HealthSpec>,
    /// Other service names that must be up first (reconciled depends-first).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Config keys a *running* instance exports into agent env, e.g.
    /// `FERRICULA_URL = "http://ferricula:8765"`.
    #[serde(default)]
    pub exposes: HashMap<String, String>,
    /// Whether `n8 serve` brings this up by default (config can still override).
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuildSpec {
    /// Build context dir (relative to the nemesis8 project root or absolute).
    pub context: String,
    /// Dockerfile path relative to the context. Defaults to `Dockerfile`.
    #[serde(default)]
    pub dockerfile: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthSpec {
    /// In-container probe. An `http(s)://…` value becomes an HTTP GET healthcheck
    /// (`curl`/`wget` inside the container); anything else is run as a shell
    /// command whose exit 0 means healthy.
    pub test: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_network() -> String {
    "gnosis-network".to_string()
}
fn default_restart() -> String {
    "unless-stopped".to_string()
}
fn default_interval() -> u64 {
    10
}
fn default_retries() -> u32 {
    6
}

impl ServiceSpec {
    /// Validate the template: exactly one of `image` / `build` must be set.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.image, &self.build) {
            (Some(_), Some(_)) => Err(format!(
                "service '{}' sets both `image` and `build` — pick one",
                self.name
            )),
            (None, None) => Err(format!(
                "service '{}' sets neither `image` nor `build`",
                self.name
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn services_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("services")
    }

    #[test]
    fn test_parse_ferricula_service() {
        let path = services_dir().join("ferricula.toml");
        let toml_str = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
        let def: ServiceDef = toml::from_str(&toml_str)
            .unwrap_or_else(|e| panic!("failed to parse ferricula.toml: {e}"));
        assert_eq!(def.service.name, "ferricula");
        assert!(def.service.image.is_some());
        assert_eq!(def.service.network, "gnosis-network");
        assert!(def.service.health.is_some());
        assert!(def.service.exposes.contains_key("FERRICULA_URL"));
        def.service.validate().expect("ferricula template must validate");
    }

    #[test]
    fn test_all_services_parse_and_validate() {
        let dir = services_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("could not read services dir: {e}"))
            .flatten()
            .filter(|e| e.path().extension().map_or(false, |x| x == "toml"))
            .collect();
        assert!(!entries.is_empty(), "no service TOMLs found");
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy();
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
            let def: ServiceDef = toml::from_str(&content)
                .unwrap_or_else(|e| panic!("failed to parse {name}.toml: {e}"));
            assert_eq!(def.service.name, name.as_ref());
            def.service
                .validate()
                .unwrap_or_else(|e| panic!("{name}.toml invalid: {e}"));
        }
    }

    #[test]
    fn test_validate_rejects_image_and_build() {
        let spec = ServiceSpec {
            name: "x".into(),
            image: Some("img".into()),
            build: Some(BuildSpec { context: "..".into(), dockerfile: None }),
            network: default_network(),
            restart: default_restart(),
            ports: vec![],
            volumes: vec![],
            env: vec![],
            health: None,
            depends_on: vec![],
            exposes: HashMap::new(),
            enabled: false,
        };
        assert!(spec.validate().is_err());
    }
}
