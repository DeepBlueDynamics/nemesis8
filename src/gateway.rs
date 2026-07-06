use anyhow::Result;
use axum::{
    Router,
    extract::{Path as AxumPath, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Config;
use crate::docker::DockerOps;
use crate::registry::{AgentRecord, AgentState, Registry};
use crate::scheduler::{Schedule, TriggerRecord, TriggerStore};
use crate::session::{self, SessionInfo};
use crate::tunnel::{self, TunnelRegistry};

/// Gateway configuration
pub struct GatewayConfig {
    pub port: u16,
    pub bind: String,
    pub max_concurrent: usize,
    pub spawn_gap_ms: u64,
    pub config: Config,
    pub workspace_root: String,
    pub danger: bool,
    pub model: Option<String>,
    pub image: String,
    pub trigger_store_path: String,
    pub scheduler_interval_secs: u64,
    pub timeout_secs: u64,
    /// "controller" (default) or "worker".
    pub role: String,
    /// For workers: the controller base URL to register up to.
    pub controller_url: Option<String>,
    /// Stable host id; defaults to hostname when empty.
    pub host_id: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let trigger_path = crate::paths::data_home()
            .join(".codex-monitor-triggers.json")
            .to_string_lossy()
            .to_string();

        Self {
            port: 4000,
            bind: "0.0.0.0".to_string(),
            max_concurrent: 2,
            spawn_gap_ms: 8000,
            config: Config::default(),
            workspace_root: "/workspace".to_string(),
            danger: false,
            model: None,
            image: "nemesis8:latest".to_string(),
            trigger_store_path: trigger_path,
            scheduler_interval_secs: 30,
            timeout_secs: 120,
            role: "controller".to_string(),
            controller_url: None,
            host_id: None,
        }
    }
}

/// Shared gateway state
struct AppState {
    docker: DockerOps,
    config: Config,
    concurrency: Semaphore,
    last_spawn: Mutex<std::time::Instant>,
    spawn_gap: std::time::Duration,
    active_count: Mutex<usize>,
    workspace_root: String,
    danger: bool,
    model: Option<String>,
    trigger_store_path: std::path::PathBuf,
    timeout_secs: u64,
    start_time: std::time::Instant,
    gateway_url: String,
    auth_token: Option<String>,
    /// Agent registry (control plane). Persisted to `registry_path`.
    registry: Mutex<Registry>,
    registry_path: std::path::PathBuf,
    /// This daemon's host id (hostname); agent ids are `{host_id}/{local_id}`.
    host_id: String,
    /// "controller" or "worker".
    role: String,
    /// For workers: controller base URL to register up to.
    controller_url: Option<String>,
    /// Reverse-tunnel mapping registry (runtime port exposure).
    tunnel_registry: Arc<Mutex<TunnelRegistry>>,
    /// Sibling chisel reverse-server port; API remains on `GatewayConfig::port`.
    tunnel_port: u16,
    /// Host string used in chisel `R:` remotes. Native host binary uses
    /// 127.0.0.1; Docker sidecar uses 0.0.0.0 inside the sidecar while Docker
    /// publishes the port range to host loopback only.
    tunnel_reverse_bind_host: &'static str,
    /// When the sidecar publishes the whole exposure range, host bind-testing
    /// sees every port as occupied. Allocation then relies on the registry.
    tunnel_ports_reserved_by_sidecar: bool,
    /// Disabled only in unit tests that exercise the HTTP control plane without
    /// a Docker daemon or chisel process.
    tunnel_transport_enabled: bool,
    telemetry: crate::telemetry::TelemetryState,
}

// ── Request / Response types ──

#[derive(Deserialize, Serialize)]
pub struct CompletionRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct CompletionResponse {
    pub session_id: String,
    pub status: String,
    pub output: String,
}

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub active: usize,
    pub max_concurrent: usize,
    pub uptime_secs: u64,
    pub scheduler: SchedulerStatus,
}

#[derive(Serialize, Deserialize)]
pub struct SchedulerStatus {
    pub trigger_count: usize,
    pub enabled_count: usize,
    pub next_fire: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Deserialize)]
struct CreateTriggerRequest {
    title: String,
    #[serde(default)]
    description: String,
    prompt_text: String,
    schedule: Schedule,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct UpdateTriggerRequest {
    title: Option<String>,
    description: Option<String>,
    prompt_text: Option<String>,
    schedule: Option<Schedule>,
    enabled: Option<bool>,
    tags: Option<Vec<String>>,
}

// ── Handlers ──

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let active = *state.active_count.lock().await;
    let store = TriggerStore::load(&state.trigger_store_path).unwrap_or_default();

    let enabled_count = store.triggers.iter().filter(|t| t.enabled).count();
    let next_fire = store
        .triggers
        .iter()
        .filter_map(|t| t.next_fire())
        .min()
        .map(|dt| dt.to_rfc3339());

    Json(StatusResponse {
        active,
        max_concurrent: state.concurrency.available_permits() + active,
        uptime_secs: state.start_time.elapsed().as_secs(),
        scheduler: SchedulerStatus {
            trigger_count: store.triggers.len(),
            enabled_count,
            next_fire,
        },
    })
}

