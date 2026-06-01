//! nemesis8-entry: container entry-point binary
//!
//! This binary runs INSIDE the Docker container. It handles:
//! - MCP server installation from /opt/mcp-source to /opt/nemesis8/mcp
//! - Data-driven provider config generation (reads providers/*.toml)
//! - API key resolution chain
//! - Danger mode flag injection
//! - Launching the configured AI CLI

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nemesis8::config::{self, Config, Provider};
use nemesis8::provider_def::{ProviderDef, ProviderSpec};
use nemesis8::provider_registry::ProviderRegistry;

const MCP_SOURCE: &str = "/opt/mcp-source";
const MCP_INSTALL: &str = "/opt/nemesis8/mcp";
const MCP_VENV_PYTHON: &str = "/opt/mcp-venv/bin/python3";
const CODEX_HOME: &str = "/opt/nemesis8";
const CODEX_CONFIG_DIR: &str = "/opt/nemesis8/.codex";
const DEFAULT_WORKSPACE: &str = "/workspace";

/// Resolve workspace root: NEMESIS8_WORKSPACE env > /workspace
fn workspace_root() -> String {
    std::env::var("NEMESIS8_WORKSPACE").unwrap_or_else(|_| DEFAULT_WORKSPACE.to_string())
}

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
                if prompt.is_none() {
                    prompt = Some(args[i].clone());
                }
            }
        }
        i += 1;
    }

    // Load config: env JSON (from host) > workspace file > defaults
    let config = if let Ok(json) = std::env::var("NEMESIS8_CONFIG_JSON") {
        serde_json::from_str::<Config>(&json).unwrap_or_else(|_| {
            let config_path = PathBuf::from(workspace_root()).join(".nemesis8.toml");
            Config::load_or_default(&config_path)
        })
    } else {
        let config_path = PathBuf::from(workspace_root()).join(".nemesis8.toml");
        Config::load_or_default(&config_path)
    };

    // Neutralize host .mcp.json — it has Windows paths that break MCP in Linux containers.
    let ws_mcp = PathBuf::from(workspace_root()).join(".mcp.json");
    if ws_mcp.is_file() {
        let _ = std::fs::copy(&ws_mcp, ws_mcp.with_extension("json.bak"));
        let _ = std::fs::write(&ws_mcp, r#"{"mcpServers":{}}"#);
    }

    // Determine provider: env var override > config file
    let provider = std::env::var("NEMESIS8_PROVIDER")
        .ok()
        .and_then(|s| s.parse::<Provider>().ok())
        .unwrap_or_else(|| config.provider.clone());

    // Load session env if CODEX_SESSION_ID is set
    if let Ok(session_id) = std::env::var("CODEX_SESSION_ID") {
        load_session_env(&session_id);
    }

    // Bring up a D-Bus session + gnome-keyring before any provider runs.
    // Providers that use libsecret (e.g. Antigravity for OAuth token storage)
    // need a Secret Service. The container has no system keyring, so we run
    // one ourselves; keyring file lives under HOME so it persists across
    // container restarts via the /opt/nemesis8 bind mount.
    init_keyring();

    // Spawn the Nemesis internal monitor — runs in the background for the
    // entire container lifetime, emits filesystem/network/process telemetry
    // to /opt/nemesis8/.monitor/events.jsonl. Best-effort: a failed spawn
    // doesn't block the provider from running.
    spawn_monitor();

    // Install MCP servers (shared between providers)
    if let Err(e) = install_mcp_servers(&config) {
        eprintln!("warning: MCP server install failed: {e}");
    }

    // Load provider definition from registry
    let registry = ProviderRegistry::load();
    let provider_name = format!("{provider}");
    let def = match registry.resolve(&provider_name) {
        Ok(d) => d.clone(),
        Err(e) => {
            eprintln!("[nemesis8-entry] {e}");
            std::process::exit(1);
        }
    };

    // Generate provider config (generic)
    if let Err(e) = write_provider_config(&def, &config) {
        eprintln!("warning: {} config generation failed: {e}", def.provider.name);
    }

    // Update CLI (generic) — skip for non-interactive runs to avoid per-invocation latency
    if interactive {
        update_cli_generic(&def);
    }

    // Refuse fast if the provider's binary isn't installed in this image.
    // Without this check the user gets "No such file or directory" from
    // run_provider, which doesn't tell them what's wrong or how to fix it.
    if !provider_binary_installed(&def) {
        let bin = &def.provider.binary;
        let pname = &def.provider.name;
        eprintln!("[nemesis8-entry] ERROR: provider '{pname}' is not installed in this image.");
        eprintln!("[nemesis8-entry] Expected '{bin}' on PATH but it's missing.");
        eprintln!("[nemesis8-entry]");
        eprintln!("[nemesis8-entry] Fix: add '{pname}' to the `providers` list in your");
        eprintln!("[nemesis8-entry] .nemesis8.toml and rebuild the image:");
        eprintln!("[nemesis8-entry]");
        eprintln!("[nemesis8-entry]   docker rmi nemesis8:latest && nemesis8 build");
        std::process::exit(1);
    }

    // Validate CLI flags (generic)
    validate_cli_flags_generic(&def, danger);

    // Run setup commands
    run_setup_commands(&config);

    // Resolve API key (generic)
    resolve_api_key_generic(&def);

    // Register with the control plane if this container was spawned by a
    // gateway (GATEWAY_URL + NEMESIS8_AGENT_ID present). Best-effort.
    register_with_gateway(&def);

    // Launch the configured CLI (generic)
    let status = run_provider(&def, prompt.as_deref(), interactive, danger);

    // Tell the control plane we're exiting (best-effort).
    deregister_from_gateway();

    std::process::exit(status);
}

