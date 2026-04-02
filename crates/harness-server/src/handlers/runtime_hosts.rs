use crate::http::AppState;
use crate::runtime_hosts::ClaimCandidate;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RegisterRuntimeHostRequest {
    pub host_id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimTaskRequest {
    pub lease_secs: Option<u64>,
    pub project: Option<String>,
}

pub async fn list_runtime_hosts(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let hosts = state.runtime_hosts.list_hosts();
    (StatusCode::OK, Json(json!({ "hosts": hosts })))
}

pub async fn register_runtime_host(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRuntimeHostRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let host_id = req.host_id.trim();
    if host_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "host_id must not be empty" })),
        );
    }
    let host = state.runtime_hosts.register(
        host_id.to_string(),
        req.display_name.map(|v| v.trim().to_string()),
        req.capabilities,
    );
    if let Err(response) = persist_runtime_state(&state).await {
        return response;
    }
    (StatusCode::OK, Json(json!({ "host": host })))
}

pub async fn heartbeat_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.runtime_hosts.heartbeat(&host_id) {
        Ok(host) => {
            if let Err(response) = persist_runtime_state(&state).await {
                return response;
            }
            (StatusCode::OK, Json(json!({ "host": host })))
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn deregister_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.runtime_hosts.deregister(&host_id) {
        state.runtime_project_cache.clear_host(&host_id);
        if let Err(response) = persist_runtime_state(&state).await {
            return response;
        }
        (StatusCode::OK, Json(json!({ "deregistered": true })))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "runtime host not found" })),
        )
    }
}

pub async fn claim_task_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
    Json(req): Json<ClaimTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tasks = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .map(|task| ClaimCandidate {
            task_id: task.id,
            status: task.status,
            created_at: task.created_at,
            project: task.project_root.map(|p| p.to_string_lossy().into_owned()),
        })
        .collect();

    let lease_secs = req.lease_secs.map(|s| s as i64);
    match state
        .runtime_hosts
        .claim_task(&host_id, tasks, lease_secs, req.project.as_deref())
    {
        Ok(Some(claim)) => {
            if let Err(response) = persist_runtime_state(&state).await {
                return response;
            }
            (
                StatusCode::OK,
                Json(json!({
                    "claimed": true,
                    "task_id": claim.task_id,
                    "lease_expires_at": claim.lease_expires_at
                })),
            )
        }
        Ok(None) => {
            if let Err(response) = persist_runtime_state(&state).await {
                return response;
            }
            (StatusCode::OK, Json(json!({ "claimed": false })))
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

async fn persist_runtime_state(
    state: &Arc<AppState>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Err(e) = state.persist_runtime_state().await {
        tracing::error!("failed to persist runtime state after runtime host mutation: {e}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to persist runtime state: {e}") })),
        ));
    }
    Ok(())
}