async fn list_sessions_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    let session_dirs = resolve_session_dirs(&state.config);
    let dir_refs: Vec<&str> = session_dirs.iter().map(|s| s.as_str()).collect();

    match session::list_sessions(&dir_refs) {
        Ok(sessions) => Ok(Json(sessions)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SessionInfo>, StatusCode> {
    let session_dirs = resolve_session_dirs(&state.config);
    let dir_refs: Vec<&str> = session_dirs.iter().map(|s| s.as_str()).collect();

    match session::find_session(&id, &dir_refs) {
        Ok(Some(info)) => Ok(Json(info)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn completion(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Check concurrency
    let permit = match state.concurrency.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse {
                    error: "max concurrent runs reached, retry later".to_string(),
                }),
            ));
        }
    };

    // Enforce spawn throttle
    {
        let mut last = state.last_spawn.lock().await;
        let elapsed = last.elapsed();
        if elapsed < state.spawn_gap {
            let wait = state.spawn_gap - elapsed;
            tokio::time::sleep(wait).await;
        }
        *last = std::time::Instant::now();
    }

    // Track active count
    {
        let mut count = state.active_count.lock().await;
        *count += 1;
    }

    let session_id = req
        .session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let model = req.model.or_else(|| state.model.clone());

    // Run prompt through Docker container
    let output = state
        .docker
        .run_capture(
            &state.config,
            &req.prompt,
            state.danger,
            model.as_deref(),
            Some(&state.workspace_root),
            Some(&session_id),
            state.timeout_secs,
            Some(&state.gateway_url),
            state.auth_token.as_deref(),
        )
        .await;

    // Decrement active count
    {
        let mut count = state.active_count.lock().await;
        *count -= 1;
    }

    drop(permit);

    match output {
        Ok(out) => Ok(Json(CompletionResponse {
            session_id,
            status: "completed".to_string(),
            output: out,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

async fn session_prompt(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut req = req;
    req.session_id = Some(id);
    completion(State(state), Json(req)).await
}

// ── Trigger CRUD ──

async fn list_triggers(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<TriggerRecord>>, StatusCode> {
    match TriggerStore::load(&state.trigger_store_path) {
        Ok(store) => Ok(Json(store.triggers)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_trigger(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TriggerRecord>, StatusCode> {
    let store = TriggerStore::load(&state.trigger_store_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    store
        .triggers
        .into_iter()
        .find(|t| t.id == id)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn create_trigger(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTriggerRequest>,
) -> Result<Json<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
    let trigger = TriggerRecord {
        id: uuid::Uuid::new_v4().to_string()[..16].to_string(),
        title: req.title,
        description: req.description,
        schedule: req.schedule,
        prompt_text: req.prompt_text,
        created_by: String::new(),
        created_at: Some(chrono::Utc::now()),
        enabled: true,
        tags: req.tags,
        last_fired: None,
        last_status: None,
        last_error: None,
    };

    let mut store = TriggerStore::load(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    store.upsert(trigger.clone());
    store.save(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    Ok(Json(trigger))
}

async fn update_trigger(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpdateTriggerRequest>,
) -> Result<Json<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
    let mut store = TriggerStore::load(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let trigger = store.triggers.iter_mut().find(|t| t.id == id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: "trigger not found".into(),
        }),
    ))?;

    if let Some(title) = req.title {
        trigger.title = title;
    }
    if let Some(desc) = req.description {
        trigger.description = desc;
    }
    if let Some(prompt) = req.prompt_text {
        trigger.prompt_text = prompt;
    }
    if let Some(schedule) = req.schedule {
        trigger.schedule = schedule;
    }
    if let Some(enabled) = req.enabled {
        trigger.enabled = enabled;
    }
    if let Some(tags) = req.tags {
        trigger.tags = tags;
    }

    let updated = trigger.clone();

    store.save(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    Ok(Json(updated))
}

async fn delete_trigger(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let mut store = TriggerStore::load(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    if !store.remove(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "trigger not found".into(),
            }),
        ));
    }

    store.save(&state.trigger_store_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

// ── Scheduler ──

/// Background scheduler loop — polls triggers and dispatches due prompts through Docker.
async fn scheduler_loop(state: Arc<AppState>, interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;

        let mut store = match TriggerStore::load(&state.trigger_store_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("scheduler: failed to load triggers: {e}");
                continue;
            }
        };

        let due: Vec<String> = store.due_triggers().iter().map(|t| t.id.clone()).collect();

        if due.is_empty() {
            continue;
        }

        for id in &due {
            let trigger = match store.triggers.iter().find(|t| &t.id == id) {
                Some(t) => t.clone(),
                None => continue,
            };

            tracing::info!(
                trigger_id = %id,
                title = %trigger.title,
                "scheduler: firing trigger"
            );

            // Try to acquire a concurrency permit
            let permit = match state.concurrency.try_acquire() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        trigger_id = %id,
                        "scheduler: skipping trigger, max concurrent reached"
                    );
                    continue;
                }
            };

            // Enforce spawn throttle
            {
                let mut last = state.last_spawn.lock().await;
                let elapsed = last.elapsed();
                if elapsed < state.spawn_gap {
                    let wait = state.spawn_gap - elapsed;
                    tokio::time::sleep(wait).await;
                }
                *last = std::time::Instant::now();
            }

            {
                let mut count = state.active_count.lock().await;
                *count += 1;
            }

            // Dispatch through Docker
            let result = state
                .docker
                .run_capture(
                    &state.config,
                    &trigger.prompt_text,
                    state.danger,
                    state.model.as_deref(),
                    Some(&state.workspace_root),
                    None,
                    state.timeout_secs,
                    Some(&state.gateway_url),
                    state.auth_token.as_deref(),
                )
                .await;

            {
                let mut count = state.active_count.lock().await;
                *count -= 1;
            }
            drop(permit);

            // Update trigger state
            store.mark_fired(id);
            if let Some(t) = store.triggers.iter_mut().find(|t| &t.id == id) {
                match &result {
                    Ok(_) => {
                        t.last_status = Some("ok".to_string());
                        t.last_error = None;
                        tracing::info!(trigger_id = %id, "scheduler: trigger completed");
                    }
                    Err(e) => {
                        t.last_status = Some("error".to_string());
                        t.last_error = Some(e.to_string());
                        tracing::warn!(trigger_id = %id, error = %e, "scheduler: trigger failed");
                    }
                }
            }
        }

        if let Err(e) = store.save(&state.trigger_store_path) {
            tracing::warn!("scheduler: failed to save trigger state: {e}");
        }
    }
}

/// Resolve session directories from config
fn resolve_session_dirs(config: &Config) -> Vec<String> {
    let codex_service = crate::paths::data_home();

    let registry = crate::provider_registry::ProviderRegistry::load();
    let mut dirs: Vec<String> = registry
        .all()
        .flat_map(|def| {
            crate::session::expand_session_dirs(&codex_service, &def.provider.hooks.session_dirs)
        })
        .collect();
    dirs.sort();
    dirs.dedup();

    if let Some(from_config) = config.env.vars.get("CODEX_GATEWAY_SESSION_DIRS") {
        if !from_config.is_empty() {
            dirs.extend(from_config.split(',').map(|s| s.to_string()));
        }
    }

    dirs
}

/// Auth middleware: if NEMESIS8_AUTH_TOKEN is set, require matching Bearer token.
async fn auth_middleware(req: Request, next: Next) -> Response {
    let expected = match std::env::var("NEMESIS8_AUTH_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return next.run(req).await, // no token configured, pass through
    };

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(val) if val.strip_prefix("Bearer ").unwrap_or("") == expected => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized: invalid or missing Bearer token".to_string(),
            }),
        )
            .into_response(),
    }
}

/// Read the last N monitor events from the host-visible events file (which
/// is the same file the nemesis8-monitor inside the container writes to,
/// via the /opt/nemesis8 bind mount). Stub: returns plain JSON array.
/// Future versions should support SSE streaming and per-container filtering.
async fn monitor_events(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, Json<ErrorResponse>)> {
    let events_path = crate::paths::data_home()
        .join(".monitor")
        .join("events.jsonl");
    if !events_path.is_file() {
        return Ok(Json(Vec::new()));
    }

    let content = match std::fs::read_to_string(&events_path) {
        Ok(c) => c,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("reading events: {e}"),
                }),
            ));
        }
    };

    let mut events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Tail the last 100 by default.
    let take_from = events.len().saturating_sub(100);
    events.drain(..take_from);

    Ok(Json(events))
}

