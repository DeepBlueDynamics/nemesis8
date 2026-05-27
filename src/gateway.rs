use anyhow::Result;
use axum::{
    extract::{Path as AxumPath, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Config;
use crate::docker::DockerOps;
use crate::registry::{AgentRecord, AgentState, Registry};
use crate::scheduler::{Schedule, TriggerRecord, TriggerStore};
use crate::session::{self, SessionInfo};

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
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let trigger_path = home
            .join(".codex-service/.codex-monitor-triggers.json")
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
            image: "nemisis8:latest".to_string(),
            trigger_store_path: trigger_path,
            scheduler_interval_secs: 30,
            timeout_secs: 120,
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
    let store = TriggerStore::load(&state.trigger_store_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

    let mut store = TriggerStore::load(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    store.upsert(trigger.clone());
    store
        .save(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    Ok(Json(trigger))
}

async fn update_trigger(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpdateTriggerRequest>,
) -> Result<Json<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
    let mut store = TriggerStore::load(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    let trigger = store
        .triggers
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or((StatusCode::NOT_FOUND, Json(ErrorResponse { error: "trigger not found".into() })))?;

    if let Some(title) = req.title { trigger.title = title; }
    if let Some(desc) = req.description { trigger.description = desc; }
    if let Some(prompt) = req.prompt_text { trigger.prompt_text = prompt; }
    if let Some(schedule) = req.schedule { trigger.schedule = schedule; }
    if let Some(enabled) = req.enabled { trigger.enabled = enabled; }
    if let Some(tags) = req.tags { trigger.tags = tags; }

    let updated = trigger.clone();

    store
        .save(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    Ok(Json(updated))
}

async fn delete_trigger(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let mut store = TriggerStore::load(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    if !store.remove(&id) {
        return Err((StatusCode::NOT_FOUND, Json(ErrorResponse { error: "trigger not found".into() })));
    }

    store
        .save(&state.trigger_store_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

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

        let due: Vec<String> = store
            .due_triggers()
            .iter()
            .map(|t| t.id.clone())
            .collect();

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
    let home = dirs::home_dir().unwrap_or_default();
    let codex_service = home.join(".codex-service");

    let registry = crate::provider_registry::ProviderRegistry::load();
    let mut dirs: Vec<String> = registry
        .all()
        .flat_map(|def| crate::session::expand_session_dirs(&codex_service, &def.provider.hooks.session_dirs))
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
        Some(val) if val.strip_prefix("Bearer ").unwrap_or("") == expected => {
            next.run(req).await
        }
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
    let home = dirs::home_dir().unwrap_or_default();
    let events_path = home.join(".codex-service").join(".monitor").join("events.jsonl");
    if !events_path.is_file() {
        return Ok(Json(Vec::new()));
    }

    let content = match std::fs::read_to_string(&events_path) {
        Ok(c) => c,
        Err(e) => return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: format!("reading events: {e}") }),
        )),
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
    reg.get(&gid).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// POST /agents/{id}/kill — stop the agent's container, mark Killed.
async fn kill_agent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentRecord>, (StatusCode, Json<ErrorResponse>)> {
    let gid;
    let container_ref;
    {
        let reg = state.registry.lock().await;
        gid = resolve_agent_id(&reg, &state.host_id, &id);
        let rec = reg.get(&gid).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse { error: format!("no agent '{id}'") }),
        ))?;
        container_ref = rec.container_id.clone().or_else(|| rec.container_name.clone());
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
        Json(ErrorResponse { error: format!("no agent '{id}'") }),
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
        provider: req.provider.or_else(|| existing.as_ref().and_then(|e| e.provider.clone())),
        workspace: req.workspace.or_else(|| existing.as_ref().and_then(|e| e.workspace.clone())),
        container_id: req.container_id.or_else(|| existing.as_ref().and_then(|e| e.container_id.clone())),
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
            Json(ErrorResponse { error: "prompt is required".into() }),
        ));
    }
    let exe = std::env::current_exe().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
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
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("spawn failed: {e}") }))
    })?;

    Ok(Json(SpawnAck {
        status: "spawning".into(),
        message: "agent launching; it will appear in /agents within one reconcile tick (~10s)".into(),
    }))
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

/// Start the HTTP gateway with integrated scheduler
pub async fn serve(gw_config: GatewayConfig) -> Result<()> {
    let docker = DockerOps::new(Some(&gw_config.image))?;
    let trigger_path = std::path::PathBuf::from(&gw_config.trigger_store_path);

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
    let host_id = crate::docker::host_id();

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
        host_id,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/sessions", get(list_sessions_handler))
        .route("/sessions/{id}", get(get_session).post(session_prompt))
        .route("/completion", post(completion))
        .route("/triggers", get(list_triggers).post(create_trigger))
        .route("/triggers/{id}", get(get_trigger).put(update_trigger).delete(delete_trigger))
        .route("/monitor/events", get(monitor_events))
        .route("/agents", get(list_agents))
        .route("/agents/spawn", post(spawn_agent))
        .route("/agents/{id}", get(get_agent))
        .route("/agents/{id}/kill", post(kill_agent))
        .route("/agents/{id}/register", post(register_agent))
        .route("/agents/{id}/deregister", post(deregister_agent))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state.clone());

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
        .route("/triggers/{id}", get(get_trigger).put(update_trigger).delete(delete_trigger))
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
}
