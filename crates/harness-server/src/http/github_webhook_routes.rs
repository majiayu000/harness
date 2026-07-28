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
    let secret = match super::github_intake_status::github_webhook_secret_for_request(
        &state.core.server.config.server,
    ) {
        Err(super::github_intake_status::GitHubWebhookSecretError::Invalid) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "invalid server.github_webhook_secret configuration"})),
            )
        }
        Err(super::github_intake_status::GitHubWebhookSecretError::Missing) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "missing server.github_webhook_secret configuration"})),
            )
        }
        Ok(secret) => secret,
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

    // Autonomous webhook intake requires github intake to be enabled AND the
    // mode to opt in (webhook/hybrid). Honor the per-repo label filter so the
    // webhook only auto-enqueues issues the poller would have considered.
    let github = state.core.server.config.intake.github.as_ref();
    let autonomous_issues = github
        .map(|github| github.enabled && github.mode.webhook_autonomous())
        .unwrap_or(false);
    let autonomous_label = github.and_then(|github| {
        let repo = serde_json::from_slice::<serde_json::Value>(body.as_ref())
            .ok()
            .and_then(|value| {
                value
                    .get("repository")
                    .and_then(|repo| repo.get("full_name"))
                    .and_then(|name| name.as_str())
                    .map(str::to_string)
            });
        repo.and_then(|repo| github.find_repo_config(&repo))
            .map(|cfg| cfg.label)
            .or_else(|| Some(github.label.clone()))
    });
    let (request, reason) = match crate::webhook::parse_github_webhook_task_request(
        event,
        body.as_ref(),
        autonomous_issues,
        autonomous_label.as_deref(),
    ) {
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
        Ok(task_id) => match task_routes::task_response_details(&state, &task_id).await {
            Ok(details) => {
                let response = json!({
                    "status": details.status,
                    "reason": reason,
                    "task_id": details.submission_id,
                    "submission_id": details.submission_id,
                    "workflow_id": details.workflow_id,
                    "workflow_state": details.workflow_state,
                    "execution_path": "workflow_runtime",
                });
                (StatusCode::ACCEPTED, Json(response))
            }
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
        },
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
#[path = "github_webhook_routes_tests.rs"]
mod tests;
