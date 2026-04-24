use crate::http::AppState;
use crate::project_registry::{check_allowed_roots, validate_project_root, Project};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tracing::{self, warn};

#[derive(Debug, Deserialize)]
pub struct RegisterProjectRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    #[serde(default)]
    pub default_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValidateProjectRequest {
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectValidationResponse {
    pub canonical_root: std::path::PathBuf,
    pub project_id: String,
    pub display_name: String,
    pub repo: Option<String>,
    pub errors: Vec<String>,
}

fn validation_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let message = message.into();
    (
        status,
        Json(json!({ "error": message, "errors": [message] })),
    )
}

fn normalize_requested_project_id(id: Option<String>) -> Option<String> {
    id.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn infer_display_name(root: &std::path::Path, repo: Option<&str>) -> String {
    if let Some(slug) = repo {
        if let Some(name) = slug.rsplit('/').next() {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

async fn validate_project_request(
    state: &AppState,
    root: std::path::PathBuf,
    requested_id: Option<String>,
) -> Result<ProjectValidationResponse, (StatusCode, Json<serde_json::Value>)> {
    let canonical_root = root.canonicalize().map_err(|e| {
        validation_error(StatusCode::BAD_REQUEST, format!("invalid root path: {e}"))
    })?;

    validate_project_root(&canonical_root)
        .map_err(|message| validation_error(StatusCode::BAD_REQUEST, message))?;

    check_allowed_roots(
        &canonical_root,
        &state.core.server.config.server.allowed_project_roots,
    )
    .map_err(|message| validation_error(StatusCode::FORBIDDEN, message))?;

    let repo = crate::task_executor::pr_detection::detect_repo_slug(&canonical_root).await;
    let project_id = normalize_requested_project_id(requested_id)
        .unwrap_or_else(|| canonical_root.to_string_lossy().into_owned());
    let display_name = infer_display_name(&canonical_root, repo.as_deref());

    Ok(ProjectValidationResponse {
        canonical_root,
        project_id,
        display_name,
        repo,
        errors: Vec::new(),
    })
}

pub async fn validate_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ValidateProjectRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match validate_project_request(&state, req.root, req.id).await {
        Ok(validation) => (StatusCode::OK, Json(json!(validation))),
        Err(error) => error,
    }
}

pub async fn register_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterProjectRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let validation = match validate_project_request(&state, req.root, req.id).await {
        Ok(validation) => validation,
        Err(error) => return error,
    };

    let already_registered = match state.project_svc.get(&validation.project_id).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    };

    let project = Project {
        id: validation.project_id.clone(),
        root: validation.canonical_root.clone(),
        name: Some(validation.display_name.clone()),
        max_concurrent: req.max_concurrent,
        default_agent: req.default_agent,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    match state.project_svc.register(project.clone()).await {
        Ok(()) => {
            let mut event = harness_core::types::Event::new(
                harness_core::types::SessionId::new(),
                "operator_funnel",
                "projects",
                harness_core::types::Decision::Complete,
            );
            event.detail = Some("project_registered".to_string());
            event.content = Some(
                json!({
                    "milestone": "project_registered",
                    "project_id": validation.project_id,
                    "root": validation.canonical_root,
                    "status": if already_registered { "existing" } else { "created" },
                })
                .to_string(),
            );
            if let Err(e) = state.observability.events.log(&event).await {
                warn!("failed to log operator_funnel project_registered event: {e}");
            }

            (
                if already_registered {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                },
                Json(json!({
                    "status": if already_registered { "existing" } else { "created" },
                    "project": project,
                    "validation": validation,
                })),
            )
        }
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

    fn init_git_repo(path: &std::path::Path, remote: Option<&str>) -> anyhow::Result<()> {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()?;
        anyhow::ensure!(status.success(), "git init must succeed");
        if let Some(remote) = remote {
            let status = std::process::Command::new("git")
                .args(["remote", "add", "origin", remote])
                .current_dir(path)
                .status()?;
            anyhow::ensure!(status.success(), "git remote add must succeed");
        }
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
    async fn validate_project_returns_detected_metadata() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let repo = crate::test_helpers::tempdir_in_home("projects-validate-")?;
        init_git_repo(repo.path(), Some("https://github.com/acme/harness.git"))?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(repo.path(), data_dir.path()).await?;

        let (status, Json(body)) = validate_project(
            State(state),
            Json(ValidateProjectRequest {
                root: repo.path().to_path_buf(),
                id: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["project_id"],
            json!(repo.path().canonicalize()?.to_string_lossy().to_string())
        );
        assert_eq!(body["display_name"], "harness");
        assert_eq!(body["repo"], "acme/harness");
        assert_eq!(body["errors"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn validate_project_rejects_non_git_root() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let repo = crate::test_helpers::tempdir_in_home("projects-not-git-")?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(repo.path(), data_dir.path()).await?;

        let (status, Json(body)) = validate_project(
            State(state),
            Json(ValidateProjectRequest {
                root: repo.path().to_path_buf(),
                id: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a git repository"));
        Ok(())
    }

    #[tokio::test]
    async fn validate_project_rejects_disallowed_root() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let allowed = crate::test_helpers::tempdir_in_home("projects-allowed-")?;
        let repo = crate::test_helpers::tempdir_in_home("projects-outside-")?;
        init_git_repo(repo.path(), None)?;
        let data_dir = tempfile::tempdir()?;

        let mut config = HarnessConfig::default();
        config.server.project_root = repo.path().to_path_buf();
        config.server.data_dir = data_dir.path().to_path_buf();
        config.server.allowed_project_roots = vec![allowed.path().to_path_buf()];
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = Arc::new(build_app_state(server).await?);

        let (status, Json(body)) = validate_project(
            State(state),
            Json(ValidateProjectRequest {
                root: repo.path().to_path_buf(),
                id: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "project root is not under an allowed base directory"
        );
        Ok(())
    }

    #[tokio::test]
    async fn register_project_returns_existing_status_for_duplicate_root() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let startup_root = crate::test_helpers::tempdir_in_home("projects-startup-root-")?;
        let repo = crate::test_helpers::tempdir_in_home("projects-register-")?;
        init_git_repo(repo.path(), None)?;
        let data_dir = tempfile::tempdir()?;
        let state = make_test_state(startup_root.path(), data_dir.path()).await?;
        let root = repo.path().to_path_buf();

        let (first_status, Json(first_body)) = register_project(
            State(state.clone()),
            Json(RegisterProjectRequest {
                id: None,
                root: root.clone(),
                max_concurrent: None,
                default_agent: None,
            }),
        )
        .await;
        assert_eq!(first_status, StatusCode::CREATED);
        assert_eq!(first_body["status"], "created");

        let (second_status, Json(second_body)) = register_project(
            State(state.clone()),
            Json(RegisterProjectRequest {
                id: None,
                root,
                max_concurrent: Some(3),
                default_agent: Some("auto".to_string()),
            }),
        )
        .await;

        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(second_body["status"], "existing");
        assert_eq!(
            second_body["project"]["id"], first_body["project"]["id"],
            "duplicate registration must resolve to the same canonical project id"
        );
        Ok(())
    }
}