// ── Agent registry handlers ──

#[derive(Deserialize)]
struct SpawnAgentRequest {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Serialize)]
struct SpawnAck {
    status: String,
    message: String,
}

#[derive(Deserialize)]
struct RegisterAgentRequest {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    container_id: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
}

/// GET /agents — current registry snapshot (refreshed by the reconcile loop).
async fn list_agents(State(state): State<Arc<AppState>>) -> Json<Vec<AgentRecord>> {
    let reg = state.registry.lock().await;
    Json(reg.agents.clone())
}

/// GET /agents/{id} — one agent record. `{id}` is the local_id; we resolve it
/// to this host's `{host_id}/{local_id}` global id.
async fn get_agent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentRecord>, StatusCode> {
    let reg = state.registry.lock().await;
    let gid = resolve_agent_id(&reg, &state.host_id, &id);
    reg.get(&gid)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// POST /agents/{id}/kill — stop the agent's container, mark Killed.
async fn kill_agent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentRecord>, (StatusCode, Json<ErrorResponse>)> {
    let gid;
    let container_ref;
    let owner_host;
    let local_id;
    {
        let reg = state.registry.lock().await;
        gid = resolve_agent_id(&reg, &state.host_id, &id);
        let rec = reg.get(&gid).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no agent '{id}'"),
            }),
        ))?;
        container_ref = rec
            .container_id
            .clone()
            .or_else(|| rec.container_name.clone());
        owner_host = rec.host_id.clone();
        local_id = rec.local_id.clone();
    }

    // If the agent lives on another host, route the kill to its daemon.
    if owner_host != state.host_id {
        let daemon_url = {
            let reg = state.registry.lock().await;
            reg.daemon_for_host(&owner_host).map(|d| d.url.clone())
        };
        let Some(url) = daemon_url else {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("no daemon registered for host '{owner_host}'"),
                }),
            ));
        };
        let client = crate::remote::RemoteClient::new(&url, state.auth_token.as_deref());
        return client.kill_agent(&local_id).await.map(Json).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        });
    }

    if let Some(cref) = container_ref {
        let _ = state.docker.stop_container(&cref).await;
    }

    let mut reg = state.registry.lock().await;
    reg.mark_state(&gid, AgentState::Killed);
    let rec = reg.get(&gid).cloned();
    let _ = reg.save(&state.registry_path);
    rec.map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("no agent '{id}'"),
        }),
    ))
}

/// POST /agents/{id}/register — container self-registers on boot.
async fn register_agent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<RegisterAgentRequest>,
) -> Json<AgentRecord> {
    let gid = AgentRecord::global_id(&state.host_id, &id);
    let now = chrono::Utc::now();
    let mut reg = state.registry.lock().await;
    let existing = reg.get(&gid).cloned();
    let record = AgentRecord {
        id: gid.clone(),
        host_id: state.host_id.clone(),
        local_id: id.clone(),
        provider: req
            .provider
            .or_else(|| existing.as_ref().and_then(|e| e.provider.clone())),
        workspace: req
            .workspace
            .or_else(|| existing.as_ref().and_then(|e| e.workspace.clone())),
        container_id: req
            .container_id
            .or_else(|| existing.as_ref().and_then(|e| e.container_id.clone())),
        container_name: existing.as_ref().and_then(|e| e.container_name.clone()),
        state: AgentState::Running,
        source: crate::registry::AgentSource::Registered,
        started_at: existing.as_ref().and_then(|e| e.started_at).or(Some(now)),
        last_seen: Some(now),
        last_prompt: existing.as_ref().and_then(|e| e.last_prompt.clone()),
    };
    reg.upsert(record.clone());
    let _ = reg.save(&state.registry_path);
    let _ = req.pid; // pid currently informational
    Json(record)
}

/// POST /agents/{id}/deregister — container exiting.
async fn deregister_agent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> StatusCode {
    let gid = AgentRecord::global_id(&state.host_id, &id);
    let mut reg = state.registry.lock().await;
    reg.mark_state(&gid, AgentState::Exited);
    let _ = reg.save(&state.registry_path);
    StatusCode::NO_CONTENT
}

/// POST /agents/spawn — launch a new agent by re-invoking the n8 binary
/// detached (`n8 run <prompt>`). The spawned container is labeled, so the
/// reconcile loop discovers it within one tick and it appears in /agents.
/// This avoids cloning Config/DockerOps into a background task and reuses the
/// exact same launch path as a manual `n8 run`.
async fn spawn_agent(
    State(_state): State<Arc<AppState>>,
    Json(req): Json<SpawnAgentRequest>,
) -> Result<Json<SpawnAck>, (StatusCode, Json<ErrorResponse>)> {
    let prompt = req.prompt.unwrap_or_default();
    if prompt.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "prompt is required".into(),
            }),
        ));
    }
    let exe = std::env::current_exe().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run").arg(&prompt);
    if let Some(p) = req.provider {
        cmd.arg("--provider").arg(p);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("spawn failed: {e}"),
            }),
        )
    })?;

    Ok(Json(SpawnAck {
        status: "spawning".into(),
        message: "agent launching; it will appear in /agents within one reconcile tick (~10s)"
            .into(),
    }))
}

#[derive(Deserialize)]
struct DaemonRegisterRequest {
    host_id: String,
    url: String,
    #[serde(default)]
    role: String,
}

#[derive(Deserialize)]
struct AgentsSyncRequest {
    host_id: String,
    agents: Vec<AgentRecord>,
}

