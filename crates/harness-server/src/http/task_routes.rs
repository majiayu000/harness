use super::AppState;
use crate::{
    services::execution::{EnqueueBackgroundOptions, EnqueueTaskError},
    task_runner,
};
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[cfg(test)]
pub(crate) use crate::services::execution::{
    resolve_project_from_registry, workflow_reuse_strategy, WorkflowReuseStrategy,
};
pub(crate) use crate::services::execution::{select_agent, QueueDomain};

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    state.execution_svc.enqueue(req).await
}

pub(crate) async fn enqueue_task_in_domain(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
    queue_domain: QueueDomain,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    state
        .execution_svc
        .enqueue_in_domain(req, queue_domain)
        .await
}

pub(crate) async fn enqueue_task_background(
    state: Arc<AppState>,
    req: task_runner::CreateTaskRequest,
    group_sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    enqueue_task_background_in_domain(state, req, group_sem, QueueDomain::Primary).await
}

pub(crate) async fn enqueue_task_background_in_domain(
    state: Arc<AppState>,
    req: task_runner::CreateTaskRequest,
    group_sem: Option<Arc<tokio::sync::Semaphore>>,
    queue_domain: QueueDomain,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    state
        .execution_svc
        .enqueue_background_with_options(
            req,
            EnqueueBackgroundOptions {
                queue_domain,
                group_sem,
            },
        )
        .await
}

/// A single task entry in the detailed batch format.
#[derive(Debug, Deserialize)]
pub struct BatchTaskItem {
    /// Free-text task description.
    pub description: Option<String>,
    /// GitHub issue number to implement.
    pub issue: Option<u64>,
}

/// Request body for `POST /tasks/batch`.
///
/// Supports two formats:
/// - Shorthand: `{ "issues": [300, 301, 302], "agent": "claude", ... }`
/// - Detailed: `{ "tasks": [{"description": "fix X", "issue": 300}, ...] }`
#[derive(Debug, Deserialize)]
pub struct BatchCreateTaskRequest {
    /// Shorthand list of GitHub issue numbers (one task per issue).
    pub issues: Option<Vec<u64>>,
    /// Detailed list of task specifications.
    pub tasks: Option<Vec<BatchTaskItem>>,
    /// Agent name override applied to all tasks in this batch.
    pub agent: Option<String>,
    /// Maximum rounds override applied to all tasks.
    pub max_rounds: Option<u32>,
    /// Per-turn timeout override in seconds applied to all tasks.
    pub turn_timeout_secs: Option<u64>,
    /// Project root or registry ID applied to all tasks in this batch.
    pub project: Option<std::path::PathBuf>,
}

/// Compute connected conflict groups from per-task file-reference sets.
///
/// Two tasks are in the same group when their file-reference sets overlap
/// (directly or transitively). Tasks whose file-reference set is empty form
/// singleton groups and are never serialised against other tasks.
fn build_conflict_groups(file_refs: &[Vec<String>]) -> Vec<Vec<usize>> {
    let n = file_refs.len();
    let mut visited = vec![false; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut group = vec![start];
        visited[start] = true;
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);

        while let Some(curr) = queue.pop_front() {
            for other in 0..n {
                if visited[other] {
                    continue;
                }
                let has_overlap = !file_refs[curr].is_empty()
                    && !file_refs[other].is_empty()
                    && file_refs[curr].iter().any(|f| file_refs[other].contains(f));
                if has_overlap {
                    visited[other] = true;
                    group.push(other);
                    queue.push_back(other);
                }
            }
        }

        groups.push(group);
    }

    groups
}

