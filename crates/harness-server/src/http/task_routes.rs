use super::{resolve_reviewer, AppState};
use crate::{
    project_registry::check_allowed_roots, services::execution::EnqueueTaskError, task_runner,
};
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use harness_core::agent::CodeAgent;
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
) -> Result<(Option<std::path::PathBuf>, Option<String>), EnqueueTaskError> {
    let (Some(registry), Some(project_path)) = (registry, project.clone()) else {
        return Ok((project, None));
    };
    if project_path.is_dir() {
        return Ok((Some(project_path), None));
    }
    let id = project_path.to_string_lossy();
    // Try primary ID first, then name as fallback so `project: "litellm"` still
    // resolves even though the registry key is now the canonical path.
    match registry.get(&id).await {
        Ok(Some(p)) => return Ok((Some(p.root), p.default_agent)),
        Ok(None) => {}
        Err(e) => return Err(EnqueueTaskError::Internal(e.to_string())),
    }
    match registry.get_by_name(&id).await {
        Ok(Some(p)) => Ok((Some(p.root), p.default_agent)),
        Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
            "project '{id}' not found in registry and is not a valid directory"
        ))),
        Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
    }
}

/// Auto-populate and normalize external_id for deduplication.
///
/// Canonical format is `"issue:N"` / `"pr:N"`.  GitHub intake sets a raw
/// numeric string (`"42"`) while API submissions leave it empty; this
/// function normalizes both paths to the same canonical form so that
/// verbatim comparison in `find_active_duplicate` matches correctly.
fn populate_external_id(req: &mut task_runner::CreateTaskRequest) {
    match &req.external_id {
        None => {
            if let Some(issue) = req.issue {
                req.external_id = Some(format!("issue:{issue}"));
            } else if let Some(pr) = req.pr {
                req.external_id = Some(format!("pr:{pr}"));
            }
        }
        Some(id) => {
            // Already canonical — nothing to do.
            if id.starts_with("issue:") || id.starts_with("pr:") {
                return;
            }
            // Raw numeric ID from intake — normalize to canonical form.
            if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
                if req.issue.is_some() {
                    req.external_id = Some(format!("issue:{id}"));
                } else if req.pr.is_some() {
                    req.external_id = Some(format!("pr:{id}"));
                }
            }
        }
    }
}

/// Return existing active TaskId if one matches project + external_id.
async fn check_duplicate(
    tasks: &Arc<crate::task_runner::TaskStore>,
    project_id: &str,
    req: &task_runner::CreateTaskRequest,
) -> Option<task_runner::TaskId> {
    let ext_id = req.external_id.as_deref()?;
    let existing_id = tasks.find_active_duplicate(project_id, ext_id).await?;
    tracing::info!(
        existing_task = %existing_id.0,
        external_id = %ext_id,
        "dedup: returning existing active task instead of creating duplicate"
    );
    Some(existing_id)
}

/// Return existing terminal TaskId if a `done` task with a PR URL matches project + external_id.
/// This is the second dedup layer — catches cases where a previous task completed with a PR
/// but a re-submission would create a duplicate.
async fn check_pr_duplicate(
    tasks: &Arc<crate::task_runner::TaskStore>,
    project_id: &str,
    req: &task_runner::CreateTaskRequest,
) -> Option<task_runner::TaskId> {
    let ext_id = req.external_id.as_deref()?;
    let (existing_id, pr_url) = tasks.find_terminal_pr_duplicate(project_id, ext_id).await?;
    tracing::info!(
        existing_task = %existing_id.0,
        external_id = %ext_id,
        pr_url = %pr_url,
        "dedup: terminal task already created PR, returning existing task instead of creating duplicate"
    );
    Some(existing_id)
}

