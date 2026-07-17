use super::{state::AppState, task_submission_routes};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

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
