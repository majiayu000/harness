use super::AppState;
use crate::workflow_runtime_submission::{
    RuntimeSubmissionCancelError, RuntimeSubmissionCancelOutcome,
};
use axum::{extract::State, http::StatusCode, Json};
use harness_workflow::issue_lifecycle::IssueMergeApprovalOutcome;
use harness_workflow::runtime::{
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
};
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

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRuntimeRecoveryRouteRequest {
    pub workflow_id: String,
    pub reason: String,
    #[serde(default)]
    pub target_state: Option<String>,
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

    match workflows
        .record_merge_approved(&project_id, task.repo.as_deref(), pr_num)
        .await
    {
        Ok(IssueMergeApprovalOutcome::Applied(_)) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": "merge_approved" })),
        ),
        Ok(IssueMergeApprovalOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "workflow not found for PR" })),
        ),
        Ok(IssueMergeApprovalOutcome::IgnoredWrongState { actual, .. }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "workflow not in ready_to_merge state",
                "state": actual,
            })),
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

pub(super) async fn unblock_workflow_runtime(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowRuntimeRecoveryRouteRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    recover_workflow_runtime(state, request, WorkflowRuntimeRecoveryAction::Unblock).await
}

pub(super) async fn retry_workflow_runtime(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowRuntimeRecoveryRouteRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    recover_workflow_runtime(state, request, WorkflowRuntimeRecoveryAction::Retry).await
}

async fn recover_workflow_runtime(
    state: Arc<AppState>,
    request: WorkflowRuntimeRecoveryRouteRequest,
    action: WorkflowRuntimeRecoveryAction,
) -> (StatusCode, Json<serde_json::Value>) {
    let workflow_id = request.workflow_id.trim();
    if workflow_id.is_empty() {
        return runtime_recovery_error(StatusCode::BAD_REQUEST, "workflow_id is required");
    }
    let reason = request.reason.trim();
    if reason.is_empty() {
        return runtime_recovery_error(StatusCode::BAD_REQUEST, "reason is required");
    }
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return runtime_recovery_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "workflow runtime store unavailable",
        );
    };

    match store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id,
            action,
            reason,
            actor: "operator",
            target_state: request.target_state.as_deref(),
        })
        .await
    {
        Ok(outcome) => runtime_recovery_response(action, outcome),
        Err(error) => {
            tracing::error!(
                action = action.as_str(),
                "recover_workflow_runtime: recovery failed: {error}"
            );
            runtime_recovery_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to recover workflow runtime submission",
            )
        }
    }
}

fn runtime_recovery_error(
    status: StatusCode,
    error: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(json!({ "error": error })))
}

