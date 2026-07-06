use anyhow::{Context, Result};
use bollard::Docker;
use bollard::container::{
    AttachContainerOptions, Config as ContainerConfig, CreateContainerOptions, LogOutput,
    RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::models::HostConfig;
use futures_util::StreamExt;
use std::path::Path;
use tokio::io::AsyncWriteExt;

use crate::config::Config;
use crate::ui::{self, BuildEvent};

const DEFAULT_IMAGE: &str = "nemesis8:latest";
const DEFAULT_NETWORK: &str = "gnosis-network";

/// Docker label keys applied to every nemesis8 agent container so the control
/// plane can discover and address agents regardless of container name. The
/// interactive path historically produced random Docker names (tender_agnesi,
/// etc.) that name-substring matching couldn't track — labels fix that.
pub const LABEL_AGENT: &str = "nemesis8.agent";
pub const LABEL_AGENT_ID: &str = "nemesis8.agent_id";
pub const LABEL_HOST_ID: &str = "nemesis8.host_id";
pub const LABEL_PROVIDER: &str = "nemesis8.provider";
pub const LABEL_SESSION_ID: &str = "nemesis8.session_id";
/// Native host path of the project workspace mounted at `/workspace/<dirname>`.
/// Stamped at launch so the control room shows the SAME workspace string the
/// Sessions tab records — deriving it from the mounts array was ambiguous (the
/// per-session scratch root also lives under `/workspace`).
pub const LABEL_WORKSPACE: &str = "nemesis8.workspace";

/// Image label stamped by `n8 build --gpu` (`nemesis8.gpu=true`). Run-time GPU
/// passthrough is gated on it so `--gpu` on a CPU image warns instead of failing.
pub const LABEL_GPU: &str = "nemesis8.gpu";

/// Labels applied to dependency-service containers (Ferricula, sidecar, …) so
/// the same reconcile-against-`docker ps` machinery used for agents can find,
/// list, and stop them by name regardless of who started them.
pub const LABEL_SERVICE: &str = "nemesis8.service";
pub const LABEL_SERVICE_NAME: &str = "nemesis8.service_name";

/// Status of a managed service container (for `n8 services` + the reconcile loop).
#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub name: String,
    pub id: String,
    /// Container run state: running / created / exited / …
    pub state: String,
    /// Health: healthy / unhealthy / starting / none (no healthcheck) / unknown.
    pub health: String,
}

/// Label set stamped on a service container.
fn service_labels(name: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(LABEL_SERVICE.to_string(), "true".to_string());
    m.insert(LABEL_SERVICE_NAME.to_string(), name.to_string());
    m.insert(LABEL_HOST_ID.to_string(), host_id());
    m
}

/// Parse `["host:cont", "ip:host:cont", "port"]` into a bollard port-binding map
/// (host binding defaults to 127.0.0.1 — this machine only — when no ip given).
fn port_binding_map(
    specs: &[String],
) -> Option<std::collections::HashMap<String, Option<Vec<bollard::models::PortBinding>>>> {
    if specs.is_empty() {
        return None;
    }
    let mut map = std::collections::HashMap::new();
    for spec in specs {
        let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
        let (ip, host, cont) = match parts.as_slice() {
            [p] => ("127.0.0.1", *p, *p),
            [h, c] => ("127.0.0.1", *h, *c),
            [i, h, c] => (*i, *h, *c),
            _ => {
                tracing::warn!(spec = %spec, "ignoring malformed service ports entry");
                continue;
            }
        };
        if host.is_empty() || cont.is_empty() {
            continue;
        }
        map.insert(
            format!("{cont}/tcp"),
            Some(vec![bollard::models::PortBinding {
                host_ip: Some(ip.to_string()),
                host_port: Some(host.to_string()),
            }]),
        );
    }
    if map.is_empty() { None } else { Some(map) }
}

/// Map a template restart string to a bollard policy. Unknown → unless-stopped.
fn restart_policy(s: &str) -> Option<bollard::models::RestartPolicy> {
    use bollard::models::{RestartPolicy, RestartPolicyNameEnum};
    let name = match s {
        "no" | "" => RestartPolicyNameEnum::NO,
        "always" => RestartPolicyNameEnum::ALWAYS,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        _ => RestartPolicyNameEnum::UNLESS_STOPPED,
    };
    Some(RestartPolicy {
        name: Some(name),
        maximum_retry_count: None,
    })
}

/// Build a Docker healthcheck from a service's HealthSpec. An `http(s)://` test
/// becomes a curl-or-wget probe; anything else runs as a shell command.
fn healthcheck_from(h: &crate::service_def::HealthSpec) -> bollard::models::HealthConfig {
    let test_cmd = if h.test.starts_with("http://") || h.test.starts_with("https://") {
        format!(
            "curl -fsS {0} >/dev/null 2>&1 || wget -qO- {0} >/dev/null 2>&1",
            h.test
        )
    } else {
        h.test.clone()
    };
    let ns = 1_000_000_000i64;
    bollard::models::HealthConfig {
        test: Some(vec!["CMD-SHELL".to_string(), test_cmd]),
        interval: Some((h.interval_secs.max(1) as i64) * ns),
        timeout: Some(5 * ns),
        retries: Some(h.retries as i64),
        start_period: Some(ns),
        ..Default::default()
    }
}

/// Stable-ish host identifier (hostname). Used in agent labels and, later, the
/// fleet registry's `{host_id}/{local_id}` agent IDs.
pub fn host_id() -> String {
    whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string())
}

/// Build the standard agent label set for a container.
fn agent_labels(
    provider: &str,
    agent_id: &str,
    session_id: Option<&str>,
    workspace: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(LABEL_AGENT.to_string(), "true".to_string());
    m.insert(LABEL_AGENT_ID.to_string(), agent_id.to_string());
    m.insert(LABEL_HOST_ID.to_string(), host_id());
    m.insert(LABEL_PROVIDER.to_string(), provider.to_string());
    if let Some(sid) = session_id {
        m.insert(LABEL_SESSION_ID.to_string(), sid.to_string());
    }
    // First entry of the (possibly comma-separated) workspace list = the
    // primary project dir, as a native host path.
    if let Some(ws) = workspace.and_then(|w| w.split(',').next()).map(str::trim) {
        if !ws.is_empty() {
            m.insert(LABEL_WORKSPACE.to_string(), ws.to_string());
        }
    }
    m
}

/// Convert a Windows path to Docker-compatible format.
/// `C:\Users\foo\bar` → `/c/Users/foo/bar`
/// Non-Windows paths pass through unchanged.
pub fn to_docker_path(path: &str) -> String {
    // Match "X:\" or "X:/" style Windows paths
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = &path[3..];
        format!("/{drive}/{}", rest.replace('\\', "/"))
    } else {
        path.replace('\\', "/")
    }
}

/// Inverse of `to_docker_path`: turn a container/Docker bind-mount source back
/// into the **native host** path for display (e.g. `/c/Users/x` → `C:\Users\x`
/// on Windows). Strips Docker Desktop host-mount prefixes. On non-Windows hosts
/// the path is already native, so it's returned as-is. Host-OS aware so the
/// control room shows paths the way the host shell would.
pub fn from_docker_path(path: &str) -> String {
    let p = path
        .strip_prefix("/run/desktop/mnt/host")
        .or_else(|| path.strip_prefix("/host_mnt"))
        .unwrap_or(path);
    #[cfg(windows)]
    {
        let b = p.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
            let drive = (b[1] as char).to_ascii_uppercase();
            return format!("{drive}:\\{}", p[3..].replace('/', "\\"));
        }
    }
    p.to_string()
}

/// Returns true if the error string looks like a Docker daemon connectivity problem
/// (not running, wrong pipe/socket, hung, etc.) rather than a build/runtime error.
pub fn is_docker_connectivity_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("client error (connect)")
        || m.contains("hyper legacy client")
        || m.contains("broken pipe")
        || m.contains("connection reset")
        || m.contains("os error 2")   // file not found (pipe/socket missing)
        || m.contains("os error 32")  // broken pipe
        || m.contains("os error 111") // connection refused (Linux)
        || m.contains("no such file or directory")
            && (m.contains("pipe") || m.contains("docker.sock"))
}

/// Advice to show the user when Docker connectivity fails.
pub const DOCKER_CONNECTIVITY_ADVICE: &str = "\
This looks like a Docker issue, not a nemesis8 issue. Try:
  1. Restart Docker Desktop
  2. Restart your computer
  3. Update Docker Desktop to the latest version (docker.com/products/docker-desktop)";

/// True if the build output shows apt rejecting the Debian package index because
/// the container VM's clock is behind real time — the "Release is not valid yet"
/// failure that hits Podman/Docker machines after the host (esp. a Mac) sleeps.
pub fn is_clock_skew_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("not valid yet") || (t.contains("invalid for another") && t.contains("release"))
}

