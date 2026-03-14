use crate::http::AppState;
use crate::project_registry::Project;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RegisterProjectRequest {
    pub id: String,
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    #[serde(default)]
    pub default_agent: Option<String>,
}

pub async fn register_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterProjectRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(registry) = state.core.project_registry.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "project registry not initialized"})),
        );
    };

    // Use the same strict validator as task execution: checks existence, is_dir, and within $HOME.
    let root = match crate::handlers::validate_project_root(&req.root) {
        Ok(p) => p,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))),
    };

    // Also require a .git entry (directory or file for worktrees).
    if !root.join(".git").exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("root is not a git repository (no .git found): {}", root.display())}),
            ),
        );
    }

    let project = Project {
        id: req.id,
        root,
        max_concurrent: req.max_concurrent,
        default_agent: req.default_agent,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    match registry.register(project.clone()).await {
        Ok(()) => (StatusCode::CREATED, Json(json!(project))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

pub async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(registry) = state.core.project_registry.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "project registry not initialized"})),
        );
    };

    match registry.list().await {
        Ok(projects) => {
            let with_counts: Vec<serde_json::Value> = projects
                .into_iter()
                .map(|p| {
                    let mut v = serde_json::to_value(&p).unwrap_or_default();
                    // task_count is best-effort; tasks do not store project_id
                    v["task_count"] = json!(0);
                    v
                })
                .collect();
            (StatusCode::OK, Json(json!(with_counts)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

pub async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(registry) = state.core.project_registry.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "project registry not initialized"})),
        );
    };

    match registry.get(&id).await {
        Ok(Some(project)) => (StatusCode::OK, Json(json!(project))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("project '{id}' not found")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(registry) = state.core.project_registry.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "project registry not initialized"})),
        );
    };

    match registry.remove(&id).await {
        Ok(true) => (StatusCode::OK, Json(json!({"deleted": id}))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("project '{id}' not found")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}