/// POST /daemons/register — a worker registers itself with the controller.
async fn register_daemon(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DaemonRegisterRequest>,
) -> Json<crate::registry::DaemonRecord> {
    let rec = crate::registry::DaemonRecord {
        host_id: req.host_id,
        url: req.url,
        role: if req.role.is_empty() {
            "worker".into()
        } else {
            req.role
        },
        last_seen: chrono::Utc::now(),
        agent_count: 0,
    };
    let mut reg = state.registry.lock().await;
    reg.upsert_daemon(rec.clone());
    let _ = reg.save(&state.registry_path);
    Json(rec)
}

/// GET /daemons — list known worker daemons (controller view).
async fn list_daemons(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::registry::DaemonRecord>> {
    let reg = state.registry.lock().await;
    Json(reg.daemons.clone())
}

/// POST /agents/sync — a worker pushes its full local agent snapshot up.
/// The controller replaces all records for that host with the snapshot.
async fn sync_agents(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentsSyncRequest>,
) -> StatusCode {
    let mut reg = state.registry.lock().await;
    reg.replace_host_agents(&req.host_id, req.agents);
    let _ = reg.save(&state.registry_path);
    StatusCode::NO_CONTENT
}

/// Resolve a user-supplied id (local_id, global id, or prefix) to a global id.
fn resolve_agent_id(reg: &Registry, host_id: &str, id: &str) -> String {
    // Exact global id?
    if reg.agents.iter().any(|a| a.id == id) {
        return id.to_string();
    }
    // host_id/local_id form for this host?
    let gid = AgentRecord::global_id(host_id, id);
    if reg.agents.iter().any(|a| a.id == gid) {
        return gid;
    }
    // Prefix match on local_id (e.g. short id).
    if let Some(a) = reg
        .agents
        .iter()
        .find(|a| a.local_id.starts_with(id) || a.id.ends_with(id))
    {
        return a.id.clone();
    }
    gid
}

/// Background loop: reconcile the registry against live containers so agents
/// started outside the API are discovered and dead ones are marked Exited.
async fn reconcile_loop(state: Arc<AppState>, interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    loop {
        interval.tick().await;
        let containers = match state.docker.list_containers("").await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("reconcile: list_containers failed: {e}");
                continue;
            }
        };
        let mut reg = state.registry.lock().await;
        reg.reconcile(&containers, &state.host_id);
        if let Err(e) = reg.save(&state.registry_path) {
            tracing::warn!("reconcile: save failed: {e}");
        }
    }
}

/// Worker daemon loop: register with the controller, then push the local
/// agent snapshot up every 15s. Re-registers if a sync fails (controller may
/// have restarted).
async fn worker_sync_loop(state: Arc<AppState>, controller_url: String, own_url: String) {
    let client = reqwest::Client::new();
    let base = controller_url.trim_end_matches('/').to_string();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    let mut registered = false;

    loop {
        interval.tick().await;

        if !registered {
            let body = serde_json::json!({
                "host_id": &state.host_id, "url": &own_url, "role": "worker"
            });
            let mut req = client.post(format!("{base}/daemons/register")).json(&body);
            if let Some(t) = &state.auth_token {
                req = req.bearer_auth(t);
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => {
                    registered = true;
                    tracing::info!(controller = %base, "worker registered with controller");
                }
                Ok(r) => tracing::warn!("worker register got {}", r.status()),
                Err(e) => tracing::warn!("worker register failed: {e}"),
            }
        }

        let agents: Vec<AgentRecord> = {
            let reg = state.registry.lock().await;
            reg.agents
                .iter()
                .filter(|a| a.host_id == state.host_id)
                .cloned()
                .collect()
        };
        let body = serde_json::json!({ "host_id": &state.host_id, "agents": agents });
        let mut req = client.post(format!("{base}/agents/sync")).json(&body);
        if let Some(t) = &state.auth_token {
            req = req.bearer_auth(t);
        }
        if let Err(e) = req.send().await {
            registered = false;
            tracing::warn!("worker sync failed: {e}");
        }
    }
}

// ── Reverse-tunnel endpoints (runtime port exposure) ──────────────────────

/// POST /expose — allocate a host port for a container's internal port.
async fn expose_port(
    State(state): State<Arc<AppState>>,
    Json(req): Json<tunnel::ExposeRequest>,
) -> Result<Json<tunnel::ExposeResponse>, (StatusCode, Json<ErrorResponse>)> {
    if req.port == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "port must be between 1 and 65535".into(),
            }),
        ));
    }
    let container_ref = if state.tunnel_transport_enabled {
        Some(resolve_tunnel_container(&state, &req.agent_id).await?)
    } else {
        None
    };
    let host_port = {
        let reg = state.tunnel_registry.lock().await;
        let used = reg.used_ports();
        let allocated = if state.tunnel_ports_reserved_by_sidecar {
            tunnel::allocate_reserved_port(&used)
        } else {
            tunnel::allocate_port(&used)
        };
        allocated.ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "no free ports in tunnel range (18000-18999)".into(),
            }),
        ))?
    };
    let id = uuid::Uuid::new_v4().to_string()[..16].to_string();
    let mapping = tunnel::PortMapping {
        id: id.clone(),
        agent_id: req.agent_id.clone(),
        internal_port: req.port,
        host_port,
        name: req.name.unwrap_or_else(|| format!("port-{}", req.port)),
        state: tunnel::MappingState::Pending,
        container_ref: container_ref.clone(),
        tunnel_port: Some(state.tunnel_port),
    };
    {
        let mut reg = state.tunnel_registry.lock().await;
        reg.mappings.insert(id.clone(), mapping);
    }
    if let Some(container_ref) = container_ref.as_deref() {
        if let Err(e) = start_chisel_client(&state, container_ref, host_port, req.port).await {
            state.tunnel_registry.lock().await.mappings.remove(&id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("starting chisel client failed: {e}"),
                }),
            ));
        }
        if !tunnel::wait_for_port(host_port, std::time::Duration::from_secs(5)).await {
            let _ = stop_chisel_client(&state, container_ref, host_port, req.port).await;
            state.tunnel_registry.lock().await.mappings.remove(&id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("chisel tunnel did not become ready on 127.0.0.1:{host_port}"),
                }),
            ));
        }
        if let Some(m) = state.tunnel_registry.lock().await.mappings.get_mut(&id) {
            m.state = tunnel::MappingState::Live;
        }
    }
    let url = format!("http://127.0.0.1:{host_port}");
    tracing::info!(%id, host_port, internal_port = req.port, "port exposed");
    Ok(Json(tunnel::ExposeResponse {
        id,
        public_url: url,
        host_port,
    }))
}