/// POST /agents/{id}/register to the gateway, if this container knows its
/// gateway + agent id. Best-effort — failures are logged and ignored.
fn register_with_gateway(def: &ProviderDef) {
    let (gw, agent_id) = match (std::env::var("GATEWAY_URL"), std::env::var("NEMESIS8_AGENT_ID")) {
        (Ok(g), Ok(a)) if !g.is_empty() && !a.is_empty() => (g, a),
        _ => return,
    };
    let url = format!("{}/agents/{}/register", gw.trim_end_matches('/'), agent_id);
    let token = std::env::var("NEMESIS8_AUTH_TOKEN").ok();
    let body = serde_json::json!({
        "provider": def.provider.name,
        "workspace": workspace_root(),
        "pid": std::process::id(),
    })
    .to_string();
    match nemesis8::monitor::http_post_json(&url, &body, token.as_deref()) {
        Ok(()) => eprintln!("[nemesis8-entry] registered with control plane ({agent_id})"),
        Err(e) => eprintln!("[nemesis8-entry] register failed (non-fatal): {e}"),
    }
}

/// POST /agents/{id}/deregister on exit. Best-effort.
fn deregister_from_gateway() {
    let (gw, agent_id) = match (std::env::var("GATEWAY_URL"), std::env::var("NEMESIS8_AGENT_ID")) {
        (Ok(g), Ok(a)) if !g.is_empty() && !a.is_empty() => (g, a),
        _ => return,
    };
    let url = format!("{}/agents/{}/deregister", gw.trim_end_matches('/'), agent_id);
    let token = std::env::var("NEMESIS8_AUTH_TOKEN").ok();
    let _ = nemesis8::monitor::http_post_json(&url, "{}", token.as_deref());
}

// ── Shared utility functions ────────────────────────────────────────────

fn load_session_env(session_id: &str) {
    let candidates = [
        format!("{CODEX_HOME}/sessions/{session_id}/.env"),
        format!("{CODEX_HOME}/.codex/sessions/{session_id}/.env"),
    ];
    for path in &candidates {
        if std::path::Path::new(path).is_file() {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        unsafe { std::env::set_var(key.trim(), value.trim().trim_matches('"')); }
                    }
                }
                eprintln!("[nemesis8-entry] loaded session env from {path}");
                return;
            }
        }
    }
}

