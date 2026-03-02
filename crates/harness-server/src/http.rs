use crate::{router, server::HarnessServer, task_runner};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use harness_protocol::{RpcRequest, RpcResponse};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct AppState {
    pub server: Arc<HarnessServer>,
    pub tasks: task_runner::TaskStore,
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");

    let state = Arc::new(AppState {
        server,
        tasks: task_runner::new_task_store(),
    });

    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/tasks", post(create_task))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RpcRequest>,
) -> Json<RpcResponse> {
    Json(router::handle_request(&state.server, req).await)
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.server.agent_registry.default_agent().is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "no agent registered"})),
        );
    }

    let agent_arc: Arc<dyn harness_core::CodeAgent> =
        Arc::new(harness_agents::claude::ClaudeCodeAgent::new(
            state.server.config.agents.claude.cli_path.clone(),
            state.server.config.agents.claude.default_model.clone(),
        ));

    let task_id = task_runner::spawn_task(state.tasks.clone(), agent_arc, req);

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "task_id": task_id.0,
            "status": "running"
        })),
    )
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mut tasks = Vec::new();
    for entry in state.tasks.iter() {
        tasks.push(entry.value().clone());
    }

    Json(json!(tasks))
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.tasks.get(&id) {
        Some(task) => {
            let state: &task_runner::TaskState = task.value();
            match serde_json::to_value(state) {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "serialization failed"}))),
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        ),
    }
}
