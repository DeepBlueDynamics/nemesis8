//! nemesis8-entry: container entry-point binary
//!
//! This binary runs INSIDE the Docker container. It handles:
//! - MCP server installation from /opt/mcp-source to /opt/nemesis8/mcp
//! - Data-driven provider config generation (reads providers/*.toml)
//! - API key resolution chain
//! - Danger mode flag injection
//! - Launching the configured AI CLI

use std::net::{TcpStream, ToSocketAddrs};
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

fn load_hyperia_env() {
    let path = PathBuf::from("/opt/nemesis8/hyperia_env.json");
    if path.is_file() {
        if let Ok(file) = std::fs::File::open(&path) {
            if let Ok(map) = serde_json::from_reader::<_, std::collections::HashMap<String, String>>(file) {
                for (k, v) in map {
                    unsafe {
                        std::env::set_var(&k, &v);
                    }
                }
                eprintln!("[nemesis8-entry] loaded Hyperia environment from host");
            }
        }
    }
}

fn main() {
    load_hyperia_env();
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

    // If the host forwarded a GitHub token (GH_TOKEN/GITHUB_TOKEN, set by
    // build_env from the locally-logged-in gh), make `git` use it too so plain
    // `git push`/`clone` over HTTPS works — gh itself is already authed via the
    // env token. Best-effort.
    setup_github_git();

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
    if let Err(e) = write_provider_config(&def, &config, danger) {
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

    // Blank the workspace's project-scoped .mcp.json for the session and
    // restore it on exit (see neutralize_workspace_mcp). Placed AFTER the
    // early "provider not installed" exits above so a misconfigured run never
    // touches the user's file.
    let mcp_guard = neutralize_workspace_mcp();

    // Launch the configured CLI (generic)
    let status = run_provider(&def, prompt.as_deref(), interactive, danger);

    // Tell the control plane we're exiting (best-effort).
    deregister_from_gateway();

    // Restore the user's .mcp.json BEFORE exiting — std::process::exit does not
    // run destructors, so the guard's Drop must be triggered explicitly. (Drop
    // still covers the panic/unwind path.)
    drop(mcp_guard);

    if interactive {
        eprintln!("[nemesis8-entry] agent exited (code {status}).");
        eprintln!("[nemesis8-entry] press Enter or any key to close the container (or Ctrl+^ to detach)...");
        let mut buf = [0u8; 1];
        use std::io::Read;
        let _ = std::io::stdin().read(&mut buf);
        std::process::exit(status);
    } else {
        std::process::exit(status);
    }
}

/// Session guard for the workspace's project-scoped `.mcp.json`. Restores the
/// user's original content when dropped.
struct McpGuard {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for McpGuard {
    fn drop(&mut self) {
        if std::fs::write(&self.path, &self.original).is_ok() {
            eprintln!("[nemesis8-entry] restored workspace .mcp.json");
        }
    }
}

/// A project-scoped `/workspace/.mcp.json` (read by Claude / Grok / etc.) often
/// carries host (Windows) command+arg paths that don't exist in this Linux
/// container, which breaks or hangs the provider's MCP startup. Blank it for the
/// session; the returned guard restores the user's original on exit.
///
/// This replaces the old behavior that overwrote the host file on EVERY start
/// and copied the (already-blanked) file over the single `.bak` — destroying the
/// real config after two runs. Now: we never capture a blank as the "original",
/// the `.bak` only ever holds real content, and the file round-trips untouched
/// on clean/panic exit.
fn neutralize_workspace_mcp() -> Option<McpGuard> {
    const NEUTRAL: &str = r#"{"mcpServers":{}}"#;
    let path = PathBuf::from(workspace_root()).join(".mcp.json");
    if !path.is_file() {
        return None;
    }
    let original = std::fs::read(&path).ok()?;
    // Already blanked (a prior hard-kill, or a nested run) — do NOT overwrite,
    // and crucially do NOT capture the blank as the original to restore.
    if original == NEUTRAL.as_bytes() {
        return None;
    }
    // Recovery aid for the hard-kill case (no graceful Drop): the .bak only
    // ever holds REAL content because we bailed above when the file was blank.
    let _ = std::fs::write(path.with_extension("json.bak"), &original);
    if std::fs::write(&path, NEUTRAL).is_err() {
        return None;
    }
    eprintln!("[nemesis8-entry] neutralized workspace .mcp.json for the session (restored on exit)");
    Some(McpGuard { path, original })
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

    // Copy ONLY the file-based (.py) tools the config names. URL/registry servers
    // (e.g. "blender") aren't files — config-gen handles those, not this copy.
    // NEVER discover-all: an empty list, or a list with no matching .py, must not
    // silently install every tool (that's the junk-drawer + ghost-server source).
    // Built-in binaries are always on regardless of this list.
    let tools: Vec<String> = config
        .mcp_tools
        .iter()
        .filter(|t| !t.starts_with("http://") && !t.starts_with("https://"))
        .filter(|t| source.join(t).is_file())
        .cloned()
        .collect();

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

    // Sync, don't just accumulate (#58). The volume mcp/ dir persists across
    // image rebuilds, and copy-only meant every tool any past image ever shipped
    // (or any `n8 mcp add`) lingered forever — the "junk drawer" that resurrects
    // retired tools as ghost MCP servers. Purge any volume `.py` that is neither
    // shipped by the current image (`/opt/mcp-source`) nor listed in this
    // workspace's `mcp_tools` (so user-added tools survive). Removing the file is
    // the real fix; no shadow-filtering hack needed downstream.
    let source_py: std::collections::HashSet<String> = std::fs::read_dir(source)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "py"))
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    for entry in std::fs::read_dir(dest)?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|x| x == "py") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let keep = source_py.contains(name) || config.mcp_tools.iter().any(|t| t == name);
        if !keep && std::fs::remove_file(&path).is_ok() {
            eprintln!("[nemesis8-entry] purged orphan MCP tool from volume: {name}");
        }
    }

    Ok(())
}

