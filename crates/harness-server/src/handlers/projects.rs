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

    let allowed = &state.core.server.config.server.allowed_project_roots;
    if !allowed.is_empty() {
        let permitted = allowed.iter().any(|base| match base.canonicalize() {
            Ok(canonical_base) => root.starts_with(&canonical_base),
            Err(e) => {
                tracing::error!("failed to canonicalize allowed root {:?}: {}", base, e);
                false
            }
        });
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "project root is not under an allowed base directory"})),
            );
        }
    }

    let project = Project {
        id: req.id,
        root,
        max_concurrent: req.max_concurrent,
        default_agent: req.default_agent,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    match state.project_svc.register(project.clone()).await {
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
    match state.project_svc.list().await {
        Ok(projects) => {
            let mut with_counts: Vec<serde_json::Value> = Vec::with_capacity(projects.len());
            for p in projects {
                let mut v = match serde_json::to_value(&p) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("projects: failed to serialize project: {e}");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "failed to serialize project listing"})),
                        );
                    }
                };
                // task_count is best-effort; tasks do not store project_id
                v["task_count"] = json!(0);
                with_counts.push(v);
            }
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
    match state.project_svc.get(&id).await {
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
    match state.project_svc.remove(&id).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;

    async fn make_test_state(
        project_root: &std::path::Path,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<Arc<AppState>> {
        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.to_path_buf();
        config.server.data_dir = data_dir.to_path_buf();
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        Ok(Arc::new(build_app_state(server).await?))
    }

    #[tokio::test]
    async fn list_projects_includes_task_count_and_project_identity() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let project_root = crate::test_helpers::tempdir_in_home("projects-handler-root-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;

        let (status, Json(body)) = list_projects(State(state)).await;
        assert_eq!(status, StatusCode::OK);

        let projects = body
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("expected project list array"))?;
        assert!(
            !projects.is_empty(),
            "startup should auto-register at least the default project"
        );
        for project in projects {
            assert!(
                project.get("id").is_some(),
                "project entry must include id: {project}"
            );
            assert!(
                project.get("root").is_some(),
                "project entry must include root: {project}"
            );
            assert_eq!(project.get("task_count"), Some(&json!(0)));
        }
        Ok(())
    }
}
