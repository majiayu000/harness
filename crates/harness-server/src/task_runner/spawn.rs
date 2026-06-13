use harness_core::agent::CodeAgent;
use harness_core::types::{Decision, Event, SessionId};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::request::{
    detect_main_worktree, summarize_request_description, CreateTaskRequest,
    PersistedRequestSettings,
};
use super::state::{RoundResult, TaskState};
use super::store::{mutate_and_persist, TaskStore};
use super::types::{TaskFailureKind, TaskId, TaskKind, TaskPhase, TaskStatus};
use super::CompletionCallback;
use crate::task_executor::SharedTurnInterceptors;

/// Patterns that indicate a transient (retryable) failure rather than a permanent one.
const TRANSIENT_PATTERNS: &[&str] = &[
    "at capacity",
    "rate limit",
    "rate_limit",
    "hit your limit",
    "429",
    "502 Bad Gateway",
    "503 Service",
    "overloaded",
    "connection reset",
    "connection refused",
    "broken pipe",
    "EOF",
    "stream idle timeout",
    "stream stall",
    "failed to fetch github",
    "timed out fetching github",
    "ECONNRESET",
    "ETIMEDOUT",
    // SQLite transient contention — SQLITE_BUSY / SQLITE_LOCKED
    "database is locked",
    "database table is locked",
    "SQLITE_BUSY",
    "SQLITE_LOCKED",
];

/// Pattern indicating the CLI account-level usage limit has been reached.
/// When detected, the global rate-limit circuit breaker is activated to
/// prevent all other tasks from wasting turns.
const ACCOUNT_LIMIT_PATTERN: &str = "hit your limit";

/// Maximum number of automatic retries for transient failures.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Return `true` when a free-text prompt is complex enough to require a Plan phase
/// before implementation.
///
/// Heuristic: prompt longer than 200 words OR contains 3 or more file-path-like
/// tokens (sequences containing `/` or ending in a recognised source extension).
pub fn prompt_requires_plan(prompt: &str) -> bool {
    let word_count = prompt.split_whitespace().count();
    if word_count > 200 {
        return true;
    }
    let file_path_count = prompt
        .split_whitespace()
        .filter(|tok| {
            // Exclude XML/HTML tags — `</foo>` contains '/' but is not a file path.
            // System prompts wrap user data in `<external_data>...</external_data>`
            // which would otherwise trigger false positives.
            let is_xml_tag = tok.starts_with('<');
            if is_xml_tag {
                return false;
            }
            tok.contains('/') || {
                let lower = tok.to_lowercase();
                lower.ends_with(".rs")
                    || lower.ends_with(".ts")
                    || lower.ends_with(".tsx")
                    || lower.ends_with(".go")
                    || lower.ends_with(".py")
                    || lower.ends_with(".toml")
                    || lower.ends_with(".json")
            }
        })
        .count();
    file_path_count >= 3
}

fn classify_task_kind(req: &CreateTaskRequest) -> TaskKind {
    req.task_kind()
}

fn refresh_preregistered_task_metadata(
    entry: &mut TaskState,
    req: &CreateTaskRequest,
    project_root: PathBuf,
    description: Option<String>,
) {
    entry.task_kind = classify_task_kind(req);
    entry.source = req.source.clone();
    entry.external_id = req.external_id.clone();
    entry.repo = req.repo.clone();
    entry.parent_id = req.parent_task_id.clone();
    entry.depends_on = req.depends_on.clone();
    entry.project_root = Some(project_root);
    entry.issue = req.issue;
    entry.description = description;
}

#[cfg(test)]
pub(super) fn is_non_decomposable_prompt_source(source: Option<&str>) -> bool {
    TaskKind::classify(source, None, None).is_non_decomposable_prompt()
}

/// Check if an error message indicates a transient failure that may succeed on retry.
pub(super) fn is_transient_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    TRANSIENT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

fn subtask_succeeded(result: &crate::parallel_dispatch::SubtaskResult) -> bool {
    result
        .response
        .as_ref()
        .is_some_and(|resp| !resp.output.trim().is_empty())
}

fn subtask_round_detail(result: &crate::parallel_dispatch::SubtaskResult) -> Option<String> {
    match (&result.response, &result.error) {
        (Some(resp), _) if !resp.output.trim().is_empty() => Some(resp.output.clone()),
        (Some(_), _) => Some("agent returned empty output".to_string()),
        (None, Some(error)) => Some(error.clone()),
        (None, None) => None,
    }
}