/// POST /unexpose — release a port mapping.
async fn unexpose_port(
    State(state): State<Arc<AppState>>,
    Json(req): Json<tunnel::UnexposeRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let mut reg = state.tunnel_registry.lock().await;
    let Some(mapping) = reg.mappings.remove(&req.id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "mapping not found".into(),
            }),
        ));
    };
    drop(reg);
    if let Some(container_ref) = mapping.container_ref.as_deref() {
        if let Err(e) = stop_chisel_client(
            &state,
            container_ref,
            mapping.host_port,
            mapping.internal_port,
        )
        .await
        {
            tracing::warn!(id = %req.id, error = %e, "failed to stop chisel client");
        }
    }
    tracing::info!(id = %req.id, "port unexposed");
    Ok(StatusCode::NO_CONTENT)
}

/// GET /exposed — list all active port mappings.
async fn list_exposed(State(state): State<Arc<AppState>>) -> Json<Vec<tunnel::PortMapping>> {
    let reg = state.tunnel_registry.lock().await;
    let mut mappings: Vec<_> = reg.mappings.values().cloned().collect();
    mappings.sort_by_key(|m| m.host_port);
    Json(mappings)
}

async fn resolve_tunnel_container(
    state: &Arc<AppState>,
    agent_id: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let reg = state.registry.lock().await;
    let gid = resolve_agent_id(&reg, &state.host_id, agent_id);
    if let Some(rec) = reg.get(&gid) {
        if let Some(id) = rec.container_id.as_ref().filter(|s| !s.is_empty()) {
            return Ok(id.clone());
        }
        if let Some(name) = rec.container_name.as_ref().filter(|s| !s.is_empty()) {
            return Ok(name.clone());
        }
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("agent '{agent_id}' has no live container"),
            }),
        ));
    }
    Ok(agent_id.to_string())
}

async fn start_chisel_client(
    state: &Arc<AppState>,
    container_ref: &str,
    host_port: u16,
    internal_port: u16,
) -> anyhow::Result<()> {
    let remote = format!(
        "R:{}:{host_port}:localhost:{internal_port}",
        state.tunnel_reverse_bind_host
    );
    let target = format!("host.docker.internal:{}", state.tunnel_port);
    let status = tokio::process::Command::new(&state.docker.runtime_binary)
        .args([
            "exec",
            "-d",
            container_ref,
            "chisel",
            "client",
            "--keepalive",
            "10s",
            &target,
            &remote,
        ])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("docker exec exited with {status}");
    }
    Ok(())
}

async fn stop_chisel_client(
    state: &Arc<AppState>,
    container_ref: &str,
    host_port: u16,
    internal_port: u16,
) -> anyhow::Result<()> {
    let pattern = format!("chisel client.*R:.*:{host_port}:localhost:{internal_port}");
    let script = format!("pkill -f '{}' || true", pattern.replace('\'', "'\\''"));
    let status = tokio::process::Command::new(&state.docker.runtime_binary)
        .args(["exec", container_ref, "sh", "-lc", &script])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("docker exec exited with {status}");
    }
    Ok(())
}

