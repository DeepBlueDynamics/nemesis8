use anyhow::Result;
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

use crate::config::Config;
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
}

impl Default for GatewayConfig {
    fn default() -> Self {
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
        }
    }
}

/// Shared gateway state
struct AppState {
    config: Config,
    concurrency: Semaphore,
    last_spawn: Mutex<std::time::Instant>,
    spawn_gap: std::time::Duration,
    active_count: Mutex<usize>,
    workspace_root: String,
    danger: bool,
    model: Option<String>,
    #[allow(dead_code)]
    image: String,
}

// ── Request / Response types ──

#[derive(Deserialize)]
struct CompletionRequest {
    prompt: String,
    model: Option<String>,
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CompletionResponse {
    session_id: String,
    status: String,
    output: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
}

#[derive(Serialize)]
struct StatusResponse {
    active: usize,
    max_concurrent: usize,
    uptime_secs: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
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
    Json(StatusResponse {
        active,
        max_concurrent: state.concurrency.available_permits() + active,
        uptime_secs: 0, // TODO: track start time
    })
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    let session_dirs_str = state
        .config
        .env
        .vars
        .get("CODEX_GATEWAY_SESSION_DIRS")
        .cloned()
        .unwrap_or_default();

    let session_dirs: Vec<&str> = if session_dirs_str.is_empty() {
        vec![]
    } else {
        session_dirs_str.split(',').collect()
    };

    match session::list_sessions(&session_dirs) {
        Ok(sessions) => Ok(Json(sessions)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SessionInfo>, StatusCode> {
    let session_dirs_str = state
        .config
        .env
        .vars
        .get("CODEX_GATEWAY_SESSION_DIRS")
        .cloned()
        .unwrap_or_default();

    let session_dirs: Vec<&str> = if session_dirs_str.is_empty() {
        vec![]
    } else {
        session_dirs_str.split(',').collect()
    };

    match session::find_session(&id, &session_dirs) {
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

    // Spawn codex in the container
    let model = req.model.or_else(|| state.model.clone());
    let output = run_codex_prompt(
        &req.prompt,
        &session_id,
        model.as_deref(),
        state.danger,
        &state.workspace_root,
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
    // Reuse completion logic with explicit session ID
    let mut req = req;
    req.session_id = Some(id);
    completion(State(state), Json(req)).await
}

/// Spawn codex CLI and capture output
async fn run_codex_prompt(
    prompt: &str,
    session_id: &str,
    model: Option<&str>,
    danger: bool,
    workspace: &str,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new("codex");
    cmd.arg("--quiet");

    if danger {
        cmd.arg("--full-auto");
    }

    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    cmd.arg("--session-id").arg(session_id);
    cmd.arg(prompt);
    cmd.current_dir(workspace);

    let output = cmd.output().await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("codex exited with {}: {stderr}", output.status)
    }
}

/// Start the HTTP gateway
pub async fn serve(gw_config: GatewayConfig) -> Result<()> {
    let state = Arc::new(AppState {
        config: gw_config.config,
        concurrency: Semaphore::new(gw_config.max_concurrent),
        last_spawn: Mutex::new(std::time::Instant::now()),
        spawn_gap: std::time::Duration::from_millis(gw_config.spawn_gap_ms),
        active_count: Mutex::new(0),
        workspace_root: gw_config.workspace_root,
        danger: gw_config.danger,
        model: gw_config.model,
        image: gw_config.image,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session).post(session_prompt))
        .route("/completion", post(completion))
        .with_state(state);

    let addr = format!("{}:{}", gw_config.bind, gw_config.port);
    tracing::info!(addr = %addr, "gateway listening");

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
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session).post(session_prompt))
        .route("/completion", post(completion))
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
        Arc::new(AppState {
            config: Config::default(),
            concurrency: Semaphore::new(2),
            last_spawn: Mutex::new(std::time::Instant::now()),
            spawn_gap: std::time::Duration::from_millis(0),
            active_count: Mutex::new(0),
            workspace_root: "/workspace".to_string(),
            danger: false,
            model: None,
            image: "test:latest".to_string(),
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
    }

    #[tokio::test]
    async fn test_sessions_endpoint_empty() {
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
        assert!(json.as_array().unwrap().is_empty());
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
}