/// Advice when a build fails on VM clock skew.
pub const CLOCK_SKEW_ADVICE: &str = "\
The container runtime's VM clock is behind real time, so apt rejected the Debian
package index as \"not valid yet\". This usually happens after the host (esp. a
Mac) sleeps and the lightweight VM doesn't resync. Fix the clock and re-run build:

  Podman:          podman machine stop && podman machine start
                   (if it persists: podman machine ssh \"sudo date -s '$(date -u '+%Y-%m-%d %H:%M:%S')'\")
  Docker Desktop:  quit and reopen Docker Desktop (Settings -> Troubleshoot -> Restart)";

/// On Windows, find the correct Docker named pipe by reading the active context
/// from ~/.docker/config.json, then looking up the host in the context meta.json.
/// Falls back to dockerDesktopLinuxEngine, then docker_engine.
#[cfg(windows)]
fn detect_windows_docker_pipe() -> String {
    // DOCKER_HOST env var takes priority
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if let Some(pipe) = host.strip_prefix("npipe://") {
            return pipe.to_string();
        }
    }
    // CONTAINER_HOST is podman's equivalent
    if let Ok(host) = std::env::var("CONTAINER_HOST") {
        if let Some(pipe) = host.strip_prefix("npipe://") {
            return pipe.to_string();
        }
    }

    // Try to read active context from ~/.docker/config.json
    if let Some(pipe) = read_docker_context_pipe() {
        return pipe;
    }

    // No Docker context. If Podman is the installed runtime (Docker isn't),
    // point at Podman's machine pipe instead of guessing the Docker Desktop one.
    if !win_cli_present("docker") && win_cli_present("podman") {
        return "//./pipe/podman-machine-default".to_string();
    }

    // Default for Docker Desktop Linux engine
    "//./pipe/dockerDesktopLinuxEngine".to_string()
}