fn install_mcp_servers(config: &Config) -> anyhow::Result<()> {
    let source = Path::new(MCP_SOURCE);
    let dest = Path::new(MCP_INSTALL);

    if !source.is_dir() {
        anyhow::bail!("MCP source directory not found: {MCP_SOURCE}");
    }

    std::fs::create_dir_all(dest)?;

    // Split configured tools into file-based (need copying) and URL-based (pass-through).
    let tools = if config.mcp_tools.is_empty() {
        discover_mcp_tools(source)?
    } else {
        let file_tools: Vec<String> = config
            .mcp_tools
            .iter()
            .filter(|t| !t.starts_with("http://") && !t.starts_with("https://"))
            .filter(|t| source.join(t).is_file())
            .cloned()
            .collect();
        if file_tools.is_empty() {
            eprintln!("[nemesis8-entry] configured file tools not found in image, discovering all");
            discover_mcp_tools(source)?
        } else {
            file_tools
        }
    };

    eprintln!(
        "[nemesis8-entry] installing {} MCP tools to {MCP_INSTALL}",
        tools.len()
    );

    for tool in &tools {
        let src = source.join(tool);
        let dst = dest.join(tool);
        if src.is_file() {
            std::fs::copy(&src, &dst)?;
        } else {
            eprintln!("[nemesis8-entry] warning: MCP tool not found: {tool}");
        }
    }

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

fn discover_mcp_tools(source: &Path) -> anyhow::Result<Vec<String>> {
    let mut tools = Vec::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "py") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name != "sample_tool.py" && name != "__init__.py" {
                    tools.push(name.to_string());
                }
            }
        }
    }
    tools.sort();
    Ok(tools)
}

fn run_setup_commands(config: &Config) {
    if config.setup_commands.is_empty() {
        return;
    }
    eprintln!(
        "[nemesis8-entry] running {} setup command(s)",
        config.setup_commands.len()
    );
    for cmd_str in &config.setup_commands {
        eprintln!("[nemesis8-entry] setup: {cmd_str}");
        let status = Command::new("sh")
            .args(["-c", cmd_str])
            .current_dir(&workspace_root())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!(
                "[nemesis8-entry] warning: setup command exited with code {}",
                s.code().unwrap_or(1)
            ),
            Err(e) => eprintln!("[nemesis8-entry] warning: setup command failed: {e}"),
        }
    }
}

/// Write the API key into Codex CLI's config file
fn write_codex_api_key(key: &str) {
    let config_dir = Path::new(CODEX_CONFIG_DIR);
    let _ = std::fs::create_dir_all(config_dir);
    let config_path = config_dir.join("config.toml");

    let mut content = std::fs::read_to_string(&config_path).unwrap_or_default();

    if content.contains("api_key") {
        let lines: Vec<&str> = content.lines().collect();
        let new_lines: Vec<String> = lines
            .iter()
            .map(|line| {
                if line.trim_start().starts_with("api_key") {
                    format!("api_key = \"{key}\"")
                } else {
                    line.to_string()
                }
            })
            .collect();
        content = new_lines.join("\n");
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!("api_key = \"{key}\"\n"));
    }

    if let Err(e) = std::fs::write(&config_path, content) {
        eprintln!("[nemesis8-entry] warning: could not write Codex API key to config: {e}");
    }
}

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

// ── Generic provider functions (data-driven from providers/*.toml) ──────

