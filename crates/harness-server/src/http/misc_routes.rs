use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use harness_protocol::methods::RpcRequest;
use serde_json::json;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use super::{state::AppState, task_routes};
use crate::{router, task_runner};

fn startup_error_code(error: Option<&str>) -> Option<&'static str> {
    let error = error?;
    let lower = error.to_ascii_lowercase();
    if lower.contains("migration") {
        Some("migration_failed")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        Some("timeout")
    } else if lower.contains("connection")
        || lower.contains("connect")
        || lower.contains("database")
        || lower.contains("postgres")
        || lower.contains("pool")
    {
        Some("database_unavailable")
    } else {
        Some("startup_failed")
    }
}

pub(crate) async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.core.tasks.count();
    let dirty = state.is_runtime_state_dirty();
    let degraded = &state.degraded_subsystems;
    let runtime_logs = &state.core.server.runtime_logs;
    let startup_statuses: Vec<serde_json::Value> = state
        .startup_statuses
        .iter()
        .map(|status| {
            json!({
                "name": status.name,
                "critical": status.is_critical(),
                "ready": status.ready,
                "error": startup_error_code(status.error.as_deref()),
            })
        })
        .collect();
    let status = if degraded.is_empty() && !dirty {
        "ok"
    } else {
        "degraded"
    };
    Json(json!({
        "status": status,
        "tasks": count,
        "persistence": {
            "degraded_subsystems": degraded,
            "runtime_state_dirty": dirty,
            "startup": {
                "stores": startup_statuses,
            }
        },
        "runtime_logs": {
            "state": runtime_logs.state.as_str(),
            "path_hint": runtime_logs.path_hint.clone(),
            "retention_days": runtime_logs.retention_days,
        }
    }))
}

