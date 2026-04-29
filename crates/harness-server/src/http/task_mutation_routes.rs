use super::AppState;
use axum::{extract::State, http::StatusCode, Json};
use harness_workflow::issue_lifecycle::IssueLifecycleState;
use serde_json::json;
use std::sync::Arc;

/// POST /tasks/{id}/merge — human-gate approval to transition a `ready_to_merge`
/// workflow to `done`.
///
/// Returns 202 on success, 404 if the task or workflow is not found, and 409
/// if prerequisites are not met (no PR URL, workflow store unavailable, PR URL
/// unparseable, or workflow not in `ready_to_merge` state).
pub(super) async fn merge_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
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
            tracing::error!("merge_task: DB lookup failed for {task_id:?}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            );
        }
    };

    let pr_url = match task.pr_url.as_deref() {
        Some(url) => url.to_owned(),
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "no PR associated with task" })),
            );
        }
    };

    let workflows = match state.core.issue_workflow_store.as_ref() {
        Some(s) => s,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "workflow tracking not available" })),
            );
        }
    };

    let pr_num = match super::parse_pr_num_from_url(&pr_url) {
        Some(n) => n,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "could not parse PR number from task pr_url" })),
            );
        }
    };

    let project_id = match task.project_root.as_ref() {
        Some(p) => p.to_string_lossy().into_owned(),
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "task has no project_root" })),
            );
        }
    };

    let workflow = match workflows
        .get_by_pr(&project_id, task.repo.as_deref(), pr_num)
        .await
    {
        Ok(Some(wf)) => wf,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "workflow not found for PR" })),
            );
        }
        Err(e) => {
            tracing::error!("merge_task: workflow lookup failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            );
        }
    };

    if workflow.state != IssueLifecycleState::ReadyToMerge {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "workflow not in ready_to_merge state" })),
        );
    }

    match workflows
        .record_merge_approved(&project_id, task.repo.as_deref(), pr_num)
        .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": "merge_approved" })),
        ),
        Err(e) => {
            tracing::error!("merge_task: record_merge_approved failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to record merge approval" })),
            )
        }
    }
}
