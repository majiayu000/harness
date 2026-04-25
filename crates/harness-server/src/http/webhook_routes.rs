use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::json;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use super::{state::AppState, task_routes};

fn configured_github_webhook_project_root(
    github: Option<&harness_core::config::intake::GitHubIntakeConfig>,
    default_root: &StdPath,
    repo: &str,
) -> Option<PathBuf> {
    github?
        .effective_repos()
        .into_iter()
        .find(|repo_cfg| repo_cfg.repo == repo)
        .map(|repo_cfg| {
            repo_cfg
                .project_root
                .map(PathBuf::from)
                .unwrap_or_else(|| default_root.to_path_buf())
        })
}

enum GitHubWebhookProjectRootError {
    RepoNotConfigured(String),
    RegistryLookup(String),
}

fn github_webhook_project_root_error_response(
    error: GitHubWebhookProjectRootError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        // Treat unknown repositories as ignored so GitHub does not retry
        // an event for a repo this harness instance is not configured to
        // serve. Registry failures remain internal errors.
        GitHubWebhookProjectRootError::RepoNotConfigured(reason) => (
            StatusCode::OK,
            Json(json!({ "status": "ignored", "reason": reason })),
        ),
        GitHubWebhookProjectRootError::RegistryLookup(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn resolve_github_webhook_project_root(
    state: &Arc<AppState>,
    repo: &str,
) -> Result<PathBuf, GitHubWebhookProjectRootError> {
    if let Some(project_root) = configured_github_webhook_project_root(
        state.core.server.config.intake.github.as_ref(),
        &state.core.project_root,
        repo,
    ) {
        return Ok(project_root);
    }

    if let Some(registry) = state.core.project_registry.as_deref() {
        if let Some(project) = registry.get(repo).await.map_err(|error| {
            GitHubWebhookProjectRootError::RegistryLookup(format!(
                "project registry lookup failed: {error}"
            ))
        })? {
            return Ok(project.root);
        }
        if let Some(project) = registry.get_by_name(repo).await.map_err(|error| {
            GitHubWebhookProjectRootError::RegistryLookup(format!(
                "project registry lookup failed: {error}"
            ))
        })? {
            return Ok(project.root);
        }
    }

    Err(GitHubWebhookProjectRootError::RepoNotConfigured(format!(
        "webhook repository '{repo}' is not configured in intake.github and was not found in the project registry"
    )))
}

pub(crate) async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let secret = match state
        .core
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
    {
        Some("") => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "invalid server.github_webhook_secret configuration"})),
            )
        }
        Some(secret) => secret,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "missing server.github_webhook_secret configuration"})),
            )
        }
    };
    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
    {
        Some(signature) => signature,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing header x-hub-signature-256"})),
            )
        }
    };
    if !crate::webhook::verify_github_signature(secret, signature, body.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid webhook signature"})),
        );
    }

    let event = match headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
    {
        Some(event) => event,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing header x-github-event"})),
            )
        }
    };
    if !crate::webhook::is_valid_github_event_name(event) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid header x-github-event"})),
        );
    }

    let (request, reason) =
        match crate::webhook::parse_github_webhook_task_request(event, body.as_ref()) {
            Ok(parsed) => parsed,
            Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
        };

    let Some(mut req) = request else {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ignored",
                "reason": reason,
            })),
        );
    };

    if req.project.is_none() {
        req.project = Some(match req.repo.as_deref() {
            Some(repo) => match resolve_github_webhook_project_root(&state, repo).await {
                Ok(project_root) => project_root,
                Err(error) => return github_webhook_project_root_error_response(error),
            },
            None => state.core.project_root.clone(),
        });
    }

    match task_routes::enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "reason": reason,
                "task_id": task_id.0,
            })),
        ),
        Err(crate::services::execution::EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(crate::services::execution::EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
        Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow {
            retry_after_secs,
        }) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "maintenance_window", "retry_after": retry_after_secs })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configured_github_webhook_project_root, github_webhook_project_root_error_response,
        GitHubWebhookProjectRootError,
    };
    use axum::http::StatusCode;
    use harness_core::config::intake::{GitHubIntakeConfig, GitHubRepoConfig};
    use std::path::PathBuf;

    #[test]
    fn multi_repo_github_webhook_uses_repo_specific_project_root_override() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![
                GitHubRepoConfig {
                    repo: "org/repo-a".to_string(),
                    label: "harness".to_string(),
                    project_root: None,
                },
                GitHubRepoConfig {
                    repo: "org/repo-b".to_string(),
                    label: "harness".to_string(),
                    project_root: Some("/srv/repo-b".to_string()),
                },
            ],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

        assert_eq!(resolved, Some(PathBuf::from("/srv/repo-b")));
    }

    #[test]
    fn configured_github_repo_without_override_falls_back_to_default_project_root() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![GitHubRepoConfig {
                repo: "org/repo-a".to_string(),
                label: "harness".to_string(),
                project_root: None,
            }],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-a");

        assert_eq!(resolved, Some(default_root));
    }

    #[test]
    fn unconfigured_github_repo_has_no_configured_project_root() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![GitHubRepoConfig {
                repo: "org/repo-a".to_string(),
                label: "harness".to_string(),
                project_root: None,
            }],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

        assert_eq!(resolved, None);
    }

    #[test]
    fn unconfigured_github_repo_returns_ignored_response() {
        let (status, body) = github_webhook_project_root_error_response(
            GitHubWebhookProjectRootError::RepoNotConfigured(
                "webhook repository 'org/repo-b' is not configured".to_string(),
            ),
        );

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["status"], "ignored");
        assert!(body.0["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not configured"));
    }

    #[test]
    fn registry_lookup_failures_return_internal_server_error() {
        let (status, body) = github_webhook_project_root_error_response(
            GitHubWebhookProjectRootError::RegistryLookup(
                "project registry lookup failed: boom".to_string(),
            ),
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0["error"], "project registry lookup failed: boom");
    }
}
