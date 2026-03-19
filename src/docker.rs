use anyhow::{Context, Result};
use bollard::container::{
    AttachContainerOptions, Config as ContainerConfig, CreateContainerOptions, LogOutput,
    RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::image::BuildImageOptions;
use bollard::models::HostConfig;
use bollard::Docker;
use futures_util::StreamExt;
use std::io::Write;
use std::path::Path;
use tokio::io::AsyncWriteExt;

use crate::config::{Config, Provider};
use crate::ui::{self, BuildEvent};

const DEFAULT_IMAGE: &str = "nemisis8:latest";
const DEFAULT_NETWORK: &str = "codex-network";

/// Convert a Windows path to Docker-compatible format.
/// `C:\Users\foo\bar` → `/c/Users/foo/bar`
/// Non-Windows paths pass through unchanged.
fn to_docker_path(path: &str) -> String {
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

/// Docker operations for nemisis8
pub struct DockerOps {
    docker: Docker,
    image: String,
}

impl DockerOps {
    /// Connect to the Docker daemon
    pub fn new(image_tag: Option<&str>) -> Result<Self> {
        // Use a long timeout (30 min) because Docker build streams can have
        // long gaps between output (apt-get, pip install, cargo build, etc.)
        // The default 120s causes spurious timeouts.
        #[cfg(windows)]
        let docker = Docker::connect_with_named_pipe(
            "//./pipe/docker_engine",
            1800, // 30 minutes
            &bollard::API_DEFAULT_VERSION,
        )
        .context("connecting to Docker daemon")?;

        #[cfg(not(windows))]
        let docker = Docker::connect_with_local(
            "unix:///var/run/docker.sock",
            1800,
            &bollard::API_DEFAULT_VERSION,
        )
        .or_else(|_| Docker::connect_with_local_defaults())
        .context("connecting to Docker daemon")?;
        Ok(Self {
            docker,
            image: image_tag.unwrap_or(DEFAULT_IMAGE).to_string(),
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

    /// Build the Docker image from the project directory.
    /// Uses a ratatui TUI progress bar when stdout is a terminal,
    /// falls back to raw output when piped.
    pub async fn build(&self, context_dir: &Path) -> Result<()> {
        tracing::info!(dir = %context_dir.display(), image = %self.image, "building Docker image");

        if ui::is_interactive() {
            self.build_tui(context_dir).await
        } else {
            let tar_body = create_tar_context(context_dir, None)
                .context("creating build context tar archive")?;
            self.build_raw(tar_body).await
        }
    }

    /// Build with ratatui progress display
    async fn build_tui(&self, context_dir: &Path) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let docker = self.docker.clone();
        let image = self.image.clone();
        let context_dir = context_dir.to_path_buf();

        // Spawn the build pipeline — tar on blocking thread, then Docker stream
        let build_handle = tokio::spawn(async move {
            // Build tar context on a blocking thread so the TUI can render
            let tx_tar = tx.clone();
            let context_dir_clone = context_dir.clone();
            let tar_result = tokio::task::spawn_blocking(move || {
                create_tar_context(&context_dir_clone, Some(&tx_tar))
            })
            .await;

            let tar_body = match tar_result {
                Ok(Ok(body)) => body,
                Ok(Err(e)) => {
                    let msg = format!("Failed to create build context: {e}");
                    let _ = tx.send(BuildEvent::Error(msg.clone()));
                    return Some(msg);
                }
                Err(e) => {
                    let msg = format!("Build context task panicked: {e}");
                    let _ = tx.send(BuildEvent::Error(msg.clone()));
                    return Some(msg);
                }
            };

            let _ = tx.send(BuildEvent::Log(
                "Sending build context to Docker daemon...".into(),
            ));

            let options = BuildImageOptions {
                dockerfile: "Dockerfile".to_string(),
                t: image,
                rm: true,
                ..Default::default()
            };

            let mut stream = docker.build_image(options, None, Some(tar_body.into()));
            let mut build_error: Option<String> = None;

            while let Some(result) = stream.next().await {
                match result {
                    Ok(info) => {
                        if let Some(text) = info.stream {
                            let line = text.trim_end();
                            if !line.is_empty() {
                                if let Some((c, t, d)) = ui::parse_docker_step(line) {
                                    let _ = tx.send(BuildEvent::Step {
                                        current: c,
                                        total: t,
                                        message: d,
                                    });
                                }
                                let _ = tx.send(BuildEvent::Log(line.to_string()));
                            }
                        }
                        if let Some(error) = info.error {
                            let _ = tx.send(BuildEvent::Error(error.clone()));
                            build_error = Some(error);
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let _ = tx.send(BuildEvent::Error(msg.clone()));
                        build_error = Some(msg);
                        break;
                    }
                }
            }

            if build_error.is_none() {
                let _ = tx.send(BuildEvent::Done);
            }

            build_error
        });

        // Run TUI on the current task (owns the terminal)
        ui::run_build_progress(rx).await?;

        // Check build result
        let build_error = build_handle.await.context("build task panicked")?;
        if let Some(err) = build_error {
            anyhow::bail!("Docker build error: {err}");
        }

        tracing::info!(image = %self.image, "image built successfully");
        Ok(())
    }

    /// Build with raw line-by-line output (for piped / non-TTY contexts)
    async fn build_raw(&self, tar_body: Vec<u8>) -> Result<()> {
        let options = BuildImageOptions {
            dockerfile: "Dockerfile",
            t: &self.image,
            rm: true,
            ..Default::default()
        };

        let mut stream = self.docker.build_image(options, None, Some(tar_body.into()));

        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(stream) = info.stream {
                        let line = stream.trim_end();
                        if !line.is_empty() {
                            println!("{line}");
                            std::io::stdout().flush().ok();
                        }
                    }
                    if let Some(error) = info.error {
                        anyhow::bail!("Docker build error: {error}");
                    }
                }
                Err(e) => anyhow::bail!("Docker build stream error: {e}"),
            }
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
        let container_name = format!("nemisis8-run-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = self.build_env(config, danger, model, session_id);

        let mut cmd = vec!["nemisis8-entry".to_string()];
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
            host_config: Some(host_config),
            tty: Some(true),
            open_stdin: Some(true),
            attach_stdin: Some(true),
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
            stdin: Some(true),
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

        // Forward host stdin to container stdin
        let mut container_stdin = output.input;
        tokio::spawn(async move {
            let mut host_stdin = tokio::io::stdin();
            tokio::io::copy(&mut host_stdin, &mut container_stdin).await.ok();
        });

        // Stream output
        let mut stdout = tokio::io::stdout();
        while let Some(Ok(log)) = output.output.next().await {
            match log {
                LogOutput::StdOut { message }
                | LogOutput::StdErr { message }
                | LogOutput::Console { message } => {
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
    ) -> Result<String> {
        let container_name = format!("nemisis8-gw-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = self.build_env(config, danger, model, session_id);

        let mut cmd = vec!["nemisis8-entry".to_string()];
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
            host_config: Some(host_config),
            tty: Some(false),
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
                    LogOutput::StdOut { message }
                    | LogOutput::StdErr { message }
                    | LogOutput::Console { message } => {
                        captured.extend_from_slice(&message);
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

        let codex_home = dirs::home_dir()
            .map(|h| h.join(".codex-service"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.codex-service"));
        let codex_home_docker = to_docker_path(&codex_home.display().to_string());

        // Ensure the directory exists on host
        std::fs::create_dir_all(&codex_home).ok();

        let login_cmd = match config.provider {
            Provider::Gemini => {
                env.push("OAUTH_CALLBACK_PORT=8766".to_string());
                env.push("OAUTH_CALLBACK_HOST=0.0.0.0".to_string());
                r#"set -euo pipefail; export PATH="/usr/local/share/npm-global/bin:${PATH}"; echo "[nemisis8] Starting Gemini CLI login..."; echo "[nemisis8] Tip: You can skip OAuth by setting GEMINI_API_KEY in your environment."; echo ""; gemini -d auth login"#.to_string()
            }
            Provider::Codex => {
                r#"set -euo pipefail; if [ -x /usr/local/bin/codex_login.sh ]; then /usr/local/bin/codex_login.sh; else socat TCP-LISTEN:1455,bind=0.0.0.0,reuseaddr,fork TCP:127.0.0.1:1455 & bridge_pid=$!; trap 'kill "$bridge_pid" 2>/dev/null || true' EXIT INT TERM; codex login; fi"#.to_string()
            }
        };

        let mut args = vec![
            "run".to_string(),
            "-it".to_string(),
            "--rm".to_string(),
            format!("--network={DEFAULT_NETWORK}"),
            "--add-host=host.docker.internal:host-gateway".to_string(),
            format!("-v={codex_home_docker}:/opt/codex-home:rw"),
            "-p=1455:1455".to_string(),
            "-p=8766:8766".to_string(),
        ];

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
        env.push(format!("NEMISIS8_PROVIDER={}", config.provider));

        // Forward API keys from host
        for key in &[
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "SERPAPI_API_KEY",
            "ELEVENLABS_API_KEY",
        ] {
            if let Ok(val) = std::env::var(key) {
                env.push(format!("{key}={val}"));
            }
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
        env.push("HOME=/opt/codex-home".to_string());
        env.push("XDG_CONFIG_HOME=/opt/codex-home".to_string());

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

        // Workspace mount — mount current directory as /workspace
        if let Some(ws) = workspace {
            let docker_ws = to_docker_path(ws);
            binds.push(format!("{docker_ws}:/workspace:rw"));
        }

        // Codex home volume (persistent across runs)
        let codex_home = dirs::home_dir()
            .map(|h| h.join(".codex-service"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.codex-service"));

        binds.push(format!(
            "{}:/opt/codex-home:rw",
            to_docker_path(&codex_home.display().to_string())
        ));

        #[allow(unused_mut)]
        let mut extra_hosts = vec!["host.docker.internal:host-gateway".to_string()];

        // Windows Docker Desktop usually handles this automatically,
        // but we add it explicitly for Linux Docker
        #[cfg(target_os = "linux")]
        {
            extra_hosts.push("host.docker.internal:172.17.0.1".to_string());
        }

        HostConfig {
            binds: Some(binds),
            network_mode: Some(DEFAULT_NETWORK.to_string()),
            privileged: Some(privileged),
            extra_hosts: Some(extra_hosts),
            ..Default::default()
        }
    }

}

/// Run `docker run -it` with the given args.
/// This is a free function (no bollard connection) so the Docker socket is not
/// held open during the subprocess — which caused hangs on Windows.
pub fn run_it(args: &[String]) -> Result<i32> {
    let status = std::process::Command::new("docker")
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run docker")?;

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
    ];

    if let Some(ref net) = host_config.network_mode {
        args.push(format!("--network={net}"));
    }

    if let Some(ref binds) = host_config.binds {
        for b in binds {
            args.push(format!("-v={b}"));
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

    args.push(image.to_string());

    for c in container_cmd {
        args.push(c.to_string());
    }

    args
}

/// Create a tar archive of the build context directory (in memory).
/// Respects .dockerignore if present, and sends progress to the TUI channel.
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
        assert_eq!(DEFAULT_IMAGE, "nemisis8:latest");
        assert_eq!(DEFAULT_NETWORK, "codex-network");
    }
}
