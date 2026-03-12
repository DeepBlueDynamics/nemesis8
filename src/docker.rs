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

use crate::config::Config;
use crate::ui::{self, BuildEvent};

const DEFAULT_IMAGE: &str = "nemisis8:latest";
const DEFAULT_NETWORK: &str = "codex-network";

/// Docker operations for nemisis8
pub struct DockerOps {
    docker: Docker,
    image: String,
}

impl DockerOps {
    /// Connect to the Docker daemon
    pub fn new(image_tag: Option<&str>) -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("connecting to Docker daemon")?;
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

        let tar_body = create_tar_context(context_dir)
            .context("creating build context tar archive")?;

        if ui::is_interactive() {
            self.build_tui(tar_body).await
        } else {
            self.build_raw(tar_body).await
        }
    }

    /// Build with ratatui progress display
    async fn build_tui(&self, tar_body: Vec<u8>) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let docker = self.docker.clone();
        let image = self.image.clone();

        // Spawn the Docker stream consumer
        let build_handle = tokio::spawn(async move {
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
        let output = self
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
        self.pipe_output(output).await;

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

    /// Start an interactive session
    pub async fn interactive(
        &self,
        config: &Config,
        danger: bool,
        privileged: bool,
        model: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<()> {
        let container_name = format!("nemisis8-int-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = self.build_env(config, danger, model, None);

        let mut cmd = vec!["nemisis8-entry".to_string(), "--interactive".to_string()];
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
            .context("creating interactive container")?;

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
            stdin: Some(true),
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
            .context("starting interactive container")?;

        // Pipe stdout (Console variant used when tty=true)
        let output_task = tokio::spawn(async move {
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
        });

        output_task.await.ok();

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

        Ok(())
    }

    /// Drop into a bash shell in the container
    pub async fn shell(
        &self,
        config: &Config,
        privileged: bool,
        workspace: Option<&str>,
    ) -> Result<()> {
        let container_name = format!("nemisis8-sh-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = self.build_env(config, false, None, None);
        let host_config = self.build_host_config(config, privileged, workspace);

        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(vec!["/bin/bash".to_string()]),
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
            .context("creating shell container")?;

        // Spawn Ctrl-C watcher
        let docker_clone = self.docker.clone();
        let cid = container.id.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("\nInterrupted — stopping container {}...", &cid[..12]);
            let stop_opts = bollard::container::StopContainerOptions { t: 5 };
            docker_clone.stop_container(&cid, Some(stop_opts)).await.ok();
        });

        // Attach BEFORE starting
        let attach_opts = AttachContainerOptions::<String> {
            stdout: Some(true),
            stderr: Some(true),
            stdin: Some(true),
            stream: Some(true),
            ..Default::default()
        };
        let output = self
            .docker
            .attach_container(&container.id, Some(attach_opts))
            .await
            .context("attaching to shell container")?;

        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("starting shell container")?;

        self.pipe_output(output).await;

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

        Ok(())
    }

    /// Run the Codex login flow.
    /// Uses codex_login.sh which starts a socat bridge so the OAuth
    /// callback on port 1455 can reach the codex CLI inside the container.
    pub async fn login(&self, config: &Config) -> Result<()> {
        use bollard::models::PortBinding;

        let container_name = format!("nemisis8-login-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let env = self.build_env(config, false, None, None);

        // Port 1455: OAuth callback. codex_login.sh runs a socat bridge
        // from container-ip:1455 → 127.0.0.1:1455 so Docker's port
        // mapping can reach the codex CLI's callback server.
        let mut port_bindings = std::collections::HashMap::new();
        port_bindings.insert(
            "1455/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("1455".to_string()),
            }]),
        );

        let codex_home = dirs::home_dir()
            .map(|h| h.join(".codex-service"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.codex-service"));

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            network_mode: Some(DEFAULT_NETWORK.to_string()),
            binds: Some(vec![format!(
                "{}:/opt/codex-home:rw",
                codex_home.display()
            )]),
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            ..Default::default()
        };

        let mut exposed_ports = std::collections::HashMap::new();
        exposed_ports.insert("1455/tcp".to_string(), std::collections::HashMap::new());

        // Use codex_login.sh when available; fall back to inline bridge logic
        // for older images that predate the script.
        let login_cmd = r#"
set -euo pipefail
if [ -x /usr/local/bin/codex_login.sh ]; then
  /usr/local/bin/codex_login.sh
else
  socat TCP-LISTEN:1455,bind=0.0.0.0,reuseaddr,fork TCP:127.0.0.1:1455 &
  bridge_pid=$!
  trap 'kill "$bridge_pid" 2>/dev/null || true; wait "$bridge_pid" 2>/dev/null || true' EXIT INT TERM
  codex login
fi
"#;

        // Must invoke via bash because the image entrypoint is `node`.
        let container_config = ContainerConfig {
            image: Some(self.image.clone()),
            cmd: Some(vec![
                "/bin/bash".to_string(),
                "-lc".to_string(),
                login_cmd.to_string(),
            ]),
            env: Some(env),
            host_config: Some(host_config),
            exposed_ports: Some(exposed_ports),
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
            .context("creating login container")?;

        // Spawn Ctrl-C watcher
        let docker_clone = self.docker.clone();
        let cid = container.id.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("\nInterrupted — stopping container {}...", &cid[..12]);
            let stop_opts = bollard::container::StopContainerOptions { t: 5 };
            docker_clone.stop_container(&cid, Some(stop_opts)).await.ok();
        });

        // Attach BEFORE starting
        let attach_opts = AttachContainerOptions::<String> {
            stdout: Some(true),
            stderr: Some(true),
            stdin: Some(true),
            stream: Some(true),
            ..Default::default()
        };
        let output = self
            .docker
            .attach_container(&container.id, Some(attach_opts))
            .await
            .context("attaching to login container")?;

        self.docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("starting login container")?;

        self.pipe_output(output).await;

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

        Ok(())
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

    fn build_env(
        &self,
        config: &Config,
        danger: bool,
        model: Option<&str>,
        session_id: Option<&str>,
    ) -> Vec<String> {
        let mut env = config.container_env();

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

        env
    }

    fn build_host_config(
        &self,
        config: &Config,
        privileged: bool,
        workspace: Option<&str>,
    ) -> HostConfig {
        let mut binds = config.docker_binds();

        // Workspace mount
        if let Some(ws) = workspace {
            binds.push(format!("{ws}:/workspace:rw"));
        }

        // Codex home volume (persistent across runs)
        let codex_home = dirs::home_dir()
            .map(|h| h.join(".codex-service"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.codex-service"));

        binds.push(format!(
            "{}:/opt/codex-home:rw",
            codex_home.display()
        ));

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

    /// Pipe a pre-attached container output to host stdout.
    /// The attach must happen BEFORE `start_container` to avoid missing output.
    async fn pipe_output(
        &self,
        mut output: bollard::container::AttachContainerResults,
    ) {
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
    }
}

/// Create a tar archive of the build context directory (in memory)
pub(crate) fn create_tar_context(dir: &Path) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let mut archive = tar::Builder::new(buf);

    archive
        .append_dir_all(".", dir)
        .context("adding build context to tar")?;

    let buf = archive.into_inner().context("finalizing tar archive")?;
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

        let tar_bytes = create_tar_context(dir.path()).unwrap();
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
        let tar_bytes = create_tar_context(dir.path()).unwrap();
        // Empty dir still produces a valid (small) tar
        assert!(!tar_bytes.is_empty());
    }

    #[test]
    fn test_create_tar_context_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();

        let tar_bytes = create_tar_context(dir.path()).unwrap();
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
