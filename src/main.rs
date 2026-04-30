use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

use nemisis8::cli::{Cli, Command, McpAction, MountAction, PokeballAction};
use nemisis8::config::Config;
use nemisis8::docker::{DockerOps, DOCKER_CONNECTIVITY_ADVICE, is_docker_connectivity_error};
use nemisis8::gateway::{self, GatewayConfig};
use nemisis8::pokeball;
use nemisis8::session;

/// Resolve the nemesis8 project directory (Dockerfile, MCP/, etc.)
fn project_dir() -> PathBuf {
    nemisis8::project_dir_fn()
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
                .unwrap_or_else(|_| "nemisis8=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Load env files before anything else
    load_env_files();

    // Non-blocking version check — fires and forgets, prints warning if behind
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

    let workspace = workspace_dir(cli.workspace.as_deref());
    let ws_arg = if cli.no_mount { None } else { Some(workspace.to_string_lossy().to_string()) };
    let mut config = load_config(&workspace);

    // Auto-discover integrations
    check_integrations(&config);

    // CLI --provider flag overrides config file
    if let Some(ref p) = cli.provider {
        match p.parse::<nemisis8::config::Provider>() {
            Ok(provider) => config.provider = provider,
            Err(e) => anyhow::bail!(e),
        }
    }

    // Resolve remote URL: CLI flag > config file
    let remote_url = cli.remote.as_deref().or(config.remote.as_deref());

    if let Some(url) = remote_url {
        let token = cli.token.as_deref().or(config.remote_token.as_deref());
        let client = nemisis8::remote::RemoteClient::new(url, token);
        return run_remote(client, cli, &config).await;
    }

    // Commands that don't need Docker
    match &cli.command {
        Command::Sessions { query } => {
            // 1. Local sessions from host filesystem
            let dirs = resolve_session_dirs(&config);
            let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
            match session::list_sessions(&dir_refs) {
                Ok(sessions) if !sessions.is_empty() => {
                    println!("Local sessions:");
                    session::print_sessions(&sessions, query.as_deref());
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

    match cli.command {
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

            // Pre-flight: check Gemini OAuth creds exist on host
            if config.provider.0 == "gemini"
                && std::env::var("GEMINI_API_KEY").is_err()
            {
                let host_creds = dirs::home_dir()
                    .map(|h| h.join(".gemini/oauth_creds.json"))
                    .unwrap_or_default();
                if !host_creds.is_file() {
                    eprintln!("[nemesis8] No Gemini OAuth credentials found at ~/.gemini/oauth_creds.json");
                    eprintln!("[nemesis8] Please run 'gemini auth login' on the host first, or set GEMINI_API_KEY.");
                    anyhow::bail!("Gemini auth required. Run 'gemini auth login' on the host or set GEMINI_API_KEY.");
                }
            }

            let env = docker.build_env(&config, cli.danger, cli.model.as_deref(), None);
            let host_config = docker.build_host_config(&config, cli.privileged, ws_arg.as_deref());
            let image = docker.image_name().to_string();
            let privileged = cli.privileged;
            let danger = cli.danger;
            let host_ws = workspace.to_string_lossy().to_string();
            let runtime = docker.runtime_binary.clone();
            drop(docker);

            let mut cmd: Vec<&str> = vec!["nemisis8-entry", "--interactive"];
            if danger { cmd.push("--danger"); }
            let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
            let status = nemisis8::docker::run_it(&args, &runtime)?;
            // Record any new sessions with the host workspace
            record_new_sessions(&config, &host_ws);
            if status != 0 {
                anyhow::bail!("interactive session exited with code {status}");
            }
        }

        Command::Serve => {
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
            let gw_config = GatewayConfig {
                port: cli.port,
                config,
                workspace_root: workspace.to_string_lossy().to_string(),
                danger: cli.danger,
                model: cli.model.clone(),
                image: cli.tag.clone().unwrap_or_else(|| "nemisis8:latest".to_string()),
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

            let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &["/bin/bash"]);
            let status = nemisis8::docker::run_it(&args, &runtime)?;
            if status != 0 {
                anyhow::bail!("shell exited with code {status}");
            }
        }

        Command::Attach { container } => {
            let runtime = docker.runtime_binary.clone();
            drop(docker);
            let status = std::process::Command::new(&runtime)
                .args(["attach", &container])
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()?;
            if !status.success() {
                anyhow::bail!("attach exited with code {}", status.code().unwrap_or(1));
            }
        }

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
            let status = nemisis8::docker::run_it(&args, &runtime)?;
            if status != 0 {
                anyhow::bail!("login exited with code {}", status);
            }
        }

        // Handled above before Docker connect — all return early, never reach here
        Command::Sessions { .. } | Command::Init | Command::Doctor | Command::Mount { .. } | Command::Mcp { .. } | Command::Update => unreachable!(),

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

        Command::Resume { id } => {
            ensure_image(&docker, &config).await?;
            let dirs = resolve_session_dirs(&config);
            let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();

            match session::find_session(&id, &dir_refs) {
                Ok(Some(info)) => {
                    println!("Resuming session: {}", info.id);
                    let ws = workspace.to_string_lossy();
                    // Resume launches interactively (with TTY), not in exec mode
                    let env = docker.build_env(&config, cli.danger, cli.model.as_deref(), Some(&info.id));
                    let host_config = docker.build_host_config(&config, cli.privileged, Some(&ws));
                    let image = docker.image_name().to_string();
                    let privileged = cli.privileged;
                    let danger = cli.danger;
                    let runtime = docker.runtime_binary.clone();
                    drop(docker);

                    let mut cmd: Vec<&str> = vec!["nemisis8-entry", "--interactive"];
                    if danger { cmd.push("--danger"); }
                    let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
                    let status = nemisis8::docker::run_it(&args, &runtime)?;
                    if status != 0 {
                        anyhow::bail!("resumed session exited with code {status}");
                    }
                }
                Ok(None) => {
                    eprintln!("No session found matching '{id}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Error finding session: {e}");
                    std::process::exit(1);
                }
            }
        }

    }

    Ok(())
}

/// Handle commands in remote mode, delegating to a remote gateway.
async fn run_remote(
    client: nemisis8::remote::RemoteClient,
    cli: Cli,
    _config: &Config,
) -> Result<()> {
    match cli.command {
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
            // Verify the session exists on the remote
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
                match cli.command {
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

        Command::Serve => {
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
            "Dockerfile not found in {}. Set NEMISIS8_PROJECT_DIR or run from the project directory.",
            context_dir.display()
        );
    }
    Ok(())
}

/// Check if the Docker image exists; if not, auto-build it
async fn ensure_image(docker: &DockerOps, config: &Config) -> Result<()> {
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
                println!("No pokeballs found. Use 'nemisis8 pokeball capture <path>' to create one.");
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

/// Detect the container runtime and offer to install Podman if nothing is found.
fn detect_or_prompt_runtime() {
    match DockerOps::new(None) {
        Ok(d) => {
            println!("[OK] {} detected", d.runtime_binary);
        }
        Err(_) => {
            eprintln!("No container runtime found.");
            eprintln!();

            #[cfg(target_os = "macos")]
            {
                // Check for Homebrew
                let has_brew = std::process::Command::new("brew")
                    .arg("--version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if has_brew {
                    use std::io::{IsTerminal, Write};
                    if std::io::stdin().is_terminal() {
                        print!("Install Podman via Homebrew? [Y/n] ");
                        std::io::stdout().flush().ok();
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line).ok();
                        let answer = line.trim().to_lowercase();
                        if answer.is_empty() || answer == "y" || answer == "yes" {
                            install_podman_brew();
                            return;
                        }
                    } else {
                        eprintln!("  Homebrew detected. To install Podman:");
                        eprintln!("    brew install podman && podman machine start");
                    }
                } else {
                    eprintln!("  macOS: install Homebrew first, then:");
                    eprintln!("    brew install podman && podman machine start");
                    eprintln!("  Or: https://docs.docker.com/desktop/install/mac/");
                }
            }

            #[cfg(target_os = "linux")]
            {
                let distro = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
                let id = distro.lines()
                    .find(|l| l.starts_with("ID="))
                    .map(|l| l.trim_start_matches("ID=").trim_matches('"').to_lowercase())
                    .unwrap_or_default();
                let cmd = match id.as_str() {
                    "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => "sudo dnf install -y podman",
                    "arch" | "manjaro" | "endeavouros" => "sudo pacman -S podman",
                    _ => "sudo apt install -y podman",
                };
                eprintln!("  Linux: {cmd}");
            }

            #[cfg(target_os = "windows")]
            {
                eprintln!("  Docker Desktop: https://docs.docker.com/desktop/install/windows/");
                eprintln!("  Podman Desktop: https://podman-desktop.io");
            }

            eprintln!();
            eprintln!("After installing, run 'nemesis8 init' again to verify.");
        }
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

# MCP tools (leave empty to discover all available)
mcp_tools = [
    "gnosis-files-basic.py",
    "gnosis-files-search.py",
    "gnosis-code-scan.py",
    "grub-crawler.py",
    "serpapi-search.py",
    "calculate.py",
    "time-tool.py",
    "tool-manager.py",
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
    let codex_home = dirs::home_dir()
        .map(|h| h.join(".codex-service"))
        .unwrap_or_else(|| PathBuf::from("/tmp/.codex-service"));
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

            // Copy file to ~/.codex-service/mcp/
            let dest = mcp_dir.join(&filename);
            std::fs::copy(&file, &dest)?;
            println!("Copied {} -> {}", file.display(), dest.display());

            // Install deps into ~/.codex-service/mcp-packages/ via one-off container
            if !deps.is_empty() {
                std::fs::create_dir_all(&packages_dir)?;
                let image = image_tag.unwrap_or("nemisis8:latest");
                let codex_home_docker = nemisis8::docker::to_docker_path(&codex_home.display().to_string());
                println!("Installing deps: {}", deps.join(", "));
                let mut args = vec![
                    "run".to_string(), "--rm".to_string(),
                    format!("-v={codex_home_docker}:/opt/codex-home:rw"),
                    image.to_string(),
                    "/opt/mcp-venv/bin/pip".to_string(),
                    "install".to_string(),
                    "--target=/opt/codex-home/mcp-packages".to_string(),
                    "--quiet".to_string(),
                ];
                args.extend(deps.iter().cloned());
                let runtime = nemisis8::docker::detect_runtime_binary();
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
fn resolve_session_dirs(config: &Config) -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let codex_service = home.join(".codex-service");

    let registry = nemisis8::provider_registry::ProviderRegistry::load();
    let mut dirs: Vec<String> = registry
        .all()
        .flat_map(|def| nemisis8::session::expand_session_dirs(&codex_service, &def.provider.hooks.session_dirs))
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
                unsafe { std::env::set_var("HYPERIA_URL", "http://127.0.0.1:9800"); }
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

fn record_new_sessions(config: &Config, host_workspace: &str) {
    let dirs = resolve_session_dirs(config);
    let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
    if let Ok(sessions) = session::list_sessions(&dir_refs) {
        // Record all sessions that don't have a workspace mapping yet
        for s in &sessions {
            if s.workspace.as_deref() == Some("/workspace") || s.workspace.is_none() {
                session::record_session_workspace(&s.id, host_workspace);
            }
        }
    }
}
