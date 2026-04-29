use crate::http::builders::intake::effective_issue_project_limits;
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
    let previous_project = match state.project_svc.get(&req.id).await {
        Ok(project) => project,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
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
        id: req.id.clone(),
        root,
        name: None,
        max_concurrent: req.max_concurrent,
        default_agent: req.default_agent,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    match state.project_svc.register(project.clone()).await {
        Ok(()) => {
            match refresh_primary_queue_limits(&state, previous_project.as_ref(), &project).await {
                Ok(()) => (StatusCode::CREATED, Json(json!(project))),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                ),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

async fn refresh_primary_queue_limits(
    state: &AppState,
    previous_project: Option<&Project>,
    project: &Project,
) -> anyhow::Result<()> {
    let registry_projects = state.project_svc.list().await?;
    let effective_limits = effective_issue_project_limits(&state.core.server, &registry_projects);
    for root in roots_to_refresh(previous_project, project) {
        if let Some(limit) = effective_limits.get(&root) {
            state
                .concurrency
                .task_queue
                .set_project_limit(&root, *limit);
        } else {
            state.concurrency.task_queue.reset_project_limit(&root);
        }
    }
    Ok(())
}

fn roots_to_refresh(previous_project: Option<&Project>, project: &Project) -> Vec<String> {
    let mut roots = Vec::with_capacity(2);
    if let Some(previous_project) = previous_project {
        roots.push(previous_project.root.to_string_lossy().into_owned());
    }
    let current_root = project.root.to_string_lossy().into_owned();
    if !roots.iter().any(|root| root == &current_root) {
        roots.push(current_root);
    }
    roots
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
    // Try canonical path ID first, then human-readable name as fallback.
    let result = match state.project_svc.get(&id).await {
        Ok(Some(p)) => return (StatusCode::OK, Json(json!(p))),
        Ok(None) => state.project_svc.get_by_name(&id).await,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    };
    match result {
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
    // Resolve the canonical registry ID (which may be an absolute path) from
    // either a direct ID match or a name-based lookup, so callers can use the
    // human-readable name just as well as the full path key.
    let canonical_id = match state.project_svc.get(&id).await {
        Ok(Some(_)) => id.clone(),
        Ok(None) => match state.project_svc.get_by_name(&id).await {
            Ok(Some(p)) => p.id,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("project '{id}' not found")})),
                )
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    };
    match state.project_svc.remove(&canonical_id).await {
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

    fn init_git_repo(root: &std::path::Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(root.join(".git"))?;
        Ok(())
    }

    #[tokio::test]
    async fn list_projects_includes_task_count_and_project_identity() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
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

    #[tokio::test]
    async fn register_project_updates_live_issue_queue_limit() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let project_root = crate::test_helpers::tempdir_in_home("projects-handler-live-limit-")?;
        init_git_repo(project_root.path())?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(project_root.path(), data_dir.path()).await?;
        let canonical_root = project_root.path().canonicalize()?;
        let queue_key = canonical_root.to_string_lossy().into_owned();

        let (status, _) = register_project(
            State(state.clone()),
            Json(RegisterProjectRequest {
                id: "live-limit".to_string(),
                root: canonical_root.clone(),
                max_concurrent: Some(4),
                default_agent: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            state
                .concurrency
                .task_queue
                .effective_project_limit(&queue_key),
            4
        );
        Ok(())
    }

    #[tokio::test]
    async fn register_project_reset_restores_fallback_limit() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let project_root = crate::test_helpers::tempdir_in_home("projects-handler-fallback-")?;
        init_git_repo(project_root.path())?;
        let canonical_root = project_root.path().canonicalize()?;
        let queue_key = canonical_root.to_string_lossy().into_owned();
        let data_dir = tempfile::tempdir()?;

        let mut config = HarnessConfig::default();
        config.server.project_root = canonical_root.clone();
        config.server.data_dir = data_dir.path().to_path_buf();
        config.concurrency.per_project.insert(queue_key.clone(), 3);
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = Arc::new(build_app_state(server).await?);

        let (status, _) = register_project(
            State(state.clone()),
            Json(RegisterProjectRequest {
                id: "fallback-limit".to_string(),
                root: canonical_root.clone(),
                max_concurrent: Some(5),
                default_agent: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            state
                .concurrency
                .task_queue
                .effective_project_limit(&queue_key),
            5
        );

        let (status, _) = register_project(
            State(state.clone()),
            Json(RegisterProjectRequest {
                id: "fallback-limit".to_string(),
                root: canonical_root,
                max_concurrent: None,
                default_agent: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            state
                .concurrency
                .task_queue
                .effective_project_limit(&queue_key),
            3
        );
        Ok(())
    }
}