/// GET /projects/queue-stats — per-project queue stats alongside the global queue summary.
pub(crate) async fn project_queue_stats(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tq = &state.concurrency.task_queue;
    let projects: serde_json::Map<String, serde_json::Value> = tq
        .all_project_stats()
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                json!({
                    "running": s.running,
                    "queued": s.queued,
                    "limit": s.limit,
                }),
            )
        })
        .collect();
    Json(json!({
        "global": {
            "running": tq.running_count(),
            "queued": tq.queued_count(),
            "limit": tq.global_limit(),
        },
        "projects": projects,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByIssueQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub issue: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByPrQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub pr: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ProjectWorkflowByProjectQuery {
    pub project_id: String,
    pub repo: Option<String>,
}

pub(crate) async fn get_issue_workflow_by_issue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByIssueQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "issue workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_issue(&query.project_id, query.repo.as_deref(), query.issue)
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "issue workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_issue_workflow_by_pr(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByPrQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "issue workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_pr(&query.project_id, query.repo.as_deref(), query.pr)
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "issue workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_project_workflow_by_project(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectWorkflowByProjectQuery>,
) -> Response {
    let Some(store) = state.core.project_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "project workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_project(&query.project_id, query.repo.as_deref())
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "project workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RpcRequest>,
) -> Response {
    match router::handle_request(&state, req).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

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

/// GET /api/intake — current status of all intake channels and recent dispatches.
pub(crate) async fn intake_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let intake_config = &state.core.server.config.intake;
    let all_tasks = state.core.tasks.list_all();

    let github_active: u64 = if let Some(store) = state.core.issue_workflow_store.as_ref() {
        match store.list().await {
            Ok(workflows) => workflows
                .into_iter()
                .filter(|workflow| {
                    !matches!(
                        workflow.state,
                        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Failed
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
                    )
                })
                .count() as u64,
            Err(_) => all_tasks
                .iter()
                .filter(|t| {
                    t.source.as_deref() == Some("github")
                        && !matches!(
                            t.status,
                            task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                        )
                })
                .count() as u64,
        }
    } else {
        all_tasks
            .iter()
            .filter(|t| {
                t.source.as_deref() == Some("github")
                    && !matches!(
                        t.status,
                        task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                    )
            })
            .count() as u64
    };

    let feishu_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            t.source.as_deref() == Some("feishu")
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let dashboard_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            (t.source.as_deref() == Some("dashboard") || t.source.is_none())
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let github_channel = json!({
        "name": "github",
        "enabled": intake_config.github.as_ref().map(|c| c.enabled).unwrap_or(false),
        "repo": intake_config.github.as_ref().map(|c| c.repo.as_str()).unwrap_or(""),
        "active": github_active,
    });

    let feishu_channel = json!({
        "name": "feishu",
        "enabled": state.intake.feishu_intake.is_some(),
        "keyword": intake_config.feishu.as_ref().map(|c| c.trigger_keyword.as_str()).unwrap_or(""),
        "active": feishu_active,
    });

    let dashboard_channel = json!({
        "name": "dashboard",
        "enabled": true,
        "active": dashboard_active,
    });

    let mut recent_dispatches: Vec<serde_json::Value> = all_tasks
        .iter()
        .filter(|t| t.source.is_some())
        .map(|t| {
            json!({
                "source": t.source,
                "external_id": t.external_id,
                "task_id": t.id.0,
                "status": serde_json::to_value(&t.status).unwrap_or(json!("unknown")),
                "pr_url": t.pr_url,
            })
        })
        .collect();
    recent_dispatches.truncate(10);

    Json(json!({
        "channels": [github_channel, feishu_channel, dashboard_channel],
        "recent_dispatches": recent_dispatches,
    }))
}

#[derive(serde::Deserialize)]
pub(crate) struct IngestSignalRequest {
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) severity: Option<harness_core::types::Severity>,
    pub(crate) payload: serde_json::Value,
}

/// Infer severity from a GitHub webhook payload: CI failure → High, changes_requested → Medium.
pub(crate) fn infer_github_severity(
    payload: &serde_json::Value,
) -> Option<harness_core::types::Severity> {
    if let Some(obj) = payload.as_object() {
        if let (Some(action), Some(check_run)) = (
            obj.get("action").and_then(|v| v.as_str()),
            obj.get("check_run"),
        ) {
            if action == "completed" {
                let conclusion = check_run
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if conclusion == "failure" {
                    return Some(harness_core::types::Severity::High);
                }
            }
        }
        if let Some(review) = obj.get("review") {
            let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.eq_ignore_ascii_case("changes_requested") {
                return Some(harness_core::types::Severity::Medium);
            }
        }
    }
    None
}

/// POST /signals — ingest an external signal (CI failure, review feedback, etc.).
///
/// Validates the `x-hub-signature-256` HMAC-SHA256 header using the configured
/// `server.github_webhook_secret`. Rate-limited to 100 requests per source per minute.
pub(crate) async fn ingest_signal(
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
        Some("") | None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server.github_webhook_secret not configured"})),
            )
        }
        Some(s) => s,
    };

    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(sig) => sig,
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

    let req: IngestSignalRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid payload: {e}")})),
            )
        }
    };

    if !state
        .observability
        .signal_rate_limiter
        .check_and_increment(&req.source)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "rate limit exceeded: max 100 signals per minute per source"})),
        );
    }

    let severity = req.severity.unwrap_or_else(|| {
        if req.source == "github" {
            infer_github_severity(&req.payload).unwrap_or(harness_core::types::Severity::Low)
        } else {
            harness_core::types::Severity::Low
        }
    });

    let signal =
        harness_core::types::ExternalSignal::new(req.source.clone(), severity, req.payload.clone());
    let signal_id = signal.id.clone();

    if let Err(e) = state.observability.events.log_external_signal(&signal) {
        tracing::error!(source = %req.source, "failed to store external signal: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to store signal"})),
        );
    }

    tracing::info!(
        source = %req.source,
        severity = ?severity,
        signal_id = %signal_id,
        "external signal ingested"
    );

    (
        StatusCode::OK,
        Json(json!({"status": "accepted", "id": signal_id.as_str()})),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct PasswordResetRequest {
    pub(crate) email: String,
}

pub(crate) fn prepare_password_reset_request(
    rate_limiter: &crate::http::rate_limit::PasswordResetRateLimiter,
    limit: u32,
    email: &str,
) -> Result<String, (StatusCode, serde_json::Value)> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "email is required"}),
        ));
    }

    if !rate_limiter.check_and_increment(&email) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": format!(
                    "rate limit exceeded: max {} password reset requests per hour",
                    limit
                )
            }),
        ));
    }

    Ok(email)
}

pub(crate) fn disabled_password_reset_response() -> (StatusCode, serde_json::Value) {
    (
        StatusCode::NOT_IMPLEMENTED,
        json!({"error": "password reset is not yet implemented"}),
    )
}

/// POST /auth/reset-password — temporarily disabled until email delivery exists.
///
/// Requests are still validated, rate-limited, and logged so the auth-exempt
/// endpoint retains its abuse protections while the actual reset flow is
/// unavailable.
pub(crate) async fn password_reset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PasswordResetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = state
        .core
        .server
        .config
        .server
        .password_reset_rate_limit_per_hour;
    let email = match prepare_password_reset_request(
        &state.observability.password_reset_rate_limiter,
        limit,
        &req.email,
    ) {
        Ok(email) => email,
        Err((status, body)) => return (status, Json(body)),
    };

    tracing::info!(
        email_hash = %format!("{:x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            email.hash(&mut h);
            h.finish()
        }),
        "password reset requested while endpoint disabled"
    );

    // TODO: wire up SMTP/transactional email before enabling this endpoint.
    let (status, body) = disabled_password_reset_response();
    (status, Json(body))
}
