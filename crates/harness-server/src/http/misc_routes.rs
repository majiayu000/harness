use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use harness_protocol::methods::RpcRequest;
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;
use crate::{router, task_runner};

pub(crate) async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.core.tasks.count();
    let dirty = state.is_runtime_state_dirty();
    let degraded = &state.degraded_subsystems;
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

pub(crate) async fn list_tasks(State(state): State<Arc<AppState>>) -> Response {
    match state.core.tasks.list_all_summaries_with_terminal().await {
        Ok(mut summaries) => {
            if let Some(workflow_store) = state.core.issue_workflow_store.as_ref() {
                // Bulk-load all workflows once to avoid N+1 queries.
                // list() returns rows ORDER BY updated_at DESC, so the first
                // entry for each key is the most recent one.
                let all_workflows = workflow_store.list().await.unwrap_or_default();
                let mut issue_map: std::collections::HashMap<
                    (String, Option<String>, u64),
                    harness_workflow::issue_lifecycle::IssueWorkflowInstance,
                > = std::collections::HashMap::new();
                let mut pr_map: std::collections::HashMap<
                    (String, Option<String>, u64),
                    harness_workflow::issue_lifecycle::IssueWorkflowInstance,
                > = std::collections::HashMap::new();
                for wf in all_workflows {
                    let issue_key = (wf.project_id.clone(), wf.repo.clone(), wf.issue_number);
                    let pr_key = wf
                        .pr_number
                        .map(|pr| (wf.project_id.clone(), wf.repo.clone(), pr));
                    issue_map.entry(issue_key).or_insert_with(|| wf.clone());
                    if let Some(key) = pr_key {
                        pr_map.entry(key).or_insert(wf);
                    }
                }

                for summary in &mut summaries {
                    let Some(project_id) = summary.project.as_deref() else {
                        continue;
                    };
                    let by_issue = summary
                        .external_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("issue:"))
                        .and_then(|n| n.parse::<u64>().ok());
                    let by_pr = summary
                        .external_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("pr:"))
                        .and_then(|n| n.parse::<u64>().ok())
                        .or_else(|| {
                            summary
                                .pr_url
                                .as_deref()
                                .and_then(crate::http::parse_pr_num_from_url)
                        });
                    summary.workflow = match (by_issue, by_pr) {
                        (Some(issue), _) => {
                            let key = (project_id.to_string(), summary.repo.clone(), issue);
                            issue_map.get(&key).cloned()
                        }
                        (None, Some(pr)) => {
                            let key = (project_id.to_string(), summary.repo.clone(), pr);
                            pr_map.get(&key).cloned()
                        }
                        (None, None) => None,
                    };
                }
            }
            Json(summaries).into_response()
        }
        Err(e) => {
            tracing::error!("list_tasks: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state
        .core
        .tasks
        .get_with_db_fallback(&harness_core::types::TaskId(id))
        .await
    {
        Ok(Some(task)) => Json(task).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("get_task: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /tasks/{id}/artifacts — all persisted artifacts for a task.
pub(crate) async fn get_task_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_artifacts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.list_artifacts(&task_id).await {
        Ok(artifacts) => Json(artifacts).into_response(),
        Err(e) => {
            tracing::error!("get_task_artifacts: list artifacts error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /tasks/{id}/prompts — all persisted redacted prompts for a task.
pub(crate) async fn get_task_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_prompts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.get_prompts(&task_id).await {
        Ok(prompts) => Json(prompts).into_response(),
        Err(e) => {
            tracing::error!("get_task_prompts: query error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
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