pub(super) async fn create_tasks_batch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchCreateTaskRequest>,
) -> Response {
    let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
    let has_tasks = req.tasks.as_ref().is_some_and(|v| !v.is_empty());

    if !has_issues && !has_tasks {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "at least one of issues or tasks must be provided" })),
        )
            .into_response();
    }

    // Build the list of per-task CreateTaskRequests.
    let mut task_requests: Vec<task_runner::CreateTaskRequest> = Vec::new();

    if let Some(issues) = req.issues {
        for issue in issues {
            let mut t = task_runner::CreateTaskRequest::default();
            t.issue = Some(issue);
            t.agent = req.agent.clone();
            t.project = req.project.clone();
            if let Some(rounds) = req.max_rounds {
                t.max_rounds = Some(rounds);
            }
            if let Some(timeout) = req.turn_timeout_secs {
                t.turn_timeout_secs = timeout;
            }
            task_requests.push(t);
        }
    }

    if let Some(tasks) = req.tasks {
        for item in tasks {
            let mut t = task_runner::CreateTaskRequest::default();
            t.prompt = item.description;
            t.issue = item.issue;
            t.agent = req.agent.clone();
            t.project = req.project.clone();
            if let Some(rounds) = req.max_rounds {
                t.max_rounds = Some(rounds);
            }
            if let Some(timeout) = req.turn_timeout_secs {
                t.turn_timeout_secs = timeout;
            }
            task_requests.push(t);
        }
    }

    // Detect file-reference overlaps and build conflict groups.
    // Tasks sharing at least one file reference (directly or transitively) are
    // placed in the same group and assigned a shared Semaphore(1) so they execute
    // sequentially instead of concurrently.
    let task_file_refs: Vec<Vec<String>> = task_requests
        .iter()
        .map(|t| {
            if let Some(p) = t.prompt.as_deref() {
                crate::parallel_dispatch::extract_file_refs(p)
            } else if t.issue.is_some() {
                // Issue-only task: no prompt to extract file refs from.
                // Treat as independent (empty refs → singleton group) so batch
                // submissions run in parallel. Real conflicts are caught by git
                // worktree isolation and GitHub merge conflict detection.
                Vec::new()
            } else {
                Vec::new()
            }
        })
        .collect();

    let conflict_groups = build_conflict_groups(&task_file_refs);

    let n = task_requests.len();
    let mut task_semaphores: Vec<Option<Arc<tokio::sync::Semaphore>>> = vec![None; n];
    let mut task_conflict_files: Vec<Vec<String>> = vec![Vec::new(); n];

    for group in &conflict_groups {
        if group.len() < 2 {
            continue;
        }
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        for &idx in group {
            task_semaphores[idx] = Some(Arc::clone(&sem));
            // Collect files from this task that overlap with any other group member.
            let mut shared: std::collections::HashSet<String> = std::collections::HashSet::new();
            for &other in group {
                if other == idx {
                    continue;
                }
                for f in &task_file_refs[idx] {
                    if task_file_refs[other].contains(f) {
                        shared.insert(f.clone());
                    }
                }
            }
            let mut files: Vec<String> = shared.into_iter().collect();
            files.sort();
            task_conflict_files[idx] = files;
        }
    }

    // Register each task without blocking on concurrency permit acquisition.
    // Each task gets an ID immediately; a background tokio task handles permit
    // waiting and execution. The HTTP handler returns as soon as all tasks are registered.
    let mut results = Vec::with_capacity(n);
    let mut all_maintenance_window = n > 0;
    let mut mw_retry_after: Option<u64> = None;
    for (i, task_req) in task_requests.into_iter().enumerate() {
        let sem = task_semaphores[i].take();
        let is_serialized = sem.is_some();
        let conflict_files = std::mem::take(&mut task_conflict_files[i]);
        let is_issue_submission = task_req.issue.is_some();
        let entry = match enqueue_task_background(state.clone(), task_req, sem).await {
            Ok(task_id) => {
                all_maintenance_window = false;
                let status = if is_issue_submission {
                    "scheduled"
                } else {
                    "queued"
                };
                if is_serialized {
                    json!({
                        "task_id": task_id.0,
                        "status": status,
                        "serialized": true,
                        "conflict_files": conflict_files,
                        "execution_path": if is_issue_submission { "workflow_runtime" } else { "task_runner" },
                    })
                } else {
                    json!({
                        "task_id": task_id.0,
                        "status": status,
                        "execution_path": if is_issue_submission { "workflow_runtime" } else { "task_runner" },
                    })
                }
            }
            Err(EnqueueTaskError::BadRequest(error)) => {
                all_maintenance_window = false;
                json!({ "error": error })
            }
            Err(EnqueueTaskError::Internal(error)) => {
                all_maintenance_window = false;
                json!({ "error": error })
            }
            Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs }) => {
                mw_retry_after.get_or_insert(retry_after_secs);
                json!({ "error": "maintenance_window", "retry_after": retry_after_secs })
            }
        };
        results.push(entry);
    }

    if all_maintenance_window {
        let retry_after = mw_retry_after.unwrap_or(0);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
            Json(json!(results)),
        )
            .into_response();
    }

    (StatusCode::ACCEPTED, Json(json!(results))).into_response()
}

pub(super) async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> Response {
    let is_issue_submission = req.issue.is_some();
    match enqueue_task_background(state, req, None).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "task_id": task_id.0,
                "status": if is_issue_submission { "scheduled" } else { "queued" },
                "execution_path": if is_issue_submission { "workflow_runtime" } else { "task_runner" }
            })),
        )
            .into_response(),
        Err(EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
        }
        Err(EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
        Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs }) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::RETRY_AFTER,
                retry_after_secs.to_string(),
            )],
            Json(json!({ "error": "maintenance_window", "retry_after": retry_after_secs })),
        )
            .into_response(),
    }
}

/// POST /tasks/{id}/cancel — abort a running task.
///
/// Sets task status to `Cancelled` then aborts the Tokio future (which kills
/// the child CLI process via `kill_on_drop(true)`).  Returns 409 if the task
/// is already in a terminal state.
pub(super) async fn cancel_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    use task_runner::TaskStatus;

    let task_id = harness_core::types::TaskId(id);

    let task = match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "task not found" })),
            );
        }
        Err(e) => {
            tracing::error!("cancel_task: DB lookup failed for {task_id:?}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            );
        }
    };

    if task.status.is_terminal() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "task already in terminal state" })),
        );
    }

    // Persist Cancelled status before aborting the future so the watcher sees
    // it and skips the record_task_failure path.
    if let Err(e) = task_runner::mutate_and_persist(&state.core.tasks, &task_id, |s| {
        s.status = TaskStatus::Cancelled;
        s.scheduler.mark_terminal(&TaskStatus::Cancelled);
    })
    .await
    {
        tracing::error!("cancel_task: failed to persist Cancelled for {task_id:?}: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "failed to persist cancellation" })),
        );
    }

    // abort() is a no-op if the task already finished — safe to call unconditionally.
    state.core.tasks.abort_task(&task_id);

    (StatusCode::OK, Json(json!({ "status": "cancelled" })))
}

#[cfg(test)]
#[path = "task_routes_tests.rs"]
mod tests;