/// Start the HTTP gateway with integrated scheduler
pub async fn serve(gw_config: GatewayConfig) -> Result<()> {
    let docker = DockerOps::new(Some(&gw_config.image))?;
    let trigger_path = std::path::PathBuf::from(&gw_config.trigger_store_path);
    // Trainer API rides along with the gateway ("starts with nemesis8"): the
    // Sailfish tool-run data plane on 127.0.0.1:18042. Localhost-only, never
    // blocks gateway startup; a failed bind (standalone `n8 trainer` already
    // running) just logs and moves on.
    tokio::spawn(async {
        if let Err(e) = crate::trainer_api::serve(crate::trainer_api::TRAINER_PORT).await {
            tracing::warn!(error = %e, "trainer API not started (port busy or bind failed)");
        }
    });
    let tunnel_port = tunnel::sibling_tunnel_port(gw_config.port);
    // The reverse-tunnel data plane (chisel) is OPTIONAL and must NEVER block or
    // slow gateway startup — the gateway, scheduler, and agent registry don't need
    // it; only runtime port-exposure (expose_port) does.
    //
    // If there's a host chisel binary, start it (fast — just spawns a process).
    // If there ISN'T, we DON'T fall back to the `docker run` chisel sidecar here:
    // that path blocked boot for ~3 minutes (slow/flaky container start, eventual
    // exit 125), so `n8 --danger` hit the readiness timeout even though the gateway
    // limped up later. Skip it — port-exposure stays disabled until a host chisel
    // is installed; everything else runs normally and binds immediately.
    let chisel_server = if tunnel::find_chisel_binary().is_none() {
        tracing::warn!("no chisel binary on host; runtime port-exposure (expose_port) disabled this session — install chisel on the host to enable it");
        None
    } else {
        match tunnel::ensure_chisel_server(tunnel_port, &docker.runtime_binary) {
            Ok(srv) => {
                if tunnel::wait_for_port(tunnel_port, std::time::Duration::from_secs(5)).await {
                    Some(srv)
                } else {
                    tracing::warn!(port = tunnel_port, "chisel reverse server did not become ready; runtime port-exposure disabled this session");
                    None
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "chisel reverse server failed to start; runtime port-exposure disabled this session");
                None
            }
        }
    };

    // Ensure trigger store parent dir exists
    if let Some(parent) = trigger_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let scheduler_interval = gw_config.scheduler_interval_secs;

    let gateway_url = format!("http://host.docker.internal:{}", gw_config.port);
    let auth_token = std::env::var("NEMESIS8_AUTH_TOKEN").ok();

    // Agent registry persisted next to the trigger store.
    let registry_path = trigger_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("agents.json");
    let registry = Registry::load(&registry_path).unwrap_or_default();
    let host_id = gw_config
        .host_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(crate::docker::host_id);
    let role = gw_config.role.clone();
    let controller_url = gw_config.controller_url.clone();
    let port = gw_config.port;

    let state = Arc::new(AppState {
        docker,
        config: gw_config.config,
        concurrency: Semaphore::new(gw_config.max_concurrent),
        last_spawn: Mutex::new(std::time::Instant::now()),
        spawn_gap: std::time::Duration::from_millis(gw_config.spawn_gap_ms),
        active_count: Mutex::new(0),
        workspace_root: gw_config.workspace_root,
        danger: gw_config.danger,
        model: gw_config.model,
        trigger_store_path: trigger_path,
        timeout_secs: gw_config.timeout_secs,
        start_time: std::time::Instant::now(),
        gateway_url,
        auth_token,
        registry: Mutex::new(registry),
        registry_path,
        host_id: host_id.clone(),
        role: role.clone(),
        controller_url: controller_url.clone(),
        tunnel_registry: Arc::new(Mutex::new(TunnelRegistry::new())),
        tunnel_port,
        tunnel_reverse_bind_host: chisel_server.as_ref().map_or("127.0.0.1", |c| c.reverse_bind_host),
        tunnel_ports_reserved_by_sidecar: chisel_server.as_ref().is_some_and(|c| c.ports_reserved_by_sidecar),
        tunnel_transport_enabled: chisel_server.is_some(),
        telemetry: crate::telemetry::TelemetryState::new(10000),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/sessions", get(list_sessions_handler))
        .route("/sessions/{id}", get(get_session).post(session_prompt))
        .route("/completion", post(completion))
        .route("/triggers", get(list_triggers).post(create_trigger))
        .route(
            "/triggers/{id}",
            get(get_trigger).put(update_trigger).delete(delete_trigger),
        )
        .route("/monitor/events", get(monitor_events))
        .route("/agents", get(list_agents))
        .route("/agents/spawn", post(spawn_agent))
        .route("/agents/sync", post(sync_agents))
        .route("/agents/{id}", get(get_agent))
        .route("/agents/{id}/kill", post(kill_agent))
        .route("/agents/{id}/register", post(register_agent))
        .route("/agents/{id}/deregister", post(deregister_agent))
        .route("/daemons", get(list_daemons))
        .route("/daemons/register", post(register_daemon))
        .route("/expose", post(expose_port))
        .route("/unexpose", post(unexpose_port))
        .route("/exposed", get(list_exposed))
        .route("/mcp", post(mcp_handler))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state.clone())
        // Fleet telemetry dashboard (#84, telemetry_web): merged post-state
        // with the SAME auth layer, so /fleet inherits the gateway's posture
        // (open when no token configured, bearer-gated when one is).
        .merge(
            crate::telemetry_web::routes(state.telemetry.clone())
                .layer(middleware::from_fn(auth_middleware)),
        );

    // Spawn the scheduler loop
    let sched_state = state.clone();
    tokio::spawn(async move {
        scheduler_loop(sched_state, scheduler_interval).await;
    });

    // Spawn the registry reconciliation loop (discovers agents started outside
    // the API, marks dead ones Exited). 10s cadence.
    let reconcile_state = state.clone();
    tokio::spawn(async move {
        reconcile_loop(reconcile_state, 10).await;
    });

    // If this daemon is a worker, register up to the controller and push the
    // local agent snapshot on a heartbeat.
    if state.role == "worker" {
        match state.controller_url.clone() {
            Some(curl) => {
                let own_url = format!("http://{host_id}:{port}");
                let worker_state = state.clone();
                tracing::info!(controller = %curl, own = %own_url, "starting as worker daemon");
                tokio::spawn(async move {
                    worker_sync_loop(worker_state, curl, own_url).await;
                });
            }
            None => {
                tracing::warn!(
                    "control_plane.role=worker but no controller_url set; running standalone"
                );
            }
        }
    }

    let addr = format!("{}:{}", gw_config.bind, gw_config.port);
    tracing::info!(
        addr = %addr,
        triggers = %gw_config.trigger_store_path,
        "gateway + scheduler listening"
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn jsonrpc_error(id: serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn jsonrpc_success(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn make_rpc_response(headers: &axum::http::HeaderMap, value: serde_json::Value) -> Response {
    let accept_sse = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("text/event-stream"));

    if accept_sse {
        let serialized = serde_json::to_string(&value).unwrap_or_default();
        let body = format!("data: {}\n\n", serialized);
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(body))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_string(&value).unwrap_or_default()))
            .unwrap()
    }
}

async fn mcp_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let request: serde_json::Value = match serde_json::from_str(&body) {
        Ok(val) => val,
        Err(_) => {
            return make_rpc_response(
                &headers,
                jsonrpc_error(serde_json::Value::Null, -32700, "Parse error: invalid JSON"),
            );
        }
    };

    let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);

    // Validate jsonrpc == "2.0"
    let jsonrpc = request.get("jsonrpc").and_then(|v| v.as_str());
    if jsonrpc != Some("2.0") {
        return make_rpc_response(
            &headers,
            jsonrpc_error(id, -32600, "Invalid Request: jsonrpc version must be '2.0'"),
        );
    }

    let method = match request.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return make_rpc_response(&headers, jsonrpc_error(id, -32600, "Invalid Request: missing method")),
    };

    let response_val = match method {
        "initialize" => {
            let params = request.get("params");
            let protocol_version = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");

            let result = serde_json::json!({
                "protocolVersion": protocol_version,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "nemesis8",
                    "version": "0.18.12"
                }
            });
            jsonrpc_success(id, result)
        }
        "tools/list" => {
            let result = serde_json::json!({
                "tools": [
                    {
                        "name": "fleet_status",
                        "description": "Get one row per agent containing agent_id, provider, workspace, state, uptime, cpu_pct, mem_used_kb, net_rx_bps, net_tx_bps, and last_ts.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    },
                    {
                        "name": "agent_events",
                        "description": "Get filtered events, newest first. Optional filters for agent_id, kinds, since, q, and limit.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "agent_id": { "type": "string", "description": "Filter by agent ID" },
                                "kinds": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Filter by event kinds"
                                },
                                "since": { "type": "integer", "description": "Filter by timestamp (since)" },
                                "q": { "type": "string", "description": "Sub-string text query" },
                                "limit": { "type": "integer", "description": "Limit response size (default 100)" }
                            }
                        }
                    },
                    {
                        "name": "agent_net",
                        "description": "Get per-agent network rate history. Optional window size (default 16).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "window": { "type": "integer", "description": "History window size" }
                            }
                        }
                    },
                    {
                        "name": "event_facets",
                        "description": "Get event counts by kind.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    },
                    {
                        "name": "telemetry_health",
                        "description": "Get telemetry health probe data.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            });
            jsonrpc_success(id, result)
        }
        "tools/call" => {
            let params = match request.get("params") {
                Some(p) => p,
                None => return make_rpc_response(&headers, jsonrpc_error(id.clone(), -32602, "Invalid params: missing params")),
            };
            let name = match params.get("name").and_then(|n| n.as_str()) {
                Some(n) => n,
                None => return make_rpc_response(&headers, jsonrpc_error(id.clone(), -32602, "Invalid params: missing tool name")),
            };
            let arguments = params.get("arguments").cloned().unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

            match name {
                "fleet_status" => {
                    state.telemetry.refresh();
                    let containers = match state.docker.list_containers("").await {
                        Ok(c) => c,
                        Err(e) => {
                            return make_rpc_response(&headers, jsonrpc_error(id.clone(), -32603, &format!("Docker error: {}", e)));
                        }
                    };

                    let fleet_containers: Vec<crate::telemetry::FleetContainer> = containers
                        .into_iter()
                        .map(|c| {
                            let cname = c
                                .names
                                .as_ref()
                                .and_then(|n| n.first())
                                .map(|n| n.trim_start_matches('/').to_string());
                            let name = c.labels.as_ref()
                                .and_then(|l| l.get(crate::docker::LABEL_AGENT_ID))
                                .cloned()
                                .or_else(|| cname.clone())
                                .unwrap_or_else(|| "unknown".to_string());
                            let provider = c.labels.as_ref()
                                .and_then(|l| l.get(crate::docker::LABEL_PROVIDER))
                                .cloned()
                                .unwrap_or_default();
                            let workspace = c.labels.as_ref()
                                .and_then(|l| l.get(crate::docker::LABEL_WORKSPACE))
                                .cloned()
                                .unwrap_or_default();
                            let state = c.state.clone().unwrap_or_default();
                            let now = chrono::Utc::now().timestamp();
                            let created = c.created.unwrap_or(0);
                            let uptime = if created > 0 && now >= created {
                                (now - created) as u64
                            } else {
                                0
                            };
                            crate::telemetry::FleetContainer {
                                name,
                                provider,
                                workspace,
                                state,
                                uptime,
                            }
                        })
                        .collect();

                    let index_guard = state.telemetry.index.lock().unwrap_or_else(|p| p.into_inner());
                    let rows = crate::telemetry::fleet_rows(&index_guard, &fleet_containers);
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&rows).unwrap_or_default()
                            }
                        ],
                        "structuredContent": rows
                    });
                    jsonrpc_success(id, result)
                }
                "agent_events" => {
                    let arg_agent_id = arguments.get("agent_id").and_then(|v| v.as_str().map(|s| s.to_string()));
                    let arg_kinds: Vec<String> = arguments.get("kinds")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                        .unwrap_or_default();
                    let arg_since = arguments.get("since").and_then(|v| v.as_u64());
                    let arg_q = arguments.get("q").and_then(|v| v.as_str().map(|s| s.to_string()));
                    let arg_limit = arguments.get("limit").and_then(|v| v.as_u64()).map(|l| l as usize).unwrap_or(100);

                    state.telemetry.refresh();
                    let index_guard = state.telemetry.index.lock().unwrap_or_else(|p| p.into_inner());
                    let query = crate::event_index::EventQuery {
                        kinds: arg_kinds,
                        since: arg_since,
                        until: None,
                        text: arg_q,
                        limit: usize::MAX,
                    };
                    let mut events = index_guard.query(&query);
                    if let Some(ref target_agent_id) = arg_agent_id {
                        events.retain(|e| e.agent_id.as_ref() == Some(target_agent_id));
                    }
                    events.truncate(arg_limit);

                    let raw_events: Vec<serde_json::Value> = events.iter().map(|e| e.raw.clone()).collect();
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&raw_events).unwrap_or_default()
                            }
                        ],
                        "structuredContent": raw_events
                    });
                    jsonrpc_success(id, result)
                }
                "agent_net" => {
                    let window = arguments.get("window").and_then(|v| v.as_u64()).map(|w| w as usize).unwrap_or(16);
                    state.telemetry.refresh();
                    let index_guard = state.telemetry.index.lock().unwrap_or_else(|p| p.into_inner());
                    let net_stats = crate::telemetry::agent_net_stats(&index_guard, window);
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&net_stats).unwrap_or_default()
                            }
                        ],
                        "structuredContent": net_stats
                    });
                    jsonrpc_success(id, result)
                }
                "event_facets" => {
                    state.telemetry.refresh();
                    let index_guard = state.telemetry.index.lock().unwrap_or_else(|p| p.into_inner());
                    let facets = index_guard.facets();
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&facets).unwrap_or_default()
                            }
                        ],
                        "structuredContent": facets
                    });
                    jsonrpc_success(id, result)
                }
                "telemetry_health" => {
                    state.telemetry.refresh();
                    let index_guard = state.telemetry.index.lock().unwrap_or_else(|p| p.into_inner());
                    let health = crate::telemetry::health(&index_guard, &state.telemetry.events_path);
                    let result = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": serde_json::to_string(&health).unwrap_or_default()
                            }
                        ],
                        "structuredContent": health
                    });
                    jsonrpc_success(id, result)
                }
                _ => return make_rpc_response(&headers, jsonrpc_error(id, -32601, &format!("Tool not found: {}", name))),
            }
        }
        _ => return make_rpc_response(&headers, jsonrpc_error(id, -32601, &format!("Method not found: {}", method))),
    };

    make_rpc_response(&headers, response_val)
}

