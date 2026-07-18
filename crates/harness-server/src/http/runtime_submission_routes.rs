use super::{state::AppState, task_submission_routes};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::agent::ApprovalDecision;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Serialize)]
struct ApprovalResponse {
    accepted: bool,
}

pub(crate) async fn respond_to_approval(
    State(state): State<Arc<AppState>>,
    Path((turn_id, request_id)): Path<(String, String)>,
    Json(decision): Json<ApprovalDecision>,
) -> Response {
    respond_to_approval_with_manager(
        &state.core.server.thread_manager,
        turn_id,
        request_id,
        decision,
    )
    .await
}

pub(super) async fn respond_to_approval_with_manager(
    thread_manager: &crate::thread_manager::ThreadManager,
    turn_id: String,
    request_id: String,
    decision: ApprovalDecision,
) -> Response {
    if turn_id.trim().is_empty() || request_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "turn_id and request_id must not be empty"})),
        )
            .into_response();
    }

    match thread_manager
        .respond_approval_on_runtime_handle(&turn_id, request_id, decision)
        .await
    {
        Ok(()) => Json(ApprovalResponse { accepted: true }).into_response(),
        Err(harness_core::error::HarnessError::TurnNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime turn not found"})),
        )
            .into_response(),
        Err(harness_core::error::HarnessError::Unsupported(message)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "approval response unavailable", "message": message})),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(turn_id = %turn_id, "runtime approval response failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "runtime approval response failed"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn get_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.core.workflow_runtime_store.is_none() {
        return runtime_store_unavailable();
    }
    task_submission_routes::get_runtime_artifacts(State(state), Path(id)).await
}

pub(crate) async fn get_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.core.workflow_runtime_store.is_none() {
        return runtime_store_unavailable();
    }
    task_submission_routes::get_runtime_prompts(State(state), Path(id)).await
}

fn runtime_store_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "workflow runtime store unavailable"})),
    )
        .into_response()
}
