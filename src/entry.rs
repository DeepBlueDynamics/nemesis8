//! nemisis8-entry: container entry-point binary
//!
//! This binary runs INSIDE the Docker container. It handles:
//! - MCP server installation from /opt/mcp-source to /opt/codex-home/mcp
//! - Provider-specific config generation (Codex config.toml / Gemini settings.json)
//! - API key resolution chain
//! - Danger mode flag injection
//! - Launching the configured AI CLI (codex or gemini)

use std::path::{Path, PathBuf};
use std::process::Command;

use nemisis8::config::{self, Config, Provider};

const MCP_SOURCE: &str = "/opt/mcp-source";
const MCP_INSTALL: &str = "/opt/codex-home/mcp";
const MCP_VENV_PYTHON: &str = "/opt/mcp-venv/bin/python3";
const CODEX_HOME: &str = "/opt/codex-home";
const CODEX_CONFIG_DIR: &str = "/opt/codex-home/.codex";
const WORKSPACE_ROOT: &str = "/workspace";

fn main() {
    // Parse entry args
    let args: Vec<String> = std::env::args().collect();
    let mut prompt: Option<String> = None;
    let mut interactive = false;
    let mut danger = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--prompt" => {
                i += 1;
                if i < args.len() {
                    prompt = Some(args[i].clone());
                }
            }
            "--interactive" => interactive = true,
            "--danger" => danger = true,
            _ => {
                // Treat bare args as prompt
                if prompt.is_none() {
                    prompt = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }

    // Load config
    let config_path = PathBuf::from(WORKSPACE_ROOT).join(".codex-container.toml");
    let config = Config::load_or_default(&config_path);

    // Determine provider: env var override > config file
    let provider = std::env::var("NEMISIS8_PROVIDER")
        .ok()
        .and_then(|s| s.parse::<Provider>().ok())
        .unwrap_or(config.provider);

    // Load session env if CODEX_SESSION_ID is set
    if let Ok(session_id) = std::env::var("CODEX_SESSION_ID") {
        load_session_env(&session_id);
    }

    // Install MCP servers (shared between providers)
    if let Err(e) = install_mcp_servers(&config) {
        eprintln!("warning: MCP server install failed: {e}");
    }

    // Generate provider-specific config
    match provider {
        Provider::Codex => {
            if let Err(e) = write_codex_config(&config) {
                eprintln!("warning: Codex config generation failed: {e}");
            }
        }
        Provider::Gemini => {
            if let Err(e) = write_gemini_config(&config) {
                eprintln!("warning: Gemini config generation failed: {e}");
            }
        }
    }

    // Update CLI version if configured
    if provider == Provider::Codex {
        update_codex_cli(&config);
    }

    // Resolve API key
    resolve_api_key(provider);

    // Launch the configured CLI
    let status = match provider {
        Provider::Codex => run_codex(prompt.as_deref(), interactive, danger),
        Provider::Gemini => run_gemini(prompt.as_deref(), interactive, danger),
    };
    std::process::exit(status);
}

/// Load env vars from a session-specific .env file
fn load_session_env(session_id: &str) {
    let session_env_paths = [
        format!("{CODEX_HOME}/sessions/{session_id}/.env"),
        format!("{CODEX_HOME}/.codex/sessions/{session_id}/.env"),
    ];

    for path in &session_env_paths {
        let p = Path::new(path);
        if p.is_file() {
            if let Ok(content) = std::fs::read_to_string(p) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        std::env::set_var(key.trim(), value.trim().trim_matches('"'));
                    }
                }
                eprintln!("[nemisis8-entry] loaded session env from {path}");
                return;
            }
        }
    }
}

/// Install MCP servers from source to the codex home mcp directory
fn install_mcp_servers(config: &Config) -> anyhow::Result<()> {
    let source = Path::new(MCP_SOURCE);
    let dest = Path::new(MCP_INSTALL);

    if !source.is_dir() {
        anyhow::bail!("MCP source directory not found: {MCP_SOURCE}");
    }

    // Create destination
    std::fs::create_dir_all(dest)?;

    // Determine which tools to install
    let tools = if config.mcp_tools.is_empty() {
        // Fall back to all .py files in source
        discover_mcp_tools(source)?
    } else {
        config.mcp_tools.clone()
    };

    eprintln!(
        "[nemisis8-entry] installing {} MCP tools to {MCP_INSTALL}",
        tools.len()
    );

    for tool in &tools {
        let src = source.join(tool);
        let dst = dest.join(tool);

        if src.is_file() {
            std::fs::copy(&src, &dst)?;
        } else {
            eprintln!("[nemisis8-entry] warning: MCP tool not found: {tool}");
        }
    }

    // Also copy any required data directories
    let data_dirs = ["product_search_data"];
    for dir_name in &data_dirs {
        let src_dir = source.join(dir_name);
        let dst_dir = dest.join(dir_name);
        if src_dir.is_dir() {
            copy_dir_recursive(&src_dir, &dst_dir)?;
        }
    }

    Ok(())
}

