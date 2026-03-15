use crate::http::AppState;
use crate::project_registry::{validate_project_root, Project};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing;

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

    let root = match req.root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid root path: {e}")})),
            )
        }
    };

    if let Err(msg) = validate_project_root(&root) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg})));
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
                    let mut v = match serde_json::to_value(&p) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("projects: failed to serialize project: {e}");
                            serde_json::Value::default()
                        }
                    };
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
