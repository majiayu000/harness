use super::{resolve_reviewer, AppState};
use crate::task_runner;
use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;
use std::sync::Arc;

#[derive(Debug)]
pub(super) enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
}

pub(super) async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    let agent =
        if let Some(name) = &req.agent {
            state.server.agent_registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            })?
        } else {
            let classification = crate::complexity_router::classify(
                req.prompt.as_deref().unwrap_or_default(),
                req.issue,
                req.pr,
            );
            state
                .server
                .agent_registry
                .dispatch(&classification)
                .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?
        };

    let (reviewer, review_config) = resolve_reviewer(
        &state.server.agent_registry,
        &state.server.config.agents.review,
        agent.name(),
    );

    let task_id = task_runner::spawn_task(
        state.tasks.clone(),
        agent,
        reviewer,
        review_config,
        state.skills.clone(),
        state.events.clone(),
        state.interceptors.clone(),
        req,
    )
    .await;

    Ok(task_id)
}

pub(super) async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "task_id": task_id.0,
                "status": "running"
            })),
        ),
        Err(EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}
