use anyhow::{Context, Result};
use bollard::container::{
    AttachContainerOptions, Config as ContainerConfig, CreateContainerOptions, LogOutput,
    RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::models::HostConfig;
use bollard::Docker;
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

/// Stable-ish host identifier (hostname). Used in agent labels and, later, the
/// fleet registry's `{host_id}/{local_id}` agent IDs.
pub fn host_id() -> String {
    whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string())
}

/// Build the standard agent label set for a container.
fn agent_labels(provider: &str, agent_id: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(LABEL_AGENT.to_string(), "true".to_string());
    m.insert(LABEL_AGENT_ID.to_string(), agent_id.to_string());
    m.insert(LABEL_HOST_ID.to_string(), host_id());
    m.insert(LABEL_PROVIDER.to_string(), provider.to_string());
    m
}

/// Convert a Windows path to Docker-compatible format.
/// `C:\Users\foo\bar` → `/c/Users/foo/bar`
/// Non-Windows paths pass through unchanged.
pub fn to_docker_path(path: &str) -> String {
    // Match "X:\" or "X:/" style Windows paths
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
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
        if pipe.contains("podman") { "podman" } else { "docker" }
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
    if tok.is_empty() {
        None
    } else {
        Some(tok)
    }
}

/// Docker/Podman operations for nemesis8
pub struct DockerOps {
    docker: Docker,
    image: String,
    /// The container runtime binary name ("docker" or "podman").
    pub runtime_binary: String,
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
            let (docker, runtime) = Docker::connect_with_named_pipe(
                &pipe,
                1800,
                &bollard::API_DEFAULT_VERSION,
            )
            .map(|d| (d, if pipe.contains("podman") { "podman" } else { "docker" }))
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
            let docker = Docker::connect_with_local(
                &socket_uri,
                1800,
                &bollard::API_DEFAULT_VERSION,
            )
            .or_else(|_| Docker::connect_with_local_defaults())
            .context("connecting to container daemon")?;
            (docker, runtime.to_string())
        };

        Ok(Self {
            docker,
            image: image_tag.unwrap_or(DEFAULT_IMAGE).to_string(),
            runtime_binary,
        })
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
        if existing.iter().any(|n| n.name.as_deref() == Some(DEFAULT_NETWORK)) {
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
    pub async fn list_containers(&self, _image: &str) -> Result<Vec<bollard::models::ContainerSummary>> {
        use bollard::container::ListContainersOptions;
        use std::collections::HashMap;

        // List all running containers, then filter by legacy/current image names or labels.
        let all = self.docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: false,
                filters: {
                    let mut f = HashMap::new();
                    f.insert("status".to_string(), vec!["running".to_string()]);
                    f
                },
                ..Default::default()
            }))
            .await
            .context("listing containers")?;

        let containers: Vec<_> = all.into_iter().filter(|c| {
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
            img.contains("nemesis8") || img.contains("nemesis8")
                || c.names.as_ref().is_some_and(|names|
                    names.iter().any(|n| n.contains("nemesis8") || n.contains("nemesis8")))
                || c.command.as_deref().unwrap_or("").contains("nemesis8-entry")
        }).collect();

        Ok(containers)
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
            let (err, _lines) = run_build_cli(runtime, image, context_dir, extra_args, tx.clone()).await;
            if err.is_none() {
                let _ = tx.send(BuildEvent::Done);
            }
        });

        // Consume events and print JSON lines
        let start = std::time::Instant::now();
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Some(BuildEvent::Step { current, total: t, message }) => {
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
        let env = self.build_env(config, danger, model, session_id);

        let mut cmd = vec!["nemesis8-entry".to_string()];
        cmd.push("--prompt".to_string());
        cmd.push(prompt.to_string());
        if danger {
            cmd.push("--danger".to_string());
        }

        let host_config = self.build_host_config(config, privileged, workspace);

        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(cmd),
            env: Some(env),
            exposed_ports: exposed_ports_from(&host_config),
            host_config: Some(host_config),
            labels: Some(agent_labels(&config.provider.to_string(), &container_name)),
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
            docker_clone.stop_container(&cid, Some(stop_opts)).await.ok();
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

        let host_config = self.build_host_config(config, false, workspace);

        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(cmd),
            env: Some(env),
            exposed_ports: exposed_ports_from(&host_config),
            host_config: Some(host_config),
            labels: Some(agent_labels(&config.provider.to_string(), &container_name)),
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

        let timed_out = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            collect,
        )
        .await
        .is_err();

        if timed_out {
            // Kill container on timeout
            let stop_opts = bollard::container::StopContainerOptions { t: 3 };
            self.docker.stop_container(&container.id, Some(stop_opts)).await.ok();
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


    /// Run a pokeball worker container with full security constraints.
    /// Returns the container ID.
    pub async fn run_pokeball_worker(
        &self,
        image: &str,
        name: &str,
        comms_dir: &str,
        source_dir: &str,
        _timeout_minutes: u64,
    ) -> Result<String> {
        let container_name = format!("pokeball-{name}-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        let binds = vec![
            format!("{comms_dir}/inbox:/comms/inbox:rw"),
            format!("{comms_dir}/outbox:/comms/outbox:rw"),
            format!("{comms_dir}/status:/comms/status:rw"),
            format!("{source_dir}:/work:rw"),
        ];

        let mut tmpfs = std::collections::HashMap::new();
        tmpfs.insert("/tmp".to_string(), "size=256m".to_string());

        let host_config = HostConfig {
            binds: Some(binds),
            network_mode: Some("none".to_string()),
            readonly_rootfs: Some(true),
            tmpfs: Some(tmpfs),
            security_opt: Some(vec!["no-new-privileges:true".to_string()]),
            cap_drop: Some(vec!["ALL".to_string()]),
            memory: Some(4 * 1024 * 1024 * 1024), // 4GB
            pids_limit: Some(256),
            privileged: Some(false),
            ..Default::default()
        };

        let container_config = ContainerConfig {
            image: Some(image.to_string()),
            cmd: Some(vec![
                "tini".to_string(),
                "--".to_string(),
                "pokeball-worker".to_string(),
            ]),
            host_config: Some(host_config),
            user: Some("pokeball".to_string()),
            working_dir: Some("/work".to_string()),
            tty: Some(false),
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
            .context("creating pokeball worker container")?;

        tracing::info!(
            id = %container.id,
            name = %container_name,
            image = %image,
            "pokeball worker container created"
        );

        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("starting pokeball worker container")?;

        Ok(container.id)
    }

    /// Stop and remove a pokeball worker container
    pub async fn stop_pokeball_worker(&self, container_id: &str) -> Result<()> {
        // Stop
        let stop_opts = bollard::container::StopContainerOptions { t: 10 };
        self.docker
            .stop_container(container_id, Some(stop_opts))
            .await
            .ok(); // Ignore error if already stopped

        // Remove
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .ok();

        Ok(())
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
            "HYPERIA_URL",
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
    ) -> HostConfig {
        let mut binds = config.docker_binds();

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
        if let Some(ws) = workspace {
            let docker_ws = to_docker_path(ws);
            let dirname = std::path::Path::new(ws)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace");
            binds.push(format!("{docker_ws}:/workspace/{dirname}:rw"));
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
        // but we add it explicitly for Linux Docker
        #[cfg(target_os = "linux")]
        {
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

        HostConfig {
            binds: Some(binds),
            network_mode: Some(DEFAULT_NETWORK.to_string()),
            privileged: Some(privileged),
            extra_hosts: Some(extra_hosts),
            port_bindings,
            ..Default::default()
        }
    }

}

/// Run `docker/podman run -it` with the given args.
/// This is a free function (no bollard connection) so the socket is not
/// held open during the subprocess — which caused hangs on Windows.
pub fn run_it(args: &[String], runtime: &str) -> Result<i32> {
    let status = std::process::Command::new(runtime)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run container runtime")?;

    Ok(status.code().unwrap_or(1))
}

/// Build `docker run -it` args from env, host_config, and container command.
pub fn build_run_it_args(
    image: &str,
    env: &[String],
    host_config: &HostConfig,
    privileged: bool,
    container_cmd: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-it".to_string(),
        "--rm".to_string(),
        // Re-map docker's detach sequence away from the default Ctrl+P /
        // Ctrl+Q. TUIs (agy, claude, codex) use Ctrl+P for their own
        // commands; the default intercepts that first chord and silently
        // swallows it, then sends a delayed burst when the second chord
        // doesn't match — which looks exactly like the terminal "going
        // half-detached". ctrl-^ (Ctrl+6) is documented as valid and is
        // virtually never produced by accident. Users still have Ctrl+C
        // to interrupt; only the rare detach-while-keeping-container-alive
        // affordance moves.
        "--detach-keys=ctrl-^".to_string(),
    ];

    // Give the interactive container a memorable name + agent labels. The
    // control plane discovers it by the nemesis8.agent label (not the name),
    // so the name can be human-friendly: n8-fun-swan rather than a uuid. This
    // also becomes the agent_id, so `n8 attach` / `n8 agents kill <name>` take
    // something you can actually read off the screen and type.
    let agent_id = crate::names::fun_name();
    let provider = env
        .iter()
        .find_map(|e| e.strip_prefix("NEMESIS8_PROVIDER="))
        .unwrap_or("unknown");
    args.push(format!("--name={agent_id}"));
    args.push(format!("--label={LABEL_AGENT}=true"));
    args.push(format!("--label={LABEL_AGENT_ID}={agent_id}"));
    args.push(format!("--label={LABEL_HOST_ID}={}", host_id()));
    args.push(format!("--label={LABEL_PROVIDER}={provider}"));

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

    for e in env {
        args.push(format!("-e={e}"));
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
async fn pipe_build_lines<R>(
    reader: R,
    tx: tokio::sync::mpsc::UnboundedSender<BuildEvent>,
    logs: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
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
            let _ = tx.send(BuildEvent::Step { current: c, total: t, message: d });
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

    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();
    if let Some(out) = child.stdout.take() {
        handles.push(tokio::spawn(pipe_build_lines(out, tx.clone(), logs.clone())));
    }
    if let Some(err) = child.stderr.take() {
        handles.push(tokio::spawn(pipe_build_lines(err, tx.clone(), logs.clone())));
    }

    let status = child.wait().await;
    for h in handles {
        let _ = h.await;
    }
    let lines = logs.lock().map(|g| g.clone()).unwrap_or_default();

    match status {
        Ok(s) if s.success() => (None, lines),
        Ok(s) => {
            let m = format!("`{runtime} build` exited with code {}", s.code().unwrap_or(-1));
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

pub(crate) fn create_tar_context(
    dir: &Path,
    tx: Option<&tokio::sync::mpsc::UnboundedSender<BuildEvent>>,
) -> Result<Vec<u8>> {
    // Load .dockerignore patterns
    let dockerignore_path = dir.join(".dockerignore");
    let ignore_patterns = if dockerignore_path.is_file() {
        std::fs::read_to_string(&dockerignore_path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let buf = Vec::new();
    let mut archive = tar::Builder::new(buf);
    let mut file_count: usize = 0;

    fn should_ignore(rel_path: &str, patterns: &[String]) -> bool {
        for pat in patterns {
            let pat_clean = pat.trim_end_matches('/');
            // Check if any path component matches
            if rel_path == pat_clean
                || rel_path.starts_with(&format!("{pat_clean}/"))
                || rel_path.starts_with(&format!("./{pat_clean}/"))
                || rel_path.starts_with(&format!("./{pat_clean}"))
            {
                return true;
            }
            // Glob-style extension match (e.g. "*.env", "*.log")
            if pat.starts_with('*') {
                let suffix = &pat[1..];
                if rel_path.ends_with(suffix) {
                    return true;
                }
            }
        }
        false
    }

    fn add_dir_recursive(
        archive: &mut tar::Builder<Vec<u8>>,
        base: &Path,
        current: &Path,
        patterns: &[String],
        file_count: &mut usize,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<BuildEvent>>,
    ) -> Result<()> {
        let entries = std::fs::read_dir(current)
            .with_context(|| format!("reading directory {}", current.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            if should_ignore(&rel, patterns) {
                continue;
            }

            if path.is_dir() {
                add_dir_recursive(archive, base, &path, patterns, file_count, tx)?;
            } else {
                archive
                    .append_path_with_name(&path, &rel)
                    .with_context(|| format!("adding {rel} to tar"))?;
                *file_count += 1;

                if *file_count % 50 == 0 {
                    if let Some(tx) = tx {
                        let _ = tx.send(BuildEvent::Log(
                            format!("Packaging build context... ({file_count} files)"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    if let Some(tx) = tx {
        let _ = tx.send(BuildEvent::Log("Packaging build context...".into()));
    }

    add_dir_recursive(&mut archive, dir, dir, &ignore_patterns, &mut file_count, tx)?;

    let buf = archive.into_inner().context("finalizing tar archive")?;
    let size_mb = buf.len() as f64 / (1024.0 * 1024.0);

    if let Some(tx) = tx {
        let _ = tx.send(BuildEvent::Log(
            format!("Build context ready: {file_count} files, {size_mb:.1} MB"),
        ));
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_tar_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM node:24-slim\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test\n").unwrap();

        let tar_bytes = create_tar_context(dir.path(), None).unwrap();
        assert!(!tar_bytes.is_empty());

        // Verify it's a valid tar by reading it back
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.path().ok().map(|p| p.to_string_lossy().to_string()))
            .collect();

        assert!(entries.iter().any(|e| e.contains("Dockerfile")));
        assert!(entries.iter().any(|e| e.contains("README.md")));
    }

    #[test]
    fn test_create_tar_context_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tar_bytes = create_tar_context(dir.path(), None).unwrap();
        // Empty dir still produces a valid (small) tar
        assert!(!tar_bytes.is_empty());
    }

    #[test]
    fn test_create_tar_context_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();

        let tar_bytes = create_tar_context(dir.path(), None).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.path().ok().map(|p| p.to_string_lossy().to_string()))
            .collect();

        assert!(entries.iter().any(|e| e.contains("main.rs")));
        assert!(entries.iter().any(|e| e.contains("Cargo.toml")));
    }

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
        let args = build_run_it_args("img", &[], &hc, false, &["cmd"]);
        assert!(args.contains(&"-p=127.0.0.1:3000:3000".to_string()));
        assert!(args.contains(&"-p=0.0.0.0:8080:80".to_string()));
        // exposed_ports helper mirrors the bindings
        let exposed = exposed_ports_from(&hc).unwrap();
        assert!(exposed.contains_key("3000/tcp") && exposed.contains_key("80/tcp"));
    }
}