/// Generic provider runner — one function for all providers
fn run_provider(def: &ProviderDef, prompt: Option<&str>, interactive: bool, danger: bool) -> i32 {
    let spec = &def.provider;

    // PATH setup
    if let Ok(path) = std::env::var("PATH") {
        if !path.contains("/usr/local/share/npm-global/bin") {
            unsafe { std::env::set_var("PATH", format!("/usr/local/share/npm-global/bin:{path}")); }
        }
    }

    // Env overrides (e.g., HOME=/opt/nemesis8 for gemini)
    for (key, val) in &spec.env_overrides {
        unsafe { std::env::set_var(key, val); }
    }

    // Git init hook
    if spec.hooks.requires_git_init {
        let ws_path = workspace_root();
        let ws = Path::new(&ws_path);
        if !ws.join(".git").exists() {
            let _ = Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(ws)
                .status();
        }
    }

    let mut cmd = Command::new(&spec.binary);

    // Prepend script path (for interpreters like python3)
    if let Some(ref script) = spec.script {
        cmd.arg(script);
    }

    // Session resume
    let session_id = if spec.hooks.supports_sessions {
        std::env::var("CODEX_SESSION_ID").ok().filter(|s| !s.is_empty())
    } else {
        None
    };

    if let Some(ref sid) = session_id {
        match &spec.hooks.resume_flag {
            Some(flag) => { cmd.arg(flag).arg(sid); }
            None => { cmd.arg("resume").arg(sid); }
        }
    } else if prompt.is_some() && !interactive {
        if let Some(ref sub) = spec.prompt.exec_subcommand {
            cmd.arg(sub);
        }
    } else if interactive {
        if let Some(ref sub) = spec.prompt.interactive_subcommand {
            cmd.arg(sub);
        }
    }

    // System prompt injection
    if let Some(ref env_var) = spec.system_prompt.env_var {
        let prompt_file = PathBuf::from(workspace_root()).join(&spec.system_prompt.source_file);
        if prompt_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&prompt_file) {
                cmd.env(env_var, content);
            }
        }
    }

    // Danger mode
    if danger {
        if let Some(ref flag) = spec.danger.flag {
            for part in flag.split_whitespace() {
                cmd.arg(part);
            }
        }
        for env_val in &spec.danger.env_vars {
            if let Some((k, v)) = env_val.split_once('=') {
                cmd.env(k, v);
            }
        }
    }

    // Model override
    if let Ok(model) = std::env::var(&spec.model.env_source) {
        if let Some(ref flag) = spec.model.flag {
            cmd.arg(flag).arg(model);
        }
    }

    // Prompt argument (only for non-resume modes)
    if session_id.is_none() {
        if let Some(p) = prompt {
            if !p.is_empty() {
                if let Some(ref flag) = spec.prompt.exec_prompt_flag {
                    cmd.arg(flag).arg(p);
                } else if let Some(ref flag) = spec.prompt.flag {
                    cmd.arg(flag).arg(p);
                } else {
                    cmd.arg(p);
                }
            }
        }
    }

    cmd.current_dir(&workspace_root());
    cmd.envs(std::env::vars());

    // Add user-installed MCP packages to PYTHONPATH so pip-installed deps are visible
    let mcp_packages = format!("{}/mcp-packages", CODEX_HOME);
    let pythonpath = std::env::var("PYTHONPATH").unwrap_or_default();
    let new_pythonpath = if pythonpath.is_empty() {
        mcp_packages
    } else {
        format!("{mcp_packages}:{pythonpath}")
    };
    cmd.env("PYTHONPATH", new_pythonpath);

    eprintln!("[nemesis8-entry] launching {}", spec.binary);

    // Set the host terminal title so the tab/window shows what's running.
    // OSC 0 = set window + icon title. Only emit when interactive — non-tty
    // exec mode would corrupt the captured output stream with control bytes.
    // Some providers (agy, codex) set their own title after launch, which
    // overrides this; either way the user sees something meaningful.
    if interactive {
        let emoji = provider_emoji(&spec.name);
        print!("\x1b]0;{} {}\x07", spec.name, emoji);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    let result = cmd.status();

    // Reset the title to a sensible default when the provider exits so the
    // user's shell isn't left wearing a stale agent name.
    if interactive {
        print!("\x1b]0;nemesis8 🐙\x07");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    match result {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("[nemesis8-entry] failed to launch {}: {e}", spec.binary);
            1
        }
    }
}

/// Map a provider name to a (playful) terminal-title emoji.
fn provider_emoji(name: &str) -> &'static str {
    match name {
        "codex"       => "📜",
        "gemini"      => "✨",
        "claude"      => "🎭",
        "antigravity" => "🛸",
        "openclaw"    => "🦞",
        "ollama"      => "🦙",
        "alacode"     => "⚗️",
        _             => "🐙",
    }
}

/// Generic config writer — one function for all providers
/// Try to reach Hyperia's MCP HTTP endpoint. Returns the working URL or None.
fn probe_hyperia() -> Option<String> {
    let timeout = Duration::from_millis(300);
    for host in &["host.docker.internal", "172.17.0.1"] {
        let addr = format!("{host}:9800");
        if let Ok(sa) = addr.parse() {
            if TcpStream::connect_timeout(&sa, timeout).is_ok() {
                return Some(format!("http://{host}:9800/mcp"));
            }
        }
    }
    None
}

/// Inject the Hyperia HTTP MCP server into an already-written provider config file.
fn inject_hyperia_mcp(path: &Path, spec: &ProviderSpec, url: &str) -> anyhow::Result<()> {
    match spec.config_dir.format.as_str() {
        "toml" => {
            let raw = std::fs::read_to_string(path)?;
            let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
            let servers = doc[&spec.config_dir.mcp_key]
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
                .as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("mcp_servers is not a table"))?;
            let mut entry = toml_edit::Table::new();
            entry["type"] = toml_edit::value("http");
            entry["url"] = toml_edit::value(url);
            servers.insert("hyperia", toml_edit::Item::Table(entry));
            std::fs::write(path, doc.to_string())?;
        }
        _ => {
            let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
            let mut doc: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
            doc[&spec.config_dir.mcp_key]["hyperia"] = serde_json::json!({
                "type": "http",
                "url": url
            });
            std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        }
    }
    Ok(())
}