/// True if a CLI responds to `--version` (installed + on PATH). Used to bias the
/// Windows pipe choice toward whichever runtime is actually present.
#[cfg(windows)]
fn win_cli_present(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read the Docker context host from ~/.docker/contexts/meta/*/meta.json
#[cfg(windows)]
fn read_docker_context_pipe() -> Option<String> {
    let home = dirs::home_dir()?;

    // Find the active context name
    let config_path = home.join(".docker").join("config.json");
    let config_data = std::fs::read_to_string(config_path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&config_data).ok()?;
    let context_name = config.get("currentContext")?.as_str()?.to_string();

    // Scan context meta directories for a matching Name
    let meta_dir = home.join(".docker").join("contexts").join("meta");
    for entry in std::fs::read_dir(meta_dir).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let meta_path = entry.path().join("meta.json");
        let meta_data = std::fs::read_to_string(&meta_path).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&meta_data).ok()?;
        if meta.get("Name").and_then(|n| n.as_str()) != Some(&context_name) {
            continue;
        }
        // Extract the Docker host ("npipe:////./pipe/dockerDesktopLinuxEngine")
        let host = meta
            .get("Endpoints")?
            .get("docker")?
            .get("Host")?
            .as_str()?;
        // Strip "npipe://" prefix for bollard ("//./pipe/...")
        if let Some(pipe) = host.strip_prefix("npipe://") {
            return Some(pipe.to_string());
        }
    }
    None
}

/// Ask the podman CLI for the running machine's host-side socket path. Works
/// across providers (qemu / applehv) and podman versions — no path guessing.
/// Returns None if podman isn't installed, no machine is running, or the
/// reported socket doesn't exist yet.
#[cfg(not(windows))]
fn podman_machine_socket() -> Option<String> {
    let out = std::process::Command::new("podman")
        .args([
            "machine",
            "inspect",
            "--format",
            "{{.ConnectionInfo.PodmanSocket.Path}}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // With multiple machines, inspect prints one path per line; take the first
    // that actually exists on disk (the running machine's socket).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|p| !p.is_empty() && std::path::Path::new(p).exists())
        .map(|p| p.to_string())
}

/// Detect which container socket to use and which runtime it belongs to.
/// Returns (socket_uri, runtime_binary) e.g. ("unix:///...", "docker") or ("unix:///...", "podman").
#[cfg(not(windows))]
pub fn detect_container_socket() -> (String, &'static str) {
    // $CONTAINER_HOST takes priority for Podman
    if let Ok(host) = std::env::var("CONTAINER_HOST") {
        return (host, "podman");
    }
    // $DOCKER_HOST takes priority for Docker
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        return (host, "docker");
    }

    // Docker Desktop macOS socket
    if let Some(sock) = dirs::home_dir()
        .map(|h| h.join(".docker/run/docker.sock"))
        .filter(|p| p.exists())
    {
        return (format!("unix://{}", sock.display()), "docker");
    }

    // Standard Docker socket (Linux + fallback)
    if std::path::Path::new("/var/run/docker.sock").exists() {
        return ("unix:///var/run/docker.sock".to_string(), "docker");
    }

    // Podman rootless socket via $XDG_RUNTIME_DIR (Linux systemd)
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let sock = format!("{xdg}/podman/podman.sock");
        if std::path::Path::new(&sock).exists() {
            return (format!("unix://{sock}"), "podman");
        }
    }

    // Podman machine — ask podman for the real socket path. This is the robust
    // way on macOS: it handles BOTH providers (qemu and applehv on Apple
    // Silicon) and any podman version, instead of guessing hardcoded paths
    // (the old guesses missed applehv, so a running machine looked absent).
    if let Some(sock) = podman_machine_socket() {
        return (format!("unix://{sock}"), "podman");
    }

    // Podman machine socket (macOS) — hardcoded fallbacks if the podman CLI
    // isn't on PATH but a socket exists.
    if let Some(home) = dirs::home_dir() {
        for candidate in [
            home.join(".local/share/containers/podman/machine/applehv/podman.sock"),
            home.join(".local/share/containers/podman/machine/qemu/podman.sock"),
            home.join(".local/share/containers/podman/machine/podman-machine-default/podman.sock"),
        ] {
            if candidate.exists() {
                return (format!("unix://{}", candidate.display()), "podman");
            }
        }
    }

    // Podman rootful socket
    if std::path::Path::new("/run/podman/podman.sock").exists() {
        return ("unix:///run/podman/podman.sock".to_string(), "podman");
    }

    // Final fallback — will fail at first API call if nothing is listening
    ("unix:///var/run/docker.sock".to_string(), "docker")
}

/// Return the name of the container runtime binary ("docker" or "podman").
/// Useful in contexts where DockerOps is not available (e.g., MCP pip install).
pub fn detect_runtime_binary() -> &'static str {
    #[cfg(not(windows))]
    {
        let (_, runtime) = detect_container_socket();
        // detect_container_socket returns a &'static str already
        runtime
    }
    #[cfg(windows)]
    {
        // On Windows, check for podman pipe; fall back to docker
        let pipe = detect_windows_docker_pipe();
        if pipe.contains("podman") {
            "podman"
        } else {
            "docker"
        }
    }
}

/// ExposedPorts map mirroring a HostConfig's port bindings — the Docker API
/// wants bound container ports also declared exposed on the container config.
fn exposed_ports_from(
    host_config: &HostConfig,
) -> Option<std::collections::HashMap<String, std::collections::HashMap<(), ()>>> {
    host_config
        .port_bindings
        .as_ref()
        .filter(|pb| !pb.is_empty())
        .map(|pb| pb.keys().map(|k| (k.clone(), Default::default())).collect())
}

/// Resolve a GitHub token from the host so the container's gh/git can act as the
/// locally-logged-in user. Prefers an explicit env token; otherwise asks the gh
/// CLI (`gh auth token`), which covers the common "I ran gh auth login" case.
/// Returns None when nothing is logged in (GitHub access simply stays off).
fn resolve_github_token() -> Option<String> {
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tok.is_empty() { None } else { Some(tok) }
}

/// Docker/Podman operations for nemesis8
#[derive(Clone)]
pub struct DockerOps {
    docker: Docker,
    image: String,
    /// The container runtime binary name ("docker" or "podman").
    pub runtime_binary: String,
    /// Pass host GPUs through to containers (docker --gpus all). Resolved once
    /// after connect (only when --gpu is requested AND the image supports it).
    gpu: bool,
}

impl DockerOps {
    /// Connect to the container daemon (Docker or Podman).
    pub fn new(image_tag: Option<&str>) -> Result<Self> {
        // Use a long timeout (30 min) because image builds have long gaps
        // between output lines (apt-get, pip install, cargo build, etc.).
        #[cfg(windows)]
        let (docker, runtime_binary) = {
            let pipe = detect_windows_docker_pipe();
            tracing::debug!(pipe = %pipe, "connecting to container daemon");
            // Try Docker pipe first; fall back to Podman machine pipe
            let (docker, runtime) =
                Docker::connect_with_named_pipe(&pipe, 1800, &bollard::API_DEFAULT_VERSION)
                    .map(|d| {
                        (
                            d,
                            if pipe.contains("podman") {
                                "podman"
                            } else {
                                "docker"
                            },
                        )
                    })
                    .or_else(|_| {
                        // Try Podman for Windows named pipe
                        Docker::connect_with_named_pipe(
                            "//./pipe/podman-machine-default",
                            1800,
                            &bollard::API_DEFAULT_VERSION,
                        )
                        .map(|d| (d, "podman"))
                    })
                    .context("connecting to container daemon")?;
            (docker, runtime.to_string())
        };

        #[cfg(not(windows))]
        let (docker, runtime_binary) = {
            let (socket_uri, runtime) = detect_container_socket();
            tracing::debug!(socket = %socket_uri, runtime = %runtime, "connecting to container daemon");
            let docker =
                Docker::connect_with_local(&socket_uri, 1800, &bollard::API_DEFAULT_VERSION)
                    .or_else(|_| Docker::connect_with_local_defaults())
                    .context("connecting to container daemon")?;
            (docker, runtime.to_string())
        };

        Ok(Self {
            docker,
            image: image_tag.unwrap_or(DEFAULT_IMAGE).to_string(),
            runtime_binary,
            gpu: false,
        })
    }

    /// Enable GPU passthrough for containers this instance creates. Set true only
    /// after confirming the image supports it (see [`Self::image_has_gpu`]).
    pub fn set_gpu(&mut self, gpu: bool) {
        self.gpu = gpu;
    }

    /// Get a reference to the underlying bollard Docker client
    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// Check if the configured image exists locally
    pub async fn image_exists(&self) -> bool {
        self.docker.inspect_image(&self.image).await.is_ok()
    }

    /// Return the image tag name
    pub fn image_name(&self) -> &str {
        &self.image
    }

    /// Ensure the shared Docker network used by nemesis8 agent containers and
    /// the Gnosis sidecar services (opensearch, transcription, etc.) exists.
    /// Creates it on first run so users don't have to remember to.
    pub async fn ensure_network(&self) -> Result<()> {
        use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
        use std::collections::HashMap;

        let mut filters: HashMap<&str, Vec<&str>> = HashMap::new();
        filters.insert("name", vec![DEFAULT_NETWORK]);
        let existing = self
            .docker
            .list_networks(Some(ListNetworksOptions { filters }))
            .await
            .context("listing docker networks")?;

        // Filter is a substring match — confirm an exact name hit.
        if existing
            .iter()
            .any(|n| n.name.as_deref() == Some(DEFAULT_NETWORK))
        {
            return Ok(());
        }

        tracing::info!(network = DEFAULT_NETWORK, "creating shared docker network");
        self.docker
            .create_network(CreateNetworkOptions {
                name: DEFAULT_NETWORK,
                driver: "bridge",
                ..Default::default()
            })
            .await
            .context("creating docker network")?;

        Ok(())
    }

    /// List running containers created by nemesis8
    pub async fn list_containers(
        &self,
        _image: &str,
    ) -> Result<Vec<bollard::models::ContainerSummary>> {
        use bollard::container::ListContainersOptions;

        // List all containers, then filter by legacy/current image names or labels.
        let all = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await
            .context("listing containers")?;

        let containers: Vec<_> = all
            .into_iter()
            .filter(|c| {
                // Preferred: the agent label (reliable, name-independent).
                let has_label = c
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(LABEL_AGENT))
                    .map(|v| v == "true")
                    .unwrap_or(false);
                if has_label {
                    return true;
                }
                // Fallback: legacy image/name/command matching so containers
                // started by older (unlabeled) binaries are still discovered.
                let img = c.image.as_deref().unwrap_or("");
                img.contains("nemesis8")
                    || img.contains("nemesis8")
                    || c.names.as_ref().is_some_and(|names| {
                        names
                            .iter()
                            .any(|n| n.contains("nemesis8") || n.contains("nemesis8"))
                    })
                    || c.command
                        .as_deref()
                        .unwrap_or("")
                        .contains("nemesis8-entry")
            })
            .collect();

        Ok(containers)
    }

    /// Find an existing container (running or stopped) by its nemesis8.session_id label
    pub async fn find_container_by_session(
        &self,
        session_id: &str,
    ) -> Result<Option<bollard::models::ContainerSummary>> {
        use bollard::container::ListContainersOptions;
        use std::collections::HashMap;

        let all = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters: {
                    let mut f = HashMap::new();
                    f.insert("label".to_string(), vec![format!("{LABEL_SESSION_ID}={session_id}")]);
                    f
                },
                ..Default::default()
            }))
            .await
            .context("finding container by session id")?;

        Ok(all.first().cloned())
    }

    /// Best-effort last non-empty, ANSI-stripped log line for a container
    /// (stdout+stderr). Empty string on any error. Used by the unified
    /// attach/resume picker to show what each running agent was last doing.
    pub async fn last_log_line(&self, id: &str) -> String {
        use bollard::container::LogsOptions;
        let opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: "5".to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.logs(id, Some(opts));
        let mut last = String::new();
        while let Some(Ok(out)) = stream.next().await {
            let chunk = out.to_string();
            for line in chunk.lines() {
                let cleaned = Self::strip_ansi(line);
                let t = cleaned.trim();
                if !t.is_empty() {
                    last = t.to_string();
                }
            }
        }
        last
    }

    /// Strip ANSI escape sequences and control chars from a line.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Consume the escape sequence up to its terminating letter.
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if !c.is_control() {
                out.push(c);
            }
        }
        out
    }

    /// Stop and remove a container by name or ID
    pub async fn stop_container(&self, name_or_id: &str) -> Result<()> {
        self.docker.stop_container(name_or_id, None).await.ok();
        self.docker.remove_container(name_or_id, None).await.ok();
        Ok(())
    }

    // ── service orchestration (M1) ──────────────────────────────────────────

    /// Ensure a dependency service is running on the shared network, idempotently.
    /// If a healthy container with the service's label already exists, returns it
    /// untouched; otherwise pulls/builds the image, (re)creates the container with
    /// the template's ports/env/volumes/labels/restart/healthcheck, starts it, and
    /// — when a health probe is declared — waits for it to report healthy.
    pub async fn ensure_service(
        &self,
        spec: &crate::service_def::ServiceSpec,
    ) -> Result<ServiceStatus> {
        spec.validate().map_err(|e| anyhow::anyhow!(e))?;
        self.ensure_network().await?;

        // Already present (our label or the same name)? Reuse if running; clean
        // up if not. A running container started outside n8 is adopted, not stomped.
        if let Some(existing) = self.find_service_container(&spec.name).await? {
            let state = existing.state.clone().unwrap_or_default();
            let id = existing.id.clone().unwrap_or_default();
            if state == "running" {
                if !Self::is_managed_service(&existing) {
                    tracing::info!(service = %spec.name, "adopting externally-managed service (already running)");
                }
                let health = self.container_health(&id).await;
                return Ok(ServiceStatus {
                    name: spec.name.clone(),
                    id,
                    state,
                    health,
                });
            }
            // Created/exited/dead — remove so we can recreate cleanly.
            self.docker
                .remove_container(
                    &id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .ok();
        }

        // Acquire the image: pull a registry ref or build from context.
        let image = match (&spec.image, &spec.build) {
            (Some(img), _) => {
                self.pull_image(img).await?;
                img.clone()
            }
            (_, Some(b)) => self.build_service_image(&spec.name, b)?,
            _ => unreachable!("validate() guarantees image xor build"),
        };

        let host_config = HostConfig {
            binds: if spec.volumes.is_empty() {
                None
            } else {
                Some(spec.volumes.clone())
            },
            network_mode: Some(spec.network.clone()),
            port_bindings: port_binding_map(&spec.ports),
            restart_policy: restart_policy(&spec.restart),
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            ..Default::default()
        };

        let container_config = ContainerConfig {
            image: Some(image.clone()),
            cmd: if spec.command.is_empty() {
                None
            } else {
                Some(spec.command.clone())
            },
            env: if spec.env.is_empty() {
                None
            } else {
                Some(spec.env.clone())
            },
            exposed_ports: exposed_ports_from(&host_config),
            host_config: Some(host_config),
            labels: Some(service_labels(&spec.name)),
            healthcheck: spec.health.as_ref().map(healthcheck_from),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: spec.name.as_str(),
            platform: None,
        };
        let container = self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
            .with_context(|| format!("creating service '{}'", spec.name))?;
        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("starting service '{}'", spec.name))?;

        let health = match spec.health.as_ref() {
            Some(h) => self.wait_healthy(&container.id, h).await,
            None => "none".to_string(),
        };

        Ok(ServiceStatus {
            name: spec.name.clone(),
            id: container.id,
            state: "running".to_string(),
            health,
        })
    }

    /// List every managed service container (any state), label-filtered.
    pub async fn list_services(&self) -> Result<Vec<ServiceStatus>> {
        use bollard::container::ListContainersOptions;
        let mut filters = std::collections::HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_SERVICE}=true")]);
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .context("listing services")?;

        let mut out = Vec::new();
        for c in list {
            let id = c.id.clone().unwrap_or_default();
            let name = c
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_SERVICE_NAME))
                .cloned()
                .or_else(|| {
                    c.names
                        .as_ref()
                        .and_then(|n| n.first())
                        .map(|n| n.trim_start_matches('/').to_string())
                })
                .unwrap_or_default();
            let state = c.state.clone().unwrap_or_default();
            let health = self.container_health(&id).await;
            out.push(ServiceStatus {
                name,
                id,
                state,
                health,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Stop + remove a managed service by name. Returns false if not found.
    pub async fn stop_service(&self, name: &str) -> Result<bool> {
        match self.find_service_container(name).await? {
            Some(c) => {
                if let Some(id) = c.id.as_deref() {
                    self.docker
                        .stop_container(
                            id,
                            Some(bollard::container::StopContainerOptions { t: 10 }),
                        )
                        .await
                        .ok();
                    self.docker
                        .remove_container(
                            id,
                            Some(RemoveContainerOptions {
                                force: true,
                                ..Default::default()
                            }),
                        )
                        .await
                        .ok();
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Find a service container (any state) by our `nemesis8.service_name` label
    /// OR an exact container-name match — so a service started outside n8 (compose,
    /// by hand) is reconciled/adopted instead of colliding on the name.
    async fn find_service_container(
        &self,
        name: &str,
    ) -> Result<Option<bollard::models::ContainerSummary>> {
        use bollard::container::ListContainersOptions;
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await
            .context("finding service container")?;
        Ok(list.into_iter().find(|c| {
            let by_label = c
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_SERVICE_NAME))
                .map(|v| v == name)
                .unwrap_or(false);
            let by_name = c
                .names
                .as_ref()
                .map(|ns| ns.iter().any(|n| n.trim_start_matches('/') == name))
                .unwrap_or(false);
            by_label || by_name
        }))
    }

    /// True if the container carries our service label (vs. an externally-managed
    /// one we only adopted by name).
    fn is_managed_service(c: &bollard::models::ContainerSummary) -> bool {
        c.labels
            .as_ref()
            .and_then(|l| l.get(LABEL_SERVICE))
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    /// Current health (or run state when no healthcheck) of a container.
    async fn container_health(&self, id: &str) -> String {
        use bollard::models::HealthStatusEnum;
        match self.docker.inspect_container(id, None).await {
            Ok(info) => {
                if let Some(status) = info
                    .state
                    .as_ref()
                    .and_then(|s| s.health.as_ref())
                    .and_then(|h| h.status)
                {
                    return match status {
                        HealthStatusEnum::HEALTHY => "healthy",
                        HealthStatusEnum::UNHEALTHY => "unhealthy",
                        HealthStatusEnum::STARTING => "starting",
                        HealthStatusEnum::NONE => "none",
                        _ => "unknown",
                    }
                    .to_string();
                }
                // No healthcheck → report the run state.
                info.state
                    .as_ref()
                    .and_then(|s| s.status)
                    .map(|st| format!("{st:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".to_string())
            }
            Err(_) => "unknown".to_string(),
        }
    }

    /// Poll the container's Docker healthcheck until healthy/unhealthy or the
    /// template's retry budget is spent. Returns the final health string.
    async fn wait_healthy(&self, id: &str, h: &crate::service_def::HealthSpec) -> String {
        let iters = h.retries + 2;
        for _ in 0..iters {
            match self.container_health(id).await.as_str() {
                "healthy" => return "healthy".to_string(),
                "unhealthy" => return "unhealthy".to_string(),
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(h.interval_secs.max(1))).await;
        }
        self.container_health(id).await
    }

    /// Pull a registry image if it isn't present locally (streams to completion).
    async fn pull_image(&self, image: &str) -> Result<()> {
        use bollard::image::CreateImageOptions;
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        tracing::info!(image, "pulling service image");
        let opts = CreateImageOptions {
            from_image: image,
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("pulling image '{image}'"))?;
        }
        Ok(())
    }

    /// Build a service image from its context via the runtime CLI; returns the tag.
    fn build_service_image(&self, name: &str, b: &crate::service_def::BuildSpec) -> Result<String> {
        let tag = format!("nemesis8-svc-{name}:latest");
        let context = std::path::PathBuf::from(&b.context);
        let mut args = vec!["build".to_string(), "-t".to_string(), tag.clone()];
        if let Some(df) = &b.dockerfile {
            args.push("-f".to_string());
            args.push(context.join(df).display().to_string());
        }
        args.push(context.display().to_string());
        tracing::info!(service = name, context = %context.display(), "building service image");
        let status = std::process::Command::new(&self.runtime_binary)
            .args(&args)
            .status()
            .with_context(|| format!("building service '{name}'"))?;
        if !status.success() {
            anyhow::bail!("docker build failed for service '{name}'");
        }
        Ok(tag)
    }

    /// Build the Docker image from the project directory.
    /// Uses a ratatui TUI progress bar when stdout is a terminal,
    /// falls back to raw output when piped.
    pub async fn build(
        &self,
        context_dir: &Path,
        extra_args: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            dir = %context_dir.display(),
            image = %self.image,
            "building Docker image"
        );

        if ui::is_interactive() {
            self.build_tui(context_dir, extra_args).await
        } else {
            self.build_raw(context_dir, extra_args).await
        }
    }

    /// Build with JSON progress lines on stdout (for Hyperia integration)
    pub async fn build_json_progress(
        &self,
        context_dir: &Path,
        extra_args: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            dir = %context_dir.display(),
            image = %self.image,
            "building Docker image (json-progress)"
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = self.runtime_binary.clone();
        let image = self.image.clone();
        let context_dir = context_dir.to_path_buf();

        let build_handle = tokio::spawn(async move {
            let (err, _lines) =
                run_build_cli(runtime, image, context_dir, extra_args, tx.clone()).await;
            if err.is_none() {
                let _ = tx.send(BuildEvent::Done);
            }
        });

        // Consume events and print JSON lines
        let start = std::time::Instant::now();
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Some(BuildEvent::Step {
                    current,
                    total: t,
                    message,
                }) => {
                    let percent = if t > 0 { (current * 100) / t } else { 0 };
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "step",
                            "step": current,
                            "total": t,
                            "message": message,
                            "percent": percent,
                        })
                    );
                }
                Some(BuildEvent::Log(line)) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "log",
                            "line": line,
                        })
                    );
                }
                Some(BuildEvent::Done) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "done",
                            "elapsed_secs": start.elapsed().as_secs(),
                            "success": true,
                        })
                    );
                    break;
                }
                Some(BuildEvent::Error(msg)) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "error",
                            "message": msg,
                        })
                    );
                    break;
                }
                None => break,
            }
        }

        let _ = build_handle.await;
        Ok(())
    }

    /// Build with ratatui progress display
    async fn build_tui(
        &self,
        context_dir: &Path,
        extra_args: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = self.runtime_binary.clone();
        let image = self.image.clone();
        let context_dir = context_dir.to_path_buf();

        // Build by shelling out to the runtime CLI; stream its output into the
        // TUI via the event channel.
        let build_handle = tokio::spawn(async move {
            let (build_error, log_lines) =
                run_build_cli(runtime, image, context_dir, extra_args, tx.clone()).await;
            if build_error.is_none() {
                let _ = tx.send(BuildEvent::Done);
            }
            (build_error, log_lines)
        });

        // Run TUI on the current task (owns the terminal)
        ui::run_build_progress(rx).await?;

        // Check build result
        let (build_error, log_lines) = build_handle.await.context("build task panicked")?;
        if let Some(err) = build_error {
            // Save full log to ~/.nemesis8/build.log for post-mortem debugging
            let log_path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".nemesis8")
                .join("build.log");
            if let Ok(mut f) = std::fs::File::create(&log_path) {
                use std::io::Write;
                for line in &log_lines {
                    let _ = writeln!(f, "{line}");
                }
            }
            let msg = err.to_string();
            if is_docker_connectivity_error(&msg) {
                anyhow::bail!(
                    "Lost connection to Docker during build: {err}\n\n{}",
                    DOCKER_CONNECTIVITY_ADVICE
                );
            }
            // Clock skew shows up in the apt step's log lines, not the top error.
            let haystack = format!("{msg}\n{}", log_lines.join("\n"));
            if is_clock_skew_error(&haystack) {
                eprintln!("\nFull build log saved to: {}", log_path.display());
                anyhow::bail!("Build failed: container VM clock is behind.\n\n{CLOCK_SKEW_ADVICE}");
            }
            eprintln!("\nFull build log saved to: {}", log_path.display());
            anyhow::bail!("Docker build error: {err}");
        }

        tracing::info!(image = %self.image, "image built successfully");
        Ok(())
    }

    /// Build with output inherited to the terminal (piped / non-TTY contexts),
    /// by shelling out to the runtime CLI (`docker` / `podman` build).
    async fn build_raw(
        &self,
        context_dir: &Path,
        extra_args: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let ts = chrono::Utc::now().timestamp().to_string();
        let mut args = extra_args;
        args.insert("CACHE_BUST".to_string(), ts);

        let mut cmd = tokio::process::Command::new(&self.runtime_binary);
        cmd.arg("build")
            .arg("-t")
            .arg(&self.image)
            .arg("-f")
            .arg(context_dir.join("Dockerfile"));
        for (k, v) in &args {
            cmd.arg("--build-arg").arg(format!("{k}={v}"));
        }
        cmd.arg(context_dir);

        let status = cmd
            .status()
            .await
            .with_context(|| format!("running `{} build`", self.runtime_binary))?;
        if !status.success() {
            anyhow::bail!(
                "{} build exited with code {}",
                self.runtime_binary,
                status.code().unwrap_or(-1)
            );
        }
        tracing::info!(image = %self.image, "image built successfully");
        Ok(())
    }

    /// Run a one-shot prompt in a container
    pub async fn run(
        &self,
        config: &Config,
        prompt: &str,
        danger: bool,
        privileged: bool,
        model: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        let container_name = crate::names::fun_name();
        let mut env = self.build_env(config, danger, model, session_id);
        // Agent id == container name == the agent_id label, matching run_capture
        // and giving in-container tools a stable way to address this agent.
        env.push(format!("NEMESIS8_AGENT_ID={container_name}"));

        let mut cmd = vec!["nemesis8-entry".to_string()];
        cmd.push("--prompt".to_string());
        cmd.push(prompt.to_string());
        if danger {
            cmd.push("--danger".to_string());
        }

        // Charon consumer-proxy sidecar (opt-in). When enabled, this brings up a
        // per-session internal network + `charon consumer` proxy; we then pin the
        // agent onto that network so it can reach only the proxy. No-op (None)
        // otherwise. Held to session end; torn down below (Drop backstops errors).
        let mut charon =
            crate::charon::CharonSidecar::maybe_start(&self.docker, &self.runtime_binary, &container_name, config)
                .await?;

        let mut host_config = self.build_host_config(config, privileged, workspace, &container_name);
        if let Some(c) = &charon {
            host_config.network_mode = Some(c.network.clone());
            env.push(format!("NEMESIS8_CHARON_PROXY={}", c.endpoint()));
        }

        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(cmd),
            env: Some(env),
            exposed_ports: exposed_ports_from(&host_config),
            host_config: Some(host_config),
            labels: Some(agent_labels(&config.provider.to_string(), &container_name, session_id, workspace)),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: container_name.as_str(),
            platform: None,
        };

        let container = self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
            .context("creating container")?;

        tracing::info!(id = %container.id, name = %container_name, "container created");

        // Spawn Ctrl-C watcher
        let docker_clone = self.docker.clone();
        let cid = container.id.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("\nInterrupted — stopping container {}...", &cid[..12]);
            let stop_opts = bollard::container::StopContainerOptions { t: 5 };
            docker_clone
                .stop_container(&cid, Some(stop_opts))
                .await
                .ok();
        });

        // Attach BEFORE starting so we don't miss early output
        let attach_opts = AttachContainerOptions::<String> {
            stdout: Some(true),
            stderr: Some(true),
            stream: Some(true),
            ..Default::default()
        };
        let mut output = self
            .docker
            .attach_container(&container.id, Some(attach_opts))
            .await
            .context("attaching to container")?;

        // Now start the container
        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("starting container")?;

        // Stream output
        let mut stdout = tokio::io::stdout();
        while let Some(Ok(log)) = output.output.next().await {
            match log {
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    stdout.write_all(&message).await.ok();
                    stdout.flush().await.ok();
                }
                LogOutput::StdErr { message } => {
                    // Suppress known Codex internal session noise that isn't actionable
                    let s = String::from_utf8_lossy(&message);
                    if s.contains("failed to record rollout items") {
                        continue;
                    }
                    stdout.write_all(&message).await.ok();
                    stdout.flush().await.ok();
                }
                _ => {}
            }
        }

        // Wait for exit
        let mut wait_stream = self
            .docker
            .wait_container(&container.id, None::<WaitContainerOptions<String>>);

        let mut exit_code = 0i64;
        while let Some(result) = wait_stream.next().await {
            match result {
                Ok(response) => {
                    exit_code = response.status_code;
                }
                Err(e) => {
                    tracing::warn!("wait error: {e}");
                }
            }
        }

        // Clean up
        self.docker
            .remove_container(
                &container.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .ok();

        // Tear down the charon sidecar + its network now that the agent is gone
        // (the network can only be removed once nothing is attached).
        if let Some(c) = &mut charon {
            c.teardown(&self.docker).await;
        }

        if exit_code != 0 {
            anyhow::bail!("container exited with code {exit_code}");
        }

        Ok(())
    }

    /// Run a one-shot prompt in a container and capture output as a String.
    /// Used by the gateway and scheduler for non-interactive execution.
    pub async fn run_capture(
        &self,
        config: &Config,
        prompt: &str,
        danger: bool,
        model: Option<&str>,
        workspace: Option<&str>,
        session_id: Option<&str>,
        timeout_secs: u64,
        gateway_url: Option<&str>,
        auth_token: Option<&str>,
    ) -> Result<String> {
        let container_name = crate::names::fun_name();
        let mut env = self.build_env(config, danger, model, session_id);
        if let Some(url) = gateway_url {
            env.push(format!("GATEWAY_URL={url}"));
        }
        if let Some(token) = auth_token {
            env.push(format!("NEMESIS8_AUTH_TOKEN={token}"));
        }
        // Agent id == container name == the agent_id label, so the entry
        // binary self-registers under the same id the registry discovers.
        env.push(format!("NEMESIS8_AGENT_ID={container_name}"));

        let mut cmd = vec!["nemesis8-entry".to_string()];
        cmd.push("--prompt".to_string());
        cmd.push(prompt.to_string());
        if danger {
            cmd.push("--danger".to_string());
        }

        // Charon consumer-proxy sidecar (opt-in) — see run() above. No-op when
        // disabled. Torn down after the run completes / times out.
        let mut charon =
            crate::charon::CharonSidecar::maybe_start(&self.docker, &self.runtime_binary, &container_name, config)
                .await?;

        let mut host_config = self.build_host_config(config, false, workspace, &container_name);
        if let Some(c) = &charon {
            host_config.network_mode = Some(c.network.clone());
            env.push(format!("NEMESIS8_CHARON_PROXY={}", c.endpoint()));
        }

        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(cmd),
            env: Some(env),
            exposed_ports: exposed_ports_from(&host_config),
            host_config: Some(host_config),
            labels: Some(agent_labels(&config.provider.to_string(), &container_name, session_id, workspace)),
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: container_name.as_str(),
            platform: None,
        };

        let container = self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
            .context("creating container")?;

        // Attach before starting
        let attach_opts = AttachContainerOptions::<String> {
            stdout: Some(true),
            stderr: Some(true),
            stream: Some(true),
            ..Default::default()
        };
        let mut output = self
            .docker
            .attach_container(&container.id, Some(attach_opts))
            .await
            .context("attaching to container")?;

        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("starting container")?;

        // Capture output with timeout
        let mut captured = Vec::new();
        let collect = async {
            while let Some(Ok(log)) = output.output.next().await {
                match log {
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        captured.extend_from_slice(&message);
                    }
                    LogOutput::StdErr { message } => {
                        let s = String::from_utf8_lossy(&message);
                        if !s.contains("failed to record rollout items") {
                            captured.extend_from_slice(&message);
                        }
                    }
                    _ => {}
                }
            }
        };

        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), collect)
            .await
            .is_err();

        if timed_out {
            // Kill container on timeout
            let stop_opts = bollard::container::StopContainerOptions { t: 3 };
            self.docker
                .stop_container(&container.id, Some(stop_opts))
                .await
                .ok();
        }

        // Wait for exit
        let mut wait_stream = self
            .docker
            .wait_container(&container.id, None::<WaitContainerOptions<String>>);
        let mut exit_code = 0i64;
        while let Some(result) = wait_stream.next().await {
            if let Ok(response) = result {
                exit_code = response.status_code;
            }
        }

        // Clean up
        self.docker
            .remove_container(
                &container.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .ok();

        // Tear down the charon sidecar + its network (agent is gone).
        if let Some(c) = &mut charon {
            c.teardown(&self.docker).await;
        }

        if timed_out {
            anyhow::bail!("container timed out after {timeout_secs}s");
        }

        let text = String::from_utf8_lossy(&captured).to_string();
        if exit_code != 0 {
            anyhow::bail!("container exited with code {exit_code}: {text}");
        }

        Ok(text)
    }

    /// Consume self, closing the bollard connection, and return login args
    /// for running `docker run -it` for the login flow.
    pub fn into_login_args(self, config: &Config) -> Result<Vec<String>> {
        let mut env = self.build_env(config, false, None, None);

        let codex_home = crate::paths::data_home();
        let codex_home_docker = to_docker_path(&codex_home.display().to_string());

        // Ensure the directory exists on host
        std::fs::create_dir_all(&codex_home).ok();

        // Everything provider-specific below comes from the provider's TOML —
        // no hardcoded per-provider login logic here.
        let registry = crate::provider_registry::ProviderRegistry::load();
        let def = registry.get(&config.provider.0).cloned();

        // Sync the provider's auth files from the host config dir into the
        // data-home volume (TOML hooks.auth_files_sync, e.g. gemini's OAuth
        // creds — its FileKeychain doesn't persist in containers, so the
        // host's copies are injected every launch). Always overwrite.
        if let Some(ref d) = def {
            let files = &d.provider.hooks.auth_files_sync;
            if !files.is_empty() {
                if let Some(home) = dirs::home_dir() {
                    let host_dir = home.join(&d.provider.config_dir.path);
                    let svc_dir = codex_home.join(&d.provider.config_dir.path);
                    std::fs::create_dir_all(&svc_dir).ok();
                    for file in files {
                        let src = host_dir.join(file);
                        let dst = svc_dir.join(file);
                        if src.is_file() && std::fs::copy(&src, &dst).is_ok() {
                            tracing::info!("synced {file} to container volume");
                        }
                    }
                }
            }
        }

        // Login command + env + callback ports from TOML [provider.login].
        let login = def
            .as_ref()
            .map(|d| d.provider.login.clone())
            .unwrap_or_default();
        for ev in &login.env_vars {
            env.push(ev.clone());
        }
        let login_cmd = login.command.clone().unwrap_or_else(|| {
            format!(
                r#"echo "[nemesis8] No login required for provider '{}'.""#,
                config.provider.0
            )
        });

        let mut args = vec![
            "run".to_string(),
            "-it".to_string(),
            "--rm".to_string(),
            format!("--network={DEFAULT_NETWORK}"),
            "--add-host=host.docker.internal:host-gateway".to_string(),
            format!("-v={codex_home_docker}:/opt/nemesis8:rw"),
        ];
        for p in &login.ports {
            args.push(format!("-p={p}"));
        }

        for e in &env {
            args.push(format!("-e={e}"));
        }

        args.push(self.image.clone());
        args.push("/bin/bash".to_string());
        args.push("-lc".to_string());
        args.push(login_cmd);

        // self is dropped here — bollard connection closed
        Ok(args)
    }

    // ── helpers ──

    pub fn build_env(
        &self,
        config: &Config,
        danger: bool,
        model: Option<&str>,
        session_id: Option<&str>,
    ) -> Vec<String> {
        let mut env = config.container_env();

        // Tell the entry binary which provider to use
        env.push(format!("NEMESIS8_PROVIDER={}", config.provider));

        // Pass the resolved config as JSON so entry always has it
        if let Ok(json) = serde_json::to_string(config) {
            env.push(format!("NEMESIS8_CONFIG_JSON={json}"));
        }

        // Tell the entry binary the workspace subdirectory name and host path
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(name) = cwd.file_name().and_then(|n| n.to_str()) {
                env.push(format!("NEMESIS8_WORKSPACE=/workspace/{name}"));
            }
            env.push(format!("NEMESIS8_HOST_WORKSPACE={}", cwd.display()));
        }

        // Ensure container has proper terminal color support
        env.push("TERM=xterm-256color".to_string());

        // Forward API keys from host: the union of every registered provider's
        // [provider.api_keys] chain/target (so a new provider TOML automatically
        // gets its keys forwarded), plus generic integration vars.
        let mut keys: Vec<String> = [
            "SERPAPI_API_KEY",
            "ELEVENLABS_API_KEY",
            "TRANSCRIPTION_SERVICE_URL",
            // The `ask` binary MCP server reaches all three providers regardless
            // of which provider the session runs, so forward all of its keys.
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "HYPERIA_URL",
            // Hyperia per-pane auth token (skeleton key for the pane n8 was
            // launched from). Without it the in-container MCP shim hits the
            // sidecar unauthenticated → "No identity" 401 on privileged tools.
            "HYPERIA_AGENT_TOKEN",
            "HYPERIA_PANE",
            "FERRICULA_URL",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let registry = crate::provider_registry::ProviderRegistry::load();
        for def in registry.all() {
            for k in def
                .provider
                .api_keys
                .chain
                .iter()
                .chain(def.provider.api_keys.target.iter())
            {
                if !k.is_empty() && !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
        // Socket-MCP registry: forward each server's bearer-token env so the
        // in-container config-gen can read the value for its Authorization
        // header (#72). Union across all servers (like the provider keys above);
        // only vars actually set on the host are forwarded by the loop below.
        let mcp_registry = crate::mcp_registry::McpRegistry::load();
        for def in mcp_registry.all() {
            if let Some(tok) = &def.server.bearer_token_env {
                if !tok.is_empty() && !keys.contains(tok) {
                    keys.push(tok.clone());
                }
            }
        }
        for key in &keys {
            if let Ok(val) = std::env::var(key) {
                env.push(format!("{key}={val}"));
            }
        }

        // GitHub: forward the locally-logged-in token so the container's gh/git
        // can act as the host user (push, open PRs). Prefers an explicit
        // GH_TOKEN/GITHUB_TOKEN env, else pulls it from the host gh CLI — the
        // usual "I ran gh auth login locally" case. entry.rs runs
        // `gh auth setup-git` when this is present so plain `git push` works too.
        if let Some(tok) = resolve_github_token() {
            env.push(format!("GH_TOKEN={tok}"));
            env.push(format!("GITHUB_TOKEN={tok}"));
        }

        if danger {
            env.push("CODEX_UNSAFE_ALLOW_NO_SANDBOX=1".to_string());
            env.push("CODEX_DANGER_MODE=1".to_string());
        }

        if let Some(m) = model {
            env.push(format!("CODEX_DEFAULT_MODEL={m}"));
        }

        if let Some(sid) = session_id {
            env.push(format!("CODEX_SESSION_ID={sid}"));
        }

        env.push("CODEX_UNSAFE_ALLOW_NO_SANDBOX=1".to_string());

        // Set HOME so Codex finds auth.json in the persistent volume
        env.push("HOME=/opt/nemesis8".to_string());
        env.push("XDG_CONFIG_HOME=/opt/nemesis8".to_string());

        env
    }

    pub fn build_host_config(
        &self,
        config: &Config,
        privileged: bool,
        workspace: Option<&str>,
        session_name: &str,
    ) -> HostConfig {
        let mut binds = config.docker_binds();

        // Per-session dedicated `/workspace` root. Without this, `/workspace`
        // (the parent of the named source mount) is ephemeral container fs, so
        // an agent that `cd ..`s out of the source dir and builds there leaves
        // artifacts we can't see and that vanish on exit. Mount a fresh host dir
        // named after the agent at `/workspace`; the source nests below at
        // `/workspace/<dirname>`. Docker mounts the shallower path first, so
        // bind order here doesn't matter.
        if let Some(base) = config.workspace_root_base() {
            let session_dir = base.join(session_name);
            // GC stale EMPTY `n8-*` mount-target dirs. They're created empty (so an
            // agent can `cd ..` into a sandbox) and otherwise pile up — dozens on a
            // busy box. Only remove ones that are EMPTY, NOT the dir we're about to
            // use, and untouched for >24h, so a live/recent session's mount is never
            // disturbed (empties are harmless clutter; this just keeps it tidy).
            if let Ok(entries) = std::fs::read_dir(&base) {
                let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
                for e in entries.flatten() {
                    let p = e.path();
                    if p == session_dir || !p.is_dir() {
                        continue;
                    }
                    if !e.file_name().to_string_lossy().starts_with("n8-") {
                        continue;
                    }
                    let empty = std::fs::read_dir(&p).map(|mut it| it.next().is_none()).unwrap_or(false);
                    let stale = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .map(|t| t < cutoff)
                        .unwrap_or(false);
                    if empty && stale {
                        let _ = std::fs::remove_dir(&p);
                    }
                }
            }
            match std::fs::create_dir_all(&session_dir) {
                Ok(()) => binds.push(format!(
                    "{}:/workspace:rw",
                    to_docker_path(&session_dir.display().to_string())
                )),
                Err(e) => eprintln!(
                    "[nemesis8] warning: could not create workspace root {}: {e}",
                    session_dir.display()
                ),
            }
        }

        // Convert all config bind mounts to Docker-compatible paths
        binds = binds
            .into_iter()
            .map(|b| {
                // Split "host:container:mode" and convert host path
                let parts: Vec<&str> = b.splitn(3, ':').collect();
                if parts.len() >= 2 {
                    let host = to_docker_path(parts[0]);
                    let rest: Vec<&str> = parts[1..].to_vec();
                    format!("{host}:{}", rest.join(":"))
                } else {
                    b
                }
            })
            .collect();

        // Workspace mount — mount at /workspace/<dirname>
        if let Some(ws_list) = workspace {
            for ws in ws_list.split(',') {
                let ws = ws.trim();
                if ws.is_empty() {
                    continue;
                }
                let docker_ws = to_docker_path(ws);
                let dirname = std::path::Path::new(ws)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("workspace");
                binds.push(format!("{docker_ws}:/workspace/{dirname}:rw"));
            }
        }

        // Data-home volume (persistent across runs) — container HOME
        let data_home = crate::paths::data_home();

        binds.push(format!(
            "{}:/opt/nemesis8:rw",
            to_docker_path(&data_home.display().to_string())
        ));

        // Hyperia drops paste-screenshots into ~/.hyperia/assets on the host.
        // Mount them read-only at /host/paste so the agent can read them via
        // `llm -a /host/paste/<id>.png` — the renderer's pathTranslate
        // rewrites the host path into this namespace on paste. Skip silently
        // if Hyperia isn't installed (no such dir).
        if let Some(home) = dirs::home_dir() {
            let assets = home.join(".hyperia").join("assets");
            if assets.is_dir() {
                binds.push(format!(
                    "{}:/host/paste:ro",
                    to_docker_path(&assets.display().to_string())
                ));
            }
        }

        #[allow(unused_mut)]
        let mut extra_hosts = vec!["host.docker.internal:host-gateway".to_string()];

        // Windows Docker Desktop usually handles this automatically,
        // but we add it explicitly for Linux Docker. DOCKER ONLY: 172.17.0.1
        // is docker0's bridge IP — podman's default bridge is 10.88.0.1, so on
        // podman this line would inject a wrong /etc/hosts entry that can
        // shadow the correct host-gateway mapping above.
        #[cfg(target_os = "linux")]
        if self.runtime_binary == "docker" {
            extra_hosts.push("host.docker.internal:172.17.0.1".to_string());
        }

        // Publish configured ports so servers an agent starts inside the
        // container (dev server on :3000, etc.) are reachable from the host.
        // Specs: "3000" | "8080:80" | "0.0.0.0:8080:80". Host binding defaults
        // to 127.0.0.1 — reachable from this machine but not the LAN — unless
        // an explicit ip is given.
        let port_bindings = if config.ports.is_empty() {
            None
        } else {
            let mut map = std::collections::HashMap::new();
            for spec in &config.ports {
                let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
                let (ip, host, cont) = match parts.as_slice() {
                    [p] => ("127.0.0.1", *p, *p),
                    [h, c] => ("127.0.0.1", *h, *c),
                    [i, h, c] => (*i, *h, *c),
                    _ => {
                        tracing::warn!(spec = %spec, "ignoring malformed ports entry");
                        continue;
                    }
                };
                if host.is_empty() || cont.is_empty() {
                    tracing::warn!(spec = %spec, "ignoring malformed ports entry");
                    continue;
                }
                map.insert(
                    format!("{cont}/tcp"),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some(ip.to_string()),
                        host_port: Some(host.to_string()),
                    }]),
                );
            }
            if map.is_empty() { None } else { Some(map) }
        };

        // GPU passthrough (docker --gpus all): request all NVIDIA GPUs with the
        // "gpu" capability. self.gpu is set true only after confirming the image
        // was built with GPU support, so this never lands on a CPU image.
        let device_requests = if self.gpu {
            Some(vec![bollard::models::DeviceRequest {
                driver: Some("nvidia".to_string()),
                count: Some(-1),
                capabilities: Some(vec![vec!["gpu".to_string()]]),
                ..Default::default()
            }])
        } else {
            None
        };

        HostConfig {
            binds: Some(binds),
            network_mode: Some(DEFAULT_NETWORK.to_string()),
            privileged: Some(privileged),
            extra_hosts: Some(extra_hosts),
            port_bindings,
            device_requests,
            ..Default::default()
        }
    }

    /// Whether the configured image was built with GPU support (carries the
    /// `nemesis8.gpu=true` label from `n8 build --gpu`). Used to decide between
    /// real `--gpus` passthrough and a warn-and-run-CPU fallback.
    pub async fn image_has_gpu(&self) -> bool {
        self.docker
            .inspect_image(&self.image)
            .await
            .ok()
            .and_then(|img| img.config)
            .and_then(|c| c.labels)
            .and_then(|l| l.get(LABEL_GPU).cloned())
            .map(|v| v == "true")
            .unwrap_or(false)
    }
}

/// Run `docker/podman run -it` with the given args.
/// This is a free function (no bollard connection) so the socket is not
/// held open during the subprocess — which caused hangs on Windows.
pub fn run_it(args: &[String], runtime: &str) -> Result<i32> {
    let _term = TermGuard::new();
    let status = std::process::Command::new(runtime)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run container runtime")?;

    Ok(status.code().unwrap_or(1))
}

/// Start a DETACHED, persistent agent container (`run_args` must include `-d` and
/// a `--name=<name>`), then attach this terminal to it. The container outlives the
/// terminal, so if Hyperia / the terminal crashes mid-session, the agent keeps
/// running with its conversation intact — re-attach with `n8 attach <name>` or
/// resume it. On a CLEAN exit (the agent quit → container stopped) the husk is
/// removed (the `--rm` we deliberately dropped); if it's still running when attach
/// returns (terminal detached or died), it's LEFT for re-attach. Returns the
/// attach exit code.
pub fn spawn_detached_and_attach(run_args: &[String], name: &str, runtime: &str) -> Result<i32> {
    use std::process::{Command, Stdio};

    // 1. Spawn detached. Capture stdout so the container id doesn't print over the
    //    TUI; surface stderr on failure.
    let out = Command::new(runtime)
        .args(run_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning detached agent container")?;
    if !out.status.success() {
        anyhow::bail!(
            "failed to start agent container: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // 2. Attach our terminal. --sig-proxy=false so a dying terminal (SIGHUP) can't
    //    signal the container; --detach-keys for explicit detach. TermGuard restores
    //    cooked mode if the attach dies abnormally.
    let code = {
        let _term = TermGuard::new();
        Command::new(runtime)
            .args(["attach", "--sig-proxy=false", "--detach-keys=ctrl-^", name])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("attaching to agent container")?
            .code()
            .unwrap_or(1)
    };

    // 3. Let a clean exit settle, then decide: container gone → remove the husk;
    //    still running → the terminal detached/crashed, LEAVE it for re-attach.
    std::thread::sleep(std::time::Duration::from_millis(800));
    let running = Command::new(runtime)
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false);
    if running {
        eprintln!(
            "[nemesis8] terminal detached — agent '{name}' is still running (session safe). \
             Re-attach: n8 attach {name}   ·   stop it: n8 agents kill {name}"
        );
    } else {
        let _ = Command::new(runtime).args(["rm", "-f", name]).output();
    }
    Ok(code)
}

/// Restores the console to its pre-`docker` cooked mode when dropped.
///
/// `docker run -it` / `docker attach` put the host TTY into raw mode and are
/// supposed to restore it on exit — but on an ABNORMAL exit (broken daemon
/// connection when the host sleeps / Docker Desktop's VM restarts) that restore
/// is skipped, leaving the parent shell in raw mode: no echo, no line editing,
/// keystrokes split between the dead attach and the shell ("half-attached").
/// n8 never managed the TTY itself, so nothing reset it. This guard does:
/// it seeds crossterm's saved original mode with the current cooked state before
/// the child runs, then reapplies it on drop regardless of how docker exited.
pub struct TermGuard {
    active: bool,
}

impl TermGuard {
    pub fn new() -> Self {
        use std::io::IsTerminal;
        // Only meddle with the console when stdin is an actual terminal — piped
        // / non-interactive callers (gateway, CI) must be left alone.
        if !std::io::stdin().is_terminal() {
            return TermGuard { active: false };
        }
        // enable→disable captures the current (cooked) mode into crossterm's
        // process-global "original", leaving the terminal cooked, so the Drop
        // restore has the correct state to reapply even though docker — not us —
        // is what flips it to raw.
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::terminal::disable_raw_mode();
        TermGuard { active: true }
    }
}

impl Default for TermGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.active {
            // Force the console back to the captured cooked mode. Idempotent on
            // a clean exit (docker already restored it); the real win is the
            // abnormal-exit path where docker left it raw.
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        }
    }
}

/// Build `docker run -it` args from env, host_config, and container command.
/// `agent_id` is the container --name / agent label — generate it once in the
/// caller and pass the SAME value to `build_host_config` so the per-session
/// `/workspace` root dir is named after the agent.
pub fn build_run_it_args(
    image: &str,
    env: &[String],
    host_config: &HostConfig,
    privileged: bool,
    container_cmd: &[&str],
    agent_id: &str,
    detached: bool,
) -> Vec<String> {
    let mut args = vec!["run".to_string()];
    if detached {
        // Persistent agent: detached (-d) with a TTY (-it), and NO --rm. A separate
        // `docker attach` becomes the terminal; the container OUTLIVES it, so a
        // crashed or closed terminal (e.g. Hyperia) can't kill the agent mid-session
        // and lose its conversation. spawn_detached_and_attach removes the husk only
        // on a clean exit; a dropped terminal leaves it running for re-attach.
        args.push("-d".to_string());
        args.push("-it".to_string());
    } else {
        // Foreground: this client IS the terminal (shell, one-off). --rm cleans up.
        args.push("-it".to_string());
        args.push("--rm".to_string());
        // Re-map docker's detach sequence away from the default Ctrl+P /
        // Ctrl+Q. TUIs (agy, claude, codex) use Ctrl+P for their own
        // commands; the default intercepts that first chord and silently
        // swallows it, then sends a delayed burst when the second chord
        // doesn't match — which looks exactly like the terminal "going
        // half-detached". ctrl-^ (Ctrl+6) is documented as valid and is
        // virtually never produced by accident.
        args.push("--detach-keys=ctrl-^".to_string());
    }

    // Give the interactive container a memorable name + agent labels. The
    // control plane discovers it by the nemesis8.agent label (not the name),
    // so the name can be human-friendly: n8-fun-swan rather than a uuid. This
    // also becomes the agent_id, so `n8 attach` / `n8 agents kill <name>` take
    // something you can actually read off the screen and type.
    let provider = env
        .iter()
        .find_map(|e| e.strip_prefix("NEMESIS8_PROVIDER="))
        .unwrap_or("unknown");
    args.push(format!("--name={agent_id}"));
    args.push(format!("--label={LABEL_AGENT}=true"));
    args.push(format!("--label={LABEL_AGENT_ID}={agent_id}"));
    args.push(format!("--label={LABEL_HOST_ID}={}", host_id()));
    args.push(format!("--label={LABEL_PROVIDER}={provider}"));
    if let Some(session_id) = env.iter().find_map(|e| e.strip_prefix("CODEX_SESSION_ID=")) {
        args.push(format!("--label={LABEL_SESSION_ID}={session_id}"));
    }
    // Stamp the project workspace (native host path) so the control room shows
    // the SAME string the Sessions tab records instead of guessing from the
    // container's mounts array. The project bind was built as
    // `<docker-host-path>:/workspace/<dirname>:rw`; the per-session scratch
    // root binds to bare `:/workspace:` and thus can't match here.
    if let Some(ws) = host_config.binds.as_ref().and_then(|binds| {
        binds
            .iter()
            .find_map(|b| b.split_once(":/workspace/").map(|(host, _)| from_docker_path(host)))
    }) {
        args.push(format!("--label={LABEL_WORKSPACE}={ws}"));
    }

    // Match host's hostname and username so Gemini's FileKeychain
    // can decrypt OAuth tokens (encryption key = scrypt(hostname + username))
    let host_hostname = whoami::fallible::hostname().unwrap_or_default();
    let host_username = whoami::username();
    if !host_hostname.is_empty() {
        args.push(format!("--hostname={host_hostname}"));
    }

    if let Some(ref net) = host_config.network_mode {
        args.push(format!("--network={net}"));
    }

    if let Some(ref binds) = host_config.binds {
        for b in binds {
            args.push(format!("-v={b}"));
        }
    }

    // Publish ports (host_config carries them as "port/tcp" → bindings)
    if let Some(ref pb) = host_config.port_bindings {
        for (cont_port, bindings) in pb {
            let cont = cont_port.trim_end_matches("/tcp");
            for b in bindings.iter().flatten() {
                let ip = b.host_ip.as_deref().unwrap_or("127.0.0.1");
                let host = b.host_port.as_deref().unwrap_or(cont);
                args.push(format!("-p={ip}:{host}:{cont}"));
            }
        }
    }

    if let Some(ref hosts) = host_config.extra_hosts {
        for h in hosts {
            args.push(format!("--add-host={h}"));
        }
    }

    if privileged {
        args.push("--privileged".to_string());
    }

    // GPU passthrough: when the host_config carries a device request, mirror it
    // as `--gpus all` for the CLI run path (interactive). build_host_config only
    // sets this after confirming the image has GPU support.
    if host_config
        .device_requests
        .as_ref()
        .is_some_and(|d| !d.is_empty())
    {
        args.push("--gpus=all".to_string());
    }

    for e in env {
        args.push(format!("-e={e}"));
    }

    // Agent identity for the in-container monitor: JsonlSink tags every event
    // with NEMESIS8_AGENT_ID, and the gateway HttpSink push requires it. The
    // bollard one-shot paths set it in build_env's caller; this CLI path is
    // how interactive sessions launch, and without it every interactive
    // agent's telemetry lands untagged (regression found 2026-07-06).
    if !env.iter().any(|e| e.starts_with("NEMESIS8_AGENT_ID=")) {
        args.push(format!("-e=NEMESIS8_AGENT_ID={agent_id}"));
    }

    // Set USER to match host so Gemini FileKeychain derives the same encryption key
    if !host_username.is_empty() {
        args.push(format!("-e=USER={host_username}"));
    }

    args.push(image.to_string());

    for c in container_cmd {
        args.push(c.to_string());
    }

    args
}

/// Create a tar archive of the build context directory (in memory).
/// Respects .dockerignore if present, and sends progress to the TUI channel.
/// Stream one of a child build's pipes (stdout or stderr) line-by-line into the
/// BuildEvent channel, also collecting lines for the post-mortem build log.
/// Count build instructions in a Dockerfile (one BuildKit node ≈ one
/// instruction). Multi-line instructions (trailing `\`) count once. Used as the
/// denominator for a WHOLE-BUILD progress bar, since BuildKit's `[stage i/j]`
/// fractions are per-stage and don't compose across a multi-stage build.
fn count_dockerfile_steps(path: &Path) -> u32 {
    const KW: &[&str] = &[
        "FROM",
        "RUN",
        "COPY",
        "ADD",
        "ARG",
        "ENV",
        "WORKDIR",
        "CMD",
        "ENTRYPOINT",
        "LABEL",
        "USER",
        "EXPOSE",
        "VOLUME",
        "HEALTHCHECK",
        "SHELL",
        "STOPSIGNAL",
        "ONBUILD",
    ];
    let Ok(content) = std::fs::read_to_string(path) else {
        return 1;
    };
    let mut count = 0u32;
    let mut continuation = false;
    for raw in content.lines() {
        let line = raw.trim_end();
        if continuation {
            continuation = line.ends_with('\\');
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if KW.contains(&trimmed.split_whitespace().next().unwrap_or("")) {
            count += 1;
        }
        continuation = line.ends_with('\\');
    }
    count.max(1)
}

async fn pipe_build_lines<R>(
    reader: R,
    tx: tokio::sync::mpsc::UnboundedSender<BuildEvent>,
    logs: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    total: u32,
    seen: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u32>>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some((c, t, d)) = ui::parse_docker_step(&line) {
            // BuildKit's `[stage i/j]` is PER-STAGE; using it as global progress
            // makes the bar hit 100% at each stage's end and regress at the next.
            // Instead count distinct BuildKit node ids (`#N`) over the Dockerfile
            // instruction total — a monotonic whole-build fraction. Legacy
            // single-stage "Step i/j" is already global, so pass it through.
            let node = line
                .trim_start()
                .strip_prefix('#')
                .and_then(|r| r.split(' ').next())
                .and_then(|n| n.parse::<u32>().ok());
            let (current, denom) = match node {
                Some(n) => {
                    let count = {
                        let mut g = seen.lock().unwrap();
                        g.insert(n);
                        g.len() as u32
                    };
                    (count, total)
                }
                None => {
                    // Podman/buildah "STEP i/j" numbering RESETS per build stage,
                    // so c/t would regress at each stage. Count distinct step lines
                    // seen (monotonic) over the whole-Dockerfile total instead —
                    // the same whole-build fraction BuildKit gets above. `_` keeps
                    // the parsed c/t unused but documents the shape.
                    let _ = (c, t);
                    let count = {
                        let mut g = seen.lock().unwrap();
                        let k = 10_000_000 + g.len() as u32;
                        g.insert(k);
                        g.len() as u32
                    };
                    (count, total)
                }
            };
            let _ = tx.send(BuildEvent::Step {
                current,
                total: denom,
                message: d,
            });
        }
        let _ = tx.send(BuildEvent::Log(line.clone()));
        if let Ok(mut g) = logs.lock() {
            g.push(line);
        }
    }
}

/// Build the image by shelling out to the runtime CLI (`docker` / `podman`
/// build), streaming its output into the BuildEvent channel. Using the CLI
/// instead of bollard's build_image avoids the `X-Registry-Config` header that
/// podman's docker-compat /build endpoint can't parse. Returns
/// (error_message, all_log_lines). Does NOT send BuildEvent::Done (the caller
/// owns the Done/teardown).
async fn run_build_cli(
    runtime: String,
    image: String,
    context_dir: std::path::PathBuf,
    extra_args: std::collections::HashMap<String, String>,
    tx: tokio::sync::mpsc::UnboundedSender<BuildEvent>,
) -> (Option<String>, Vec<String>) {
    let ts = chrono::Utc::now().timestamp().to_string();
    let mut args = extra_args;
    args.insert("CACHE_BUST".to_string(), ts);

    let mut cmd = tokio::process::Command::new(&runtime);
    cmd.arg("build")
        .arg("-t")
        .arg(&image)
        .arg("-f")
        .arg(context_dir.join("Dockerfile"));
    // BuildKit's default output is ANSI/TTY-oriented and unparseable when
    // piped — force plain so the TUI gets clean "#N [i/j] ..." step lines.
    // (podman doesn't take --progress; it already emits plain "STEP i/j:".)
    if runtime == "docker" {
        cmd.arg("--progress=plain");
    }
    for (k, v) in &args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    cmd.arg(&context_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let m = format!("failed to start `{runtime} build`: {e}");
            let _ = tx.send(BuildEvent::Error(m.clone()));
            return (Some(m.clone()), vec![m]);
        }
    };

    // Whole-build progress: denominator = Dockerfile instruction count; numerator
    // = distinct BuildKit nodes seen (shared across the stdout/stderr pipes).
    let total_steps = count_dockerfile_steps(&context_dir.join("Dockerfile"));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<u32>::new(),
    ));

    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();
    if let Some(out) = child.stdout.take() {
        handles.push(tokio::spawn(pipe_build_lines(
            out,
            tx.clone(),
            logs.clone(),
            total_steps,
            seen.clone(),
        )));
    }
    if let Some(err) = child.stderr.take() {
        handles.push(tokio::spawn(pipe_build_lines(
            err,
            tx.clone(),
            logs.clone(),
            total_steps,
            seen.clone(),
        )));
    }

    let status = child.wait().await;
    for h in handles {
        let _ = h.await;
    }
    let lines = logs.lock().map(|g| g.clone()).unwrap_or_default();

    match status {
        Ok(s) if s.success() => (None, lines),
        Ok(s) => {
            let m = format!(
                "`{runtime} build` exited with code {}",
                s.code().unwrap_or(-1)
            );
            let _ = tx.send(BuildEvent::Error(m.clone()));
            (Some(m), lines)
        }
        Err(e) => {
            let m = format!("`{runtime} build` failed to run: {e}");
            let _ = tx.send(BuildEvent::Error(m.clone()));
            (Some(m), lines)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_IMAGE, "nemesis8:latest");
        assert_eq!(DEFAULT_NETWORK, "gnosis-network");
    }

    #[test]
    fn test_run_it_args_publish_ports() {
        // "3000" → 127.0.0.1:3000:3000, "0.0.0.0:8080:80" → as given
        let mut pb = std::collections::HashMap::new();
        pb.insert(
            "3000/tcp".to_string(),
            Some(vec![bollard::models::PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some("3000".to_string()),
            }]),
        );
        pb.insert(
            "80/tcp".to_string(),
            Some(vec![bollard::models::PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("8080".to_string()),
            }]),
        );
        let hc = HostConfig {
            port_bindings: Some(pb),
            ..Default::default()
        };
        let args = build_run_it_args("img", &[], &hc, false, &["cmd"], "test-agent", false);
        assert!(args.contains(&"-p=127.0.0.1:3000:3000".to_string()));
        assert!(args.contains(&"-p=0.0.0.0:8080:80".to_string()));
        // exposed_ports helper mirrors the bindings
        let exposed = exposed_ports_from(&hc).unwrap();
        assert!(exposed.contains_key("3000/tcp") && exposed.contains_key("80/tcp"));
    }
}