/// Discover all .py tool files in the MCP source directory
fn discover_mcp_tools(source: &Path) -> anyhow::Result<Vec<String>> {
    let mut tools = Vec::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "py") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Skip sample_tool and __init__
                if name != "sample_tool.py" && name != "__init__.py" {
                    tools.push(name.to_string());
                }
            }
        }
    }
    tools.sort();
    Ok(tools)
}

/// Generate Codex config.toml with MCP tool registrations
fn write_codex_config(ws_config: &Config) -> anyhow::Result<()> {
    let config_dir = Path::new(CODEX_CONFIG_DIR);
    std::fs::create_dir_all(config_dir)?;

    let config_path = config_dir.join("config.toml");

    let tools = if ws_config.mcp_tools.is_empty() {
        discover_mcp_tools(Path::new(MCP_INSTALL))?
    } else {
        ws_config.mcp_tools.clone()
    };

    let content = config::generate_codex_config(&tools, MCP_VENV_PYTHON);

    // Merge with existing config if present
    if config_path.is_file() {
        let existing = std::fs::read_to_string(&config_path)?;
        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_default();

        let new_doc: toml_edit::DocumentMut = content.parse()?;

        // Replace mcpServers section
        if let Some(servers) = new_doc.get("mcpServers") {
            doc["mcpServers"] = servers.clone();
        }

        std::fs::write(&config_path, doc.to_string())?;
    } else {
        std::fs::write(&config_path, content)?;
    }

    eprintln!(
        "[nemisis8-entry] wrote Codex config with {} MCP tools",
        tools.len()
    );

    Ok(())
}

/// Generate Gemini settings.json with MCP tool registrations
fn write_gemini_config(ws_config: &Config) -> anyhow::Result<()> {
    let gemini_dir = PathBuf::from(CODEX_HOME).join(".gemini");
    std::fs::create_dir_all(&gemini_dir)?;

    // Gemini CLI expects projects.json to exist
    let projects_path = gemini_dir.join("projects.json");
    if !projects_path.exists() {
        std::fs::write(&projects_path, "{}")?;
    }

    // Auto-trust /workspace so gemini doesn't prompt — always overwrite to fix stale files
    let trust_path = gemini_dir.join("trustedFolders.json");
    std::fs::write(&trust_path, r#"{"/workspace":"TRUST_FOLDER","/":"TRUST_PARENT"}"#)?;

    let settings_path = gemini_dir.join("settings.json");

    let tools = if ws_config.mcp_tools.is_empty() {
        discover_mcp_tools(Path::new(MCP_INSTALL))?
    } else {
        ws_config.mcp_tools.clone()
    };

    let content = config::generate_gemini_config(&tools, MCP_VENV_PYTHON);

    // Merge with existing settings if present
    if settings_path.is_file() {
        let existing = std::fs::read_to_string(&settings_path)?;
        if let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&existing) {
            let new_doc: serde_json::Value = serde_json::from_str(&content)?;
            if let Some(servers) = new_doc.get("mcpServers") {
                doc["mcpServers"] = servers.clone();
            }
            std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
        } else {
            std::fs::write(&settings_path, content)?;
        }
    } else {
        std::fs::write(&settings_path, content)?;
    }

    eprintln!(
        "[nemisis8-entry] wrote Gemini config with {} MCP tools",
        tools.len()
    );

    Ok(())
}

/// Resolve the API key from various env var sources
fn resolve_api_key(provider: Provider) {
    match provider {
        Provider::Gemini => {
            // Gemini uses GEMINI_API_KEY or GOOGLE_API_KEY
            let key_vars = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];
            for var in &key_vars {
                if let Ok(val) = std::env::var(var) {
                    if !val.is_empty() {
                        if *var != "GEMINI_API_KEY" {
                            std::env::set_var("GEMINI_API_KEY", &val);
                        }
                        return;
                    }
                }
            }
            // Gemini also supports OAuth login, so no key is okay
            eprintln!("[nemisis8-entry] note: no GEMINI_API_KEY found (will use OAuth if logged in)");
        }
        Provider::Codex => {
            let key_vars = [
                "CODEX_API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
            ];
            for var in &key_vars {
                if let Ok(val) = std::env::var(var) {
                    if !val.is_empty() {
                        if *var != "OPENAI_API_KEY" {
                            std::env::set_var("OPENAI_API_KEY", &val);
                        }
                        return;
                    }
                }
            }
            eprintln!("[nemisis8-entry] warning: no API key found (set OPENAI_API_KEY or ANTHROPIC_API_KEY)");
        }
    }
}