fn record_parallel_subtask_results(
    state: &mut TaskState,
    run_result: &crate::parallel_dispatch::ParallelRunResult,
) {
    let succeeded = run_result.results.iter().all(subtask_succeeded);
    for result in &run_result.results {
        state.rounds.push(RoundResult::new(
            (result.index as u32).saturating_add(1),
            format!("parallel_subtask_{}", result.index),
            if subtask_succeeded(result) {
                "success"
            } else {
                "failed"
            },
            subtask_round_detail(result),
            None,
            None,
        ));
    }
    if succeeded {
        state.status = TaskStatus::Done;
        state.scheduler.mark_terminal(&TaskStatus::Done);
    } else {
        let failed_count = run_result
            .results
            .iter()
            .filter(|result| !subtask_succeeded(result))
            .count();
        let total_count = run_result.results.len();
        state.status = TaskStatus::Failed;
        state.scheduler.mark_terminal(&TaskStatus::Failed);
        state.error = Some(if run_result.is_sequential {
            format!(
                "{failed_count}/{total_count} sequential subtasks failed; remaining steps were skipped"
            )
        } else {
            format!("{failed_count}/{total_count} parallel subtasks failed")
        });
    }
}

/// Extract `(owner, repo, number)` from a GitHub PR URL.
///
/// Expects format: `https://github.com/{owner}/{repo}/pull/{number}[#...]`
pub(super) fn parse_pr_url(pr_url: &str) -> Option<(String, String, u64)> {
    // Strip fragment (e.g. #discussion_r...)
    let url = pr_url.split('#').next().unwrap_or(pr_url);
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    // Expected: ["https:", "", "github.com", owner, repo, "pull", number]
    let pull_idx = parts.iter().rposition(|&p| p == "pull")?;
    if pull_idx + 1 >= parts.len() || pull_idx < 2 {
        return None;
    }
    let number: u64 = parts[pull_idx + 1].parse().ok()?;
    let repo = parts[pull_idx - 1].to_string();
    let owner = parts[pull_idx - 2].to_string();
    Some((owner, repo, number))
}

/// Convert a user-facing timeout value to a Duration.
/// `0` means "no timeout" and maps to ~277 hours (effectively unlimited).
pub fn effective_turn_timeout(secs: u64) -> tokio::time::Duration {
    if secs == 0 {
        tokio::time::Duration::from_secs(999_999)
    } else {
        tokio::time::Duration::from_secs(secs)
    }
}

fn describe_detect_main_worktree_join_error(join_err: &tokio::task::JoinError) -> String {
    if join_err.is_panic() {
        format!("detect_main_worktree panicked: {join_err}")
    } else if join_err.is_cancelled() {
        format!("detect_main_worktree was cancelled: {join_err}")
    } else {
        format!("detect_main_worktree failed: {join_err}")
    }
}

async fn resolve_project_root_with(
    requested_project: Option<PathBuf>,
    detect_worktree: impl FnOnce() -> PathBuf + Send + 'static,
) -> anyhow::Result<PathBuf> {
    match requested_project {
        Some(project) => Ok(project),
        None => tokio::task::spawn_blocking(detect_worktree)
            .await
            .map_err(|join_err| {
                let reason = describe_detect_main_worktree_join_error(&join_err);
                tracing::error!("{reason}");
                anyhow::anyhow!("{reason}")
            }),
    }
}