/// Three-tier agent selection: request override > project default > complexity dispatch.
///
/// Tier 1: `req.agent` is `Some` — use the named agent directly (unchanged behaviour).
/// Tier 2a: project root is known — load `.harness/config.toml`, resolve its
///          `agent.default` field. The sentinel value `"auto"` means "fall through".
/// Tier 2b: `registry_agent` is `Some` — the project registry record has a
///          `default_agent` configured via `POST /api/projects`. Also treats `"auto"` as
///          fall-through so callers can explicitly opt in to complexity dispatch.
/// Tier 3: fall back to complexity-based dispatch via [`crate::complexity_router`].
///
/// Calling this helper from both enqueue paths ensures the three tiers are
/// enforced identically and cannot drift apart.
pub(crate) fn select_agent(
    req: &task_runner::CreateTaskRequest,
    registry: &harness_agents::registry::AgentRegistry,
    registry_agent: Option<&str>,
) -> Result<Arc<dyn CodeAgent>, EnqueueTaskError> {
    // Tier 1: explicit agent override from the request.
    if let Some(name) = &req.agent {
        return registry
            .get(name)
            .ok_or_else(|| EnqueueTaskError::BadRequest(format!("agent '{name}' not registered")));
    }

    // Tier 2a: project-level default agent from .harness/config.toml.
    // Honor the explicit setting unconditionally — even when it matches the
    // server default — so that a project pinning the global default agent name
    // still bypasses complexity dispatch (tier 3).
    // The sentinel value "auto" means "fall through to complexity dispatch".
    if let Some(project_root) = &req.project {
        let project_cfg = harness_core::config::project::load_project_config(project_root)
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        if let Some(agent_name) = project_cfg.agent.as_ref().and_then(|a| a.default.as_ref()) {
            if agent_name != "auto" {
                return registry.get(agent_name).ok_or_else(|| {
                    EnqueueTaskError::BadRequest(format!("agent '{agent_name}' not registered"))
                });
            }
            // "auto" => fall through to tier 2b / tier 3
        }
    }

    // Tier 2b: project registry default agent (configured via POST /api/projects).
    // Also treats "auto" as a fall-through sentinel.
    if let Some(name) = registry_agent {
        if name != "auto" {
            return registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            });
        }
        // "auto" => fall through to tier 3
    }

    // Tier 3: complexity-based dispatch (global fallback).
    let classification = crate::complexity_router::classify(
        req.prompt.as_deref().unwrap_or_default(),
        req.issue,
        req.pr,
    );
    registry
        .dispatch(&classification)
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))
}

