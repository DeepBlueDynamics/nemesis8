//! Charon consumer-proxy sidecar lifecycle.
//!
//! When `[charon] enabled = true` in `.nemesis8.toml`, a headless agent run
//! comes up on a **per-session internal bridge network** alongside a `charon
//! consumer` proxy container. The agent is scoped to that network and can reach
//! *only* the proxy (the network is `internal`, so there is no other egress) at
//! `http://<alias>:<port>/v1`. Teardown removes both the sidecar and the network
//! together. See `../charon/spec/07-consumer-nemesis8.md`.
//!
//! Default (no `[charon]` block, or `enabled = false`) is a complete no-op:
//! [`CharonSidecar::maybe_start`] returns `Ok(None)` and nothing changes.

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, NetworkingConfig, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::models::{EndpointSettings, HostConfig};
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use std::collections::HashMap;
use std::process::Stdio;

use crate::config::{CharonConfig, Config};

/// Prefix for the per-session internal network. The session name is appended so
/// concurrent sessions get isolated networks (and isolated `charon-proxy`
/// aliases, since the alias is only unique *within* a network).
const NETWORK_PREFIX: &str = "n8-charon";

/// Label marking sidecar containers + networks this module owns, so a stray one
/// (e.g. from a hard crash before teardown) is identifiable.
const LABEL_CHARON: &str = "nemesis8.charon";

/// A live consumer-proxy sidecar plus its session network. Held by the agent run
/// for the duration of the session; [`teardown`](Self::teardown) removes both.
/// `Drop` is a best-effort CLI backstop for error/panic paths where the async
/// teardown never ran.
pub struct CharonSidecar {
    /// Per-session internal network the agent must join.
    pub network: String,
    /// Sidecar container name.
    pub container: String,
    /// DNS alias the agent uses (`http://<alias>:<port>/v1`).
    pub alias: String,
    /// Proxy port.
    pub port: u16,
    /// Container runtime binary ("docker"/"podman") for the Drop backstop.
    runtime: String,
    /// False once async teardown has run — makes Drop a no-op.
    active: bool,
}

impl CharonSidecar {
    /// Start the sidecar if `[charon] enabled`. Returns `Ok(None)` when disabled
    /// so callers can wire this in unconditionally with zero default-path cost.
    ///
    /// On success the caller MUST place the agent container on `self.network`
    /// (set its `network_mode`) so it can resolve and reach the proxy.
    pub async fn maybe_start(
        docker: &Docker,
        runtime: &str,
        session: &str,
        config: &Config,
    ) -> Result<Option<Self>> {
        let Some(cfg) = config.charon.as_ref().filter(|c| c.enabled) else {
            return Ok(None);
        };

        let network = format!("{NETWORK_PREFIX}-{session}");
        let container = format!("{}-{session}", cfg.alias);

        // 1. Per-session internal bridge: members reach each other, nothing else.
        ensure_internal_network(docker, &network).await?;

        // 2. Start the proxy on it, aliased so the agent reaches it by name.
        if let Err(e) = start_sidecar(docker, &network, &container, session, cfg).await {
            // Don't leak the network we just created if the sidecar failed.
            docker.remove_network(&network).await.ok();
            return Err(e);
        }

        tracing::info!(
            network = %network,
            container = %container,
            endpoint = %cfg.endpoint(),
            "charon consumer-proxy sidecar started"
        );

        Ok(Some(Self {
            network,
            container,
            alias: cfg.alias.clone(),
            port: cfg.port,
            runtime: runtime.to_string(),
            active: true,
        }))
    }

    /// OpenAI base URL the agent should use (`http://<alias>:<port>/v1`).
    pub fn endpoint(&self) -> String {
        format!("http://{}:{}/v1", self.alias, self.port)
    }