/// Update Codex CLI if codex_cli_version is set in config
fn update_codex_cli(config: &Config) {
    let version = match &config.codex_cli_version {
        Some(v) => v.as_str(),
        None => return, // not configured, skip
    };

    let package = if version == "latest" {
        "@openai/codex@latest".to_string()
    } else {
        format!("@openai/codex@{version}")
    };

    eprintln!("[nemesis8-entry] updating codex CLI to {version}");
    let status = Command::new("npm")
        .args(["install", "-g", &package])
        .stdout(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => eprintln!("[nemesis8-entry] codex CLI updated to {version}"),
        Ok(s) => eprintln!("[nemesis8-entry] warning: npm install exited with code {}", s.code().unwrap_or(1)),
        Err(e) => eprintln!("[nemesis8-entry] warning: failed to update codex CLI: {e}"),
    }
}

/// Build and execute the Codex CLI
fn run_codex(prompt: Option<&str>, _interactive: bool, danger: bool) -> i32 {
    // Ensure npm global bin is on PATH so we can find codex
    if let Ok(path) = std::env::var("PATH") {
        if !path.contains("/usr/local/share/npm-global/bin") {
            std::env::set_var("PATH", format!("/usr/local/share/npm-global/bin:{path}"));
        }
    }

    // Ensure /workspace is a git repo so codex trusts it
    let ws = Path::new(WORKSPACE_ROOT);
    if !ws.join(".git").exists() {
        let _ = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(ws)
            .status();
    }

    let mut cmd = Command::new("codex");

    // Non-interactive prompt mode uses `codex exec` for clean output
    let is_exec = prompt.is_some() && !_interactive;
    if is_exec {
        cmd.arg("exec");
    }

    // System prompt — use CODEX_INSTRUCTIONS env var (supported by Codex CLI)
    let prompt_file = PathBuf::from(WORKSPACE_ROOT).join("PROMPT.md");
    if prompt_file.is_file() {
        if let Ok(system_prompt) = std::fs::read_to_string(&prompt_file) {
            cmd.env("CODEX_INSTRUCTIONS", system_prompt);
        }
    }

    // Danger mode
    if danger {
        cmd.arg("--full-auto");
        cmd.env("CODEX_UNSAFE_ALLOW_NO_SANDBOX", "1");
    }

    // Model override
    if let Ok(model) = std::env::var("CODEX_DEFAULT_MODEL") {
        cmd.arg("--model").arg(model);
    }

    // Session resume (only for interactive mode)
    if !is_exec {
        if let Ok(session_id) = std::env::var("CODEX_SESSION_ID") {
            if !session_id.is_empty() {
                cmd.arg("--session-id").arg(session_id);
            }
        }
    }

    // Prompt
    if let Some(p) = prompt {
        if !p.is_empty() {
            cmd.arg(p);
        }
    }

    cmd.current_dir(WORKSPACE_ROOT);

    // Inherit all env vars
    cmd.envs(std::env::vars());

    eprintln!("[nemisis8-entry] launching codex");

    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("[nemisis8-entry] failed to launch codex: {e}");
            1
        }
    }
}

/// Build and execute the Gemini CLI
fn run_gemini(prompt: Option<&str>, _interactive: bool, danger: bool) -> i32 {
    // Ensure npm global bin is on PATH so we can find gemini
    if let Ok(path) = std::env::var("PATH") {
        if !path.contains("/usr/local/share/npm-global/bin") {
            std::env::set_var("PATH", format!("/usr/local/share/npm-global/bin:{path}"));
        }
    }

    let mut cmd = Command::new("gemini");

    // System prompt via GEMINI_INSTRUCTIONS env var
    let prompt_file = PathBuf::from(WORKSPACE_ROOT).join("PROMPT.md");
    if prompt_file.is_file() {
        if let Ok(system_prompt) = std::fs::read_to_string(&prompt_file) {
            cmd.env("GEMINI_INSTRUCTIONS", &system_prompt);
        }
    }

    // Danger mode — gemini uses -y (yolo) to auto-approve all actions
    if danger {
        cmd.arg("-y");
    }

    // Model override
    if let Ok(model) = std::env::var("CODEX_DEFAULT_MODEL") {
        cmd.arg("--model").arg(model);
    }

    // Non-interactive: use -p flag for headless mode
    if let Some(p) = prompt {
        if !p.is_empty() {
            cmd.arg("-p").arg(p);
        }
    }

    cmd.current_dir(WORKSPACE_ROOT);

    // Inherit all env vars
    cmd.envs(std::env::vars());

    eprintln!("[nemisis8-entry] launching gemini");

    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("[nemisis8-entry] failed to launch gemini: {e}");
            1
        }
    }
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