fn runtime_recovery_response(
    action: WorkflowRuntimeRecoveryAction,
    outcome: WorkflowRuntimeRecoveryOutcome,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, body) = match outcome {
        WorkflowRuntimeRecoveryOutcome::Recovered {
            workflow,
            previous_state,
            ..
        } => (
            StatusCode::OK,
            json!({
                "status": match action {
                    WorkflowRuntimeRecoveryAction::Unblock => "unblocked",
                    WorkflowRuntimeRecoveryAction::Retry => "retried",
                },
                "execution_path": "workflow_runtime",
                "workflow_id": workflow.id,
                "previous_state": previous_state,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::WrongState { workflow } => (
            StatusCode::CONFLICT,
            json!({
                "error": match action {
                    WorkflowRuntimeRecoveryAction::Unblock => "workflow not in blocked state",
                    WorkflowRuntimeRecoveryAction::Retry => "workflow not in failed state",
                },
                "workflow_id": workflow.id,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::NonRetryableFailure {
            workflow,
            error_kind,
        } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow failure is not retryable",
                "workflow_id": workflow.id,
                "state": workflow.state,
                "error_kind": error_kind,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { workflow, activity } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow runtime recovery cannot determine a supported stopped activity",
                "workflow_id": workflow.id,
                "state": workflow.state,
                "last_stop_activity": activity,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::MissingRecoveryTarget { workflow } => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": "target_state is required for declarative workflow unblock",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::InvalidRecoveryTarget {
            workflow,
            target_state,
        } => (
            StatusCode::CONFLICT,
            json!({
                "error": "target_state is not an active declared recovery target",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
                "target_state": target_state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::DefinitionPinMismatch { workflow } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow declarative definition pin does not match a loaded version",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "definition_version": workflow.definition_version,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::DeclarativeWrongState { workflow } => (
            StatusCode::CONFLICT,
            json!({
                "error": "declarative workflow is not in blocked state",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::UnsupportedRecoveryAction { workflow, action } => (
            StatusCode::CONFLICT,
            json!({
                "error": "declarative workflow recovery supports only unblock",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
                "action": action.as_str(),
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition { workflow } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow runtime recovery supports only GitHub issue PR workflows",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            json!({ "error": "workflow not found" }),
        ),
    };
    (status, Json(body))
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

#[cfg(test)]
mod recovery_response_tests {
    use super::*;
    use harness_workflow::runtime::{WorkflowInstance, WorkflowSubject};

    fn workflow(definition_id: &str, state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition_id,
            1,
            state,
            WorkflowSubject::new("issue", "issue:1609"),
        )
        .with_id("workflow-1609")
    }

    #[test]
    fn recovery_request_accepts_optional_target_state() {
        let legacy: WorkflowRuntimeRecoveryRouteRequest = serde_json::from_value(json!({
            "workflow_id": "workflow-1",
            "reason": "operator recovery",
        }))
        .expect("legacy recovery request should deserialize");
        assert_eq!(legacy.target_state, None);

        let declarative: WorkflowRuntimeRecoveryRouteRequest = serde_json::from_value(json!({
            "workflow_id": "workflow-2",
            "reason": "operator recovery",
            "target_state": "running",
        }))
        .expect("declarative recovery request should deserialize");
        assert_eq!(declarative.target_state.as_deref(), Some("running"));
    }

    #[test]
    fn builtin_wrong_state_bodies_remain_exact() {
        let (status, Json(body)) = runtime_recovery_response(
            WorkflowRuntimeRecoveryAction::Unblock,
            WorkflowRuntimeRecoveryOutcome::WrongState {
                workflow: workflow("github_issue_pr", "implementing"),
            },
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "error": "workflow not in blocked state",
                "workflow_id": "workflow-1609",
                "state": "implementing",
            })
        );

        let (status, Json(body)) = runtime_recovery_response(
            WorkflowRuntimeRecoveryAction::Retry,
            WorkflowRuntimeRecoveryOutcome::WrongState {
                workflow: workflow("github_issue_pr", "blocked"),
            },
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "error": "workflow not in failed state",
                "workflow_id": "workflow-1609",
                "state": "blocked",
            })
        );
    }

    #[test]
    fn declarative_recovery_rejections_have_stable_statuses() {
        let declarative = || workflow("release_workflow", "blocked");
        let (status, Json(body)) = runtime_recovery_response(
            WorkflowRuntimeRecoveryAction::Unblock,
            WorkflowRuntimeRecoveryOutcome::MissingRecoveryTarget {
                workflow: declarative(),
            },
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!("target_state is required for declarative workflow unblock")
        );

        for outcome in [
            WorkflowRuntimeRecoveryOutcome::InvalidRecoveryTarget {
                workflow: declarative(),
                target_state: "done".to_string(),
            },
            WorkflowRuntimeRecoveryOutcome::DefinitionPinMismatch {
                workflow: declarative(),
            },
            WorkflowRuntimeRecoveryOutcome::DeclarativeWrongState {
                workflow: declarative(),
            },
            WorkflowRuntimeRecoveryOutcome::UnsupportedRecoveryAction {
                workflow: declarative(),
                action: WorkflowRuntimeRecoveryAction::Retry,
            },
        ] {
            assert_eq!(
                runtime_recovery_response(WorkflowRuntimeRecoveryAction::Unblock, outcome).0,
                StatusCode::CONFLICT
            );
        }
    }

    #[test]
    fn unsupported_definition_body_remains_exact() {
        let (status, Json(body)) = runtime_recovery_response(
            WorkflowRuntimeRecoveryAction::Unblock,
            WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition {
                workflow: workflow("quality_gate", "blocked"),
            },
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({
                "error": "workflow runtime recovery supports only GitHub issue PR workflows",
                "workflow_id": "workflow-1609",
                "definition_id": "quality_gate",
                "state": "blocked",
            })
        );
    }
}