pub(crate) async fn enqueue_task(
    state: &Arc<AppState>,
    mut req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    let now = chrono::Utc::now();
    if state
        .core
        .server
        .config
        .maintenance_window
        .in_quiet_window(now)
    {
        let retry_after_secs = state
            .core
            .server
            .config
            .maintenance_window
            .secs_until_window_end(now);
        return Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs });
    }

    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }
    if req.priority > task_runner::MAX_TASK_PRIORITY {
        return Err(EnqueueTaskError::BadRequest(format!(
            "priority {} out of range; maximum is {} (0=normal, 1=high, 2=critical)",
            req.priority,
            task_runner::MAX_TASK_PRIORITY,
        )));
    }

    // Resolve project: if the supplied path does not exist as a directory,
    // treat it as a project ID and look it up in the registry.
    let (resolved_project, registry_default_agent) =
        resolve_project_from_registry(state.core.project_registry.as_deref(), req.project).await?;
    req.project = resolved_project;

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
    check_allowed_roots(
        &canonical_project,
        &state.core.server.config.server.allowed_project_roots,
    )
    .map_err(EnqueueTaskError::BadRequest)?;

    let project_id = canonical_project.to_string_lossy().into_owned();
    req.project = Some(canonical_project);

    // Auto-populate external_id and check for duplicates before acquiring
    // a concurrency permit (same dedup as enqueue_task_background).
    populate_external_id(&mut req);
    if let Some(existing_id) = check_duplicate(&state.core.tasks, &project_id, &req).await {
        return Ok(existing_id);
    }
    if let Some(existing_id) = check_pr_duplicate(&state.core.tasks, &project_id, &req).await {
        return Ok(existing_id);
    }

    // Tasks with unresolved dependencies are registered as AwaitingDeps without
    // acquiring a concurrency permit. The dep watcher will dispatch them later.
    // If all declared deps are already Done we skip the AwaitingDeps path and
    // fall through to the normal concurrency-permit dispatch below.
    if !req.depends_on.is_empty() {
        let mut all_deps_done = true;
        for dep_id in &req.depends_on {
            if !matches!(
                state.core.tasks.dep_status(dep_id).await,
                Some(task_runner::TaskStatus::Done)
            ) {
                all_deps_done = false;
                break;
            }
        }
        if !all_deps_done {
            let task_id = task_runner::spawn_task_awaiting_deps(state.core.tasks.clone(), req)
                .await
                .map_err(|e| EnqueueTaskError::BadRequest(e.to_string()))?;
            return Ok(task_id);
        }
        // All deps satisfied — clear the list so the normal spawn path
        // does not re-enter the AwaitingDeps branch.
        req.depends_on.clear();
    }

    // Acquire concurrency permit before spawning. Blocks if all slots are
    // occupied; rejects immediately if the waiting queue is full.
    let permit = state
        .concurrency
        .task_queue
        .acquire(&project_id, req.priority)
        .await
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

    let agent = select_agent(
        &req,
        &state.core.server.agent_registry,
        registry_default_agent.as_deref(),
    )?;

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
    let now = chrono::Utc::now();
    if state
        .core
        .server
        .config
        .maintenance_window
        .in_quiet_window(now)
    {
        let retry_after_secs = state
            .core
            .server
            .config
            .maintenance_window
            .secs_until_window_end(now);
        return Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs });
    }

    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }
    if req.priority > task_runner::MAX_TASK_PRIORITY {
        return Err(EnqueueTaskError::BadRequest(format!(
            "priority {} out of range; maximum is {} (0=normal, 1=high, 2=critical)",
            req.priority,
            task_runner::MAX_TASK_PRIORITY,
        )));
    }

    // Resolve project: if the supplied path does not exist as a directory,
    // treat it as a project ID and look it up in the registry.
    let (resolved_project, registry_default_agent) =
        resolve_project_from_registry(state.core.project_registry.as_deref(), req.project).await?;
    req.project = resolved_project;

    // Canonicalize the project root (detects worktree when project is None) BEFORE
    // the security check and BEFORE reading any per-project config files, so that:
    //   (a) The detected worktree root is exposed to select_agent's tier-2 lookup.
    //   (b) An untrusted caller cannot read .harness/config.toml from an arbitrary
    //       path before the allowlist check rejects the request.
    let canonical_project = task_runner::resolve_canonical_project(req.project.clone())
        .await
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

    // Enforce allowed_project_roots allowlist (same guard as enqueue_task).
    check_allowed_roots(
        &canonical_project,
        &state.core.server.config.server.allowed_project_roots,
    )
    .map_err(EnqueueTaskError::BadRequest)?;

    let project_id = canonical_project.to_string_lossy().into_owned();
    req.project = Some(canonical_project);
    task_runner::fill_missing_repo_from_project(&mut req).await;

    // Resolve agent after the canonical project path is written into req.project,
    // so that tier-2 (project config) and the security boundary both see the
    // fully-resolved path.
    let agent = select_agent(
        &req,
        &state.core.server.agent_registry,
        registry_default_agent.as_deref(),
    )?;

    let (reviewer, _review_config) = resolve_reviewer(
        &state.core.server.agent_registry,
        &state.core.server.config.agents.review,
        agent.name(),
    );

    // Auto-populate external_id and check for duplicates.
    populate_external_id(&mut req);
    if let Some(existing_id) = check_duplicate(&state.core.tasks, &project_id, &req).await {
        return Ok(existing_id);
    }
    if let Some(existing_id) = check_pr_duplicate(&state.core.tasks, &project_id, &req).await {
        return Ok(existing_id);
    }

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
            match state
                .concurrency
                .task_queue
                .acquire(&project_id, req.priority)
                .await
            {
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
) -> Response {
    let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
    let has_tasks = req.tasks.as_ref().is_some_and(|v| !v.is_empty());

    if !has_issues && !has_tasks {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "at least one of issues or tasks must be provided" })),
        )
            .into_response();
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
                // Treat as independent (empty refs → singleton group) so batch
                // submissions run in parallel. Real conflicts are caught by git
                // worktree isolation and GitHub merge conflict detection.
                Vec::new()
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
    let mut all_maintenance_window = n > 0;
    let mut mw_retry_after: Option<u64> = None;
    for (i, task_req) in task_requests.into_iter().enumerate() {
        let sem = task_semaphores[i].take();
        let is_serialized = sem.is_some();
        let conflict_files = std::mem::take(&mut task_conflict_files[i]);
        let entry = match enqueue_task_background(state.clone(), task_req, sem).await {
            Ok(task_id) => {
                all_maintenance_window = false;
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
            Err(EnqueueTaskError::BadRequest(error)) => {
                all_maintenance_window = false;
                json!({ "error": error })
            }
            Err(EnqueueTaskError::Internal(error)) => {
                all_maintenance_window = false;
                json!({ "error": error })
            }
            Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs }) => {
                mw_retry_after.get_or_insert(retry_after_secs);
                json!({ "error": "maintenance_window", "retry_after": retry_after_secs })
            }
        };
        results.push(entry);
    }

    if all_maintenance_window {
        let retry_after = mw_retry_after.unwrap_or(0);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
            Json(json!(results)),
        )
            .into_response();
    }

    (StatusCode::ACCEPTED, Json(json!(results))).into_response()
}