fn write_provider_config(def: &ProviderDef, ws_config: &Config) -> anyhow::Result<()> {
    let spec = &def.provider;

    // Providers with format = "none" manage their own config (or need none)
    if spec.config_dir.format == "none" || spec.config_dir.filename.is_empty() {
        return Ok(());
    }

    let provider_dir = PathBuf::from(CODEX_HOME).join(&spec.config_dir.path);
    std::fs::create_dir_all(&provider_dir)?;

    let tools = if ws_config.mcp_tools.is_empty() {
        discover_mcp_tools(Path::new(MCP_INSTALL))?
    } else {
        // URLs pass through as-is; file tools must be present in MCP_INSTALL.
        ws_config
            .mcp_tools
            .iter()
            .filter(|t| {
                t.starts_with("http://") || t.starts_with("https://")
                    || Path::new(MCP_INSTALL).join(t).is_file()
            })
            .cloned()
            .collect()
    };

    let content = match spec.config_dir.format.as_str() {
        "toml" => config::generate_codex_config(&tools, MCP_VENV_PYTHON),
        _ => config::generate_gemini_config(&tools, MCP_VENV_PYTHON),
    };

    let settings_path = provider_dir.join(&spec.config_dir.filename);

    if spec.config_dir.format == "json" && settings_path.is_file() {
        let existing = std::fs::read_to_string(&settings_path)?;
        if let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&existing) {
            let new_doc: serde_json::Value = serde_json::from_str(&content)?;
            if let Some(servers) = new_doc.get(&spec.config_dir.mcp_key) {
                doc[&spec.config_dir.mcp_key] = servers.clone();
            }
            std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
        } else {
            std::fs::write(&settings_path, &content)?;
        }
    } else {
        std::fs::write(&settings_path, &content)?;
    }

    // Disable Codex built-in web search when we have our own search/crawl MCP tools
    // and a SerpAPI key. No point in two crawlers competing.
    if spec.config_dir.format == "toml" {
        let serpapi_in_env = std::env::var("SERPAPI_API_KEY").map_or(false, |v| !v.is_empty());
        let serpapi_env_file = PathBuf::from(workspace_root()).join(".serpapi.env").is_file();
        let has_serpapi = tools.iter().any(|t| t.contains("serpapi"))
            && (serpapi_in_env || serpapi_env_file);
        let has_crawler = tools.iter().any(|t| t.contains("grub") || t.contains("crawl"));
        if has_serpapi && has_crawler {
            // Re-read, inject web_search = "disabled", re-write
            let raw = std::fs::read_to_string(&settings_path).unwrap_or_default();
            if let Ok(mut doc) = raw.parse::<toml_edit::DocumentMut>() {
                doc["web_search"] = toml_edit::value("disabled");
                std::fs::write(&settings_path, doc.to_string())?;
                eprintln!("[nemesis8-entry] disabled Codex built-in web search (serpapi + grub-crawler available)");
            }
        }
    }

    for extra in &spec.hooks.extra_config_files {
        write_extra_config_file(&provider_dir, extra)?;
    }

    if let Some(hyperia_url) = probe_hyperia() {
        if let Err(e) = inject_hyperia_mcp(&settings_path, spec, &hyperia_url) {
            eprintln!("[nemesis8-entry] warning: could not inject Hyperia MCP: {e}");
        } else {
            eprintln!("[nemesis8-entry] Hyperia MCP connected at {hyperia_url}");
        }
    }

    eprintln!(
        "[nemesis8-entry] wrote {} config with {} MCP tools",
        spec.name,
        tools.len()
    );

    Ok(())
}

