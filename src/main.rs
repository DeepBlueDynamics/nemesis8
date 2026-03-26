use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};

use nemisis8::cli::{Cli, Command, MountAction, PokeballAction};
use nemisis8::config::Config;
use nemisis8::docker::DockerOps;
use nemisis8::gateway::{self, GatewayConfig};
use nemisis8::pokeball;
use nemisis8::session;

/// The nemisis8 project root baked in at compile time.
/// This is where Dockerfile, MCP/, and other project files live.
const PROJECT_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Resolve the nemisis8 project directory (Dockerfile, MCP/, etc.)
/// Priority: NEMISIS8_PROJECT_DIR env > compile-time CARGO_MANIFEST_DIR
fn project_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NEMISIS8_PROJECT_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(PROJECT_DIR)
}

/// Resolve the user's workspace directory (mounted as /workspace in container).
/// Priority: --workspace flag > CWD
fn workspace_dir(flag: Option<&str>) -> PathBuf {
    if let Some(ws) = flag {
        return PathBuf::from(ws);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Load config by searching upward from workspace, then falling back to project dir.
fn load_config(workspace: &Path) -> Config {
    // First try to find config searching upward from workspace
    if let Some(found) = Config::find(workspace) {
        if let Ok(config) = Config::load(&found) {
            tracing::info!(path = %found.display(), "loaded config");
            return config;
        }
    }

    // Fall back to project dir config
    let project_config = project_dir().join(".codex-container.toml");
    if project_config.is_file() {
        if let Ok(config) = Config::load(&project_config) {
            tracing::info!(path = %project_config.display(), "loaded config from project dir");
            return config;
        }
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

    let workspace = workspace_dir(cli.workspace.as_deref());
    let ws_arg = if cli.no_mount { None } else { Some(workspace.to_string_lossy().to_string()) };
    let mut config = load_config(&workspace);

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
        Command::Sessions => {
            // 1. Local sessions from host filesystem
            let dirs = resolve_session_dirs(&config);
            let dir_refs: Vec<&str> = dirs.iter().map(|s| s.as_str()).collect();
            match session::list_sessions(&dir_refs) {
                Ok(sessions) if !sessions.is_empty() => {
                    println!("Local sessions:");
                    session::print_sessions(&sessions);
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
        Command::Ps => {
            // Handled after Docker connect below
        }
        _ => {}
    }

    // Connect to Docker — give a friendly error if it's not available
    let docker = match DockerOps::new(cli.tag.as_deref()) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("Error: Could not connect to Docker.");
            eprintln!();
            eprintln!("nemesis8 requires Docker to run containers. Install it:");
            eprintln!();
            eprintln!("  Windows:  https://docs.docker.com/desktop/install/windows/");
            eprintln!("  macOS:    https://docs.docker.com/desktop/install/mac/");
            eprintln!("            or: brew install colima && colima start");
            eprintln!("  Linux:    sudo apt install docker.io   (Ubuntu/Debian)");
            eprintln!("            sudo dnf install docker       (Fedora)");
            eprintln!();
            eprintln!("Make sure Docker is running, then try again.");
            eprintln!("Run 'nemesis8 doctor' for a full diagnostic.");
            std::process::exit(1);
        }
    };

    match cli.command {
        Command::Build => {
            ensure_dockerfile()?;
            docker.build(&project_dir()).await?;
            println!("Image built successfully.");
        }

        Command::Run { prompt } => {
            ensure_image(&docker).await?;
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
            ensure_image(&docker).await?;
            let env = docker.build_env(&config, cli.danger, cli.model.as_deref(), None);
            let host_config = docker.build_host_config(&config, cli.privileged, ws_arg.as_deref());
            let image = docker.image_name().to_string();
            let privileged = cli.privileged;
            let danger = cli.danger;
            let host_ws = workspace.to_string_lossy().to_string();
            drop(docker);

            let mut cmd: Vec<&str> = vec!["nemisis8-entry", "--interactive"];
            if danger { cmd.push("--danger"); }
            let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
            let status = nemisis8::docker::run_it(&args)?;
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
            ensure_image(&docker).await?;
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
            ensure_image(&docker).await?;
            let ws = workspace.to_string_lossy();
            let env = docker.build_env(&config, false, None, None);
            let host_config = docker.build_host_config(&config, cli.privileged, Some(&ws));
            let image = docker.image_name().to_string();
            let privileged = cli.privileged;
            drop(docker);

            let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &["/bin/bash"]);
            let status = nemisis8::docker::run_it(&args)?;
            if status != 0 {
                anyhow::bail!("shell exited with code {status}");
            }
        }

        Command::Attach { container } => {
            drop(docker);
            let status = std::process::Command::new("docker")
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
            ensure_image(&docker).await?;
            let args = docker.into_login_args(&config)?;
            // docker is consumed/dropped — bollard connection closed
            let status = nemisis8::docker::run_it(&args)?;
            if status != 0 {
                anyhow::bail!("login exited with code {}", status);
            }
        }

        // Handled above before Docker connect
        Command::Sessions | Command::Init | Command::Doctor | Command::Mount { .. } => unreachable!(),

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
            ensure_image(&docker).await?;
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
                    drop(docker);

                    let mut cmd: Vec<&str> = vec!["nemisis8-entry", "--interactive"];
                    if danger { cmd.push("--danger"); }
                    let args = nemisis8::docker::build_run_it_args(&image, &env, &host_config, privileged, &cmd);
                    let status = nemisis8::docker::run_it(&args)?;
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

        Command::Sessions => {
            let sessions = client.list_sessions().await?;
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

        Command::Build | Command::Shell | Command::Login | Command::Interactive => {
            eprintln!(
                "Error: '{}' requires local Docker and cannot run in remote mode.",
                match cli.command {
                    Command::Build => "build",
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
                    std::env::set_var(key, value);
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
async fn ensure_image(docker: &DockerOps) -> Result<()> {
    if docker.image_exists().await {
        return Ok(());
    }

    let image = docker.image_name();
    eprintln!("Image '{image}' not found locally — building now...");

    ensure_dockerfile()?;
    docker.build(&project_dir()).await?;

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
            let tag = pokeball::build::build_pokeball(&spec, &store).await?;
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
            let tag = pokeball::build::build_pokeball(&spec, &store).await?;
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
    }

    Ok(())
}

/// Scaffold a .codex-container.toml config in the target directory
fn init_config(workspace: &Path) -> Result<()> {
    let config_path = workspace.join(".codex-container.toml");
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
        r#"# nemisis8 container config for: {dir_name}
# Generated by: nemisis8 init

# Workspace mount behavior: "root" (default) or "named"
workspace_mount_mode = "root"

# MCP tools to enable inside the container (filenames from MCP/ directory)
# mcp_tools = ["calculate.py", "time-tool.py"]

[env]
# Environment variables injected into the container
# MY_API_URL = "https://api.example.com"

# Import host env vars into the container if they exist
env_imports = []

# Extra volume mounts
# [[mounts]]
# host = "C:/Users/you/data"
# container = "/workspace/data"
# mode = "ro"
"#
    );

    std::fs::write(&config_path, &template)?;
    println!("Created {}", config_path.display());
    println!("Edit this file to configure MCP tools, mounts, and environment variables.");
    Ok(())
}

/// Handle mount subcommands: add, remove, list
fn handle_mount(action: &MountAction, workspace: &Path) -> Result<()> {
    let config_path = workspace.join(".codex-container.toml");
    if !config_path.is_file() {
        // Check parent directories
        let mut dir = workspace.parent();
        let mut found = None;
        while let Some(d) = dir {
            let p = d.join(".codex-container.toml");
            if p.is_file() {
                found = Some(p);
                break;
            }
            dir = d.parent();
        }
        if found.is_none() {
            anyhow::bail!("No .codex-container.toml found. Run 'nemesis8 init' first.");
        }
    }

    let search_path = {
        let mut dir = Some(workspace.to_path_buf());
        let mut result = config_path.clone();
        while let Some(d) = dir {
            let p = d.join(".codex-container.toml");
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

/// Resolve session directories — always includes host default, plus any config dirs that exist
fn resolve_session_dirs(config: &Config) -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let default_dir = home.join(".codex-service/.codex/sessions");

    let mut dirs = vec![default_dir.to_string_lossy().to_string()];

    // Add config-specified dirs only if they exist on the host filesystem
    if let Some(from_config) = config.env.vars.get("CODEX_GATEWAY_SESSION_DIRS") {
        for dir in from_config.split(',') {
            let dir = dir.trim();
            if !dir.is_empty() && std::path::Path::new(dir).is_dir() {
                dirs.push(dir.to_string());
            }
        }
    }

    dirs
}

/// After a container exits, scan for any new sessions and record their host workspace
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
