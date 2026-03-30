use super::{resolve_reviewer, AppState};
use crate::{services::execution::EnqueueTaskError, task_runner};
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Resolve a project path-or-ID through the registry.
///
/// If `project` is `None` or already points to an existing directory it is
/// returned unchanged.  If it is not a directory and a `registry` is
/// available, the value is treated as a project ID and looked up; a missing
/// ID is a `BadRequest` error.  When no registry is available the raw value
/// is passed through so downstream canonicalization can handle it.
async fn resolve_project_from_registry(
    registry: Option<&crate::project_registry::ProjectRegistry>,
    project: Option<std::path::PathBuf>,
) -> Result<Option<std::path::PathBuf>, EnqueueTaskError> {
    let (Some(registry), Some(project_path)) = (registry, project.clone()) else {
        return Ok(project);
    };
    if project_path.is_dir() {
        return Ok(Some(project_path));
    }
    let id = project_path.to_string_lossy();
    match registry.resolve_path(&id).await {
        Ok(Some(root)) => Ok(Some(root)),
        Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
            "project '{id}' not found in registry and is not a valid directory"
        ))),
        Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
    }
}

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    mut req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    // Resolve project: if the supplied path does not exist as a directory,
    // treat it as a project ID and look it up in the registry.
    req.project =
        resolve_project_from_registry(state.core.project_registry.as_deref(), req.project).await?;

    // Resolve and canonicalize the project root BEFORE acquiring the
    // concurrency permit so that:
    //   (a) None is mapped to the real worktree path rather than the literal
    //       "default" key, so per_project config for that path is respected.
    //   (b) Symlinked / relative / differently-spelled paths are normalised
    //       to the same canonical bucket, preventing limit bypass.
    // Overwrite req.project with the resolved path so spawn_task does not
    // re-detect the worktree inside the spawned future.
    let canonical_project = task_runner::resolve_canonical_project(req.project.clone())
        .await
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

    // Enforce allowed_project_roots allowlist on the resolved canonical path so
    // callers cannot bypass it by supplying a real directory path directly
    // instead of registering the project first.
    let allowed = &state.core.server.config.server.allowed_project_roots;
    if !allowed.is_empty()
        && !allowed
            .iter()
            .any(|base| canonical_project.starts_with(base))
    {
        return Err(EnqueueTaskError::BadRequest(
            "project root is not under an allowed base directory".to_string(),
        ));
    }

    let project_id = canonical_project.to_string_lossy().into_owned();
    req.project = Some(canonical_project);

    // Acquire concurrency permit before spawning. Blocks if all slots are
    // occupied; rejects immediately if the waiting queue is full.
    let permit = state
        .concurrency
        .task_queue
        .acquire(&project_id)
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

    let (reviewer, _review_config) = resolve_reviewer(
        &state.core.server.agent_registry,
        &state.core.server.config.agents.review,
        agent.name(),
    );

    let task_id = task_runner::spawn_task(
        state.core.tasks.clone(),
        agent,
        reviewer,
        Arc::new(state.core.server.config.clone()),
        state.engines.skills.clone(),
        state.observability.events.clone(),
        state.interceptors.clone(),
        req,
        state.concurrency.workspace_mgr.clone(),
        permit,
        state.intake.completion_callback.clone(),
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

/// Enqueues a task for background execution, returning its ID immediately.
///
/// Unlike `enqueue_task`, this function never blocks on concurrency permit
/// acquisition. The task is registered with Pending status right away, and a
/// background tokio task waits for a slot and then begins execution. This
/// keeps the `/tasks/batch` HTTP handler responsive even when all concurrency
/// slots are occupied.
///
/// When `group_sem` is `Some`, the background task acquires a permit from that
/// semaphore before competing for the per-project concurrency slot. Sharing the
/// same `Semaphore(1)` across multiple tasks in a conflict group serialises their
/// execution and prevents concurrent edits to the same files.
async fn enqueue_task_background(
    state: Arc<AppState>,
    mut req: task_runner::CreateTaskRequest,
    group_sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    // Resolve project: if the supplied path does not exist as a directory,
    // treat it as a project ID and look it up in the registry.
    req.project =
        resolve_project_from_registry(state.core.project_registry.as_deref(), req.project).await?;

    // Resolve agent up-front (fast, no I/O) so we can return an error immediately
    // if the agent name is invalid, before registering the task.
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

    let (reviewer, _review_config) = resolve_reviewer(
        &state.core.server.agent_registry,
        &state.core.server.config.agents.review,
        agent.name(),
    );

    // Resolve canonical project for per-project concurrency limits.
    let canonical_project = task_runner::resolve_canonical_project(req.project.clone())
        .await
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

    // Enforce allowed_project_roots allowlist (same guard as enqueue_task).
    let allowed = &state.core.server.config.server.allowed_project_roots;
    if !allowed.is_empty()
        && !allowed
            .iter()
            .any(|base| canonical_project.starts_with(base))
    {
        return Err(EnqueueTaskError::BadRequest(
            "project root is not under an allowed base directory".to_string(),
        ));
    }

    let project_id = canonical_project.to_string_lossy().into_owned();
    req.project = Some(canonical_project);
    task_runner::fill_missing_repo_from_project(&mut req).await;

    let server_config = std::sync::Arc::new(state.core.server.config.clone());

    tracing::info!(
        project = %req.project.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "None".to_string()),
        "enqueue_task_background: resolved project for batch task"
    );

    // Register the task immediately so the caller gets an ID without blocking.
    let task_id = task_runner::register_pending_task(state.core.tasks.clone(), &req).await;

    // Spawn a background tokio task that waits for a concurrency slot then executes.
    // The HTTP handler returns the task_id before this future completes.
    {
        let task_id2 = task_id.clone();
        tokio::spawn(async move {
            // Acquire the group serialisation permit before competing for the
            // per-project concurrency slot, then pass it into spawn_preregistered_task
            // so it is held inside the innermost future for the full task lifetime.
            let group_permit = if let Some(sem) = group_sem {
                sem.acquire_owned().await.ok()
            } else {
                None
            };
            match state.concurrency.task_queue.acquire(&project_id).await {
                Ok(permit) => {
                    task_runner::spawn_preregistered_task(
                        task_id2,
                        state.core.tasks.clone(),
                        agent,
                        reviewer,
                        server_config,
                        state.engines.skills.clone(),
                        state.observability.events.clone(),
                        state.interceptors.clone(),
                        req,
                        state.concurrency.workspace_mgr.clone(),
                        permit,
                        state.intake.completion_callback.clone(),
                        group_permit,
                    )
                    .await;
                }
                Err(e) => {
                    // Queue is full; mark the pre-registered task as failed.
                    if let Err(persist_err) =
                        task_runner::mutate_and_persist(&state.core.tasks, &task_id2, |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.error = Some(format!("task queue full: {e}"));
                        })
                        .await
                    {
                        tracing::error!(
                            task_id = %task_id2.0,
                            "failed to persist task failure after queue full: {persist_err}"
                        );
                    }
                    state.core.tasks.close_task_stream(&task_id2);
                }
            }
        });
    }

    Ok(task_id)
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
                t.max_rounds = Some(rounds);
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
                t.max_rounds = Some(rounds);
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