fn validate_spawn_project_root(
    raw_project: &Path,
    home_dir: &Path,
    allowed_project_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    if allowed_project_roots.is_empty() {
        return crate::handlers::validate_project_root(raw_project, home_dir);
    }

    let canonical = raw_project
        .canonicalize()
        .map_err(|e| format!("invalid project root '{}': {e}", raw_project.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    crate::project_registry::check_allowed_roots(&canonical, allowed_project_roots)?;
    Ok(canonical)
}

/// Resolve and canonicalize the project root so the caller can obtain a stable
/// semaphore key before acquiring the concurrency permit.
///
/// When `project` is `None` the main git worktree is detected automatically.
/// Symlinks, relative paths and the `None` sentinel all converge to the same
/// canonical `PathBuf`, preventing the same repository from landing in
/// different per-project buckets due to path aliasing.
///
/// This function only resolves symlinks — it does NOT enforce the HOME-boundary
/// restriction applied by `validate_project_root`. Full validation still
/// happens inside `spawn_task` once the task is running.
/// Returns true when `external_id` marks this as an issue- or PR-keyed task.
/// Issue/PR-keyed workspaces are reused across consecutive tasks (issue #969).
fn is_issue_pr_task(eid: Option<&str>) -> bool {
    eid.is_some_and(|id| id.starts_with("issue:") || id.starts_with("pr:"))
}

fn is_issue_pr_task_state(state: &TaskState) -> bool {
    is_issue_pr_task(state.external_id.as_deref())
        || state.issue.is_some()
        || matches!(state.task_kind, TaskKind::Issue | TaskKind::Pr)
}

fn should_remove_workspace_after_task(state: Option<&TaskState>, auto_cleanup: bool) -> bool {
    let Some(state) = state else {
        return auto_cleanup;
    };
    if !state.status.is_terminal() {
        return false;
    }
    if state.status == TaskStatus::Failed {
        return true;
    }
    auto_cleanup && !is_issue_pr_task_state(state)
}

fn should_abort_after_abort_handle_registration(state: &TaskState, handle_finished: bool) -> bool {
    state.status.is_terminal() && !handle_finished
}

pub async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = resolve_project_root_with(project, detect_main_worktree).await?;
    // Best-effort canonicalize: if the path doesn't exist yet (e.g. in tests
    // using a path that will be created later) fall back to the raw path so
    // we at least get a consistent string key.
    Ok(raw.canonicalize().unwrap_or(raw))
}

async fn log_task_failure_event(
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event).await {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

/// Record a task failure, marking the task as failed.
async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.failure_kind = Some(TaskFailureKind::Task);
        s.error = Some(reason);
        s.scheduler.mark_terminal(&TaskStatus::Failed);
    })
    .await
    {
        tracing::error!("failed to persist task failure for {task_id:?}: {e}");
    }
}

async fn record_workspace_lifecycle_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    workspace_path: PathBuf,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.failure_kind = Some(TaskFailureKind::WorkspaceLifecycle);
        s.error = Some(reason.clone());
        s.workspace_path = Some(workspace_path.clone());
        s.scheduler.mark_terminal(&TaskStatus::Failed);
        s.rounds.push(RoundResult::new(
            s.turn.saturating_add(1),
            "workspace_lifecycle",
            "missing_workspace",
            Some(reason.clone()),
            None,
            None,
        ));
    })
    .await
    {
        tracing::error!("failed to persist workspace lifecycle failure for {task_id:?}: {e}");
    }
}

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
/// Called before the new task is inserted into the store.
fn detect_cycle(store: &TaskStore, new_id: &TaskId, depends_on: &[TaskId]) -> bool {
    let mut stack: Vec<TaskId> = depends_on.to_vec();
    let mut visited: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    while let Some(dep_id) = stack.pop() {
        if dep_id == *new_id {
            return true;
        }
        if visited.contains(&dep_id) {
            continue;
        }
        visited.insert(dep_id.clone());
        if let Some(dep_state) = store.get(&dep_id) {
            for transitive in &dep_state.depends_on {
                stack.push(transitive.clone());
            }
        }
    }
    false
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: impl Into<SharedTurnInterceptors>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    allowed_project_roots: Vec<PathBuf>,
) -> TaskId {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors.into(),
        req,
        detect_main_worktree,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        workflow_runtime_store,
        allowed_project_roots,
        None,
        None,
    )
    .await
}

/// Register a task with Pending status and return its ID immediately, without waiting
/// for a concurrency permit. Pair with `spawn_preregistered_task` (called from a
/// background tokio task after `task_queue.acquire()`) to begin execution.
pub async fn register_pending_task(store: Arc<TaskStore>, req: &CreateTaskRequest) -> TaskId {
    let task_id = TaskId::new();
    let mut state = TaskState::new(task_id.clone());
    state.task_kind = classify_task_kind(req);
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.parent_id = req.parent_task_id.clone();
    state.depends_on = req.depends_on.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    // Keep queued tasks visible to duplicate detection while they wait for a permit.
    state.project_root = req.project.clone();
    state.phase = state.task_kind.default_phase();
    state.description = summarize_request_description(req);
    state.request_settings = Some(PersistedRequestSettings::from_req(req));
    store.insert(&state).await;
    // Register stream channel now so SSE clients can subscribe before execution begins.
    store.register_task_stream(&task_id);
    task_id
}

