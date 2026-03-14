use super::{resolve_reviewer, AppState};
use crate::task_runner;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
}

impl std::fmt::Display for EnqueueTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    // Acquire concurrency permit before spawning. Blocks if all slots are
    // occupied; rejects immediately if the waiting queue is full.
    let permit = state
        .concurrency
        .task_queue
        .acquire()
        .await
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

    let agent =
        if let Some(name) = &req.agent {
            state.core.server.agent_registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            })?
        } else {
            let classification = crate::complexity_router::classify(
                req.prompt.as_deref().unwrap_or_default(),
                req.issue,
                req.pr,
            );
            state
                .core
                .server
                .agent_registry
                .dispatch(&classification)
                .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?
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
}

pub(super) async fn create_tasks_batch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchCreateTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
    let has_tasks = req.tasks.as_ref().is_some_and(|v| !v.is_empty());

    if !has_issues && !has_tasks {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "at least one of issues or tasks must be provided" })),
        );
    }

    // Build the list of per-task CreateTaskRequests.
    let mut task_requests: Vec<task_runner::CreateTaskRequest> = Vec::new();

    if let Some(issues) = req.issues {
        for issue in issues {
            let mut t = task_runner::CreateTaskRequest::default();
            t.issue = Some(issue);
            t.agent = req.agent.clone();
            if let Some(rounds) = req.max_rounds {
                t.max_rounds = rounds;
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
            if let Some(rounds) = req.max_rounds {
                t.max_rounds = rounds;
            }
            if let Some(timeout) = req.turn_timeout_secs {
                t.turn_timeout_secs = timeout;
            }
            task_requests.push(t);
        }
    }

    // Enqueue each task; collect results (task_id or error per entry).
    let mut results = Vec::with_capacity(task_requests.len());
    for task_req in task_requests {
        let entry = match enqueue_task(&state, task_req).await {
            Ok(task_id) => json!({ "task_id": task_id.0, "status": "running" }),
            Err(EnqueueTaskError::BadRequest(error)) => json!({ "error": error }),
            Err(EnqueueTaskError::Internal(error)) => json!({ "error": error }),
        };
        results.push(entry);
    }

    (StatusCode::ACCEPTED, Json(json!(results)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_request_deserializes_issues_format() {
        let json = r#"{"issues": [300, 301, 302], "agent": "claude", "max_rounds": 3, "turn_timeout_secs": 600}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.issues, Some(vec![300, 301, 302]));
        assert_eq!(req.agent.as_deref(), Some("claude"));
        assert_eq!(req.max_rounds, Some(3));
        assert_eq!(req.turn_timeout_secs, Some(600));
        assert!(req.tasks.is_none());
    }

    #[test]
    fn batch_request_deserializes_tasks_format() {
        let json = r#"{"tasks": [{"description": "fix bug X", "issue": 300}, {"description": "add feature Y", "issue": 301}]}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        let tasks = req.tasks.unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].description.as_deref(), Some("fix bug X"));
        assert_eq!(tasks[0].issue, Some(300));
        assert_eq!(tasks[1].description.as_deref(), Some("add feature Y"));
        assert_eq!(tasks[1].issue, Some(301));
        assert!(req.issues.is_none());
    }

    #[test]
    fn batch_request_deserializes_tasks_without_issue() {
        let json = r#"{"tasks": [{"description": "refactor module"}]}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        let tasks = req.tasks.unwrap();
        assert_eq!(tasks[0].description.as_deref(), Some("refactor module"));
        assert!(tasks[0].issue.is_none());
    }

    #[test]
    fn batch_request_empty_issues_list() {
        let json = r#"{"issues": []}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
        assert!(!has_issues);
    }

    #[test]
    fn batch_request_neither_issues_nor_tasks() {
        let json = r#"{"agent": "claude"}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
        let has_tasks = req.tasks.as_ref().is_some_and(|v| !v.is_empty());
        assert!(!has_issues && !has_tasks);
    }
}