pub(super) async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> Response {
    match enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "task_id": task_id.0,
                "status": "running"
            })),
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

    let task = match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "task not found" })),
            );
        }
        Err(e) => {
            tracing::error!("cancel_task: DB lookup failed for {task_id:?}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            );
        }
    };

    if task.status.is_terminal() {
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
        assert!(result.unwrap().0.is_none());
    }

    #[tokio::test]
    async fn resolve_project_from_registry_passes_through_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let result = resolve_project_from_registry(None, Some(path.clone())).await;
        assert_eq!(result.unwrap().0, Some(path));
    }

    #[tokio::test]
    async fn resolve_project_from_registry_no_registry_passes_through_nondir() {
        // When no registry is available, non-dir paths are returned as-is
        // (downstream canonicalization handles them).
        let path = std::path::PathBuf::from("/nonexistent/path");
        let result = resolve_project_from_registry(None, Some(path.clone())).await;
        assert_eq!(result.unwrap().0, Some(path));
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
                name: None,
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
        let (path, agent) = result.unwrap();
        assert_eq!(path, Some(std::path::PathBuf::from("/home/user/my-repo")));
        assert_eq!(agent, None);
    }

    #[tokio::test]
    async fn resolve_project_from_registry_returns_default_agent_from_record() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
            .await
            .unwrap();
        registry
            .register(crate::project_registry::Project {
                id: "pinned-repo".to_string(),
                root: std::path::PathBuf::from("/home/user/pinned-repo"),
                name: None,
                max_concurrent: None,
                default_agent: Some("opus".to_string()),
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await
            .unwrap();

        let result = resolve_project_from_registry(
            Some(&registry),
            Some(std::path::PathBuf::from("pinned-repo")),
        )
        .await;
        let (path, agent) = result.unwrap();
        assert_eq!(
            path,
            Some(std::path::PathBuf::from("/home/user/pinned-repo"))
        );
        assert_eq!(agent.as_deref(), Some("opus"));
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

    // ── select_agent three-tier precedence tests ─────────────────────────────

    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem},
        config::HarnessConfig,
        types::{Capability, TokenUsage},
    };

    struct StubAgent {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl CodeAgent for StubAgent {
        fn name(&self) -> &str {
            self.name
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Ok(AgentResponse {
                output: String::new(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: 0.0,
                },
                model: self.name.to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            Ok(())
        }
    }

    fn registry_with(default: &str, names: &[&'static str]) -> AgentRegistry {
        let mut reg = AgentRegistry::new(default);
        for &n in names {
            reg.register(n, Arc::new(StubAgent { name: n }));
        }
        reg
    }

    fn req_with_agent(
        agent: Option<&str>,
        project: Option<std::path::PathBuf>,
    ) -> task_runner::CreateTaskRequest {
        let mut req = task_runner::CreateTaskRequest::default();
        req.agent = agent.map(str::to_owned);
        req.project = project;
        req.prompt = Some("do something".to_string());
        req
    }

    #[test]
    fn agent_override_wins() {
        let reg = registry_with("auto", &["opus", "claude"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "claude".to_string();

        // Build a temp project dir with no .harness/config.toml
        let dir = tempfile::tempdir().unwrap();
        let req = req_with_agent(Some("opus"), Some(dir.path().to_path_buf()));

        let agent = select_agent(&req, &reg, None).unwrap();
        assert_eq!(agent.name(), "opus");
    }

    #[test]
    fn project_default_used() {
        let reg = registry_with("auto", &["claude", "sonnet"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "auto".to_string();

        // Write a project config that sets agent.default = "claude"
        let dir = tempfile::tempdir().unwrap();
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir).unwrap();
        std::fs::write(
            harness_dir.join("config.toml"),
            "[agent]\ndefault = \"claude\"\n",
        )
        .unwrap();

        let req = req_with_agent(None, Some(dir.path().to_path_buf()));
        let agent = select_agent(&req, &reg, None).unwrap();
        assert_eq!(agent.name(), "claude");
    }

    #[test]
    fn global_fallback_when_no_project_config() {
        let reg = registry_with("sonnet", &["sonnet"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "sonnet".to_string();

        // No .harness/config.toml in the temp dir
        let dir = tempfile::tempdir().unwrap();
        let req = req_with_agent(None, Some(dir.path().to_path_buf()));

        let agent = select_agent(&req, &reg, None).unwrap();
        assert_eq!(agent.name(), "sonnet");
    }

    #[test]
    fn global_fallback_when_project_config_has_no_agent_field() {
        let reg = registry_with("sonnet", &["sonnet"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "sonnet".to_string();

        let dir = tempfile::tempdir().unwrap();
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir).unwrap();
        // Config file exists but has no [agent] section
        std::fs::write(
            harness_dir.join("config.toml"),
            "[git]\nbase_branch = \"main\"\n",
        )
        .unwrap();

        let req = req_with_agent(None, Some(dir.path().to_path_buf()));
        let agent = select_agent(&req, &reg, None).unwrap();
        assert_eq!(agent.name(), "sonnet");
    }

    // Regression for issue #2: a project that explicitly pins the same agent
    // name as the server default must still honor tier 2 (i.e. bypass tier 3
    // complexity dispatch). Previously the textual equality check
    // `if &resolved != server_default` caused tier 2 to be silently skipped,
    // allowing complex tasks to be rerouted by the complexity router.
    #[test]
    fn project_pinning_same_agent_as_server_default_bypasses_complexity_dispatch() {
        // Registry has two agents; complexity dispatch would pick "opus" for
        // complex tasks, but the project explicitly pins "claude" (same as
        // server default) — tier 2 must win.
        let reg = registry_with("claude", &["claude", "opus"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "claude".to_string();

        let dir = tempfile::tempdir().unwrap();
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir).unwrap();
        // Explicitly pin the same agent as the server default.
        std::fs::write(
            harness_dir.join("config.toml"),
            "[agent]\ndefault = \"claude\"\n",
        )
        .unwrap();

        let req = req_with_agent(None, Some(dir.path().to_path_buf()));
        let agent = select_agent(&req, &reg, None).unwrap();
        // Must be "claude" (explicit project pin), not whatever complexity
        // dispatch would have chosen.
        assert_eq!(agent.name(), "claude");
    }

    #[test]
    fn unknown_agent_in_project_config_returns_bad_request() {
        let reg = registry_with("auto", &["sonnet"]);
        let mut server = HarnessConfig::default();
        server.agents.default_agent = "auto".to_string();

        let dir = tempfile::tempdir().unwrap();
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir).unwrap();
        std::fs::write(
            harness_dir.join("config.toml"),
            "[agent]\ndefault = \"nonexistent\"\n",
        )
        .unwrap();

        let req = req_with_agent(None, Some(dir.path().to_path_buf()));
        let result = select_agent(&req, &reg, None);
        assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
    }

    // Regression for Issue 1: `[agent] default = "auto"` in .harness/config.toml
    // must NOT be treated as a literal agent name (which would 400). It is a
    // sentinel meaning "fall through to complexity dispatch (tier 3)".
    #[test]
    fn config_toml_auto_falls_through_to_complexity_dispatch() {
        let reg = registry_with("sonnet", &["sonnet"]);

        let dir = tempfile::tempdir().unwrap();
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir).unwrap();
        std::fs::write(
            harness_dir.join("config.toml"),
            "[agent]\ndefault = \"auto\"\n",
        )
        .unwrap();

        let req = req_with_agent(None, Some(dir.path().to_path_buf()));
        // "auto" must not 400; it must fall through to complexity dispatch which
        // returns the registry default ("sonnet").
        let agent = select_agent(&req, &reg, None).unwrap();
        assert_eq!(agent.name(), "sonnet");
    }

    // Regression for Issue 2: project registry default_agent (configured via
    // POST /api/projects) must be honoured when .harness/config.toml has no
    // [agent] section.
    #[test]
    fn registry_default_agent_used_when_config_toml_has_no_agent_section() {
        let reg = registry_with("sonnet", &["sonnet", "opus"]);

        // No .harness/config.toml → config.toml tier produces no agent.
        let dir = tempfile::tempdir().unwrap();
        let req = req_with_agent(None, Some(dir.path().to_path_buf()));

        // Registry record says to use "opus".
        let agent = select_agent(&req, &reg, Some("opus")).unwrap();
        assert_eq!(agent.name(), "opus");
    }

    // "auto" in registry default_agent must also fall through to complexity
    // dispatch (tier 3), not 400.
    #[test]
    fn registry_auto_agent_falls_through_to_complexity_dispatch() {
        let reg = registry_with("sonnet", &["sonnet"]);

        let dir = tempfile::tempdir().unwrap();
        let req = req_with_agent(None, Some(dir.path().to_path_buf()));

        let agent = select_agent(&req, &reg, Some("auto")).unwrap();
        assert_eq!(agent.name(), "sonnet");
    }
}