// (discover_mcp_tools removed — empty/unmatched config no longer "discovers all"
// the image's tools; you enable exactly what you list. #32/#58.)

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

    // System prompt injection — n8-owned BASE guardrails + the provider's
    // persona (composed in the lib). Replaces the per-workspace PROMPT.md, which
    // drifted and went stale (wrong identity, retired tool names).
    if let Some(ref env_var) = spec.system_prompt.env_var {
        cmd.env(env_var, nemesis8::config::compose_system_prompt(&spec.system_prompt));
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

    // Model override: an explicit pick (e.g. CODEX_DEFAULT_MODEL passed by the
    // host new-session modal) wins; otherwise fall back to the provider's
    // declared default model. NOTE: env_source must NOT also live in
    // [provider.env_overrides] — those are applied via set_var above and would
    // clobber the host-passed selection before we read it here (issue #65).
    let model = std::env::var(&spec.model.env_source)
        .ok()
        .or_else(|| spec.model.default.clone());
    if let Some(model) = model {
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

    // Tell agents that sandbox writes to their own session dir where the real
    // workspace is (e.g. agy --add-dir), so generated files land in the mounted
    // project, not an internal folder.
    if let Some(ref flag) = spec.workspace_flag {
        cmd.arg(flag).arg(workspace_root());
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
        let emoji = spec.emoji.as_deref().unwrap_or("🐙");
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

/// Generic config writer — one function for all providers
/// Which name means "the host machine" from inside THIS container. Docker
/// Desktop and daemons we launch (--add-host=host.docker.internal:host-gateway)
/// resolve host.docker.internal; host-network containers and alias-less
/// runtimes don't — there the host is plain localhost. DNS-probe once and let
/// the config writer swap every occurrence, so tools that talk to host
/// services (blender's addon socket, Hyperia, …) work in both worlds without
/// per-tool special cases.
fn host_gateway_alias() -> &'static str {
    // Candidates in preference order: docker's alias (also added by podman and
    // by our own --add-host), podman's canonical alias, then plain localhost
    // (host networking — podman 6 formalizes host.containers.internal as
    // 127.0.0.1 under --net=host, which is exactly this fallback).
    for alias in ["host.docker.internal", "host.containers.internal"] {
        let resolves = (alias, 0u16)
            .to_socket_addrs()
            .map(|mut addrs| addrs.next().is_some())
            .unwrap_or(false);
        if resolves {
            return alias;
        }
    }
    "127.0.0.1"
}

/// Try to reach Hyperia's MCP HTTP endpoint. Returns the working URL or None.
fn probe_hyperia() -> Option<String> {
    let timeout = Duration::from_millis(300);
    for host in &[
        "host.docker.internal",
        "host.containers.internal",
        "172.17.0.1",
        "127.0.0.1",
    ] {
        // to_socket_addrs (NOT SocketAddr::parse) so the hostname candidate
        // actually resolves — parse() is IP-only and silently skipped it,
        // which meant the primary alias was never probed at all.
        let Ok(addrs) = (*host, 9800u16).to_socket_addrs() else {
            continue;
        };
        for sa in addrs {
            if TcpStream::connect_timeout(&sa, timeout).is_ok() {
                return Some(format!("http://{host}:9800/mcp"));
            }
        }
    }
    None
}

/// Inject the Hyperia HTTP MCP server into an already-written provider config
/// file, WITH auth — in the provider's own schema. Shared lib implementation
/// (config::inject_hyperia_server) so `n8 mcp test` exercises the same code.
fn inject_hyperia_mcp(path: &Path, spec: &ProviderSpec, url: &str) -> anyhow::Result<()> {
    config::inject_hyperia_server(
        path,
        &spec.config_dir.format,
        &spec.config_dir.mcp_key,
        &spec.config_dir.mcp_http_style,
        url,
    )
}

fn merge_json(target: &mut serde_json::Value, source: &serde_json::Value) {
    if let (Some(t), Some(s)) = (target.as_object_mut(), source.as_object()) {
        for (k, v) in s {
            if v.is_object() && t.contains_key(k) && t[k].is_object() {
                merge_json(&mut t[k], v);
            } else {
                t.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Set `value` at a JSON-pointer path (e.g. "/permissions/allow") in `doc`,
/// creating intermediate objects as needed (serde_json's `pointer_mut` won't).
fn set_json_pointer(doc: &mut serde_json::Value, pointer: &str, value: serde_json::Value) {
    let parts: Vec<&str> = pointer.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        *doc = value;
        return;
    }
    let mut cur = doc;
    for part in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry((*part).to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    cur[parts[parts.len() - 1]] = value;
}

fn merge_json_into_toml(target: &mut toml_edit::Table, source: &serde_json::Map<String, serde_json::Value>) {
    for (k, v) in source {
        match v {
            serde_json::Value::Object(obj) => {
                let entry = target.entry(k).or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
                if let Some(t_table) = entry.as_table_mut() {
                    merge_json_into_toml(t_table, obj);
                }
            }
            serde_json::Value::Array(arr) => {
                let mut t_arr = toml_edit::Array::new();
                for val in arr {
                    if let Some(s) = val.as_str() {
                        t_arr.push(s);
                    } else if let Some(b) = val.as_bool() {
                        t_arr.push(b);
                    } else if let Some(i) = val.as_i64() {
                        t_arr.push(i);
                    } else if let Some(f) = val.as_f64() {
                        t_arr.push(f);
                    }
                }
                target.insert(k, toml_edit::Item::Value(toml_edit::Value::Array(t_arr)));
            }
            serde_json::Value::String(s) => {
                target.insert(k, toml_edit::value(s.clone()));
            }
            serde_json::Value::Bool(b) => {
                target.insert(k, toml_edit::value(*b));
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    target.insert(k, toml_edit::value(i));
                } else if let Some(f) = n.as_f64() {
                    target.insert(k, toml_edit::value(f));
                }
            }
            serde_json::Value::Null => {
                target.remove(k);
            }
        }
    }
}

/// Query a provider's local daemon (`/api/tags`) for its downloaded model ids.
/// Resolves the daemon URL from model.local_daemon_env (e.g. OLLAMA_HOST, set to
/// host.docker.internal by env_overrides in-container) → local_daemon_default_url.
/// Blocking HTTP (entry.rs has no async runtime). None if not configured/unreachable.
fn fetch_daemon_model_names(spec: &ProviderSpec) -> Option<Vec<String>> {
    let default_url = spec
        .model
        .local_daemon_default_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    // write_provider_config runs BEFORE run_provider applies env_overrides, so the
    // live OLLAMA_HOST isn't set yet here. Read the provider's DECLARED override
    // (the container-intended host.docker.internal) first, then the live env, then
    // the default. (default_url stays localhost for the host-side modal fetch.)
    let mut base = spec
        .model
        .local_daemon_env
        .as_deref()
        .and_then(|e| {
            spec.env_overrides
                .get(e)
                .cloned()
                .or_else(|| std::env::var(e).ok())
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default_url.to_string());
    if !base.starts_with("http://") && !base.starts_with("https://") {
        base = format!("http://{base}");
    }
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?
        .get(&url)
        .send()
        .ok()?;
    let tags: serde_json::Value = serde_json::from_str(&resp.text().ok()?).ok()?;
    let names: Vec<String> = tags
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

/// Generic local-daemon model enumeration (see ProviderSpec.local_models). Writes
/// the daemon's downloaded models into the agent's config — in the declared file,
/// JSON path, and shape, under an optional static wrapper. Names NO provider:
/// opencode (map shape, main file) and pi (array shape, separate models.json +
/// compat wrapper) are distinguished entirely by their TOML. No-op if unconfigured
/// or the daemon is unreachable (the agent simply won't list local models).
fn write_local_models(spec: &ProviderSpec, provider_dir: &Path) -> anyhow::Result<()> {
    let Some(lm) = &spec.local_models else { return Ok(()) };
    let Some(names) = fetch_daemon_model_names(spec) else { return Ok(()) };

    // Build the model list in the declared shape.
    let models_value = if lm.shape == "array" {
        serde_json::Value::Array(
            names.iter().map(|n| serde_json::json!({ "id": n })).collect(),
        )
    } else {
        let mut map = serde_json::Map::new();
        for n in &names {
            map.insert(n.clone(), serde_json::json!({ "name": n, "tools": true }));
        }
        serde_json::Value::Object(map)
    };

    // Nest the list under the dotted models_key (e.g. provider.ollama.models).
    let mut node = models_value;
    for part in lm.models_key.split('.').rev() {
        let mut obj = serde_json::Map::new();
        obj.insert(part.to_string(), node);
        node = serde_json::Value::Object(obj);
    }

    // Target file: lm.file (relative to provider_dir) or the main config file.
    let fname = lm
        .file
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| spec.config_dir.filename.clone());
    let path = provider_dir.join(&fname);
    let mut doc = if path.is_file() {
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path)?)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let Some(wrapper) = &lm.wrapper {
        merge_json(&mut doc, wrapper);
    }
    merge_json(&mut doc, &node);
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    eprintln!(
        "[nemesis8-entry] wrote {} local model(s) into {fname} at {}",
        names.len(),
        lm.models_key
    );
    Ok(())
}

fn write_provider_config(def: &ProviderDef, ws_config: &Config, danger: bool) -> anyhow::Result<()> {
    let spec = &def.provider;

    // Providers with format = "none" manage their own config (or need none)
    if spec.config_dir.format == "none" || spec.config_dir.filename.is_empty() {
        return Ok(());
    }

    let provider_dir = PathBuf::from(CODEX_HOME).join(&spec.config_dir.path);
    std::fs::create_dir_all(&provider_dir)?;

    // Sweep config locations this provider abandoned in a past version (declared
    // in config_dir.legacy_paths, relative to HOME). A path migration otherwise
    // strands an orphan the agent still reads/merges — e.g. antigravity moved
    // from ~/.gemini/config/mcp_config.json to ~/.gemini/antigravity-cli/, and
    // kept merging the stale one. Data-driven, so it covers any provider that
    // moves its files without per-provider code.
    for legacy in &spec.config_dir.legacy_paths {
        let p = PathBuf::from(CODEX_HOME).join(legacy);
        if p.is_file() {
            match std::fs::remove_file(&p) {
                Ok(()) => eprintln!("[nemesis8-entry] swept legacy {} config: {legacy}", spec.name),
                Err(e) => eprintln!("[nemesis8-entry] warning: could not sweep legacy config {}: {e}", p.display()),
            }
        } else if p.is_dir() {
            if std::fs::remove_dir_all(&p).is_ok() {
                eprintln!("[nemesis8-entry] swept legacy {} config dir: {legacy}", spec.name);
            }
        }
    }

    // Write system prompt to a file if requested (e.g. SYSTEM.md for Pi) — the
    // composed BASE + persona, n8-owned (not the per-workspace PROMPT.md).
    if let Some(ref target_filename) = spec.system_prompt.write_to_file {
        let content = nemesis8::config::compose_system_prompt(&spec.system_prompt);
        let dest_path = provider_dir.join(target_filename);
        if let Err(e) = std::fs::write(&dest_path, content) {
            eprintln!("[nemesis8-entry] warning: failed to write system prompt to {}: {e}", dest_path.display());
        } else {
            eprintln!("[nemesis8-entry] wrote system prompt to {}", dest_path.display());
        }
    }

    let settings_path = provider_dir.join(&spec.config_dir.filename);

    if spec.config_dir.mcp_key.is_empty() {
        // MCP-less provider config generation
        if spec.config_dir.format == "json" {
            let mut doc = if settings_path.is_file() {
                let existing = std::fs::read_to_string(&settings_path)?;
                serde_json::from_str::<serde_json::Value>(&existing).unwrap_or_else(|_| serde_json::json!({}))
            } else {
                serde_json::json!({})
            };

            // Always-applied provider defaults (e.g. pi's defaultProvider), then
            // danger-only overrides on top.
            if let Some(ref defaults) = spec.config_defaults {
                merge_json(&mut doc, defaults);
            }
            if danger {
                if let Some(ref config_merge) = spec.danger.config_merge {
                    merge_json(&mut doc, config_merge);
                }
            }

            std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
        } else {
            // TOML
            let mut doc = if settings_path.is_file() {
                let existing = std::fs::read_to_string(&settings_path)?;
                existing.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new())
            } else {
                toml_edit::DocumentMut::new()
            };

            if let Some(ref defaults) = spec.config_defaults {
                if let Some(obj) = defaults.as_object() {
                    merge_json_into_toml(doc.as_table_mut(), obj);
                }
            }
            if danger {
                if let Some(ref config_merge) = spec.danger.config_merge {
                    if let Some(obj) = config_merge.as_object() {
                        merge_json_into_toml(doc.as_table_mut(), obj);
                    }
                }
            }

            std::fs::write(&settings_path, doc.to_string())?;
        }

        for extra in &spec.hooks.extra_config_files {
            write_extra_config_file(&provider_dir, extra)?;
        }

        // Enumerate the local daemon's models into the agent's config (generic;
        // opencode → main config, pi → models.json) so it can select them.
        write_local_models(spec, &provider_dir)?;

        eprintln!(
            "[nemesis8-entry] wrote {} config (no MCP servers)",
            spec.name
        );
        return Ok(());
    }

    // Socket/stdio MCP servers are referenced by registry NAME (e.g. blender,
    // hyperia) — they're neither URLs nor installed .py files, so they must be
    // kept explicitly or the filter below drops them before config-gen.
    let mcp_registry = nemesis8::mcp_registry::McpRegistry::load();
    // Enable EXACTLY what the config names (URLs, installed .py, or registry server
    // names) — plus the always-on binaries added by generate_*_config. An empty
    // list means just those binaries; it does NOT discover-all (#32/#58).
    let tools: Vec<String> = ws_config
        .mcp_tools
        .iter()
        .filter(|t| {
            t.starts_with("http://")
                || t.starts_with("https://")
                || Path::new(MCP_INSTALL).join(t).is_file()
                || mcp_registry.get(t.as_str()).is_some()
        })
        .cloned()
        .collect();

    // (No shadow-filter here anymore: install_mcp_servers now syncs the volume so
    // a stale same-named `.py` can't exist, and generate_*_config registers the
    // built-in binaries last — so the binary always wins the key regardless. The
    // old is_binary_server() drop was a band-aid for the un-synced drawer. #58)

    // Agents whose MCP client can't parse the HTTP/socket spec (antigravity:
    // inherits Gemini's `httpUrl` schema but its connector only handles
    // command/url) get native HTTP registry servers AND raw URL servers dropped
    // — but with a matching stdio shim (`<name>-mcp.py` in the image) the
    // capability is SUBSTITUTED, not lost: hyperia → hyperia-mcp.py, meridian →
    // meridian-mcp.py. Data-driven via the provider TOML flag; the shared lib
    // fn is also exercised host-side by `n8 mcp test`.
    let tools: Vec<String> = if spec.config_dir.http_mcp_unsupported {
        let shim_exists = |name: &str| Path::new(MCP_SOURCE).join(name).is_file();
        let (mut adapted, notes) =
            config::adapt_tools_http_unsupported(&tools, &mcp_registry, &shim_exists);
        for n in &notes {
            eprintln!("[nemesis8-entry] {} ({})", n, spec.name);
        }
        // Hyperia integration parity with the HTTP auto-inject below: when the
        // sidecar is live and nothing wires hyperia yet, add the stdio shim —
        // the HTTP form this provider would otherwise get is exactly what its
        // connector rejects.
        let hyperia_wired = adapted
            .iter()
            .any(|t| t.trim_end_matches(".py").trim_end_matches("-mcp") == "hyperia");
        if !hyperia_wired && shim_exists("hyperia-mcp.py") && probe_hyperia().is_some() {
            eprintln!("[nemesis8-entry] auto-adding hyperia-mcp.py (Hyperia live; HTTP MCP unsupported for {})", spec.name);
            adapted.push("hyperia-mcp.py".to_string());
        }
        // Substituted/auto-added shims weren't in the config's mcp_tools, so
        // install_mcp_servers didn't copy them — make them real now.
        for t in &adapted {
            if t.ends_with(".py") {
                let dst = Path::new(MCP_INSTALL).join(t);
                if !dst.is_file() {
                    let _ = std::fs::copy(Path::new(MCP_SOURCE).join(t), &dst);
                }
            }
        }
        adapted
    } else {
        tools
    };

    let disabled = &ws_config.disabled_builtins;
    let mut content = match spec.config_dir.format.as_str() {
        "toml" => config::generate_codex_config_disabled(&tools, MCP_VENV_PYTHON, disabled),
        _ => config::generate_json_config_styled_disabled(
            &tools,
            MCP_VENV_PYTHON,
            &spec.config_dir.mcp_http_style,
            disabled,
        ),
    };

    // Merge provider config_defaults into the generated config (model provider,
    // base_url, etc.) for MCP-bearing providers too — the MCP-less branch above
    // already does this. No-op unless the provider TOML sets [config_defaults]
    // (only sakana does today; codex/grok/antigravity don't). Lets a codex-based
    // provider point at a custom OpenAI-compatible endpoint (Sakana Fugu) while
    // keeping native MCP wiring.
    if let Some(ref defaults) = spec.config_defaults {
        if let Some(obj) = defaults.as_object() {
            if spec.config_dir.format == "toml" {
                let mut doc = content
                    .parse::<toml_edit::DocumentMut>()
                    .unwrap_or_else(|_| toml_edit::DocumentMut::new());
                merge_json_into_toml(doc.as_table_mut(), obj);
                content = doc.to_string();
            } else if let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&content) {
                merge_json(&mut doc, defaults);
                content = serde_json::to_string_pretty(&doc).unwrap_or(content);
            }
        }
    }

    // Toggle host.docker.internal ↔ localhost for THIS runtime. Registry defs
    // ship host.docker.internal (the normal containerized case: blender's
    // BLENDER_HOST, the hyperia shim's HYPERIA_URL, raw urls); when that alias
    // doesn't resolve here (host networking, alias-less runtime) the host IS
    // localhost, so swap every occurrence — service- and provider-agnostic,
    // applied to the final content string all format branches consume.
    let alias = host_gateway_alias();
    if alias != "host.docker.internal" && content.contains("host.docker.internal") {
        eprintln!(
            "[nemesis8-entry] host.docker.internal does not resolve here; using {alias} for host services"
        );
        content = content.replace("host.docker.internal", alias);
    }

    if spec.config_dir.format == "json" {
        let mut doc = if settings_path.is_file() {
            let existing = std::fs::read_to_string(&settings_path)?;
            serde_json::from_str::<serde_json::Value>(&existing).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        let new_doc: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(servers) = new_doc.get(&spec.config_dir.mcp_key) {
            doc[&spec.config_dir.mcp_key] = servers.clone();
        }

        if danger {
            if let Some(ref config_merge) = spec.danger.config_merge {
                merge_json(&mut doc, config_merge);
            }
        }

        std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
    } else if spec.config_dir.merge && spec.config_dir.format == "toml" && settings_path.is_file() {
        // Merge the MCP servers table into the existing config.toml, preserving
        // the provider's own keys — only for providers that co-own the file
        // (grok: [cli]/[marketplace]/OAuth). Codex keeps merge=false and
        // falls through to a clean overwrite below, so it never persists a
        // CLI-written value a future CLI version can't parse.
        let existing = std::fs::read_to_string(&settings_path)?;
        let mut doc = existing.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new());
        let new_doc = content.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new());

        if let Some(item) = new_doc.get(&spec.config_dir.mcp_key) {
            doc[spec.config_dir.mcp_key.as_str()] = item.clone();
        }

        // config_defaults must reach co-owned files too (grok: [telemetry]
        // trace_upload=false, [harness] disable_codebase_upload=true — the
        // privacy hard-off). Before this, the merge branch copied ONLY the
        // mcp_key table and silently dropped every other defaults section.
        if let Some(ref defaults) = spec.config_defaults {
            if let Some(obj) = defaults.as_object() {
                merge_json_into_toml(doc.as_table_mut(), obj);
            }
        }

        if danger {
            if let Some(ref config_merge) = spec.danger.config_merge {
                if let Some(obj) = config_merge.as_object() {
                    merge_json_into_toml(doc.as_table_mut(), obj);
                }
            }
        }

        std::fs::write(&settings_path, doc.to_string())?;
    } else {
        if danger && spec.danger.config_merge.is_some() {
            if spec.config_dir.format == "toml" {
                let mut doc = content.parse::<toml_edit::DocumentMut>().unwrap_or_else(|_| toml_edit::DocumentMut::new());
                if let Some(ref config_merge) = spec.danger.config_merge {
                    if let Some(obj) = config_merge.as_object() {
                        merge_json_into_toml(doc.as_table_mut(), obj);
                    }
                }
                std::fs::write(&settings_path, doc.to_string())?;
            } else {
                let mut doc: serde_json::Value = serde_json::from_str(&content)?;
                if let Some(ref config_merge) = spec.danger.config_merge {
                    merge_json(&mut doc, config_merge);
                }
                std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
            }
        } else {
            std::fs::write(&settings_path, &content)?;
        }
    }

    // Pre-populate the agent's MCP permission allowlist so its tools are callable
    // from the FIRST token. agy reads `permissions.allow` once at startup — with
    // no entries every connected MCP tool errors "not enabled for server", and
    // mid-session grants never reload. Data-driven: the provider declares the
    // file + JSON pointer + entry template; we fill one entry per MCP server. n8
    // owns this list (fresh each session) so it can't drift.
    if !spec.config_dir.mcp_allowlist_file.is_empty() {
        if let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(servers) = cfg.get(&spec.config_dir.mcp_key).and_then(|v| v.as_object()) {
                let entries: Vec<serde_json::Value> = servers
                    .keys()
                    .map(|s| serde_json::Value::String(spec.config_dir.mcp_allowlist_entry.replace("{server}", s)))
                    .collect();
                let n = entries.len();
                let allow_path = provider_dir.join(&spec.config_dir.mcp_allowlist_file);
                let mut doc = std::fs::read_to_string(&allow_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                set_json_pointer(&mut doc, &spec.config_dir.mcp_allowlist_pointer, serde_json::Value::Array(entries));
                match std::fs::write(&allow_path, serde_json::to_string_pretty(&doc)?) {
                    Ok(()) => eprintln!("[nemesis8-entry] pre-allowed {n} MCP servers in {}", allow_path.display()),
                    Err(e) => eprintln!("[nemesis8-entry] warning: could not write MCP allowlist {}: {e}", allow_path.display()),
                }
            }
        }
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

    // Auto-inject the Hyperia HTTP server ONLY when the workspace doesn't already
    // wire hyperia itself (the hyperia-mcp.py stdio shim or the `hyperia` registry
    // server) — and NEVER for http_mcp_unsupported providers: their connector
    // rejects any HTTP form, and the adapt step above already auto-added the
    // stdio shim for them. Injection uses the provider's own schema
    // (config::inject_hyperia_server: codex TOML / claude / opencode / gemini).
    let hyperia_already = tools.iter().any(|t| {
        let s = t.trim_end_matches(".py");
        s == "hyperia" || s == "hyperia-mcp"
    });
    if !spec.config_dir.mcp_key.is_empty()
        && !hyperia_already
        && !spec.config_dir.http_mcp_unsupported
    {
        if let Some(hyperia_url) = probe_hyperia() {
            if let Err(e) = inject_hyperia_mcp(&settings_path, spec, &hyperia_url) {
                eprintln!("[nemesis8-entry] warning: could not inject Hyperia MCP: {e}");
            } else {
                eprintln!("[nemesis8-entry] Hyperia MCP connected at {hyperia_url}");
            }
        }
    }

    // Reconcile the provider's per-tool schema cache to the config we just wrote.
    // antigravity-cli keeps `<provider_dir>/mcp/<server>/` schema dirs and does NOT
    // drop them when a server leaves the config, so removed tools (e.g. the old
    // gnosis-* family) keep showing in its plugin list. No-op for providers without
    // that cache dir.
    if spec.config_dir.format == "json" && !spec.config_dir.mcp_key.is_empty() {
        if let Err(e) = prune_mcp_schema_cache(&provider_dir, &settings_path, &spec.config_dir.mcp_key) {
            eprintln!("[nemesis8-entry] warning: could not prune MCP schema cache: {e}");
        }
    }

    // Mirror the finished config to any additional paths this provider also
    // reads/merges (config_dir.mirror_paths, relative to HOME). antigravity MERGES
    // ~/.gemini/config/mcp_config.json on top of its own dir, and on Windows its
    // execution sandbox reads ONLY that copy — so it must hold the CURRENT config,
    // not a swept/empty file (else tools register in the UI but the sandbox is
    // blind). Done LAST so the mirror captures the fully-written settings_path
    // (after danger-merge, allowlist, hyperia inject). The write truncates, so a
    // stale prior copy can't strand servers.
    if !spec.config_dir.mirror_paths.is_empty() && settings_path.is_file() {
        let final_content = std::fs::read(&settings_path)?;
        for mirror in &spec.config_dir.mirror_paths {
            let dest = PathBuf::from(CODEX_HOME).join(mirror);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&dest, &final_content) {
                Ok(()) => eprintln!("[nemesis8-entry] mirrored {} config -> {mirror}", spec.name),
                Err(e) => eprintln!("[nemesis8-entry] warning: could not mirror config to {}: {e}", dest.display()),
            }
        }
    }

    eprintln!(
        "[nemesis8-entry] wrote {} config with {} MCP tools",
        spec.name,
        tools.len()
    );

    Ok(())
}

/// Drop stale per-tool schema-cache dirs (`<provider_dir>/mcp/<server>/`) that are
/// no longer in the written config. antigravity-cli builds its plugin list from these
/// dirs and never removes them on its own, so a tool removed from `.nemesis8.toml`
/// (or from the image) otherwise keeps appearing. Safe no-op when the cache dir is
/// absent; never prunes when the active set is empty (guards against a parse hiccup
/// wiping the whole cache).
fn prune_mcp_schema_cache(
    provider_dir: &Path,
    settings_path: &Path,
    mcp_key: &str,
) -> anyhow::Result<()> {
    let cache_dir = provider_dir.join("mcp");
    if !cache_dir.is_dir() {
        return Ok(());
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(settings_path)?)?;
    let active: std::collections::HashSet<String> = doc
        .get(mcp_key)
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if active.is_empty() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&cache_dir)? {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !active.contains(&name) {
            let _ = std::fs::remove_dir_all(entry.path());
            eprintln!("[nemesis8-entry] pruned stale MCP schema cache: {name}");
        }
    }
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

/// When a GitHub token was forwarded from the host (GH_TOKEN/GITHUB_TOKEN),
/// configure git to use it for HTTPS so `git push`/`clone` works — `gh` itself
/// already reads the token directly. Best-effort: no token, or no gh on PATH,
/// is a silent no-op.
fn setup_github_git() {
    let has_token = ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|k| std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false));
    if !has_token {
        return;
    }
    match Command::new("gh").args(["auth", "setup-git"]).status() {
        Ok(s) if s.success() => {
            eprintln!("[nemesis8-entry] github: authed via host token; git credential helper configured");
        }
        Ok(_) => eprintln!(
            "[nemesis8-entry] github: 'gh auth setup-git' failed; gh works but git push over HTTPS may not"
        ),
        Err(e) => eprintln!("[nemesis8-entry] github: gh not available ({e})"),
    }
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