/// Build the gateway router with the given state (used by tests)
#[cfg(test)]
fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/sessions", get(list_sessions_handler))
        .route("/sessions/{id}", get(get_session).post(session_prompt))
        .route("/completion", post(completion))
        .route("/triggers", get(list_triggers).post(create_trigger))
        .route(
            "/triggers/{id}",
            get(get_trigger).put(update_trigger).delete(delete_trigger),
        )
        .route("/expose", post(expose_port))
        .route("/unexpose", post(unexpose_port))
        .route("/exposed", get(list_exposed))
        .route("/mcp", post(mcp_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        let docker = DockerOps::new(None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let trigger_path = dir.path().join("triggers.json");
        let registry_path = dir.path().join("agents.json");
        // Leak the tempdir so it lives for the test
        std::mem::forget(dir);

        Arc::new(AppState {
            docker,
            config: Config::default(),
            concurrency: Semaphore::new(2),
            last_spawn: Mutex::new(std::time::Instant::now()),
            spawn_gap: std::time::Duration::from_millis(0),
            active_count: Mutex::new(0),
            workspace_root: "/workspace".to_string(),
            danger: false,
            model: None,
            trigger_store_path: trigger_path,
            timeout_secs: 120,
            start_time: std::time::Instant::now(),
            gateway_url: "http://host.docker.internal:4000".to_string(),
            auth_token: None,
            registry: Mutex::new(Registry::default()),
            registry_path,
            host_id: "testhost".to_string(),
            role: "controller".to_string(),
            controller_url: None,
            tunnel_registry: Arc::new(Mutex::new(TunnelRegistry::new())),
            tunnel_port: 4001,
            tunnel_reverse_bind_host: "127.0.0.1",
            tunnel_ports_reserved_by_sidecar: false,
            tunnel_transport_enabled: false,
            telemetry: crate::telemetry::TelemetryState::new(10000),
        })
    }

    fn test_router() -> Router {
        build_router(test_state())
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn test_status_endpoint() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active"], 0);
        assert_eq!(json["max_concurrent"], 2);
        assert!(json["scheduler"].is_object());
    }

    #[tokio::test]
    async fn test_sessions_endpoint_ok() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array());
    }

    #[tokio::test]
    async fn test_session_not_found() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sessions/nonexistent-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_unknown_route_returns_404() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_gateway_config_defaults() {
        let config = GatewayConfig::default();
        assert_eq!(config.port, 4000);
        assert_eq!(config.bind, "0.0.0.0");
        assert_eq!(config.max_concurrent, 2);
        assert_eq!(config.spawn_gap_ms, 8000);
        assert!(!config.danger);
        assert!(config.model.is_none());
    }

    // Integration test: /expose now starts the chisel data plane, which needs a
    // docker daemon + the chisel binary + a real container. The unit harness has
    // none, so the handler correctly 503s here. Ignored by default; run with
    // `cargo test -- --ignored` against a live environment.
    #[tokio::test]
    #[ignore = "integration: requires docker + chisel data plane (unavailable in unit tests)"]
    async fn test_expose_lifecycle() {
        let app = test_router();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/expose")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"agent_id":"test-agent","port":3000,"name":"dev"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = json["id"].as_str().unwrap().to_string();
        let host_port = json["host_port"].as_u64().unwrap();
        assert!((18000..=18999).contains(&host_port));
        assert_eq!(json["public_url"], format!("http://127.0.0.1:{host_port}"));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/exposed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], id);
        assert_eq!(list[0]["state"], "pending");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/unexpose")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"id":"{id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_expose_rejects_zero_port() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/expose")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"agent_id":"test-agent","port":0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_triggers_endpoint_empty() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/triggers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_trigger_not_found() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/triggers/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_mcp_integration() {
        // Skip the test if no container socket or named pipe is available
        #[cfg(windows)]
        let docker_available = std::path::Path::new("//./pipe/docker_engine").exists() || std::env::var("DOCKER_HOST").is_ok();
        #[cfg(not(windows))]
        let docker_available = std::path::Path::new("/var/run/docker.sock").exists() || std::env::var("DOCKER_HOST").is_ok();

        if !docker_available {
            println!("Skipping test_mcp_integration because Docker daemon is not available");
            return;
        }

        let app = test_router();

        // 1. initialize
        let req_init = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#))
            .unwrap();
        let res_init = app.clone().oneshot(req_init).await.unwrap();
        assert_eq!(res_init.status(), StatusCode::OK);
        let body_init = res_init.into_body().collect().await.unwrap().to_bytes();
        let json_init: serde_json::Value = serde_json::from_slice(&body_init).unwrap();
        assert_eq!(json_init["result"]["protocolVersion"], "2024-11-05");

        // 2. tools/list
        let req_list = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#))
            .unwrap();
        let res_list = app.clone().oneshot(req_list).await.unwrap();
        assert_eq!(res_list.status(), StatusCode::OK);
        let body_list = res_list.into_body().collect().await.unwrap().to_bytes();
        let json_list: serde_json::Value = serde_json::from_slice(&body_list).unwrap();
        let tools = json_list["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "fleet_status"));

        // 3. tools/call(fleet_status)
        let req_call = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fleet_status","arguments":{}}}"#))
            .unwrap();
        let res_call = app.oneshot(req_call).await.unwrap();
        assert_eq!(res_call.status(), StatusCode::OK);
        let body_call = res_call.into_body().collect().await.unwrap().to_bytes();
        let json_call: serde_json::Value = serde_json::from_slice(&body_call).unwrap();
        assert!(json_call.get("result").is_some() || json_call.get("error").is_some());
    }

    #[tokio::test]
    async fn test_mcp_malformed_body() {
        // Skip the test if no container socket or named pipe is available
        #[cfg(windows)]
        let docker_available = std::path::Path::new("//./pipe/docker_engine").exists() || std::env::var("DOCKER_HOST").is_ok();
        #[cfg(not(windows))]
        let docker_available = std::path::Path::new("/var/run/docker.sock").exists() || std::env::var("DOCKER_HOST").is_ok();

        if !docker_available {
            println!("Skipping test_mcp_malformed_body because Docker daemon is not available");
            return;
        }

        let app = test_router();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from("{invalid_json}"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], serde_json::Value::Null);
        assert_eq!(json["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn test_mcp_invalid_jsonrpc_version() {
        // Skip the test if no container socket or named pipe is available
        #[cfg(windows)]
        let docker_available = std::path::Path::new("//./pipe/docker_engine").exists() || std::env::var("DOCKER_HOST").is_ok();
        #[cfg(not(windows))]
        let docker_available = std::path::Path::new("/var/run/docker.sock").exists() || std::env::var("DOCKER_HOST").is_ok();

        if !docker_available {
            println!("Skipping test_mcp_invalid_jsonrpc_version because Docker daemon is not available");
            return;
        }

        let app = test_router();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":42,"method":"tools/list"}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn test_mcp_poisoned_lock() {
        // Skip the test if no container socket or named pipe is available
        #[cfg(windows)]
        let docker_available = std::path::Path::new("//./pipe/docker_engine").exists() || std::env::var("DOCKER_HOST").is_ok();
        #[cfg(not(windows))]
        let docker_available = std::path::Path::new("/var/run/docker.sock").exists() || std::env::var("DOCKER_HOST").is_ok();

        if !docker_available {
            println!("Skipping test_mcp_poisoned_lock because Docker daemon is not available");
            return;
        }

        let state = test_state();
        let app = build_router(state.clone());

        // Poison the lock in telemetry.index
        let index_clone = state.telemetry.index.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = index_clone.lock().unwrap();
            panic!("poisoning");
        });

        // Querying telemetry_health should succeed because the lock poisoning is recovered
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"telemetry_health","arguments":{}}}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("result").is_some());
    }
}
