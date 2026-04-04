use crate::http::AppState;
use crate::runtime_hosts::{ClaimCandidate, ClaimTaskError};
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
            // Heartbeat is intentionally not persisted (transient data, self-healing
            // after restart).  However if a prior mutation left the dirty flag, piggyback
            // on this frequent call to converge durable state.
            if state.is_runtime_state_dirty() {
                if let Err(e) = state.persist_runtime_state().await {
                    tracing::warn!(
                        host_id = %host_id,
                        error = %e,
                        "opportunistic dirty-state flush on heartbeat failed; will retry next heartbeat"
                    );
                }
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
        // Host already gone from memory (idempotent retry).  If a prior
        // deregister mutated memory but failed to persist, converge now.
        if state.is_runtime_state_dirty() {
            if let Err(response) = persist_runtime_state(&state).await {
                return response;
            }
        }
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
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
    }

    let project_filter = req
        .project
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut tasks: Vec<ClaimCandidate> = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .filter(|task| task.status.as_ref() == "pending")
        .map(|task| ClaimCandidate {
            task_id: task.id,
            status: task.status,
            created_at: task.created_at,
            project: task.project_root.map(|p| p.to_string_lossy().into_owned()),
        })
        .filter(|candidate| match project_filter {
            Some(filter) => candidate.project.as_deref() == Some(filter),
            None => true,
        })
        .collect();
    tasks.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.task_id.as_str().cmp(b.task_id.as_str()))
    });

    let lease_secs = match req.lease_secs {
        Some(value) => match i64::try_from(value) {
            Ok(v) => Some(v),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "lease_secs must be <= i64::MAX" })),
                )
            }
        },
        None => None,
    };

    for candidate in tasks {
        let live_claim = state
            .core
            .tasks
            .with_task_if_pending(&candidate.task_id, || {
                state
                    .runtime_hosts
                    .claim_task_id(&host_id, &candidate.task_id, lease_secs)
            });
        let Some(claim_result) = live_claim else {
            continue;
        };
        match claim_result {
            Ok(Some(claim)) => {
                if let Err((_, Json(error_body))) = persist_runtime_state(&state).await {
                    tracing::error!(
                        host_id = %host_id,
                        task_id = %claim.task_id,
                        error = %error_body["error"].as_str().unwrap_or("unknown persistence error"),
                        "runtime claim persisted in memory but runtime state persistence failed"
                    );
                }
                return (
                    StatusCode::OK,
                    Json(json!({
                        "claimed": true,
                        "task_id": claim.task_id,
                        "lease_expires_at": claim.lease_expires_at
                    })),
                );
            }
            Ok(None) => continue,
            Err(e) => return claim_task_error_response(e),
        }
    }

    if let Err(response) = persist_runtime_state(&state).await {
        return response;
    }
    (StatusCode::OK, Json(json!({ "claimed": false })))
}

fn claim_task_error_response(e: ClaimTaskError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        ClaimTaskError::HostNotRegistered(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        ),
        ClaimTaskError::LeaseTtlOutOfRange(_) => (
            StatusCode::BAD_REQUEST,
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
