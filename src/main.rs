use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

use nemesis8::cli::{Cli, Command, McpAction, MountAction, PokeballAction};
use nemesis8::config::Config;
use nemesis8::docker::{DockerOps, DOCKER_CONNECTIVITY_ADVICE, is_docker_connectivity_error};
use nemesis8::gateway::{self, GatewayConfig};
use nemesis8::pokeball;
use nemesis8::session;

/// Resolve the nemesis8 project directory (Dockerfile, MCP/, etc.)
fn project_dir() -> PathBuf {
    nemesis8::project_dir_fn()
}

/// Resolve the user's workspace directory (mounted as /workspace in container).
/// Priority: --workspace flag > CWD
fn workspace_dir(flag: Option<&str>) -> PathBuf {
    if let Some(ws) = flag {
        return PathBuf::from(ws);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Load config by searching upward from workspace.
/// If no config is found, auto-init one in the workspace directory.
fn load_config(workspace: &Path) -> Config {
    if let Some(found) = Config::find(workspace) {
        if let Ok(config) = Config::load(&found) {
            tracing::info!(path = %found.display(), "loaded config");
            return config;
        }
    }

    // No config found — auto-init a fresh one in the workspace
    eprintln!("[nemesis8] No .nemesis8.toml found — initializing one in {}", workspace.display());
    let _ = init_config(workspace);
    let new_config = workspace.join(".nemesis8.toml");
    if let Ok(config) = Config::load(&new_config) {
        tracing::info!(path = %new_config.display(), "loaded auto-initialized config");
        return config;
    }

    tracing::info!("no config found, using defaults");
    Config::default()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nemesis8=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Load env files before anything else
    load_env_files();

    // Non-blocking version check — fires and forgets. ONLY for quick
    // print-and-exit commands: for anything that opens a TUI (home screen,
    // pickers) or hands the terminal to a container (interactive/resume/run/
    // shell), this async eprintln would land on top of the alt-screen / the
    // agent's session. Those paths get the notice via the UI / `n8 update`.
    if update_notice_allowed(&cli.command) {
    tokio::spawn(async {
        if let Some(latest) = fetch_latest_version().await {
            let current = env!("CARGO_PKG_VERSION");
            if latest != current {
                #[cfg(target_os = "windows")]
                let install_hint = "powershell -c \"irm https://nemesis8.nuts.services/install.ps1 | iex\"";
                #[cfg(not(target_os = "windows"))]
                let install_hint = "curl -fsSL https://nemesis8.nuts.services/install.sh | sh";
                eprintln!("\r[nemesis8] update available: v{latest} (you have v{current})");
                eprintln!("\r[nemesis8] upgrade: {install_hint}");
            }
        }
    });
    }

    // Make sure the data home exists, porting a legacy ~/.codex-service
    // forward (copy) the first time — logins + session history come along.
    nemesis8::paths::ensure_data_home();

    let workspace = workspace_dir(cli.workspace.as_deref());
    let ws_arg = if cli.no_mount { None } else { Some(workspace.to_string_lossy().to_string()) };
    let mut config = load_config(&workspace);

    // Auto-discover integrations
    check_integrations(&config);

    // CLI --provider flag overrides config file
    if let Some(ref p) = cli.provider {
        match p.parse::<nemesis8::config::Provider>() {
            Ok(provider) => config.provider = provider,
            Err(e) => anyhow::bail!(e),
        }
    }

    // CLI --publish entries add to the config's published ports
    config.ports.extend(cli.publish.iter().cloned());

    // Resolve remote URL: CLI flag > config file
    let remote_url = cli.remote.as_deref().or(config.remote.as_deref());

    // Fleet control is a pure gateway client — it talks HTTP to a gateway
    // (remote if set, else the local one on --port) and never needs Docker.
    if let Some(Command::Agents { action }) = &cli.command {
        let gw = remote_url
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("http://localhost:{}", cli.port));
        let token = cli.token.as_deref().or(config.remote_token.as_deref());
        let client = nemesis8::remote::RemoteClient::new(&gw, token);
        return handle_agents(action.as_ref(), &client).await;
    }

    if let Some(url) = remote_url {
        let token = cli.token.as_deref().or(config.remote_token.as_deref());
        let client = nemesis8::remote::RemoteClient::new(url, token);
        return run_remote(client, cli, &config).await;
    }

    // Bare `n8` (no subcommand) → home screen. Resolve to a concrete Command so
    // both the no-docker match and the docker match below handle one type.
    let command = cli.command.unwrap_or(Command::Home);

    // Commands that don't need Docker
    match &command {
        Command::Sessions { query } => {
            // 1. Local sessions from host filesystem
            let dirs = resolve_session_dirs(&config);
            let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
            match session::list_sessions(&dir_refs) {
                Ok(mut sessions) if !sessions.is_empty() => {
                    let dir_to_provider = provider_dir_map();
                    session::annotate_providers(&mut sessions, &dir_to_provider);
                    match query.as_deref() {
                        // Content search: rank sessions by what's *inside* the
                        // transcript (lume BM25), then append any id/workspace
                        // substring matches not already surfaced — so this is a
                        // strict superset of the old substring-only behavior.
                        Some(q) if !q.trim().is_empty() => {
                            let ranked = nemesis8::search::rank_sessions(&sessions, q);
                            let mut seen = std::collections::HashSet::new();
                            let mut ordered: Vec<session::SessionInfo> = Vec::new();
                            for (idx, _score) in &ranked {
                                if seen.insert(*idx) {
                                    ordered.push(sessions[*idx].clone());
                                }
                            }
                            let ql = q.to_lowercase();
                            for (idx, s) in sessions.iter().enumerate() {
                                let hit = s.id.to_lowercase().contains(&ql)
                                    || s.workspace.as_deref().unwrap_or("")
                                        .to_lowercase()
                                        .contains(&ql);
                                if hit && seen.insert(idx) {
                                    ordered.push(s.clone());
                                }
                            }
                            println!("Local sessions matching \"{q}\" ({} hits):", ordered.len());
                            session::print_sessions(&ordered, None);
                        }
                        _ => {
                            println!("Local sessions:");
                            session::print_sessions(&sessions, None);
                        }
                    }
                }
                Ok(_) => println!("No local sessions."),
                Err(e) => eprintln!("Failed to list local sessions: {e}"),
            }

            // 2. Gateway sessions (try common ports)
            let gateway_url = cli.remote.as_deref()
                .or(config.remote.as_deref())
                .unwrap_or("http://localhost:4000");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap_or_default();
            if let Ok(resp) = client.get(format!("{gateway_url}/sessions")).send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(arr) = body.as_array() {
                        if !arr.is_empty() {
                            println!("\nGateway sessions ({gateway_url}):");
                            println!("{:<40} {:<25} {}", "ID", "MODIFIED", "SIZE");
                            println!("{}", "-".repeat(75));
                            for s in arr {
                                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                let modified = s.get("modified").and_then(|v| v.as_str()).unwrap_or("?");
                                let size = s.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                                let size_str = if size > 1_048_576 {
                                    format!("{:.1} MB", size as f64 / 1_048_576.0)
                                } else if size > 1024 {
                                    format!("{:.1} KB", size as f64 / 1024.0)
                                } else {
                                    format!("{} B", size)
                                };
                                println!("{:<40} {:<25} {}", id, modified, size_str);
                            }
                        }
                    }
                }
            }

            return Ok(());
        }
        Command::Init => {
            init_config(&workspace)?;
            return Ok(());
        }
        Command::Doctor => {
            pokeball::runner::doctor();
            return Ok(());
        }
        Command::Mount { action } => {
            handle_mount(action, &workspace)?;
            return Ok(());
        }
        Command::Mcp { action } => {
            handle_mcp(action, &workspace, cli.tag.as_deref())?;
            return Ok(());
        }
        Command::Update => {
            self_update().await?;
            return Ok(());
        }
        Command::Ps => {
            // Handled after Docker connect below
        }
        _ => {}
    }

    // Preflight: confirm a runtime actually responds (bollard's connect is lazy
    // and never pings, so a missing/stopped daemon would otherwise surface as a
    // cryptic error deep inside build/run). Exits with install/start guidance.
    preflight_runtime_or_exit();

    // Connect to Docker — give a friendly error if it's not available
    let docker = match DockerOps::new(cli.tag.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: Could not connect to Docker.");
            eprintln!();
            if is_docker_connectivity_error(&e.to_string()) {
                eprintln!("{}", DOCKER_CONNECTIVITY_ADVICE);
            } else {
                eprintln!("No container runtime found. Install one:");
                eprintln!();
                eprintln!("  Docker Desktop (Docker Inc.)");
                eprintln!("    Windows:  https://docs.docker.com/desktop/install/windows/");
                eprintln!("    macOS:    https://docs.docker.com/desktop/install/mac/");
                eprintln!();
                eprintln!("  Podman (free, open source)");
                eprintln!("    macOS:    brew install podman && podman machine start");
                eprintln!("    Linux:    sudo apt install podman   (Ubuntu/Debian)");
                eprintln!("              sudo dnf install podman   (Fedora)");
                eprintln!("    Windows:  https://podman-desktop.io");
                eprintln!();
                eprintln!("Run 'nemesis8 init' to auto-detect or install a runtime.");
            }
            eprintln!();
            eprintln!("Run 'nemesis8 doctor' for a full diagnostic.");
            std::process::exit(1);
        }
    };

    match command {
        Command::Build { json_progress, ffmpeg } => {
            ensure_dockerfile()?;
            let build_args = config.docker_build_args_with_flags(ffmpeg);
            if json_progress {
                docker.build_json_progress(&project_dir(), build_args).await?;
            } else {
                docker.build(&project_dir(), build_args).await?;
                println!("Image built successfully.");
            }
        }

        Command::Run { prompt } => {
            ensure_image(&docker, &config).await?;
            let ws = workspace.to_string_lossy();
            docker
                .run(
                    &config,
                    &prompt,
                    cli.danger,
                    cli.privileged,
                    cli.model.as_deref(),
                    Some(&ws),
                    None,
                )
                .await?;
        }

        Command::Interactive => {
            ensure_image(&docker, &config).await?;

            // Pre-flight: provider-declared host auth check (TOML
            // [provider.login.preflight]) — fail with the provider's hint here
            // instead of cryptically inside the container.
            run_login_preflight(&config)?;

            let env = docker.build_env(&config, cli.danger, cli.model.as_deref(), None);
            let host_config = docker.build_host_config(&config, cli.privileged, ws_arg.as_deref());
            let image = docker.image_name().to_string();
            let privileged = cli.privileged;
            let danger = cli.danger;
            let host_ws = workspace.to_string_lossy().to_string();
            let runtime = docker.runtime_binary.clone();
            drop(docker);

            let mut cmd: Vec<&str> = vec!["nemesis8-entry", "--interactive"];
            if danger { cmd.push("--danger"); }
            let args = nemesis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
            let before_sessions = snapshot_session_ids(&config);
            let status = nemesis8::docker::run_it(&args, &runtime)?;
            // Record the workspace for the session(s) this run created, and
            // show how to resume it with n8 (works for any provider).
            let new_ids = record_new_sessions(&config, &host_ws, &before_sessions);
            print_resume_hint(&new_ids, danger);
            if status != 0 {
                anyhow::bail!("interactive session exited with code {status}");
            }
        }

        Command::Serve { background, status, stop } => {
            // Daemon control paths short-circuit before touching Docker.
            if stop {
                nemesis8::daemon::stop()?;
                return Ok(());
            }
            if status {
                nemesis8::daemon::status(cli.port).await?;
                return Ok(());
            }
            if background {
                nemesis8::daemon::spawn_background(cli.port)?;
                return Ok(());
            }

            // Check if gateway is already running on this port
            let check_url = format!("http://127.0.0.1:{}/health", cli.port);
            if let Ok(resp) = reqwest::get(&check_url).await {
                if resp.status().is_success() {
                    eprintln!("Gateway is already running on port {}.", cli.port);
                    eprintln!("Access it at: http://localhost:{}", cli.port);
                    eprintln!("To restart, stop the existing gateway first.");
                    std::process::exit(1);
                }
            }
            ensure_image(&docker, &config).await?;
            drop(docker); // Gateway creates its own Docker connection
            let (role, controller_url, host_id) = match &config.control_plane {
                Some(cp) => (
                    cp.role.clone(),
                    cp.controller_url.clone(),
                    cp.host_id.clone(),
                ),
                None => ("controller".to_string(), None, None),
            };
            let gw_config = GatewayConfig {
                port: cli.port,
                config,
                workspace_root: workspace.to_string_lossy().to_string(),
                danger: cli.danger,
                model: cli.model.clone(),
                image: cli.tag.clone().unwrap_or_else(|| "nemesis8:latest".to_string()),
                role,
                controller_url,
                host_id,
                ..Default::default()
            };
            gateway::serve(gw_config).await?;
        }

        Command::Shell => {
            ensure_image(&docker, &config).await?;
            let ws = workspace.to_string_lossy();
            let env = docker.build_env(&config, false, None, None);
            let host_config = docker.build_host_config(&config, cli.privileged, Some(&ws));
            let image = docker.image_name().to_string();
            let privileged = cli.privileged;
            let runtime = docker.runtime_binary.clone();
            drop(docker);

            let args = nemesis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &["/bin/bash"]);
            let status = nemesis8::docker::run_it(&args, &runtime)?;
            if status != 0 {
                anyhow::bail!("shell exited with code {status}");
            }
        }

        Command::Attach { container } => match container {
            // Direct attach by name (back-compat).
            Some(name) => {
                let runtime = docker.runtime_binary.clone();
                drop(docker);
                attach_container_by_name(&runtime, &name)?;
            }
            // No arg → unified resume/attach picker.
            None => {
                let sessions = list_sessions_annotated(&config)?;
                let running = gather_running_agents(&docker, &sessions).await;
                let action = nemesis8::picker::pick_agent(running, sessions, false)?;
                dispatch_pick(
                    action, docker, config,
                    cli.danger, cli.privileged, cli.model.as_deref(), &workspace,
                )
                .await?;
            }
        },

        Command::Stop { container } => {
            if container == "all" {
                let image = docker.image_name().to_string();
                let containers = docker.list_containers(&image).await?;
                if containers.is_empty() {
                    println!("No running nemesis8 containers.");
                } else {
                    for c in &containers {
                        let id = c.id.as_deref().unwrap_or("");
                        let name = c.names.as_ref()
                            .and_then(|n| n.first())
                            .map(|n| n.trim_start_matches('/'))
                            .unwrap_or(id);
                        docker.stop_container(id).await?;
                        println!("Stopped: {name}");
                    }
                    println!("{} container(s) stopped.", containers.len());
                }
            } else {
                docker.stop_container(&container).await?;
                println!("Stopped: {container}");
            }
        }

        Command::Login => {
            ensure_image(&docker, &config).await?;
            let runtime = docker.runtime_binary.clone();
            let args = docker.into_login_args(&config)?;
            // docker is consumed/dropped — bollard connection closed
            let status = nemesis8::docker::run_it(&args, &runtime)?;
            if status != 0 {
                anyhow::bail!("login exited with code {}", status);
            }
        }

        // Bare `n8` / `n8 --danger` → home screen: + New session over the
        // resume/attach control room.
        Command::Home => {
            run_home(
                docker, config,
                cli.danger, cli.privileged, cli.model.as_deref(), &workspace,
            )
            .await?;
        }

        // Handled above before Docker connect — all return early, never reach here
        Command::Sessions { .. } | Command::Init | Command::Doctor | Command::Mount { .. } | Command::Mcp { .. } | Command::Update | Command::Agents { .. } => unreachable!(),

        Command::Ps => {
            let image = docker.image_name();
            let containers = docker.list_containers(image).await?;
            if containers.is_empty() {
                println!("No running nemesis8 containers.");
            } else {
                println!("{:<30} {:<20} {}", "NAME", "STATUS", "CREATED");
                println!("{}", "-".repeat(70));
                for c in &containers {
                    let name = c.names.as_ref()
                        .and_then(|n| n.first())
                        .map(|n| n.trim_start_matches('/'))
                        .unwrap_or("unknown");
                    let status = c.status.as_deref().unwrap_or("unknown");
                    let created = c.created.unwrap_or(0);
                    let created_dt = chrono::DateTime::from_timestamp(created, 0)
                        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    println!("{:<30} {:<20} {}", name, status, created_dt);
                }
                println!("\n{} container(s)", containers.len());
            }
        }

        Command::Pokeball { action } => {
            handle_pokeball(action, &docker).await?;
        }

        Command::Resume { id } => match id {
            // Direct resume by id (full UUID or first/last 5 chars).
            Some(session_id) => {
                run_resume(
                    docker, config,
                    cli.danger, cli.privileged, cli.model.as_deref(), &workspace,
                    &session_id, false,
                )
                .await?;
            }
            // No id → unified resume/attach picker (running containers + sessions).
            None => {
                let sessions = list_sessions_annotated(&config)?;
                let running = gather_running_agents(&docker, &sessions).await;
                let action = nemesis8::picker::pick_agent(running, sessions, false)?;
                dispatch_pick(
                    action, docker, config,
                    cli.danger, cli.privileged, cli.model.as_deref(), &workspace,
                )
                .await?;
            }
        },

    }

    Ok(())
}

