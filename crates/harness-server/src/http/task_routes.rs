use super::{resolve_reviewer, AppState};
use crate::task_runner;
use axum::{extract::State, http::StatusCode, Json};
use harness_core::HarnessError;
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> harness_core::Result<task_runner::TaskId> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(HarnessError::InvalidState(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    // Acquire concurrency permit before spawning. Blocks if all slots are
    // occupied; rejects immediately if the waiting queue is full.
    let permit = state.concurrency.task_queue.acquire().await?;

    let agent =
        if let Some(name) = &req.agent {
            state.core.server.agent_registry.get(name).ok_or_else(|| {
                HarnessError::AgentNotFound(format!("agent '{name}' not registered"))
            })?
        } else {
            let classification = crate::complexity_router::classify(
                req.prompt.as_deref().unwrap_or_default(),
                req.issue,
                req.pr,
            );
            state.core.server.agent_registry.dispatch(&classification)?
        };

    let (reviewer, review_config) = resolve_reviewer(
        &state.core.server.agent_registry,
        &state.core.server.config.agents.review,
        agent.name(),
    );

    let task_id = task_runner::spawn_task(
        state.core.tasks.clone(),
        agent,
        reviewer,
        review_config,
        state.engines.skills.clone(),
        state.observability.events.clone(),
        state.interceptors.clone(),
        req,
        state.concurrency.workspace_mgr.clone(),
        permit,
        state.completion_callback.clone(),
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
        Err(err) if err.is_client_error() => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        ),
    }
}