fn write_extra_config_file(provider_dir: &Path, kind: &str) -> anyhow::Result<()> {
    let ws = workspace_root();
    let ws_name = std::path::Path::new(&ws)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    match kind {
        "projects" => {
            let path = provider_dir.join("projects.json");
            let mut map = serde_json::Map::new();
            map.insert("/workspace".to_string(), serde_json::json!("workspace"));
            if ws != "/workspace" {
                map.insert(ws.clone(), serde_json::json!(ws_name));
            }
            let json = serde_json::json!({"projects": serde_json::Value::Object(map)});
            std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
        }
        "trustedFolders" => {
            let path = provider_dir.join("trustedFolders.json");
            let mut trust = serde_json::Map::new();
            trust.insert("/workspace".to_string(), serde_json::json!("TRUST_FOLDER"));
            trust.insert("/".to_string(), serde_json::json!("TRUST_PARENT"));
            if ws != "/workspace" {
                trust.insert(ws, serde_json::json!("TRUST_FOLDER"));
            }
            std::fs::write(
                &path,
                serde_json::to_string(&serde_json::Value::Object(trust))?,
            )?;
        }
        other => {
            eprintln!("[nemesis8-entry] unknown extra config type: {other}");
        }
    }
    Ok(())
}

/// Generic API key resolver
fn resolve_api_key_generic(def: &ProviderDef) {
    let spec = &def.provider;
    let key_spec = &spec.api_keys;

    for var in &key_spec.chain {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() {
                if let Some(ref target) = key_spec.target {
                    if var != target {
                        unsafe { std::env::set_var(target, &val); }
                    }
                }
                if key_spec.write_to_config {
                    write_codex_api_key(&val);
                }
                return;
            }
        }
    }

    if key_spec.optional {
        eprintln!(
            "[nemesis8-entry] info: no API key set for {} (OAuth or login may be used)",
            spec.name
        );
    } else if !key_spec.chain.is_empty() {
        eprintln!(
            "[nemesis8-entry] info: no API key set for {} (checked: {})",
            spec.name,
            key_spec.chain.join(", ")
        );
    }
}

/// Generic CLI updater — only called for interactive sessions.
/// Skips the npm update if it ran successfully within the last hour
/// (stamp stored in CODEX_HOME so it persists across container restarts).
fn update_cli_generic(def: &ProviderDef) {
    let spec = &def.provider;
    // Skip when install_package is None OR an empty string. Empty-string
    // means "this provider doesn't install via npm" (e.g. antigravity, which
    // uses a curl installer). Without this check, format!() builds the
    // invalid package name "@latest" and npm fails with EINVALIDTAGNAME.
    let package = match &spec.install_package {
        Some(pkg) if !pkg.is_empty() => format!("{pkg}@latest"),
        _ => return,
    };

    let stamp = PathBuf::from(CODEX_HOME).join(format!(".update-{}", spec.name));
    if let Ok(meta) = std::fs::metadata(&stamp) {
        if let Ok(age) = meta.modified().and_then(|m| m.elapsed().map_err(|e| std::io::Error::other(e))) {
            if age < Duration::from_secs(3600) {
                return;
            }
        }
    }

    eprintln!("[nemesis8-entry] updating {} CLI to latest", spec.name);
    let status = Command::new("npm")
        .args(["install", "-g", &package])
        .stdout(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("[nemesis8-entry] {} CLI updated", spec.name);
            let _ = std::fs::write(&stamp, b"");
        }
        Ok(s) => eprintln!(
            "[nemesis8-entry] warning: npm install exited with code {}",
            s.code().unwrap_or(1)
        ),
        Err(e) => eprintln!(
            "[nemesis8-entry] warning: failed to update {} CLI: {e}",
            spec.name
        ),
    }
}

/// Generic CLI flag validator
fn validate_cli_flags_generic(def: &ProviderDef, danger: bool) {
    let spec = &def.provider;
    let mut flags_to_check = spec.validation.flags.clone();
    if danger {
        flags_to_check.extend(spec.validation.danger_flags.iter().cloned());
    }

    if flags_to_check.is_empty() {
        return;
    }

    let output = Command::new(&spec.binary).arg("--help").output();

    match output {
        Ok(out) => {
            let help_text = String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr);
            let version = Command::new(&spec.binary)
                .arg("--version")
                .output()
                .ok()
                .map(|v| String::from_utf8_lossy(&v.stdout).trim().to_string())
                .unwrap_or_default();

            if !version.is_empty() {
                eprintln!("[nemesis8-entry] {} version: {version}", spec.name);
            }

            let mut missing = Vec::new();
            for flag in &flags_to_check {
                if !help_text.contains(flag.as_str()) {
                    missing.push(flag.as_str());
                }
            }

            if missing.is_empty() {
                eprintln!(
                    "[nemesis8-entry] {} flag check: all {} flags valid",
                    spec.name,
                    flags_to_check.len()
                );
            } else {
                eprintln!(
                    "[nemesis8-entry] WARNING: {} missing flags: {}",
                    spec.name,
                    missing.join(", ")
                );
                eprintln!("[nemesis8-entry] these flags may have been renamed or removed");
                eprintln!("[nemesis8-entry] full --help output:");
                for line in help_text.lines() {
                    eprintln!("[nemesis8-entry]   {line}");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[nemesis8-entry] WARNING: could not run {} --help: {e}",
                spec.binary
            );
        }
    }
}