/// Handle commands in remote mode, delegating to a remote gateway.
async fn run_remote(
    client: nemesis8::remote::RemoteClient,
    cli: Cli,
    _config: &Config,
) -> Result<()> {
    // Bare `n8` is a local-only home screen; in remote mode it falls to the
    // catch-all below ("not yet supported in remote mode").
    let command = cli.command.unwrap_or(Command::Home);
    match command {
        Command::Run { prompt } => {
            let output = client
                .run_prompt(&prompt, cli.model.as_deref(), cli.danger, None)
                .await?;
            println!("{output}");
        }

        Command::Sessions { query } => {
            let mut sessions = client.list_sessions().await?;
            if let Some(q) = &query {
                let q = q.to_lowercase();
                sessions.retain(|s| {
                    s["id"].as_str().unwrap_or("").to_lowercase().contains(&q)
                        || s["last_prompt"].as_str().unwrap_or("").to_lowercase().contains(&q)
                });
            }
            if sessions.is_empty() {
                println!("No sessions found.");
            } else {
                for s in &sessions {
                    let id = s["id"].as_str().unwrap_or("?");
                    let updated = s["updated"].as_str().unwrap_or("");
                    let prompt = s["last_prompt"].as_str().unwrap_or("");
                    let short = if prompt.len() > 60 {
                        format!("{}...", &prompt[..57])
                    } else {
                        prompt.to_string()
                    };
                    println!("{id}  {updated}  {short}");
                }
                println!("  ({} sessions)", sessions.len());
            }
        }

        Command::Resume { id } => {
            let id = id.ok_or_else(|| anyhow::anyhow!(
                "remote mode: pass a session id (the interactive picker only runs locally)"
            ))?;
            let session = client.get_session(&id).await?;
            let session_id = session["id"]
                .as_str()
                .unwrap_or(&id);
            eprintln!("Resuming session: {session_id}");
            let output = client
                .run_prompt("", cli.model.as_deref(), cli.danger, Some(session_id))
                .await?;
            println!("{output}");
        }

        Command::Doctor => {
            let health = client.health().await?;
            let status = client.status().await?;
            println!("Remote gateway health:");
            println!(
                "  status:  {}",
                health["status"].as_str().unwrap_or("unknown")
            );
            println!(
                "  version: {}",
                health["version"].as_str().unwrap_or("unknown")
            );
            println!();
            println!("Remote gateway status:");
            println!(
                "  active:         {}",
                status["active"].as_u64().unwrap_or(0)
            );
            println!(
                "  max_concurrent: {}",
                status["max_concurrent"].as_u64().unwrap_or(0)
            );
            println!(
                "  uptime_secs:    {}",
                status["uptime_secs"].as_u64().unwrap_or(0)
            );
            if let Some(sched) = status.get("scheduler") {
                println!(
                    "  triggers:       {}",
                    sched["trigger_count"].as_u64().unwrap_or(0)
                );
                println!(
                    "  enabled:        {}",
                    sched["enabled_count"].as_u64().unwrap_or(0)
                );
                if let Some(next) = sched["next_fire"].as_str() {
                    println!("  next_fire:      {next}");
                }
            }
        }

        Command::Init => {
            // Init doesn't need Docker or remote — handle locally
            let workspace = workspace_dir(cli.workspace.as_deref());
            init_config(&workspace)?;
        }

        Command::Build { .. } | Command::Shell | Command::Login | Command::Interactive => {
            eprintln!(
                "Error: '{}' requires local Docker and cannot run in remote mode.",
                match command {
                    Command::Build { .. } => "build",
                    Command::Shell => "shell",
                    Command::Login => "login",
                    Command::Interactive => "interactive",
                    _ => unreachable!(),
                }
            );
            eprintln!("Remove --remote / NEMESIS8_REMOTE to use local Docker.");
            std::process::exit(1);
        }

        Command::Serve { .. } => {
            eprintln!("Error: 'serve' IS the gateway server. It cannot delegate to a remote.");
            eprintln!("Remove --remote / NEMESIS8_REMOTE to start the server locally.");
            std::process::exit(1);
        }

        _ => {
            eprintln!("This command is not yet supported in remote mode.");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Load environment variables from env files.
/// Priority (later wins): ~/.nemesis8/env -> workspace .env -> workspace .*.env files
/// Fetch the latest release tag from GitHub. Returns the version string (without 'v' prefix)
/// or None if the check fails or times out.
/// True only for quick stdout commands where an async "update available"
/// notice won't race a TUI (home screen / pickers) or an interactive container
/// session. Everything that owns the terminal is excluded; it gets the notice
/// via the UI or an explicit `n8 update`.
fn update_notice_allowed(cmd: &Option<Command>) -> bool {
    matches!(
        cmd,
        Some(Command::Sessions { .. })
            | Some(Command::Ps)
            | Some(Command::Doctor)
            | Some(Command::Agents { .. })
            | Some(Command::Mcp { .. })
            | Some(Command::Mount { .. })
            | Some(Command::Init)
    )
}

async fn fetch_latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .user_agent(concat!("nemesis8/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.github.com/repos/DeepBlueDynamics/nemesis8/releases/latest")
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json["tag_name"].as_str()?;
    Some(tag.trim_start_matches('v').to_string())
}

async fn self_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    eprint!("Checking for updates... ");
    let latest = match fetch_latest_version().await {
        Some(v) => v,
        None => anyhow::bail!("Could not reach GitHub. Check your network connection."),
    };

    if latest == current {
        println!("already up to date (v{current})");
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    let cmd = "powershell -c \"irm https://nemesis8.nuts.services/install.ps1 | iex\"";
    #[cfg(not(target_os = "windows"))]
    let cmd = "curl -fsSL https://nemesis8.nuts.services/install.sh | sh";

    println!("update available: v{current} → v{latest}");
    println!("to upgrade, run:");
    println!("  {cmd}");

    Ok(())
}

fn load_env_files() {
    let mut files: Vec<std::path::PathBuf> = Vec::new();

    // 1. Global: ~/.nemesis8/env
    if let Some(home) = dirs::home_dir() {
        let global = home.join(".nemesis8").join("env");
        if global.is_file() {
            files.push(global);
        }
    }

    // 2. Workspace .env
    let cwd = std::env::current_dir().unwrap_or_default();
    let dot_env = cwd.join(".env");
    if dot_env.is_file() {
        files.push(dot_env);
    }

    // 3. Workspace .*.env files (e.g. .serpapi.env, .openai.env)
    if let Ok(entries) = std::fs::read_dir(&cwd) {
        let mut env_files: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.') && n.ends_with(".env") && n != ".env")
            })
            .collect();
        env_files.sort();
        files.extend(env_files);
    }

    for path in &files {
        if let Ok(content) = std::fs::read_to_string(path) {
            let mut count = 0;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    let value = value.trim().trim_matches('"').trim_matches('\'');
                    unsafe { std::env::set_var(key, value); }
                    count += 1;
                }
            }
            if count > 0 {
                tracing::info!("loaded {} env var(s) from {}", count, path.display());
            }
        }
    }
}

