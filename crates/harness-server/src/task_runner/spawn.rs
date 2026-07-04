#[path = "spawn/classification.rs"]
mod classification;
#[path = "spawn/dependencies.rs"]
mod dependencies;
#[path = "spawn/exit_guard.rs"]
mod exit_guard;
#[path = "spawn/parallel.rs"]
mod parallel;
#[path = "spawn/project.rs"]
mod project;
#[path = "spawn/workspace_lifecycle.rs"]
mod workspace_lifecycle;

use harness_core::agent::CodeAgent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) use classification::parse_pr_url;
pub use classification::{effective_turn_timeout, prompt_requires_plan};
#[cfg(test)]
pub(super) use dependencies::detect_cycle;
pub(super) use dependencies::{check_awaiting_deps, spawn_task_awaiting_deps};
use exit_guard::ensure_terminal_or_requeued_on_spawn_exit;
#[cfg(test)]
pub(super) use exit_guard::spawn_exit_state_is_allowed;
pub(super) use parallel::record_parallel_subtask_results;
pub use project::resolve_canonical_project;
use project::{resolve_project_root_with, validate_spawn_project_root};
#[cfg(test)]
pub(super) use workspace_lifecycle::is_issue_pr_task;
use workspace_lifecycle::{
    log_task_failure_event, record_task_failure, record_workspace_lifecycle_failure,
};
pub(super) use workspace_lifecycle::{
    should_abort_after_abort_handle_registration, should_remove_workspace_after_task,
};

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
        interceptors,
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
        interceptors,
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
                        ensure_terminal_or_requeued_on_spawn_exit(
                            &store,
                            &events,
                            &id,
                            "parallel_dispatch_rejected",
                        )
                        .await;
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
                    ensure_terminal_or_requeued_on_spawn_exit(
                        &store,
                        &events,
                        &id,
                        "parallel_dispatch_completed",
                    )
                    .await;
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
                    ensure_terminal_or_requeued_on_spawn_exit(
                        &store,
                        &events,
                        &id,
                        "workspace_admission_failure",
                    )
                    .await;
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

        ensure_terminal_or_requeued_on_spawn_exit(&store, &events, &id, "spawn_task_run_loop")
            .await;

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
                // User cancellation persists Cancelled before abort(). Shutdown-style
                // aborts do not, so the exit guard terminalizes any non-terminal row.
                tracing::info!("task {id_watcher:?} cancelled via abort");
                ensure_terminal_or_requeued_on_spawn_exit(
                    &store_watcher,
                    &events_watcher,
                    &id_watcher,
                    "spawn_task_aborted",
                )
                .await;
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

#[cfg(test)]
#[path = "spawn_tests/mod.rs"]
mod tests;
