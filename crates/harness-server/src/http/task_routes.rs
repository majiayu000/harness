use crate::{app_state::AppState, services::EnqueueTaskError, task_runner};
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    state.execution_svc.enqueue(req).await
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
    /// Project root or registry ID applied to all tasks in this batch.
    pub project: Option<std::path::PathBuf>,
}

/// Compute connected conflict groups from per-task file-reference sets.
///
/// Two tasks are in the same group when their file-reference sets overlap
/// (directly or transitively). Tasks whose file-reference set is empty form
/// singleton groups and are never serialised against other tasks.
fn build_conflict_groups(file_refs: &[Vec<String>]) -> Vec<Vec<usize>> {
    let n = file_refs.len();
    let mut visited = vec![false; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut group = vec![start];
        visited[start] = true;
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);

        while let Some(curr) = queue.pop_front() {
            for other in 0..n {
                if visited[other] {
                    continue;
                }
                let has_overlap = !file_refs[curr].is_empty()
                    && !file_refs[other].is_empty()
                    && file_refs[curr].iter().any(|f| file_refs[other].contains(f));
                if has_overlap {
                    visited[other] = true;
                    group.push(other);
                    queue.push_back(other);
                }
            }
        }

        groups.push(group);
    }

    groups
}

async fn enqueue_task_background(
    state: Arc<AppState>,
    req: task_runner::CreateTaskRequest,
    group_sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    state
        .execution_svc
        .enqueue_background_with_group(req, group_sem)
        .await
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
            t.project = req.project.clone();
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
            t.project = req.project.clone();
            if let Some(rounds) = req.max_rounds {
                t.max_rounds = rounds;
            }
            if let Some(timeout) = req.turn_timeout_secs {
                t.turn_timeout_secs = timeout;
            }
            task_requests.push(t);
        }
    }

    // Detect file-reference overlaps and build conflict groups.
    // Tasks sharing at least one file reference (directly or transitively) are
    // placed in the same group and assigned a shared Semaphore(1) so they execute
    // sequentially instead of concurrently.
    let task_file_refs: Vec<Vec<String>> = task_requests
        .iter()
        .map(|t| {
            if let Some(p) = t.prompt.as_deref() {
                crate::parallel_dispatch::extract_file_refs(p)
            } else if t.issue.is_some() {
                // Issue-only task: no prompt to extract file refs from.
                // Use a sentinel so all such tasks in this batch are placed in
                // the same conflict group and serialised (conservative: we
                // cannot know which files they will touch without fetching the
                // issue body).
                vec!["__unresolved_issue__".to_string()]
            } else {
                Vec::new()
            }
        })
        .collect();

    let conflict_groups = build_conflict_groups(&task_file_refs);

    let n = task_requests.len();
    let mut task_semaphores: Vec<Option<Arc<tokio::sync::Semaphore>>> = vec![None; n];
    let mut task_conflict_files: Vec<Vec<String>> = vec![Vec::new(); n];

    for group in &conflict_groups {
        if group.len() < 2 {
            continue;
        }
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        for &idx in group {
            task_semaphores[idx] = Some(Arc::clone(&sem));
            // Collect files from this task that overlap with any other group member.
            let mut shared: std::collections::HashSet<String> = std::collections::HashSet::new();
            for &other in group {
                if other == idx {
                    continue;
                }
                for f in &task_file_refs[idx] {
                    if task_file_refs[other].contains(f) {
                        shared.insert(f.clone());
                    }
                }
            }
            let mut files: Vec<String> = shared.into_iter().collect();
            files.sort();
            task_conflict_files[idx] = files;
        }
    }

    // Register each task without blocking on concurrency permit acquisition.
    // Each task gets an ID immediately; a background tokio task handles permit
    // waiting and execution. The HTTP handler returns as soon as all tasks are registered.
    let mut results = Vec::with_capacity(n);
    for (i, task_req) in task_requests.into_iter().enumerate() {
        let sem = task_semaphores[i].take();
        let is_serialized = sem.is_some();
        let conflict_files = std::mem::take(&mut task_conflict_files[i]);
        let entry = match enqueue_task_background(state.clone(), task_req, sem).await {
            Ok(task_id) => {
                if is_serialized {
                    json!({
                        "task_id": task_id.0,
                        "status": "queued",
                        "serialized": true,
                        "conflict_files": conflict_files,
                    })
                } else {
                    json!({ "task_id": task_id.0, "status": "queued" })
                }
            }
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

    #[test]
    fn batch_request_deserializes_project_field() {
        let json = r#"{"issues": [1, 2], "project": "/home/user/my-repo"}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.project,
            Some(std::path::PathBuf::from("/home/user/my-repo"))
        );
    }

    #[test]
    fn batch_request_project_defaults_to_none() {
        let json = r#"{"issues": [1]}"#;
        let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
        assert!(req.project.is_none());
    }

    #[test]
    fn conflict_groups_empty_refs_are_singletons() {
        let refs: Vec<Vec<String>> = vec![vec![], vec![], vec![]];
        let groups = build_conflict_groups(&refs);
        assert_eq!(groups.len(), 3);
        for g in &groups {
            assert_eq!(g.len(), 1);
        }
    }

    #[test]
    fn conflict_groups_two_tasks_share_file() {
        let refs = vec![
            vec!["src/auth.rs".to_string()],
            vec!["src/auth.rs".to_string(), "src/db.rs".to_string()],
            vec!["src/config.rs".to_string()],
        ];
        let groups = build_conflict_groups(&refs);
        // Tasks 0 and 1 share auth.rs; task 2 is independent.
        assert_eq!(groups.len(), 2);
        assert!(
            groups.iter().any(|g| g.contains(&0) && g.contains(&1)),
            "tasks 0 and 1 must be in the same conflict group"
        );
        assert!(
            groups.iter().any(|g| g.len() == 1 && g[0] == 2),
            "task 2 must be a singleton group"
        );
    }

    #[test]
    fn conflict_groups_transitive_overlap() {
        // A-B share a.rs, B-C share b.rs → all three in same group.
        let refs = vec![
            vec!["a.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["b.rs".to_string()],
        ];
        let groups = build_conflict_groups(&refs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn conflict_groups_no_overlap() {
        let refs = vec![
            vec!["a.rs".to_string()],
            vec!["b.rs".to_string()],
            vec!["c.rs".to_string()],
        ];
        let groups = build_conflict_groups(&refs);
        assert_eq!(groups.len(), 3);
        for g in &groups {
            assert_eq!(g.len(), 1);
        }
    }

    #[test]
    fn conflict_groups_single_task() {
        let refs = vec![vec!["src/main.rs".to_string()]];
        let groups = build_conflict_groups(&refs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], vec![0]);
    }
}