/// Check whether the provider's binary is actually installed in this image.
/// Returns true if either `which <binary>` succeeds or the binary path is
/// executable when given as an absolute path.
fn provider_binary_installed(def: &ProviderDef) -> bool {
    let bin = &def.provider.binary;

    // Absolute path? Just check executability.
    if bin.starts_with('/') {
        return std::path::Path::new(bin).is_file();
    }

    // Otherwise scan PATH.
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Start a session D-Bus and unlock a gnome-keyring with an empty password.
/// This gives libsecret-using clients a working Secret Service so OAuth
/// tokens (e.g. Antigravity's) can persist to disk under HOME — instead of
/// dying with the container because keyring write failed.
///
/// Best-effort: if dbus-launch or gnome-keyring-daemon aren't on PATH (older
/// images), we log and continue. Providers that don't need keyring won't
/// notice; ones that do will just be back to the previous broken behavior.
fn init_keyring() {
    use std::io::Write;
    use std::process::Stdio;

    let dbus_ok = Command::new("which")
        .arg("dbus-launch")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let kr_ok = Command::new("which")
        .arg("gnome-keyring-daemon")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !dbus_ok || !kr_ok {
        eprintln!("[nemesis8-entry] keyring not started (dbus-launch or gnome-keyring-daemon missing)");
        return;
    }

    // 1. Start a session D-Bus. dbus-launch prints `NAME=value;` lines.
    let dbus_out = match Command::new("dbus-launch").arg("--sh-syntax").output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("[nemesis8-entry] keyring: dbus-launch failed; continuing without Secret Service");
            return;
        }
    };
    for line in String::from_utf8_lossy(&dbus_out.stdout).lines() {
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            // dbus-launch emits "export NAME;" lines mixed with assignments — skip them.
            if key.is_empty() || key.contains(' ') {
                continue;
            }
            let val = line[eq + 1..].trim_end_matches(';').trim_matches('\'');
            unsafe { std::env::set_var(key, val); }
        }
    }

    // 2. Start gnome-keyring-daemon with --components=secrets and feed an
    // empty password on stdin so it unlocks (or creates) the keyring file.
    let mut child = match Command::new("gnome-keyring-daemon")
        .args(["--unlock", "--components=secrets"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[nemesis8-entry] keyring: gnome-keyring-daemon spawn failed: {e}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"\n");
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[nemesis8-entry] keyring: gnome-keyring-daemon wait failed: {e}");
            return;
        }
    };
    // Any extra env vars it prints (e.g. GNOME_KEYRING_CONTROL) — propagate them.
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            if !key.is_empty() && !key.contains(' ') {
                unsafe { std::env::set_var(key, line[eq + 1..].trim()); }
            }
        }
    }

    eprintln!("[nemesis8-entry] keyring: Secret Service ready (DBUS_SESSION_BUS_ADDRESS set)");
}

/// Spawn nemesis8-monitor as a background subprocess. Tini will reap it
/// when this process tree exits. Best-effort: any failure (binary missing,
/// permission denied) is logged and ignored — the provider must still run.
fn spawn_monitor() {
    use std::process::Stdio;
    let monitor_bin = "/usr/local/bin/nemesis8-monitor";
    if !std::path::Path::new(monitor_bin).is_file() {
        // Older images won't have it; that's fine, just skip.
        eprintln!("[nemesis8-entry] monitor not installed; skipping telemetry");
        return;
    }
    match Command::new(monitor_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            eprintln!("[nemesis8-entry] monitor started (pid={})", child.id());
        }
        Err(e) => {
            eprintln!("[nemesis8-entry] could not start monitor: {e}");
        }
    }
}