/// POST /tasks/{id}/cancel — abort a running task.
///
/// Sets task status to `Cancelled` then aborts the Tokio future (which kills
/// the child CLI process via `kill_on_drop(true)`).  Returns 409 if the task
/// is already in a terminal state.
pub(super) async fn cancel_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    use task_runner::TaskStatus;

    let task_id = harness_core::types::TaskId(id);

    let task = match state.core.tasks.get(&task_id) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "task not found" })),
            );
        }
    };

    if matches!(
        task.status,
        TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
    ) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "task already in terminal state" })),
        );
    }

    // Persist Cancelled status before aborting the future so the watcher sees
    // it and skips the record_task_failure path.
    if let Err(e) = task_runner::mutate_and_persist(&state.core.tasks, &task_id, |s| {
        s.status = TaskStatus::Cancelled;
    })
    .await
    {
        tracing::error!("cancel_task: failed to persist Cancelled for {task_id:?}: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "failed to persist cancellation" })),
        );
    }

    // abort() is a no-op if the task already finished — safe to call unconditionally.
    state.core.tasks.abort_task(&task_id);

    (StatusCode::OK, Json(json!({ "status": "cancelled" })))
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

    #[tokio::test]
    async fn resolve_project_from_registry_passes_through_none() {
        let result = resolve_project_from_registry(None, None).await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_project_from_registry_passes_through_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let result = resolve_project_from_registry(None, Some(path.clone())).await;
        assert_eq!(result.unwrap(), Some(path));
    }

    #[tokio::test]
    async fn resolve_project_from_registry_no_registry_passes_through_nondir() {
        // When no registry is available, non-dir paths are returned as-is
        // (downstream canonicalization handles them).
        let path = std::path::PathBuf::from("/nonexistent/path");
        let result = resolve_project_from_registry(None, Some(path.clone())).await;
        assert_eq!(result.unwrap(), Some(path));
    }

    #[tokio::test]
    async fn resolve_project_from_registry_resolves_id() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
            .await
            .unwrap();
        registry
            .register(crate::project_registry::Project {
                id: "my-repo".to_string(),
                root: std::path::PathBuf::from("/home/user/my-repo"),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await
            .unwrap();

        let result = resolve_project_from_registry(
            Some(&registry),
            Some(std::path::PathBuf::from("my-repo")),
        )
        .await;
        assert_eq!(
            result.unwrap(),
            Some(std::path::PathBuf::from("/home/user/my-repo"))
        );
    }

    #[tokio::test]
    async fn resolve_project_from_registry_unknown_id_returns_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
            .await
            .unwrap();

        let result = resolve_project_from_registry(
            Some(&registry),
            Some(std::path::PathBuf::from("unknown-repo")),
        )
        .await;
        assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
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