/// Check that the Dockerfile exists in the project directory
fn ensure_dockerfile() -> Result<()> {
    let context_dir = project_dir();
    if !context_dir.join("Dockerfile").is_file() {
        anyhow::bail!(
            "Dockerfile not found in {}. Set NEMESIS8_PROJECT_DIR or run from the project directory.",
            context_dir.display()
        );
    }
    Ok(())
}

/// Check if the Docker image exists; if not, auto-build it
async fn ensure_image(docker: &DockerOps, config: &Config) -> Result<()> {
    // Make sure the shared Docker network exists before any container runs.
    // Cheap idempotent check; safe to call every time.
    docker.ensure_network().await?;

    if docker.image_exists().await {
        return Ok(());
    }

    let image = docker.image_name();
    eprintln!("Image '{image}' not found locally — building now...");

    ensure_dockerfile()?;
    docker.build(&project_dir(), config.docker_build_args()).await?;

    eprintln!("Image built successfully.");
    Ok(())
}

/// Handle `n8 agents` — fleet control via the gateway HTTP API.
async fn handle_agents(
    action: Option<&nemesis8::cli::AgentsAction>,
    client: &nemesis8::remote::RemoteClient,
) -> Result<()> {
    use nemesis8::cli::AgentsAction;
    match action {
        None | Some(AgentsAction::List) => {
            let agents = client.list_agents().await?;
            if agents.is_empty() {
                println!("No agents.");
                return Ok(());
            }
            println!(
                "{:<30}  {:<11}  {:<10}  {:<11}  {}",
                "ID", "PROVIDER", "STATE", "SOURCE", "WORKSPACE"
            );
            println!("{}", "-".repeat(90));
            for a in &agents {
                println!(
                    "{:<30}  {:<11}  {:<10}  {:<11}  {}",
                    a.id,
                    a.provider.as_deref().unwrap_or("-"),
                    format!("{:?}", a.state).to_lowercase(),
                    format!("{:?}", a.source).to_lowercase(),
                    a.workspace.as_deref().unwrap_or("")
                );
            }
            println!("  ({} agents)", agents.len());
        }
        Some(AgentsAction::Kill { id }) => {
            let rec = client.kill_agent(id).await?;
            println!("killed {} (state now {:?})", rec.id, rec.state);
        }
        Some(AgentsAction::Spawn { prompt }) => {
            let resp = client.spawn_agent(prompt, None).await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
    }
    Ok(())
}

/// Handle pokeball subcommands
async fn handle_pokeball(action: PokeballAction, docker: &DockerOps) -> Result<()> {
    match action {
        PokeballAction::Capture { project } => {
            let (spec, _name) = pokeball::capture::capture_from_string(&project)?;
            let yaml = spec.to_yaml()?;
            println!("{yaml}");

            // Also save to store
            let store = pokeball::store::PokeballStore::open()?;
            let pokeball_dir = store.pokeball_dir(&spec.metadata.name);
            std::fs::create_dir_all(&pokeball_dir)?;
            spec.save(&pokeball_dir.join("pokeball.yaml"))?;
            eprintln!(
                "Saved to {}",
                pokeball_dir.join("pokeball.yaml").display()
            );
        }

        PokeballAction::Build { path } => {
            let store = pokeball::store::PokeballStore::open()?;

            // path can be a pokeball.yaml file or a directory containing one
            let spec_path = if std::path::Path::new(&path).is_file() {
                std::path::PathBuf::from(&path)
            } else {
                let p = std::path::Path::new(&path).join("pokeball.yaml");
                if p.is_file() {
                    p
                } else {
                    // Maybe it's a name in the store
                    store.spec_path(&path)
                }
            };

            let spec = pokeball::spec::PokeballSpec::load(&spec_path)?;
            eprintln!("Building pokeball '{}'...", spec.metadata.name);
            let tag = pokeball::build::build_pokeball(&spec, &store, &docker.runtime_binary).await?;
            println!("Built image: {tag}");
        }

        PokeballAction::Seal { project } => {
            let (spec, _name) = pokeball::capture::capture_from_string(&project)?;

            let store = pokeball::store::PokeballStore::open()?;
            let pokeball_dir = store.pokeball_dir(&spec.metadata.name);
            std::fs::create_dir_all(&pokeball_dir)?;
            spec.save(&pokeball_dir.join("pokeball.yaml"))?;
            eprintln!("Captured '{}'", spec.metadata.name);

            eprintln!("Building pokeball image...");
            let tag = pokeball::build::build_pokeball(&spec, &store, &docker.runtime_binary).await?;
            println!("Sealed: {tag}");
        }

        PokeballAction::Run { name, prompt } => {
            let store = pokeball::store::PokeballStore::open()?;
            let spec = store.load_spec(&name)?;

            let prompt = match prompt {
                Some(p) => p,
                None => {
                    eprintln!("No --prompt provided, entering interactive mode");
                    anyhow::bail!("interactive mode not yet implemented — use --prompt");
                }
            };

            // Ensure comms dirs
            store.ensure_comms(&name)?;
            let comms_dir = store.comms_dir(&name);

            // Start worker container
            let source_dir = spec.source.local_path();
            eprintln!("Starting pokeball worker for '{name}'...");
            let container_id = docker
                .run_pokeball_worker(
                    &spec.image_tag(),
                    &name,
                    &comms_dir.to_string_lossy(),
                    source_dir,
                    spec.resources.timeout_minutes,
                )
                .await?;

            eprintln!("Worker container: {}", &container_id[..12]);

            // Start broker
            let provider = pokeball::broker::AnthropicProvider::new(&spec)?;
            let mut broker = pokeball::broker::Broker::new(
                provider,
                comms_dir,
                spec.resources.timeout_minutes,
            );

            eprintln!("Running prompt: {prompt}");
            let result = broker.run(&prompt).await;

            // Always clean up container
            docker.stop_pokeball_worker(&container_id).await?;

            match result {
                Ok(text) => {
                    println!("{text}");
                }
                Err(e) => {
                    eprintln!("Pokeball run error: {e}");
                    std::process::exit(1);
                }
            }
        }

        PokeballAction::List => {
            let store = pokeball::store::PokeballStore::open()?;
            let pokeballs = store.list()?;

            if pokeballs.is_empty() {
                println!("No pokeballs found. Use 'nemesis8 pokeball capture <path>' to create one.");
                return Ok(());
            }

            println!("{:<20} {:<30} {}", "NAME", "IMAGE", "SPEC");
            println!("{}", "-".repeat(60));
            for pb in &pokeballs {
                let image = pb.image_tag.as_deref().unwrap_or("(not built)");
                let spec_status = if pb.has_spec { "yes" } else { "no" };
                println!("{:<20} {:<30} {}", pb.name, image, spec_status);
            }
        }

        PokeballAction::Inspect { name } => {
            let store = pokeball::store::PokeballStore::open()?;
            let spec = store.load_spec(&name)?;
            let yaml = spec.to_yaml()?;
            println!("{yaml}");
        }

        PokeballAction::Remove { name } => {
            let store = pokeball::store::PokeballStore::open()?;

            if !store.exists(&name) {
                eprintln!("Pokeball '{name}' not found");
                std::process::exit(1);
            }

            // Try to remove Docker image
            let spec = store.load_spec(&name).ok();
            if let Some(spec) = spec {
                let tag = spec.image_tag();
                eprintln!("Removing image {tag}...");
                let _ = docker
                    .docker()
                    .remove_image(&tag, None, None)
                    .await;
            }

            store.remove(&name)?;
            println!("Removed pokeball '{name}'");
        }

        PokeballAction::Publish { name, description } => {
            let store = pokeball::store::PokeballStore::open()?;
            let spec = store.load_spec(&name)?;
            let yaml = spec.to_yaml()?;
            let registry_url = std::env::var("POKEBALL_REGISTRY_URL")
                .unwrap_or_else(|_| "https://pokeball-registry-949870462453.us-central1.run.app".to_string());

            let body = serde_json::json!({
                "name": name,
                "spec": yaml,
                "submitter": whoami::username(),
                "description": description.unwrap_or_default(),
            });

            eprintln!("Publishing pokeball '{name}' to registry...");
            let client = reqwest::Client::new();
            let resp = client.post(format!("{registry_url}/submit"))
                .json(&body)
                .send()
                .await?;

            if resp.status().is_success() {
                let result: serde_json::Value = resp.json().await?;
                if let Some(url) = result.get("pr_url").and_then(|v| v.as_str()) {
                    println!("PR created: {url}");
                } else {
                    println!("Submitted: {}", serde_json::to_string_pretty(&result)?);
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("Registry error {status}: {body}");
            }
        }

        PokeballAction::Pull { name } => {
            let registry_url = std::env::var("POKEBALL_REGISTRY_URL")
                .unwrap_or_else(|_| "https://pokeball-registry-949870462453.us-central1.run.app".to_string());

            eprintln!("Pulling pokeball '{name}' from registry...");
            let client = reqwest::Client::new();
            let resp = client.get(format!("{registry_url}/pokeballs/{name}"))
                .send()
                .await?;

            if resp.status().is_success() {
                let result: serde_json::Value = resp.json().await?;
                if let Some(spec_str) = result.get("spec").and_then(|v| v.as_str()) {
                    let store = pokeball::store::PokeballStore::open()?;
                    let pokeball_dir = store.pokeball_dir(&name);
                    std::fs::create_dir_all(&pokeball_dir)?;
                    std::fs::write(pokeball_dir.join("pokeball.yaml"), spec_str)?;
                    println!("Pulled '{name}' to {}", pokeball_dir.display());
                    println!("Run: nemesis8 pokeball build {name}");
                } else {
                    eprintln!("Invalid response from registry");
                }
            } else if resp.status().as_u16() == 404 {
                eprintln!("Pokeball '{name}' not found in registry");
            } else {
                let body = resp.text().await.unwrap_or_default();
                eprintln!("Registry error: {body}");
            }
        }
    }

    Ok(())
}

/// True if a CLI responds to `--version` (installed + on PATH).
fn cli_present(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Prompt a yes/no question; defaults to yes on bare Enter. Returns false when
/// stdin isn't a TTY (non-interactive: never auto-install).
fn prompt_yes(question: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [Y/n] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let a = line.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

/// Gate every command that needs a container runtime. bollard's connect is lazy
/// — it never touches the daemon until the first API call — so without this the
/// user gets a cryptic failure deep inside `build`/`run` instead of being told
/// what to install. Uses the CLI probe (which makes a real daemon round-trip),
/// so it works for ANY runtime on ANY platform: Docker Desktop, a bare dockerd
/// (no Desktop), podman + podman machine, or Docker inside WSL2. Exits the
/// process with guidance if nothing usable is found.
fn preflight_runtime_or_exit() {
    let probe = pokeball::runner::detect_runtime();

    if !probe.available.is_empty() {
        // A Windows-native binary can't reach a dockerd that only lives inside
        // WSL (there's no Windows named pipe for it). If that's ALL we found,
        // say so up front rather than failing later inside bollard.
        #[cfg(target_os = "windows")]
        {
            use pokeball::runner::ContainerRuntime;
            let only_wsl = probe
                .available
                .iter()
                .all(|r| matches!(r, ContainerRuntime::Wsl2Docker { .. }));
            if only_wsl {
                if let Some(ContainerRuntime::Wsl2Docker { distro, .. }) = probe.available.first() {
                    eprintln!("Docker is only reachable inside WSL (distro '{distro}').");
                    eprintln!("The Windows nemesis8 binary can't talk to it directly.");
                    eprintln!();
                    eprintln!("Pick one:");
                    eprintln!("  - Docker Desktop with WSL integration (exposes a Windows pipe):");
                    eprintln!("      https://docs.docker.com/desktop/install/windows/");
                    eprintln!("  - Or run nemesis8 from inside that distro:  wsl -d {distro}");
                    std::process::exit(1);
                }
            }
        }
        return;
    }

    runtime_missing_help(&probe);
    std::process::exit(1);
}

/// Accurate, platform-aware guidance when no runtime responds. Distinguishes
/// "installed but not running" (just start it) from "not installed" (offer to
/// install Podman, the free/OSS default).
fn runtime_missing_help(probe: &pokeball::runner::RuntimeProbe) {
    let have_docker = cli_present("docker");
    let have_podman = cli_present("podman");

    eprintln!("No working container runtime found.");
    eprintln!();

    if have_docker || have_podman {
        // Installed but the daemon/machine isn't up.
        if have_docker {
            eprintln!("  Docker is installed but its daemon isn't responding.");
            #[cfg(target_os = "windows")]
            eprintln!("    Start Docker Desktop and wait until it reports 'running'.");
            #[cfg(target_os = "macos")]
            eprintln!("    Start Docker Desktop (or Colima) and wait until it's ready.");
            #[cfg(target_os = "linux")]
            eprintln!("    Start it:  sudo systemctl start docker");
        }
        if have_podman {
            eprintln!("  Podman is installed but no machine/socket is running.");
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            eprintln!("    Start its VM:  podman machine start   (first run: podman machine init && podman machine start)");
            #[cfg(target_os = "linux")]
            eprintln!("    Start its socket:  systemctl --user start podman.socket");
        }
        eprintln!();
        eprintln!("Then re-run. 'nemesis8 doctor' shows full diagnostics.");
        for e in &probe.errors {
            eprintln!("    ({e})");
        }
        return;
    }

    // Nothing installed → offer Podman.
    offer_install_podman();
    eprintln!();
    eprintln!("After installing, run 'nemesis8 doctor' to verify.");
}

/// Offer to install Podman (free/OSS). Prompts before acting; falls back to
/// printed instructions when non-interactive or the package manager is absent.
fn offer_install_podman() {
    #[cfg(target_os = "macos")]
    {
        if cli_present("brew") {
            if prompt_yes("Install Podman via Homebrew now?") {
                install_podman_brew();
                return;
            }
            eprintln!("  Install later:  brew install podman && podman machine start");
        } else {
            eprintln!("  Install Homebrew, then:  brew install podman && podman machine start");
            eprintln!("  Or Docker Desktop:  https://docs.docker.com/desktop/install/mac/");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if cli_present("winget") {
            if prompt_yes("Install Podman now via winget?") {
                install_podman_winget();
                return;
            }
            eprintln!("  Install later:  winget install -e --id RedHat.Podman");
            eprintln!("  then (new terminal):  podman machine init && podman machine start");
        } else {
            eprintln!("  Podman Desktop:  https://podman-desktop.io");
            eprintln!("  Or Docker Desktop:  https://docs.docker.com/desktop/install/windows/");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let id = os
            .lines()
            .find(|l| l.starts_with("ID="))
            .map(|l| l.trim_start_matches("ID=").trim_matches('"').to_lowercase())
            .unwrap_or_default();
        let cmd = match id.as_str() {
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => "sudo dnf install -y podman",
            "arch" | "manjaro" | "endeavouros" => "sudo pacman -S --noconfirm podman",
            _ => "sudo apt-get install -y podman",
        };
        eprintln!("  Install Podman:  {cmd}");
    }
}

/// Install Podman on Windows via winget. PATH won't refresh inside this process,
/// so we hand the machine-init/start back to the user in a fresh terminal.
#[cfg(target_os = "windows")]
fn install_podman_winget() {
    println!("Installing Podman via winget...");
    let ok = std::process::Command::new("winget")
        .args([
            "install",
            "-e",
            "--id",
            "RedHat.Podman",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("winget install failed. Try manually:  winget install -e --id RedHat.Podman");
        return;
    }
    println!("[OK] Podman installed.");
    println!("Open a NEW terminal, then run:");
    println!("    podman machine init && podman machine start");
    println!("...and re-run your nemesis8 command.");
}

/// Detect the container runtime and offer to install Podman if nothing is found.
/// Used by `nemesis8 init`.
fn detect_or_prompt_runtime() {
    let probe = pokeball::runner::detect_runtime();
    if !probe.available.is_empty() {
        println!(
            "[OK] container runtime available ({})",
            probe.recommended.as_deref().unwrap_or("detected")
        );
    } else {
        runtime_missing_help(&probe);
    }
}

/// Install Podman via Homebrew and start the Podman machine (macOS).
#[cfg(target_os = "macos")]
fn install_podman_brew() {
    println!("Installing Podman...");
    let brew_ok = std::process::Command::new("brew")
        .args(["install", "podman"])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !brew_ok {
        eprintln!("brew install podman failed. Try running it manually.");
        return;
    }

    // Check if a machine already exists
    let machine_exists = std::process::Command::new("podman")
        .args(["machine", "list", "--format", "{{.Name}}"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    if !machine_exists {
        println!("Initializing Podman machine (this takes ~1-2 minutes)...");
        let init_ok = std::process::Command::new("podman")
            .args(["machine", "init"])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !init_ok {
            eprintln!("podman machine init failed. Try running it manually.");
            return;
        }
    }

    println!("Starting Podman machine...");
    let start_ok = std::process::Command::new("podman")
        .args(["machine", "start"])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if start_ok {
        println!("[OK] Podman is running.");
    } else {
        eprintln!("podman machine start failed. Try running it manually.");
    }
}

/// Scaffold a .nemesis8.toml config in the target directory
fn init_config(workspace: &Path) -> Result<()> {
    detect_or_prompt_runtime();
    println!();

    let config_path = workspace.join(".nemesis8.toml");
    if config_path.exists() {
        eprintln!("Config already exists: {}", config_path.display());
        eprintln!("Edit it directly or delete it to re-initialize.");
        return Ok(());
    }

    let dir_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());

    let template = format!(
        r#"# nemesis8 config for: {dir_name}

# MCP tools (leave empty to discover all available).
# File read/write/edit/search/diff is the built-in `nuts-files` server (always
# on, no entry needed) — it replaced the gnosis-files-* tools.
mcp_tools = [
    "grub-crawler.py",
    "serpapi-search.py",
    "calculate.py",
    "time-tool.py",
    "tool-manager.py",
    "nemesis8-orchestrator.py",
    "ask.py",
    "hyperia-mcp.py",
]

[env]
# env_imports = ["SERPAPI_API_KEY"]
HYPERIA_URL = "http://host.docker.internal:9800"

[integrations]
hyperia = true
# ferricula = "http://nemesis:8764"

# [[mounts]]
# host = "C:/Users/you/data"
# container = "/workspace/data"
"#
    );

    std::fs::write(&config_path, &template)?;
    println!("Created {}", config_path.display());
    println!("Edit this file to configure MCP tools, mounts, and environment variables.");
    Ok(())
}

/// Handle mount subcommands: add, remove, list
fn handle_mount(action: &MountAction, workspace: &Path) -> Result<()> {
    let config_path = workspace.join(".nemesis8.toml");
    if !config_path.is_file() {
        // Check parent directories
        let mut dir = workspace.parent();
        let mut found = None;
        while let Some(d) = dir {
            let p = d.join(".nemesis8.toml");
            if p.is_file() {
                found = Some(p);
                break;
            }
            dir = d.parent();
        }
        if found.is_none() {
            anyhow::bail!("No .nemesis8.toml found. Run 'nemesis8 init' first.");
        }
    }

    let search_path = {
        let mut dir = Some(workspace.to_path_buf());
        let mut result = config_path.clone();
        while let Some(d) = dir {
            let p = d.join(".nemesis8.toml");
            if p.is_file() {
                result = p;
                break;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        result
    };

    match action {
        MountAction::Add { host, container } => {
            let host_path = std::fs::canonicalize(host)
                .unwrap_or_else(|_| PathBuf::from(host));
            let dirname = host_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("mount");
            let container_path = container.clone()
                .unwrap_or_else(|| format!("/workspace/{dirname}"));

            let mut content = std::fs::read_to_string(&search_path)?;
            content.push_str(&format!(
                "\n[[mounts]]\nhost = \"{}\"\ncontainer = \"{}\"\n",
                host_path.display().to_string().replace('\\', "/"),
                container_path
            ));
            std::fs::write(&search_path, content)?;
            println!("Added mount: {} -> {}", host_path.display(), container_path);
        }
        MountAction::Remove { host } => {
            let content = std::fs::read_to_string(&search_path)?;
            let mut lines: Vec<&str> = content.lines().collect();
            let mut i = 0;
            let mut removed = false;
            while i < lines.len() {
                if lines[i].trim() == "[[mounts]]" {
                    // Check if next line has the host we want to remove
                    let block_start = i;
                    let mut block_end = i + 1;
                    let mut matches = false;
                    while block_end < lines.len() && !lines[block_end].trim().starts_with("[[") && !lines[block_end].trim().starts_with("[") {
                        if lines[block_end].contains(host) {
                            matches = true;
                        }
                        block_end += 1;
                    }
                    if matches {
                        for _ in block_start..block_end {
                            lines.remove(block_start);
                        }
                        removed = true;
                        continue;
                    }
                }
                i += 1;
            }
            if removed {
                std::fs::write(&search_path, lines.join("\n"))?;
                println!("Removed mount for: {host}");
            } else {
                println!("No mount found matching: {host}");
            }
        }
        MountAction::List => {
            let config = Config::load_or_default(&search_path);
            if config.mounts.is_empty() {
                println!("No mounts configured.");
            } else {
                println!("{:<50} {}", "HOST", "CONTAINER");
                println!("{}", "-".repeat(70));
                for m in &config.mounts {
                    println!("{:<50} {}", m.host, m.container);
                }
            }
        }
    }
    Ok(())
}

/// Parse `# requires: pkg1, pkg2` lines from a Python file header.
fn parse_requires(content: &str) -> Vec<String> {
    content
        .lines()
        .take(30) // only scan the header
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("# requires:") {
                Some(rest.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

fn handle_mcp(action: &McpAction, workspace: &Path, image_tag: Option<&str>) -> Result<()> {
    let codex_home = nemesis8::paths::data_home();
    let mcp_dir = codex_home.join("mcp");
    let packages_dir = codex_home.join("mcp-packages");
    std::fs::create_dir_all(&mcp_dir)?;

    // Find .nemesis8.toml
    let config_path = {
        let mut dir = Some(workspace.to_path_buf());
        let mut found = workspace.join(".nemesis8.toml");
        while let Some(d) = dir {
            let p = d.join(".nemesis8.toml");
            if p.is_file() { found = p; break; }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        found
    };

    match action {
        McpAction::Add { file, requires } => {
            if !file.is_file() {
                anyhow::bail!("File not found: {}", file.display());
            }
            let filename = file.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("invalid filename"))?
                .to_string();
            if !filename.ends_with(".py") {
                anyhow::bail!("MCP tools must be .py files");
            }

            let content = std::fs::read_to_string(&file)?;

            // Collect deps from file header + --requires flag
            let mut deps: Vec<String> = parse_requires(&content);
            for r in requires.iter() {
                for pkg in r.split(',') {
                    let pkg = pkg.trim().to_string();
                    if !pkg.is_empty() && !deps.contains(&pkg) {
                        deps.push(pkg);
                    }
                }
            }

            // Copy file to ~/.nemesis8/home/mcp/
            let dest = mcp_dir.join(&filename);
            std::fs::copy(&file, &dest)?;
            println!("Copied {} -> {}", file.display(), dest.display());

            // Install deps into ~/.nemesis8/home/mcp-packages/ via one-off container
            if !deps.is_empty() {
                std::fs::create_dir_all(&packages_dir)?;
                let image = image_tag.unwrap_or("nemesis8:latest");
                let codex_home_docker = nemesis8::docker::to_docker_path(&codex_home.display().to_string());
                println!("Installing deps: {}", deps.join(", "));
                let mut args = vec![
                    "run".to_string(), "--rm".to_string(),
                    format!("-v={codex_home_docker}:/opt/nemesis8:rw"),
                    image.to_string(),
                    "/opt/mcp-venv/bin/pip".to_string(),
                    "install".to_string(),
                    "--target=/opt/nemesis8/mcp-packages".to_string(),
                    "--quiet".to_string(),
                ];
                args.extend(deps.iter().cloned());
                let runtime = nemesis8::docker::detect_runtime_binary();
                let status = std::process::Command::new(runtime)
                    .args(&args)
                    .status()
                    .context("running container runtime for pip install")?;
                if !status.success() {
                    anyhow::bail!("pip install failed");
                }
                println!("Deps installed to {}", packages_dir.display());
            }

            // Update mcp_tools in .nemesis8.toml
            if config_path.is_file() {
                let toml_content = std::fs::read_to_string(&config_path)?;
                let mut doc = toml_content.parse::<toml_edit::DocumentMut>()
                    .context("parsing .nemesis8.toml")?;
                let tools = doc["mcp_tools"]
                    .or_insert(toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())))
                    .as_array_mut()
                    .context("mcp_tools must be an array")?;
                let already = tools.iter().any(|v: &toml_edit::Value| v.as_str() == Some(filename.as_str()));
                if !already {
                    tools.push(filename.as_str());
                    std::fs::write(&config_path, doc.to_string())?;
                    println!("Registered '{}' in mcp_tools", filename);
                } else {
                    println!("'{}' already in mcp_tools", filename);
                }
            } else {
                println!("No .nemesis8.toml found — add '{}' to mcp_tools manually", filename);
            }
        }

        McpAction::List => {
            let installed: Vec<_> = std::fs::read_dir(&mcp_dir)
                .map(|rd| rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map_or(false, |x| x == "py"))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect())
                .unwrap_or_default();
            if installed.is_empty() {
                println!("No MCP tools installed in {}", mcp_dir.display());
            } else {
                let config = Config::load_or_default(&config_path);
                println!("{:<40} {}", "TOOL", "REGISTERED");
                println!("{}", "-".repeat(50));
                for name in &installed {
                    let registered = config.mcp_tools.contains(name);
                    println!("{:<40} {}", name, if registered { "yes" } else { "no" });
                }
            }
        }

        McpAction::Remove { name } => {
            let dest = mcp_dir.join(&name);
            if dest.is_file() {
                std::fs::remove_file(&dest)?;
                println!("Removed {}", dest.display());
            } else {
                println!("Not found: {}", dest.display());
            }
            // Remove from mcp_tools
            if config_path.is_file() {
                let toml_content = std::fs::read_to_string(&config_path)?;
                let mut doc = toml_content.parse::<toml_edit::DocumentMut>()
                    .context("parsing .nemesis8.toml")?;
                if let Some(tools) = doc["mcp_tools"].as_array_mut() {
                    let idx = tools.iter().position(|v: &toml_edit::Value| v.as_str() == Some(name.as_str()));
                    if let Some(i) = idx {
                        tools.remove(i);
                        std::fs::write(&config_path, doc.to_string())?;
                        println!("Removed '{}' from mcp_tools", name);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolve session directories — always includes host default, plus any config dirs that exist
/// Build (session_dir, provider_name) pairs by expanding each provider's
/// session_dirs against the data home (~/.nemesis8/home). Used to annotate listings and
/// to detect which provider owns a given session at resume time.
/// List + provider-annotate all local sessions (the picker's resume targets).
fn list_sessions_annotated(config: &Config) -> Result<Vec<session::SessionInfo>> {
    let dirs = resolve_session_dirs(config);
    let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
    let mut sessions = session::list_sessions(&dir_refs)?;
    let dir_to_provider = provider_dir_map();
    session::annotate_providers(&mut sessions, &dir_to_provider);
    Ok(sessions)
}

/// Build the running-agent list (the picker's attach targets) from labeled
/// containers, each with its last log line so the picker shows what it was doing.
async fn gather_running_agents(
    docker: &DockerOps,
    sessions: &[session::SessionInfo],
) -> Vec<nemesis8::picker::RunningAgent> {
    let image = docker.image_name().to_string();
    let containers = docker.list_containers(&image).await.unwrap_or_default();
    let mut out = Vec::with_capacity(containers.len());
    for c in &containers {
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "?".to_string());
        let provider = c
            .labels
            .as_ref()
            .and_then(|l| l.get("nemesis8.provider"))
            .cloned()
            .unwrap_or_else(|| "?".to_string());
        let uptime = c.status.clone().unwrap_or_default();
        let last_log = match c.id.as_deref() {
            Some(id) => docker.last_log_line(id).await,
            None => String::new(),
        };
        // Workspace = the host source of the container's /workspace bind mount.
        let workspace = c.mounts.as_ref().and_then(|mounts| {
            mounts
                .iter()
                .find(|m| {
                    m.destination
                        .as_deref()
                        .map(|d| d == "/workspace" || d.starts_with("/workspace/"))
                        .unwrap_or(false)
                })
                .and_then(|m| m.source.clone())
                .filter(|s| !s.is_empty())
                // Translate the Docker mount source back to the native host
                // path (e.g. /c/Users/x → C:\Users\x), so it displays like the
                // Sessions tab AND matches the session workspace for correlation.
                .map(|s| nemesis8::docker::from_docker_path(&s))
        });
        // Best-effort session id: newest session in the same provider + workspace
        // (the live one is the most recently modified).
        let session_id = workspace.as_deref().and_then(|ws| {
            sessions
                .iter()
                .filter(|s| {
                    s.provider.as_deref() == Some(provider.as_str())
                        && s.workspace.as_deref() == Some(ws)
                })
                .max_by(|a, b| a.modified.cmp(&b.modified))
                .map(|s| s.id.clone())
        });
        out.push(nemesis8::picker::RunningAgent {
            name,
            provider,
            state: nemesis8::theme::AgentUiState::from_docker_status(&uptime),
            uptime,
            last_log,
            session_id,
            workspace,
        });
    }
    out
}

/// Attach the terminal to a running container by name (shells out to the runtime).
fn attach_container_by_name(runtime: &str, name: &str) -> Result<()> {
    // Same detach-keys remap as build_run_it_args: the default Ctrl+P/Ctrl+Q
    // chord swallows Ctrl+P (which agent TUIs use constantly) and a following
    // Ctrl+Q silently detaches — leaving the agent running while keystrokes
    // split between the dying attach and the shell ("half disconnected").
    let status = std::process::Command::new(runtime)
        .args(["attach", "--detach-keys=ctrl-^", name])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("attach exited with code {}", status.code().unwrap_or(1));
    }
    Ok(())
}

/// Execute a unified-picker result: attach to the chosen container, resume the
/// chosen session, or do nothing on cancel. Consumes `docker`/`config` since
/// both downstream paths take ownership.
async fn dispatch_pick(
    action: Option<nemesis8::picker::PickAction>,
    docker: DockerOps,
    config: Config,
    danger: bool,
    privileged: bool,
    model: Option<&str>,
    workspace: &std::path::Path,
) -> Result<()> {
    use nemesis8::picker::PickAction;
    match action {
        None => {
            println!("Cancelled.");
            Ok(())
        }
        Some(PickAction::Attach(name)) => {
            let runtime = docker.runtime_binary.clone();
            drop(docker);
            attach_container_by_name(&runtime, &name)
        }
        Some(PickAction::Resume { session, current_dir }) => {
            run_resume(
                docker, config, danger, privileged, model, workspace, &session.id, current_dir,
            )
            .await
        }
        // "+ New session" only originates from the home screen, which handles
        // it before delegating here (resume/attach pickers pass show_new=false).
        Some(PickAction::New) => unreachable!("PickAction::New is handled by run_home"),
    }
}

/// Home screen (bare `n8`): the unified picker with a "+ New session" entry on
/// top of the resume/attach control room. New → launcher → fresh interactive
/// session; everything else routes through dispatch_pick.
async fn run_home(
    docker: DockerOps,
    config: Config,
    danger: bool,
    privileged: bool,
    model: Option<&str>,
    workspace: &std::path::Path,
) -> Result<()> {
    use nemesis8::controlroom::Outcome;
    let sessions = list_sessions_annotated(&config)?;
    let running = gather_running_agents(&docker, &sessions).await;
    let providers: Vec<String> = nemesis8::provider_registry::ProviderRegistry::load()
        .names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Background refresher: re-gathers the running list every ~2s (or on
    // demand via the request channel) so the control room stays live without
    // blocking its draw loop (v3 design §4.5, stale-while-revalidate).
    let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (upd_tx, upd_rx) = std::sync::mpsc::channel();
    {
        let docker_bg = docker.clone();
        let sessions_bg = sessions.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    r = req_rx.recv() => { if r.is_none() { break; } }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
                let fresh = gather_running_agents(&docker_bg, &sessions_bg).await;
                if upd_tx.send(fresh).is_err() {
                    break; // control room exited
                }
            }
        });
    }
    // Model catalog for the new-session pulldown: background fetch with a
    // disk cache (~/.nemesis8/models-cache.json) honoring the endpoint's TTL,
    // so opening the modal never blocks and repeat opens don't re-hit the
    // endpoint. Degrades silently — no catalog → model field stays free-text.
    let (models_tx, models_rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        if let Some(cat) = fetch_model_catalog().await {
            let _ = models_tx.send(cat);
        }
    });
    let ctx = nemesis8::controlroom::Ctx {
        runtime: docker.runtime_binary.clone(),
        tools: config.mcp_tools.clone(),
        refresh_request: Some(req_tx),
        updates: Some(upd_rx),
        models: Some(models_rx),
    };
    match nemesis8::controlroom::run(running, sessions, providers, &config.provider.0, model, danger, ctx)? {
        None => {
            println!("Cancelled.");
            Ok(())
        }
        Some(Outcome::Attach(name)) => {
            let runtime = docker.runtime_binary.clone();
            drop(docker);
            attach_container_by_name(&runtime, &name)
        }
        Some(Outcome::Resume { session, current_dir }) => {
            run_resume(docker, config, danger, privileged, model, workspace, &session.id, current_dir)
                .await
        }
        Some(Outcome::NewSession { provider, model: sel_model, danger: sel_danger }) => {
            let mut cfg = config;
            cfg.provider = nemesis8::config::Provider(provider);
            run_new_interactive(docker, cfg, sel_danger, privileged, sel_model.as_deref(), workspace)
                .await
        }
    }
}

/// Launch a fresh interactive session with the chosen provider/model/danger,
/// mounting the current workspace. Mirrors `Command::Interactive`.
async fn run_new_interactive(
    docker: DockerOps,
    config: Config,
    danger: bool,
    privileged: bool,
    model: Option<&str>,
    workspace: &std::path::Path,
) -> Result<()> {
    ensure_image(&docker, &config).await?;
    let ws = workspace.to_string_lossy();
    let env = docker.build_env(&config, danger, model, None);
    let host_config = docker.build_host_config(&config, privileged, Some(&ws));
    let image = docker.image_name().to_string();
    let runtime = docker.runtime_binary.clone();
    let host_ws = ws.to_string();
    drop(docker);

    let mut cmd: Vec<&str> = vec!["nemesis8-entry", "--interactive"];
    if danger {
        cmd.push("--danger");
    }
    let args = nemesis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
    let before_sessions = snapshot_session_ids(&config);
    let status = nemesis8::docker::run_it(&args, &runtime)?;
    record_new_sessions(&config, &host_ws, &before_sessions);
    if status != 0 {
        anyhow::bail!("session exited with code {status}");
    }
    Ok(())
}

/// Resume a session interactively: ensure the image exists, auto-detect the
/// session's provider (so resuming a gemini/antigravity session doesn't launch
/// codex), then launch `nemesis8-entry --interactive` with the session id.
/// Consumes `docker` (dropped before the blocking run) and `config`.
async fn run_resume(
    docker: DockerOps,
    mut config: Config,
    danger: bool,
    privileged: bool,
    model: Option<&str>,
    workspace: &std::path::Path,
    session_id: &str,
    current_dir: bool,
) -> Result<()> {
    ensure_image(&docker, &config).await?;
    let dirs = resolve_session_dirs(&config);
    let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();

    let info = match session::find_session(session_id, &dir_refs)? {
        Some(info) => info,
        None => anyhow::bail!("No session found matching '{session_id}'"),
    };

    for (dir, name) in provider_dir_map() {
        if info.path.starts_with(&dir) {
            if config.provider.0 != name {
                println!(
                    "Detected session provider: {} (overriding config provider {})",
                    name, config.provider.0
                );
                config.provider = nemesis8::config::Provider(name);
            }
            break;
        }
    }

    // Resume in the session's ORIGINAL workspace by default ("cd to where it
    // was"); Ctrl+Enter / `.` (current_dir), or a missing/invalid original,
    // falls back to where n8 was launched from. SessionInfo.workspace is a host
    // path when recorded in the workspace index; guard with is_dir() so we
    // never try to mount a container-only cwd (e.g. /workspace).
    let ws_path: std::path::PathBuf = if current_dir {
        workspace.to_path_buf()
    } else {
        match info.workspace.as_deref() {
            Some(w) if std::path::Path::new(w).is_dir() => std::path::PathBuf::from(w),
            _ => workspace.to_path_buf(),
        }
    };
    let ws = ws_path.to_string_lossy();
    println!("Resuming session: {} (workspace: {ws})", info.id);
    let env = docker.build_env(&config, danger, model, Some(&info.id));
    let host_config = docker.build_host_config(&config, privileged, Some(&ws));
    let image = docker.image_name().to_string();
    let runtime = docker.runtime_binary.clone();
    drop(docker);

    let mut cmd: Vec<&str> = vec!["nemesis8-entry", "--interactive"];
    if danger {
        cmd.push("--danger");
    }
    let args = nemesis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
    let status = nemesis8::docker::run_it(&args, &runtime)?;
    if status != 0 {
        anyhow::bail!("resumed session exited with code {status}");
    }
    Ok(())
}

fn provider_dir_map() -> Vec<(String, String)> {
    let codex_service = nemesis8::paths::data_home();
    let registry = nemesis8::provider_registry::ProviderRegistry::load();
    let mut out = Vec::new();
    for def in registry.all() {
        let dirs = nemesis8::session::expand_session_dirs(
            &codex_service,
            &def.provider.hooks.session_dirs,
        );
        for d in dirs {
            out.push((d, def.provider.name.clone()));
        }
    }
    out
}

fn resolve_session_dirs(config: &Config) -> Vec<String> {
    let codex_service = nemesis8::paths::data_home();

    let registry = nemesis8::provider_registry::ProviderRegistry::load();
    let mut dirs: Vec<String> = registry
        .all()
        .flat_map(|def| nemesis8::session::expand_session_dirs(&codex_service, &def.provider.hooks.session_dirs))
        .collect();
    dirs.sort();
    dirs.dedup();

    if let Some(from_config) = config.env.vars.get("CODEX_GATEWAY_SESSION_DIRS") {
        for dir in from_config.split(',') {
            let dir = dir.trim();
            if !dir.is_empty() {
                dirs.push(dir.to_string());
            }
        }
    }

    dirs
}

/// After a container exits, scan for any new sessions and record their host workspace
/// Check integrations and set env vars for auto-discovery
fn check_integrations(config: &Config) {
    let integrations = &config.integrations;

    // Hyperia: check if sidecar is running on port 9800
    if integrations.hyperia == Some(true) {
        match std::net::TcpStream::connect_timeout(
            &"127.0.0.1:9800".parse().unwrap(),
            std::time::Duration::from_millis(200),
        ) {
            Ok(_) => {
                // Probe on loopback (the sidecar runs on THIS host), but the
                // value we publish is forwarded into containers via build_env —
                // and inside a container 127.0.0.1 is the container itself, not
                // the host. So hand consumers the container-reachable address.
                unsafe { std::env::set_var("HYPERIA_URL", "http://host.docker.internal:9800"); }
                tracing::info!("integration: Hyperia connected (port 9800)");
            }
            Err(_) => {
                tracing::debug!("integration: Hyperia not running (port 9800)");
            }
        }
    }

    // Ferricula: set URL if configured, verify reachable
    if let Some(ref url) = integrations.ferricula {
        unsafe { std::env::set_var("FERRICULA_URL", url); }
        // Quick health check
        match std::net::TcpStream::connect_timeout(
            &url.trim_start_matches("http://")
                .trim_start_matches("https://")
                .parse()
                .unwrap_or_else(|_| "127.0.0.1:8765".parse().unwrap()),
            std::time::Duration::from_millis(500),
        ) {
            Ok(_) => {
                tracing::info!("integration: ferricula connected ({url})");
            }
            Err(_) => {
                tracing::debug!("integration: ferricula not reachable ({url})");
            }
        }
    }
}

/// Fetch the model catalog for the new-session pulldown. Resolution order:
/// fresh disk cache (within the endpoint's ttl_seconds) → network (5s
/// timeout, result written to the cache) → stale disk cache → None.
/// Endpoint override: NEMESIS8_MODELS_URL.
async fn fetch_model_catalog() -> Option<nemesis8::controlroom::ModelCatalog> {
    use nemesis8::controlroom::ModelCatalog;
    let url = std::env::var("NEMESIS8_MODELS_URL")
        .unwrap_or_else(|_| "https://nemesis8.nuts.services/models".to_string());
    let cache_path = nemesis8::paths::nemesis_root().join("models-cache.json");

    // Fresh cache?
    if let (Ok(meta), Ok(text)) = (
        std::fs::metadata(&cache_path),
        std::fs::read_to_string(&cache_path),
    ) {
        if let Ok(cat) = serde_json::from_str::<ModelCatalog>(&text) {
            let ttl = if cat.ttl_seconds == 0 { 3600 } else { cat.ttl_seconds };
            let fresh = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|e| e.as_secs() < ttl)
                .unwrap_or(false);
            if fresh {
                return Some(cat);
            }
        }
    }

    // Network.
    let fetched: Option<ModelCatalog> = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .user_agent(concat!("nemesis8/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        let text = client.get(&url).send().await.ok()?.text().await.ok()?;
        let cat = serde_json::from_str::<ModelCatalog>(&text).ok()?;
        let _ = std::fs::write(&cache_path, &text);
        Some(cat)
    }
    .await;
    if fetched.is_some() {
        return fetched;
    }

    // Stale cache beats nothing.
    std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Provider-declared host auth preflight (TOML [provider.login.preflight]):
/// if the provider's env fallback isn't set and its auth file is missing on
/// the host, bail with the provider's hint. Providers without a preflight
/// block pass through untouched.
fn run_login_preflight(config: &Config) -> Result<()> {
    let registry = nemesis8::provider_registry::ProviderRegistry::load();
    let Some(def) = registry.get(&config.provider.0) else {
        return Ok(());
    };
    let Some(pf) = &def.provider.login.preflight else {
        return Ok(());
    };
    let env_ok = pf
        .env_fallback
        .as_deref()
        .map(|k| std::env::var(k).is_ok())
        .unwrap_or(false);
    if env_ok {
        return Ok(());
    }
    if let Some(rel) = &pf.file {
        let path = dirs::home_dir().unwrap_or_default().join(rel);
        if !path.is_file() {
            let hint = pf.hint.clone().unwrap_or_default();
            eprintln!(
                "[nemesis8] {} auth missing: {} not found on the host.",
                config.provider.0,
                path.display()
            );
            if !hint.is_empty() {
                eprintln!("[nemesis8] {hint}");
            }
            anyhow::bail!("{} auth required. {}", config.provider.0, hint);
        }
    }
    Ok(())
}

/// Snapshot the session ids that exist right now — call before launching a
/// container so `record_new_sessions` can tell which sessions the run created.
fn snapshot_session_ids(config: &Config) -> std::collections::HashSet<String> {
    let dirs = resolve_session_dirs(config);
    let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
    session::session_id_set(&dir_refs)
}

/// After a container exits, record the host workspace for the session(s) this
/// run *created* (ids not present in `before`). Recording only the new ids —
/// rather than every session missing a workspace — keeps binary providers
/// (antigravity `.pb`/`.db`, whose workspace lives only in the index) from
/// getting an unrelated run's workspace stamped onto old sessions.
/// Records the host workspace for sessions created this run, and returns the
/// id(s) that are new since `before` (newest-modified first) so the caller can
/// surface an `n8 resume` hint.
fn record_new_sessions(
    config: &Config,
    host_workspace: &str,
    before: &std::collections::HashSet<String>,
) -> Vec<String> {
    let dirs = resolve_session_dirs(config);
    let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
    let mut new_ids = Vec::new();
    if let Ok(sessions) = session::list_sessions(&dir_refs) {
        // list_sessions is newest-first.
        for s in &sessions {
            if before.contains(&s.id) {
                continue;
            }
            new_ids.push(s.id.clone());
            let needs_workspace =
                s.workspace.as_deref() == Some("/workspace") || s.workspace.is_none();
            if needs_workspace {
                session::record_session_workspace(&s.id, host_workspace);
            }
        }
    }
    new_ids
}

/// Print a provider-agnostic resume hint for the session a run just created.
/// `n8 resume <id>` works for every session-supporting provider (the picker
/// resolves it via each provider's resume_flag/subcommand).
fn print_resume_hint(new_ids: &[String], danger: bool) {
    if let Some(id) = new_ids.first() {
        let danger_flag = if danger { " --danger" } else { "" };
        eprintln!("[nemesis8] resume this session:  n8{danger_flag} resume {id}");
    }
}
