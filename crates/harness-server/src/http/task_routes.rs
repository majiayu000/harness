use super::AppState;
use crate::{
    runtime_projection::RuntimeWorkflowProjection,
    services::execution::EnqueueTaskError,
    workflow_runtime_submission::{CreateTaskRequest, TaskId},
};
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde_json::json;
use std::sync::Arc;

pub(crate) use crate::services::execution::QueueDomain;

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    req: CreateTaskRequest,
) -> Result<TaskId, EnqueueTaskError> {
    state.execution_svc.enqueue(req).await
}

pub(crate) async fn enqueue_task_background(
    state: Arc<AppState>,
    req: CreateTaskRequest,
) -> Result<TaskId, EnqueueTaskError> {
    enqueue_task_background_in_domain(state, req, QueueDomain::Primary).await
}

pub(crate) async fn enqueue_task_background_in_domain(
    state: Arc<AppState>,
    req: CreateTaskRequest,
    queue_domain: QueueDomain,
) -> Result<TaskId, EnqueueTaskError> {
    state
        .execution_svc
        .enqueue_in_domain(req, queue_domain)
        .await
}

pub(crate) struct TaskResponseDetails {
    pub(crate) status: String,
    pub(crate) workflow_state: String,
    pub(crate) submission_id: String,
    pub(crate) workflow_id: String,
}

pub(crate) async fn task_response_details(
    state: &AppState,
    task_id: &TaskId,
) -> Result<TaskResponseDetails, EnqueueTaskError> {
    let store =
        state.core.workflow_runtime_store.as_ref().ok_or_else(|| {
            EnqueueTaskError::Internal("workflow runtime store unavailable".into())
        })?;
    let workflow = store
        .get_instance_by_submission_id(task_id.as_str())
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?
        .ok_or_else(|| {
            EnqueueTaskError::Internal(format!(
                "workflow runtime submission not found for {}",
                task_id.as_str()
            ))
        })?;
    let submission_id = crate::workflow_runtime_submission::runtime_issue_task_handle(&workflow)
        .map(|task_id| task_id.0)
        .unwrap_or_else(|| task_id.as_str().to_string());
    let projection = RuntimeWorkflowProjection::from_workflow(&workflow);
    Ok(TaskResponseDetails {
        status: projection.task_status.as_ref().to_string(),
        workflow_state: workflow.state,
        submission_id,
        workflow_id: workflow.id,
    })
}

fn task_submission_response(task_id: &TaskId, details: TaskResponseDetails) -> serde_json::Value {
    json!({
        "task_id": details.submission_id,
        "submission_id": details.submission_id,
        "status": details.status,
        "workflow_state": details.workflow_state,
        "execution_path": "workflow_runtime",
        "workflow_id": details.workflow_id,
        "request_id": task_id.as_str(),
    })
}

pub(super) async fn create_runtime_submission(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTaskRequest>,
) -> Response {
    match enqueue_task_background(state.clone(), req).await {
        Ok(task_id) => match task_response_details(&state, &task_id).await {
            Ok(details) => (
                StatusCode::ACCEPTED,
                Json(task_submission_response(&task_id, details)),
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
        },
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
