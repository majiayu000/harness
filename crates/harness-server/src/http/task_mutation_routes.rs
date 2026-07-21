use super::AppState;
use crate::workflow_runtime_submission::{
    RuntimeSubmissionCancelError, RuntimeSubmissionCancelOutcome,
};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use harness_workflow::runtime::{
    WorkflowEvidence, WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome,
    WorkflowRuntimeRecoveryRequest,
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
    #[serde(default)]
    pub evidence: Vec<WorkflowEvidence>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RuntimeTranscriptReconstructionRequest {
    pub workflow_id: String,
    pub runtime_job_id: String,
    pub content: String,
    #[serde(default)]
    pub expected_checksum: Option<String>,
}

pub(super) async fn require_authenticated_transcript_access(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    match super::auth::resolve_api_auth_mode(&state.core.server.config.server) {
        Ok(super::auth::ApiAuthMode::Enforced(_)) => next.run(request).await,
        Ok(super::auth::ApiAuthMode::Open) => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "transcript_reconstruction_requires_authentication"
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                "transcript reconstruction authentication configuration is invalid: {error}"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "authentication_misconfigured" })),
            )
                .into_response()
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

pub(super) async fn reconstruct_runtime_transcript(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RuntimeTranscriptReconstructionRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let workflow_id = request.workflow_id.trim();
    let runtime_job_id = request.runtime_job_id.trim();
    if workflow_id.is_empty() || runtime_job_id.is_empty() || request.content.trim().is_empty() {
        return runtime_recovery_error(
            StatusCode::BAD_REQUEST,
            "workflow_id, runtime_job_id, and content are required",
        );
    }
    let expected_checksum = request
        .expected_checksum
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if expected_checksum.is_some_and(|expected| {
        expected != harness_workflow::runtime::runtime_transcript_checksum(&request.content)
    }) {
        return runtime_recovery_error(
            StatusCode::BAD_REQUEST,
            "re-exported transcript checksum does not match expected_checksum",
        );
    }
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return runtime_recovery_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "workflow runtime store unavailable",
        );
    };
    let sources = match store
        .command_sources_for_runtime_jobs(&[runtime_job_id.to_string()])
        .await
    {
        Ok(sources) => sources,
        Err(error) => {
            tracing::error!(
                workflow_id,
                runtime_job_id,
                "reconstruct_runtime_transcript: ownership lookup failed: {error}"
            );
            return runtime_recovery_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to verify transcript producer ownership",
            );
        }
    };
    if sources
        .get(runtime_job_id)
        .is_none_or(|source| source.workflow_id != workflow_id)
    {
        return runtime_recovery_error(
            StatusCode::CONFLICT,
            "runtime job does not belong to the requested workflow",
        );
    }
    match store
        .reconstruct_runtime_transcript(
            workflow_id,
            runtime_job_id,
            &request.content,
            expected_checksum,
            "operator_api",
        )
        .await
    {
        Ok(record) => (
            StatusCode::OK,
            Json(json!({
                "status": "reconstructed",
                "workflow_id": record.workflow_id,
                "artifact_ref": record.reference.artifact_ref,
                "producer_runtime_job_id": record.reference.producer_runtime_job_id,
                "size_bytes": record.reference.size_bytes,
                "checksum": record.reference.checksum,
            })),
        ),
        Err(error) => {
            tracing::error!(
                workflow_id,
                runtime_job_id,
                "reconstruct_runtime_transcript: persistence failed: {error}"
            );
            runtime_recovery_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to reconstruct runtime transcript",
            )
        }
    }
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
            evidence: &request.evidence,
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
        WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition { workflow } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow runtime recovery supports only GitHub issue PR workflows",
                "workflow_id": workflow.id,
                "definition_id": workflow.definition_id,
                "state": workflow.state,
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::InvalidDefinitionPin { workflow, error } => (
            StatusCode::CONFLICT,
            json!({
                "error": "workflow declarative definition pin is invalid",
                "workflow_id": workflow.id,
                "state": workflow.state,
                "pin_error": format!("{error:?}"),
            }),
        ),
        WorkflowRuntimeRecoveryOutcome::OperatorRequired { workflow } => (
            StatusCode::FORBIDDEN,
            json!({ "error": "declarative workflow recovery requires an operator", "workflow_id": workflow.id }),
        ),
        WorkflowRuntimeRecoveryOutcome::TargetRequired { workflow } => (
            StatusCode::BAD_REQUEST,
            json!({ "error": "target_state is required when multiple recovery targets are declared", "workflow_id": workflow.id }),
        ),
        WorkflowRuntimeRecoveryOutcome::TargetNotAllowed {
            workflow,
            target_state,
        } => (
            StatusCode::CONFLICT,
            json!({ "error": "target_state is not an allowed pinned recovery target", "workflow_id": workflow.id, "target_state": target_state }),
        ),
        WorkflowRuntimeRecoveryOutcome::MissingRequiredEvidence { workflow, detail } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "error": "declarative workflow recovery is missing required evidence", "workflow_id": workflow.id, "detail": detail }),
        ),
        WorkflowRuntimeRecoveryOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            json!({ "error": "workflow not found" }),
        ),
    };
    (status, Json(body))
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
            WorkflowRuntimeRecoveryOutcome::TargetRequired {
                workflow: declarative(),
            },
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!("target_state is required when multiple recovery targets are declared")
        );

        let outcome = WorkflowRuntimeRecoveryOutcome::TargetNotAllowed {
            workflow: declarative(),
            target_state: "done".to_string(),
        };
        assert_eq!(
            runtime_recovery_response(WorkflowRuntimeRecoveryAction::Unblock, outcome).0,
            StatusCode::CONFLICT
        );
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
