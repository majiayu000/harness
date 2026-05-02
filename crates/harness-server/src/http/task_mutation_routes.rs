use super::AppState;
use crate::workflow_runtime_submission::{
    RuntimeSubmissionCancelError, RuntimeSubmissionCancelOutcome,
};
use axum::{extract::State, http::StatusCode, Json};
use harness_workflow::issue_lifecycle::IssueLifecycleState;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRuntimeMergeRequest {
    pub workflow_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRuntimeCancelRequest {
    pub workflow_id: String,
}

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
            return merge_runtime_task_handle(&state, task_id.as_str()).await;
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

pub(super) async fn merge_workflow_runtime(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowRuntimeMergeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        );
    };
    match crate::workflow_runtime_pr_feedback::approve_runtime_merge_by_workflow_id(
        store,
        &request.workflow_id,
    )
    .await
    {
        Ok(outcome) => runtime_merge_response(outcome),
        Err(error) => {
            tracing::error!("merge_workflow_runtime: approval failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to approve workflow runtime merge" })),
            )
        }
    }
}

pub(super) async fn cancel_workflow_runtime(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowRuntimeCancelRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        );
    };

    match crate::workflow_runtime_submission::cancel_submission_by_workflow_id(
        store,
        &request.workflow_id,
    )
    .await
    {
        Ok(RuntimeSubmissionCancelOutcome::Cancelled(workflow)) => {
            if let Err(error) =
                crate::workflow_runtime_worker::notify_runtime_submission_terminal_workflow(
                    &state,
                    &workflow.id,
                    None,
                )
                .await
            {
                tracing::warn!(
                    workflow_id = %workflow.id,
                    "cancel_workflow_runtime: terminal notification failed: {error}"
                );
            }
            (
                StatusCode::OK,
                Json(json!({
                    "status": "cancelled",
                    "execution_path": "workflow_runtime",
                    "workflow_id": workflow.id,
                })),
            )
        }
        Ok(RuntimeSubmissionCancelOutcome::AlreadyTerminal(workflow)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow already terminal",
                "state": workflow.state,
            })),
        ),
        Ok(RuntimeSubmissionCancelOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "workflow not found" })),
        ),
        Err(RuntimeSubmissionCancelError::UnsupportedDefinition { definition_id }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow cannot be cancelled as a runtime submission",
                "definition_id": definition_id,
            })),
        ),
        Err(error) => {
            tracing::error!("cancel_workflow_runtime: cancellation failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to cancel workflow runtime submission" })),
            )
        }
    }
}

async fn merge_runtime_task_handle(
    state: &AppState,
    task_id: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "task not found" })),
        );
    };
    match crate::workflow_runtime_pr_feedback::approve_runtime_merge_by_task_id(store, task_id)
        .await
    {
        Ok(crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "task not found" })),
        ),
        Ok(outcome) => runtime_merge_response(outcome),
        Err(error) => {
            tracing::error!("merge_task: runtime workflow approval failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to approve workflow runtime merge" })),
            )
        }
    }
}

fn runtime_merge_response(
    outcome: crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome,
) -> (StatusCode, Json<serde_json::Value>) {
    match outcome {
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::Approved {
            workflow_id,
        } => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "merge_approved",
                "execution_path": "workflow_runtime",
                "workflow_id": workflow_id,
            })),
        ),
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "workflow not found" })),
        ),
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotCandidate {
            definition_id,
            ..
        } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow is not a GitHub issue PR workflow",
                "definition_id": definition_id,
            })),
        ),
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotReady {
            state,
            ..
        } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow not in ready_to_merge state",
                "state": state,
            })),
        ),
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::Rejected {
            reason,
            ..
        } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow runtime merge approval rejected",
                "reason": reason,
            })),
        ),
    }
}
