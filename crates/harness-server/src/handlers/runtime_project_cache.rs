use crate::http::AppState;
use crate::project_registry::{check_allowed_roots, validate_project_root};
use crate::runtime_project_cache::WatchedProjectInput;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct SyncWatchedProjectsRequest {
    #[serde(default)]
    pub projects: Vec<SyncProjectItem>,
}

#[derive(Debug, Deserialize)]
pub struct SyncProjectItem {
    pub project: String,
}

pub async fn list_runtime_host_projects(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !host_exists(&state, &host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime host not found"})),
        );
    }
    let snapshot = state
        .runtime_project_cache
        .get_host_cache(&host_id)
        .unwrap_or_else(|| state.runtime_project_cache.empty_snapshot(&host_id));
    (StatusCode::OK, Json(json!(snapshot)))
}

pub async fn sync_runtime_host_projects(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
    Json(req): Json<SyncWatchedProjectsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !host_exists(&state, &host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime host not found"})),
        );
    }

    let mut inputs: Vec<WatchedProjectInput> = Vec::with_capacity(req.projects.len());
    for item in req.projects {
        let token = item.project.trim();
        if token.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "project token must not be empty"})),
            );
        }
        let (project_id, root) = match resolve_project_token(&state, token).await {
            Ok(v) => v,
            Err(msg) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))),
        };
        if let Err(msg) = validate_allowed_root(&state, &root) {
            return (StatusCode::FORBIDDEN, Json(json!({"error": msg})));
        }
        inputs.push(WatchedProjectInput {
            project_id,
            root: root.to_string_lossy().into_owned(),
        });
    }

    // Keep a read guard while writing cache to prevent concurrent deregister()
    // from deleting the host between validation and cache sync.
    let Some(_host_guard) = state.runtime_hosts.hosts.get(&host_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime host not found"})),
        );
    };
    let snapshot = state
        .runtime_project_cache
        .sync_host_projects(&host_id, inputs);
    if let Err(response) = persist_runtime_state(&state).await {
        return response;
    }
    (StatusCode::OK, Json(json!(snapshot)))
}

fn host_exists(state: &AppState, host_id: &str) -> bool {
    state.runtime_hosts.hosts.contains_key(host_id)
}

async fn resolve_project_token(
    state: &AppState,
    token: &str,
) -> Result<(Option<String>, PathBuf), String> {
    let as_path = PathBuf::from(token);
    if as_path.is_dir() {
        return as_path
            .canonicalize()
            .map(|root| (None, root))
            .map_err(|e| format!("invalid project path '{token}': {e}"));
    }

    // Try primary ID first (canonical path), then name as fallback so
    // `project: "litellm"` still resolves when the registry key is the canonical path.
    match state.project_svc.resolve_path(token).await {
        Ok(Some(root)) => {
            return root
                .canonicalize()
                .map(|canon| (Some(token.to_string()), canon))
                .map_err(|e| format!("project '{token}' root is not accessible: {e}"));
        }
        Ok(None) => {}
        Err(e) => return Err(format!("failed to resolve project '{token}': {e}")),
    }
    match state.project_svc.get_by_name(token).await {
        Ok(Some(p)) => p
            .root
            .canonicalize()
            .map(|canon| (Some(token.to_string()), canon))
            .map_err(|e| format!("project '{token}' root is not accessible: {e}")),
        Ok(None) => Err(format!(
            "project '{token}' not found in registry and is not a valid directory"
        )),
        Err(e) => Err(format!("failed to resolve project '{token}': {e}")),
    }
}

fn validate_allowed_root(state: &AppState, root: &std::path::Path) -> Result<(), String> {
    validate_project_root(root)?;
    check_allowed_roots(root, &state.core.server.config.server.allowed_project_roots)
}

async fn persist_runtime_state(
    state: &Arc<AppState>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Err(e) = state.persist_runtime_state().await {
        tracing::error!("failed to persist runtime state after project cache sync: {e}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to persist runtime state: {e}") })),
        ));
    }
    Ok(())
}
