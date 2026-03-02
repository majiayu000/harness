use crate::{router, server::HarnessServer, task_runner};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
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
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one of prompt, issue, or pr must be provided"})),
        );
    }

    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "no agent registered"})),
            );
        }
    };

    let task_id = task_runner::spawn_task(state.tasks.clone(), agent, req);

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
) -> Json<Vec<task_runner::TaskState>> {
    let tasks = state.tasks.iter().map(|entry| entry.value().clone()).collect();
    Json(tasks)
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.tasks.get(&id) {
        Some(task) => Json(task.value().clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
    }
}