    /// Graceful async teardown via the API: remove the sidecar, then its
    /// network. Idempotent; disarms the `Drop` backstop. Best-effort — a failure
    /// to remove is logged, not propagated, so it never masks the run's result.
    pub async fn teardown(&mut self, docker: &Docker) {
        if !self.active {
            return;
        }
        self.active = false;

        docker
            .remove_container(
                &self.container,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .ok();
        // Network removal only succeeds once no container is attached, hence
        // after the sidecar (and the agent, torn down by the caller) are gone.
        if let Err(e) = docker.remove_network(&self.network).await {
            tracing::warn!(network = %self.network, "charon network removal failed: {e}");
        } else {
            tracing::info!(network = %self.network, "charon sidecar + network torn down");
        }
    }
}

impl Drop for CharonSidecar {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Backstop for paths that returned early (errors/panics) before the
        // async teardown could run. Shell out to the runtime — best effort,
        // output silenced. Safe to run even if the objects are already gone.
        let _ = std::process::Command::new(&self.runtime)
            .args(["rm", "-f", &self.container])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new(&self.runtime)
            .args(["network", "rm", &self.network])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Create the per-session network as an `internal` bridge if it doesn't exist.
/// `internal: true` is what restricts the agent to the sidecar — Docker gives
/// the network no gateway to the outside, so the only reachable host is the
/// proxy that shares it.
async fn ensure_internal_network(docker: &Docker, name: &str) -> Result<()> {
    let mut filters: HashMap<&str, Vec<&str>> = HashMap::new();
    filters.insert("name", vec![name]);
    let existing = docker
        .list_networks(Some(ListNetworksOptions { filters }))
        .await
        .context("listing docker networks")?;
    // The name filter is a substring match — confirm an exact hit.
    if existing.iter().any(|n| n.name.as_deref() == Some(name)) {
        return Ok(());
    }

    docker
        .create_network(CreateNetworkOptions {
            name,
            driver: "bridge",
            internal: true,
            ..Default::default()
        })
        .await
        .with_context(|| format!("creating internal network '{name}'"))?;
    Ok(())
}

/// Create + start the proxy container on `network`, aliased so the agent can
/// resolve it by `cfg.alias`.
async fn start_sidecar(
    docker: &Docker,
    network: &str,
    container: &str,
    session: &str,
    cfg: &CharonConfig,
) -> Result<()> {
    let binds: Vec<String> = cfg
        .mounts
        .iter()
        .map(|m| match &m.mode {
            Some(mode) => format!("{}:{}:{}", m.host, m.container, mode),
            None => format!("{}:{}", m.host, m.container),
        })
        .collect();

    let host_config = HostConfig {
        network_mode: Some(network.to_string()),
        binds: if binds.is_empty() { None } else { Some(binds) },
        ..Default::default()
    };

    // Network-scoped alias: the agent resolves `cfg.alias` → this container.
    let mut endpoints = HashMap::new();
    endpoints.insert(
        network.to_string(),
        EndpointSettings {
            aliases: Some(vec![cfg.alias.clone()]),
            ..Default::default()
        },
    );

    // CHARON_GATEWAY points the client at the always-on relay VM; any explicit
    // [charon.env] entry wins (placed last → overrides).
    let mut env: Vec<String> = vec![format!("CHARON_GATEWAY={}", cfg.gateway)];
    env.extend(cfg.env.iter().map(|(k, v)| format!("{k}={v}")));

    let mut labels = HashMap::new();
    labels.insert(LABEL_CHARON.to_string(), "true".to_string());
    labels.insert("nemesis8.charon_session".to_string(), session.to_string());

    let container_config = ContainerConfig {
        image: Some(cfg.image.clone()),
        cmd: if cfg.command.is_empty() {
            None
        } else {
            Some(cfg.command.clone())
        },
        env: if env.is_empty() { None } else { Some(env) },
        host_config: Some(host_config),
        networking_config: Some(NetworkingConfig {
            endpoints_config: endpoints,
        }),
        labels: Some(labels),
        ..Default::default()
    };

    docker
        .create_container(
            Some(CreateContainerOptions {
                name: container,
                platform: None,
            }),
            container_config,
        )
        .await
        .with_context(|| format!("creating charon sidecar '{container}'"))?;
    docker
        .start_container(container, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("starting charon sidecar '{container}'"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> Config {
        Config {
            charon: Some(CharonConfig {
                enabled,
                image: "deepbluedynamics/charon".to_string(),
                command: vec!["charon".to_string(), "consumer".to_string()],
                gateway: "wss://gateway.nuts.services/ws".to_string(),
                port: 8088,
                alias: "charon-proxy".to_string(),
                mounts: vec![],
                env: HashMap::new(),
            }),
            ..Config::default()
        }
    }

    #[test]
    fn endpoint_is_versioned_openai_url() {
        let charon = CharonSidecar {
            network: "n8-charon-foo".to_string(),
            container: "charon-proxy-foo".to_string(),
            alias: "charon-proxy".to_string(),
            port: 8088,
            runtime: "docker".to_string(),
            active: false, // inert: Drop must not shell out
        };
        assert_eq!(charon.endpoint(), "http://charon-proxy:8088/v1");
    }

    #[test]
    fn config_endpoint_matches() {
        let c = cfg(true);
        assert_eq!(
            c.charon.unwrap().endpoint(),
            "http://charon-proxy:8088/v1"
        );
    }

    #[test]
    fn disabled_or_absent_is_inert() {
        // A present-but-disabled block must read as "no sidecar".
        let c = cfg(false);
        assert!(c.charon.as_ref().filter(|x| x.enabled).is_none());
        // An absent block likewise.
        let bare = Config::default();
        assert!(bare.charon.is_none());
    }
}