/// Begin execution for a task pre-registered via `register_pending_task`.
/// The caller must have already acquired a concurrency permit; it is held for the task's lifetime.
/// `group_permit` is an optional semaphore permit that serialises tasks within a conflict group;
/// it is held inside the innermost spawned future for the full duration of task execution.
pub async fn spawn_preregistered_task(
    task_id: TaskId,
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: impl Into<SharedTurnInterceptors>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    allowed_project_roots: Vec<PathBuf>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors.into(),
        req,
        detect_main_worktree,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        workflow_runtime_store,
        allowed_project_roots,
        Some(task_id),
        group_permit,
    )
    .await;
}

pub(super) async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: impl Into<SharedTurnInterceptors>,
    mut req: CreateTaskRequest,
    detect_worktree: F,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    allowed_project_roots: Vec<PathBuf>,
    preregistered_id: Option<TaskId>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let interceptors = interceptors.into();
    let task_id = if let Some(id) = preregistered_id {
        // Task was pre-registered (e.g. by register_pending_task for batch submission);
        // store.insert and register_task_stream were already called.
        id
    } else {
        let task_id = TaskId::new();
        let mut state = TaskState::new(task_id.clone());
        state.task_kind = classify_task_kind(&req);
        state.source = req.source.clone();
        state.external_id = req.external_id.clone();
        state.repo = req.repo.clone();
        state.priority = req.priority;
        state.project_root = req.project.clone();
        state.phase = state.task_kind.default_phase();
        state.request_settings = Some(PersistedRequestSettings::from_req(&req));
        store.insert(&state).await;
        // Register stream channel before spawning so SSE clients can subscribe immediately.
        store.register_task_stream(&task_id);
        task_id
    };

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let workspace_mgr_watcher = workspace_mgr.clone();
    let detect_worktree = Arc::new(detect_worktree);
    // Clones used to store the abort handle after the main future is spawned
    // (store and id are moved into the spawn closure).
    let store_for_abort = store.clone();
    let id_for_abort = id.clone();

    let captured_home_dir = std::env::var("HOME").ok().map(PathBuf::from);
    let handle = tokio::spawn(async move {
        // Hold both permits for the task's lifetime so that the group serialisation
        // semaphore is not released until actual execution completes (not just until
        // spawn_preregistered_task returns, which happens almost immediately).
        let _permit = permit;
        let _group_permit = group_permit;
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let home_dir = captured_home_dir
            .clone()
            .unwrap_or_else(|| raw_project.clone());
        let project_root =
            validate_spawn_project_root(&raw_project, &home_dir, &allowed_project_roots)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

        if req.repo.is_none() {
            req.repo = crate::task_executor::pr_detection::detect_repo_slug(&project_root).await;
        }
        let description = summarize_request_description(&req);
        if let Some(mut entry) = store.cache.get_mut(&id) {
            refresh_preregistered_task_metadata(
                &mut entry,
                &req,
                project_root.clone(),
                description.clone(),
            );
        }
        mutate_and_persist(&store, &id, |s| {
            s.source = req.source.clone();
            s.external_id = req.external_id.clone();
            s.repo = req.repo.clone();
            s.parent_id = req.parent_task_id.clone();
            s.depends_on = req.depends_on.clone();
            s.project_root = Some(project_root.clone());
            s.issue = req.issue;
            s.description = description.clone();
            s.run_generation = s.run_generation.saturating_add(1);
            s.failure_kind = None;
            s.workspace_path = None;
            s.workspace_owner = None;
            s.error = None;
        })
        .await?;
        let run_generation = store
            .get(&id)
            .map(|state| state.run_generation)
            .unwrap_or(1);

        // Parallel dispatch for Complex+ prompt-only tasks when workspace isolation is active.
        // Issue and PR tasks have their own structured prompt flow and are not decomposed.
        let classification = crate::complexity_router::classify(
            req.prompt.as_deref().unwrap_or_default(),
            req.issue,
            req.pr,
        );
        let is_complex = matches!(
            classification.complexity,
            harness_core::agent::TaskComplexity::Complex
                | harness_core::agent::TaskComplexity::Critical
        );
        let task_kind = classify_task_kind(&req);
        if req.issue.is_none()
            && req.pr.is_none()
            && is_complex
            && !task_kind.is_non_decomposable_prompt()
        {
            if let Some(ref wmgr) = workspace_mgr {
                let mut subtask_specs = match crate::parallel_dispatch::decompose(
                    req.prompt.as_deref().unwrap_or_default(),
                ) {
                    Ok(specs) => specs,
                    Err(msg) => {
                        tracing::warn!(task_id = %id.0, "parallel_dispatch rejected: {}", msg);
                        mutate_and_persist(&store, &id, |s| {
                            s.status = TaskStatus::Failed;
                            s.scheduler.mark_terminal(&TaskStatus::Failed);
                            s.error = Some(msg);
                        })
                        .await?;
                        return Ok(());
                    }
                };
                if subtask_specs.len() > 1 {
                    // Prepend sibling-awareness context to each subtask prompt so parallel
                    // agents know what other top-level tasks are running on the same project.
                    let siblings = store.list_siblings(&project_root, &id);
                    if !siblings.is_empty() {
                        let sibling_tasks: Vec<harness_core::prompts::SiblingTask> = siblings
                            .into_iter()
                            .filter_map(|s| {
                                s.description.and_then(|description| {
                                    if description.is_empty() {
                                        None
                                    } else {
                                        Some(harness_core::prompts::SiblingTask {
                                            issue: s.issue,
                                            description,
                                        })
                                    }
                                })
                            })
                            .collect();
                        let ctx = harness_core::prompts::sibling_task_context(&sibling_tasks);
                        subtask_specs = subtask_specs
                            .into_iter()
                            .map(|mut spec| {
                                spec.prompt = format!("{ctx}\n\n{}", spec.prompt);
                                spec
                            })
                            .collect();
                    }
                    let project_config = harness_core::config::project::load_project_config(
                        &project_root,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "failed to load project config for {}: {e}",
                            project_root.display()
                        )
                    })?;
                    let remote = project_config.git.remote.clone();
                    let base_branch = project_config.git.base_branch.clone();

                    // Register subtask IDs in the parent before dispatch.
                    let sub_ids: Vec<TaskId> = (0..subtask_specs.len())
                        .map(|i| harness_core::types::TaskId(format!("{}-p{i}", id.0)))
                        .collect();
                    mutate_and_persist(&store, &id, |s| {
                        s.subtask_ids = sub_ids.clone();
                    })
                    .await?;

                    let context_items = crate::task_executor::helpers::collect_context_items(
                        &skills,
                        &project_root,
                        req.prompt.as_deref().unwrap_or_default(),
                    )
                    .await;

                    let turn_timeout = effective_turn_timeout(req.turn_timeout_secs);
                    let run_result = crate::parallel_dispatch::run_parallel_subtasks(
                        &id,
                        agent.clone(),
                        subtask_specs,
                        wmgr.clone(),
                        &project_root,
                        &remote,
                        &base_branch,
                        context_items,
                        turn_timeout,
                    )
                    .await;

                    mutate_and_persist(&store, &id, |s| {
                        record_parallel_subtask_results(s, &run_result);
                    })
                    .await?;
                    return Ok(());
                }
            }
        }

        // Planning gate: complex prompt-only tasks must go through the Plan phase
        // before implementation to reduce drift.
        // Heuristic: prompt longer than 200 words OR containing 3+ file path tokens.
        if req.issue.is_none() && req.pr.is_none() && !task_kind.is_non_decomposable_prompt() {
            if let Some(ref prompt) = req.prompt {
                if prompt_requires_plan(prompt) {
                    mutate_and_persist(&store, &id, |s| {
                        if s.phase == TaskPhase::Implement {
                            s.phase = TaskPhase::Plan;
                        }
                    })
                    .await?;
                    tracing::info!(
                        task_id = %id.0,
                        "planning gate: complex prompt — forcing Plan phase before Implement"
                    );
                }
            }
        }

        // If workspace isolation is configured, create a per-task git worktree.
        // Save the canonical root before it may be moved into run_project.
        let canonical_project_root = project_root.clone();
        let run_project = if let Some(ref wmgr) = workspace_mgr {
            let project_config = harness_core::config::project::load_project_config(&project_root)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to load project config for {}: {e}",
                        project_root.display()
                    )
                })?;
            let lease = match wmgr
                .create_workspace(
                    &id,
                    &project_root,
                    &project_config.git.remote,
                    &project_config.git.base_branch,
                    run_generation,
                    req.external_id.as_deref(),
                    req.repo.as_deref(),
                )
                .await
            {
                Ok(lease) => lease,
                Err(err) => {
                    let error_message = err.to_string();
                    let workspace_path = err.workspace_path().to_path_buf();
                    let workspace_owner =
                        err.workspace_owner().map(std::string::ToString::to_string);
                    mutate_and_persist(&store, &id, |s| {
                        s.status = TaskStatus::Failed;
                        s.failure_kind = Some(TaskFailureKind::WorkspaceLifecycle);
                        s.error = Some(error_message.clone());
                        s.workspace_path = Some(workspace_path.clone());
                        s.workspace_owner = workspace_owner.clone();
                        s.scheduler.mark_terminal(&TaskStatus::Failed);
                        s.rounds.push(RoundResult::new(
                            s.turn.saturating_add(1),
                            "workspace_admission",
                            "workspace_lifecycle_failure",
                            Some(error_message.clone()),
                            None,
                            None,
                        ));
                    })
                    .await?;
                    tracing::warn!(task_id = %id.0, error = %error_message, "workspace admission failed");
                    return Ok(());
                }
            };
            tracing::info!(
                task_id = %id.0,
                workspace_path = %lease.workspace_path.display(),
                workspace_owner = %lease.owner_session,
                run_generation = lease.run_generation,
                workspace_project_key = %lease.project_key,
                workspace_slot_index = lease.slot_index,
                workspace_decision = ?lease.decision,
                "workspace admitted"
            );
            mutate_and_persist(&store, &id, |s| {
                s.workspace_path = Some(lease.workspace_path.clone());
                s.workspace_owner = Some(lease.owner_session.clone());
                s.run_generation = lease.run_generation;
            })
            .await?;
            lease.workspace_path
        } else {
            project_root
        };

        // Retry loop: on transient errors, back off and retry up to MAX_TRANSIENT_RETRIES.
        // Retrying inside the spawn means the stream stays open and the task actually re-runs.
        let mut transient_attempts = 0u32;
        // Track total turns used across all transient-retry attempts so the
        // max_turns budget is enforced globally over the full task lifecycle.
        let mut total_turns_used: u32 = 0;
        let task_result = loop {
            // Wait if global rate-limit circuit breaker is active (another task hit the limit).
            store.wait_for_rate_limit().await;

            if workspace_mgr.is_some() && !run_project.exists() {
                let reason = format!(
                    "WorkspaceLifecycle: workspace path missing before task turn: {}",
                    run_project.display()
                );
                record_workspace_lifecycle_failure(
                    &store,
                    &events,
                    &id,
                    run_project.clone(),
                    reason.clone(),
                )
                .await;
                tracing::warn!(task_id = %id.0, workspace_path = %run_project.display(), "workspace path missing before task turn");
                break Ok(());
            }

            let result = crate::task_executor::run_task(
                &store,
                &id,
                agent.as_ref(),
                reviewer.as_deref(),
                skills.clone(),
                events.clone(),
                interceptors.clone(),
                &req,
                run_project.clone(),
                canonical_project_root.clone(),
                server_config.as_ref(),
                issue_workflow_store.clone(),
                workflow_runtime_store.clone(),
                &mut total_turns_used,
            )
            .await;

            match result {
                ok @ Ok(()) => break ok,
                Err(ref e) if workspace_mgr.is_some() && !run_project.exists() => {
                    let reason = format!(
                        "WorkspaceLifecycle: workspace path disappeared during task execution: {} ({:#})",
                        run_project.display(),
                        e
                    );
                    record_workspace_lifecycle_failure(
                        &store,
                        &events,
                        &id,
                        run_project.clone(),
                        reason.clone(),
                    )
                    .await;
                    tracing::warn!(task_id = %id.0, workspace_path = %run_project.display(), error = %reason, "workspace path disappeared during task execution");
                    break Ok(());
                }
                Err(ref e)
                    if is_transient_error(&format!("{:#}", e))
                        && transient_attempts < MAX_TRANSIENT_RETRIES =>
                {
                    transient_attempts += 1;
                    let reason = format!("{:#}", e);

                    // Account-level limit: activate global circuit breaker (60 min pause)
                    // so all other tasks stop burning turns.
                    let backoff_secs = if reason.to_lowercase().contains(ACCOUNT_LIMIT_PATTERN) {
                        let pause = std::time::Duration::from_secs(3600);
                        store.set_rate_limit(pause).await;
                        3600u64
                    } else {
                        30u64 * (1u64 << (transient_attempts - 1).min(4))
                    };

                    tracing::warn!(
                        task_id = %id.0,
                        attempt = transient_attempts,
                        max = MAX_TRANSIENT_RETRIES,
                        backoff_secs,
                        "transient failure detected; retrying after backoff: {reason}"
                    );
                    log_task_failure_event(
                        &events,
                        &id,
                        &format!("transient (attempt {transient_attempts}): {reason}"),
                    )
                    .await;
                    if let Err(pe) = mutate_and_persist(&store, &id, |s| {
                        s.rounds.push(RoundResult::new(
                            s.turn,
                            "transient_retry",
                            format!("attempt {transient_attempts}/{MAX_TRANSIENT_RETRIES}"),
                            Some(reason.clone()),
                            None,
                            None,
                        ));
                        s.status = TaskStatus::Pending;
                        s.failure_kind = None;
                        s.scheduler.mark_retry_backoff();
                        s.error = Some(format!(
                            "retrying after transient failure (attempt {transient_attempts}): {reason}"
                        ));
                    })
                    .await
                    {
                        tracing::error!("failed to record retry for task {id:?}: {pe}");
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                }
                err @ Err(_) => break err,
            }
        };

        let task_result: anyhow::Result<()> = match task_result {
            Ok(()) => Ok(()),
            Err(err) => {
                record_task_failure(&store, &events, &id, format!("{:#}", err)).await;
                Ok(())
            }
        };

        // Cleanup workspace when task ends.
        // Cleanup is based on the persisted task status, not the raw executor
        // result, so waiting/review/validation workspaces cannot be removed
        // before the task has reached a true terminal state.
        if let Some(wmgr) = workspace_mgr {
            let final_state = store.get(&id);
            if should_remove_workspace_after_task(final_state.as_ref(), wmgr.config.auto_cleanup) {
                if let Err(e) = wmgr.remove_workspace(&id).await {
                    tracing::warn!("workspace cleanup failed for {id:?}: {e}");
                }
            } else {
                wmgr.release_workspace(&id).await;
            }
        }

        task_result
    });

    // Store abort handle before the watcher consumes the JoinHandle, so the
    // cancel endpoint can abort the task's Tokio future (which also kills the
    // child process via kill_on_drop(true)).
    store_for_abort.store_abort_handle(&id_for_abort, handle.abort_handle());

    // Race fix: a cancel request that landed between the queue-side terminal-state
    // gate (services/execution.rs) and `store_abort_handle` above would have
    // persisted Cancelled but found no abort handle, so its `abort_task` was a
    // no-op. Now that the handle is registered, re-read the status; if any writer
    // has already moved the task to a terminal state and the future is still
    // running, abort the just-spawned future before it can invoke the agent.
    // handle.abort() is idempotent so a concurrent abort_task call from the
    // cancel route is safe.
    if let Some(state) = store_for_abort.get(&id_for_abort) {
        if should_abort_after_abort_handle_registration(&state, handle.is_finished()) {
            tracing::info!(
                task_id = %id_for_abort.0,
                status = state.status.as_ref(),
                "aborting freshly-spawned task: status reached terminal between queue gate and abort-handle registration"
            );
            handle.abort();
        }
    }

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, e.to_string())
                    .await;
            }
            Err(join_err) if join_err.is_cancelled() => {
                // abort() was called by the cancel endpoint; status already set to
                // Cancelled before abort() was called — do not overwrite with Failed.
                tracing::info!("task {id_watcher:?} cancelled via abort");
                if let Some(wmgr) = workspace_mgr_watcher.as_ref() {
                    let final_state = store_watcher.get(&id_watcher);
                    if should_remove_workspace_after_task(
                        final_state.as_ref(),
                        wmgr.config.auto_cleanup,
                    ) {
                        if let Err(e) = wmgr.remove_workspace_family(&id_watcher).await {
                            tracing::warn!("workspace cleanup failed for {id_watcher:?}: {e}");
                        }
                    } else {
                        wmgr.release_workspace_family(&id_watcher).await;
                    }
                }
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked: {join_err}");
                let reason = format!("task failed unexpectedly: {join_err}");
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, reason).await;
            }
        }
        store_watcher.remove_abort_handle(&id_watcher);
        // Close the stream channel so SSE clients receive EOF.
        store_watcher.close_task_stream(&id_watcher);
        if let Some(cb) = completion_callback {
            match store_watcher.get(&id_watcher) {
                Some(final_state) => cb(final_state).await,
                None => tracing::warn!(
                    "completion_callback: task {:?} not found in store after completion",
                    id_watcher
                ),
            }
        }
    });

    task_id
}

/// Create a task that waits for its dependencies before starting.
///
/// If all deps are already Done, registers as Pending immediately.
/// If deps are unresolved, registers as AwaitingDeps.
/// Returns Err if a circular dependency is detected.
pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    let depends_on = req.depends_on.clone();
    let task_id = TaskId::new();

    if detect_cycle(&store, &task_id, &depends_on) {
        anyhow::bail!("circular dependency detected for task {}", task_id.0);
    }

    let mut all_done = true;
    for dep_id in &depends_on {
        if !matches!(store.dep_status(dep_id).await, Some(TaskStatus::Done)) {
            all_done = false;
            break;
        }
    }

    let mut state = TaskState::new(task_id.clone());
    state.task_kind = classify_task_kind(&req);
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.phase = state.task_kind.default_phase();
    state.description = summarize_request_description(&req);
    state.request_settings = Some(PersistedRequestSettings::from_req(&req));
    // Persist the caller's resolved project root so that duplicate detection
    // (which keys on project_root + external_id) and the dep-watcher's project
    // path resolution both work correctly for waiting tasks.
    state.project_root = req.project.clone();
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
        state.scheduler = crate::task_runner::TaskSchedulerState::awaiting_dependencies();
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
/// Returns the IDs of tasks that were transitioned to Pending.
///
/// Async because dependency status checks fall back to the database for
/// terminal tasks (Done/Failed) that were evicted from the startup cache.
///
/// Returns `(ready_ids, newly_failed_ids)`.  Both sets must be persisted by
/// the caller so that status transitions survive a process restart.
pub async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
    let mut ready = Vec::new();
    // (task_id, dep_id, dep_terminal_status_label) — label is "failed" or "cancelled".
    let mut failed_deps: Vec<(TaskId, TaskId, &'static str)> = Vec::new();

    // Snapshot AwaitingDeps tasks from cache before async work.
    let awaiting: Vec<(TaskId, Vec<TaskId>)> = store
        .cache
        .iter()
        .filter_map(|e| {
            let task = e.value();
            if matches!(task.status, TaskStatus::AwaitingDeps) {
                Some((task.id.clone(), task.depends_on.clone()))
            } else {
                None
            }
        })
        .collect();

    for (task_id, depends_on) in awaiting {
        // Single pass: one DB lookup per dep (cache-first, then lightweight
        // `SELECT status` — no `rounds` JSON decode).
        let mut failed_dep_id: Option<(TaskId, &'static str)> = None;
        let mut all_done = true;
        for dep_id in &depends_on {
            match store.dep_status(dep_id).await {
                Some(TaskStatus::Failed) => {
                    failed_dep_id = Some((dep_id.clone(), "failed"));
                    break; // fail-fast — no need to inspect remaining deps
                }
                Some(TaskStatus::Cancelled) => {
                    // A cancelled dependency hard-fails its dependents. When a
                    // task is re-queued it always receives a new TaskId, so the
                    // original (now-cancelled) TaskId in `depends_on` can never
                    // transition to Done. Leaving the dependent in AwaitingDeps
                    // would block it indefinitely; failing it immediately lets
                    // the caller re-submit with correct dependency IDs.
                    failed_dep_id = Some((dep_id.clone(), "cancelled"));
                    break; // fail-fast — no need to inspect remaining deps
                }
                Some(TaskStatus::Done) => {} // this dep is satisfied
                _ => {
                    // Pending, Implementing, or unknown — not ready yet.
                    // Keep scanning in case a later dep is Failed.
                    all_done = false;
                }
            }
        }
        if let Some((fd, label)) = failed_dep_id {
            failed_deps.push((task_id, fd, label));
            continue;
        }
        if all_done {
            ready.push(task_id);
        }
    }

    let mut newly_failed: Vec<TaskId> = Vec::new();
    for (task_id, failed_dep_id, label) in &failed_deps {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            // Only overwrite if the task is still AwaitingDeps — a concurrent
            // cancel or status transition must not be clobbered.
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Failed;
                entry.scheduler.mark_terminal(&TaskStatus::Failed);
                entry.error = Some(format!("dependency {} {}", failed_dep_id.0, label));
                newly_failed.push(task_id.clone());
            }
        }
    }
    // Only return IDs where the transition actually happened.  If a task was
    // cancelled or otherwise moved out of AwaitingDeps between the snapshot
    // and this guard, it must NOT be included — the dep-watcher would otherwise
    // launch an already-terminal or concurrently-cancelled task.
    let mut actually_ready: Vec<TaskId> = Vec::with_capacity(ready.len());
    for task_id in &ready {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Pending;
                entry.scheduler.clear_to_queued();
                actually_ready.push(task_id.clone());
            }
        }
    }

    (actually_ready, newly_failed)
}

#[cfg(test)]
#[path = "spawn_tests/mod.rs"]
mod tests;
