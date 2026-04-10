pub mod helpers;
pub mod store;
pub mod types;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_spawn;

pub use self::store::*;
pub use self::types::*;

pub(crate) use helpers::fill_missing_repo_from_project;
pub(crate) use helpers::is_non_decomposable_prompt_source;
pub(crate) use helpers::prompt_requires_plan;
pub(crate) use helpers::resolve_canonical_project;

use harness_core::agent::CodeAgent;
use harness_core::types::{Decision, Event, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use helpers::{detect_main_worktree, resolve_project_root_with, summarize_request_description};

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
    "ECONNRESET",
    "ETIMEDOUT",
];

/// Pattern indicating the CLI account-level usage limit has been reached.
/// When detected, the global rate-limit circuit breaker is activated to
/// prevent all other tasks from wasting turns.
const ACCOUNT_LIMIT_PATTERN: &str = "hit your limit";

/// Maximum number of automatic retries for transient failures.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Check if an error message indicates a transient failure that may succeed on retry.
pub(crate) fn is_transient_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    TRANSIENT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
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
        s.error = Some(reason);
    })
    .await
    {
        tracing::error!("failed to persist task failure for {task_id:?}: {e}");
    }
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
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
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.parent_id = req.parent_task_id.clone();
    state.depends_on = req.depends_on.clone();
    state.issue = req.issue;
    state.description = summarize_request_description(req);
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
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
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
        Some(task_id),
        group_permit,
    )
    .await;
}

pub(crate) async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    mut req: CreateTaskRequest,
    detect_worktree: F,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    preregistered_id: Option<TaskId>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let task_id = if let Some(id) = preregistered_id {
        id
    } else {
        let task_id = TaskId::new();
        let mut state = TaskState::new(task_id.clone());
        state.source = req.source.clone();
        state.external_id = req.external_id.clone();
        state.repo = req.repo.clone();
        store.insert(&state).await;
        store.register_task_stream(&task_id);
        task_id
    };

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);
    let store_for_abort = store.clone();
    let id_for_abort = id.clone();

    let handle = tokio::spawn(async move {
        let _permit = permit;
        let _group_permit = group_permit;
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let home_dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| raw_project.clone());
        let project_root = crate::handlers::validate_project_root(&raw_project, &home_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if req.repo.is_none() {
            req.repo = crate::task_executor::pr_detection::detect_repo_slug(&project_root).await;
        }
        let description = summarize_request_description(&req);
        if let Some(mut entry) = store.cache.get_mut(&id) {
            entry.source = req.source.clone();
            entry.external_id = req.external_id.clone();
            entry.repo = req.repo.clone();
            entry.parent_id = req.parent_task_id.clone();
            entry.depends_on = req.depends_on.clone();
            entry.project_root = Some(project_root.clone());
            entry.issue = req.issue;
            entry.description = description;
        }

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
        let is_non_decomposable_source = is_non_decomposable_prompt_source(req.source.as_deref());
        if req.issue.is_none() && req.pr.is_none() && is_complex && !is_non_decomposable_source {
            if let Some(ref wmgr) = workspace_mgr {
                let mut subtask_specs =
                    crate::parallel_dispatch::decompose(req.prompt.as_deref().unwrap_or_default());
                if subtask_specs.len() > 1 {
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
                    let results = crate::parallel_dispatch::run_parallel_subtasks(
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

                    let any_success = results.iter().any(|r| r.response.is_some());
                    mutate_and_persist(&store, &id, |s| {
                        for r in &results {
                            let detail = if let Some(ref resp) = r.response {
                                if resp.output.is_empty() {
                                    None
                                } else {
                                    Some(resp.output.clone())
                                }
                            } else {
                                r.error.clone()
                            };
                            s.rounds.push(RoundResult {
                                turn: (r.index as u32).saturating_add(1),
                                action: format!("parallel_subtask_{}", r.index),
                                result: if r.response.is_some() {
                                    "success".into()
                                } else {
                                    "failed".into()
                                },
                                detail,
                            });
                        }
                        if any_success {
                            s.status = TaskStatus::Done;
                        } else {
                            s.status = TaskStatus::Failed;
                            s.error = Some("all parallel subtasks failed".to_string());
                        }
                    })
                    .await?;
                    return Ok(());
                }
            }
        }

        if req.issue.is_none()
            && req.pr.is_none()
            && !is_non_decomposable_prompt_source(req.source.as_deref())
        {
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

        let run_project = if let Some(ref wmgr) = workspace_mgr {
            let project_config = harness_core::config::project::load_project_config(&project_root)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to load project config for {}: {e}",
                        project_root.display()
                    )
                })?;
            wmgr.create_workspace(
                &id,
                &project_root,
                &project_config.git.remote,
                &project_config.git.base_branch,
            )
            .await?
        } else {
            project_root
        };

        let mut transient_attempts = 0u32;
        // Track total turns used across all transient-retry attempts so the
        // max_turns budget is enforced globally over the full task lifecycle.
        let mut total_turns_used: u32 = 0;
        let task_result = loop {
            store.wait_for_rate_limit().await;

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
                server_config.as_ref(),
                &mut total_turns_used,
            )
            .await;

            match result {
                ok @ Ok(()) => break ok,
                Err(ref e)
                    if is_transient_error(&e.to_string())
                        && transient_attempts < MAX_TRANSIENT_RETRIES =>
                {
                    transient_attempts += 1;
                    let reason = e.to_string();
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
                        s.rounds.push(RoundResult {
                            turn: s.turn,
                            action: "transient_retry".into(),
                            result: format!("attempt {transient_attempts}/{MAX_TRANSIENT_RETRIES}"),
                            detail: Some(reason.clone()),
                        });
                        s.status = TaskStatus::Pending;
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

        if let Some(wmgr) = workspace_mgr {
            if wmgr.config.auto_cleanup {
                if let Err(e) = wmgr.remove_workspace(&id).await {
                    tracing::warn!("workspace cleanup failed for {id:?}: {e}");
                }
            }
        }

        task_result
    });

    store_for_abort.store_abort_handle(&id_for_abort, handle.abort_handle());

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, e.to_string())
                    .await;
            }
            Err(join_err) if join_err.is_cancelled() => {
                tracing::info!("task {id_watcher:?} cancelled via abort");
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked: {join_err}");
                let reason = format!("task failed unexpectedly: {join_err}");
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, reason).await;
            }
        }
        store_watcher.remove_abort_handle(&id_watcher);
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

pub(crate) async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) -> anyhow::Result<()> {
    let status_str = status.as_ref().to_string();
    mutate_and_persist(store, task_id, |s| {
        s.status = status;
        s.turn = turn;
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::StatusChanged {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        status: status_str,
        turn,
    });
    Ok(())
}

/// Mutate a task in the cache then persist to SQLite.
pub(crate) async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    if let Some(mut entry) = store.cache.get_mut(id) {
        f(entry.value_mut());
    }
    store.persist(id).await
}

// --- dependency scheduling ---

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
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

/// Create a task that waits for its dependencies before starting.
pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    let depends_on = req.depends_on.clone();
    let task_id = TaskId::new();

    if detect_cycle(&store, &task_id, &depends_on) {
        anyhow::bail!("circular dependency detected for task {}", task_id.0);
    }

    let all_done = depends_on.iter().all(|dep_id| {
        store
            .get(dep_id)
            .map(|s| matches!(s.status, TaskStatus::Done))
            .unwrap_or(false)
    });

    let mut state = TaskState::new(task_id.clone());
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
/// Returns the IDs of tasks that were transitioned to Pending.
pub fn check_awaiting_deps(store: &TaskStore) -> Vec<TaskId> {
    let mut ready = Vec::new();
    let mut failed_deps: Vec<(TaskId, TaskId)> = Vec::new();

    for entry in store.cache.iter() {
        let task = entry.value();
        if !matches!(task.status, TaskStatus::AwaitingDeps) {
            continue;
        }
        let any_failed = task.depends_on.iter().any(|dep_id| {
            store
                .get(dep_id)
                .map(|s| matches!(s.status, TaskStatus::Failed))
                .unwrap_or(false)
        });
        if any_failed {
            let Some(failed_dep) = task
                .depends_on
                .iter()
                .find(|dep_id| {
                    store
                        .get(dep_id)
                        .map(|s| matches!(s.status, TaskStatus::Failed))
                        .unwrap_or(false)
                })
                .cloned()
            else {
                continue;
            };
            failed_deps.push((task.id.clone(), failed_dep));
            continue;
        }
        let all_done = task.depends_on.iter().all(|dep_id| {
            store
                .get(dep_id)
                .map(|s| matches!(s.status, TaskStatus::Done))
                .unwrap_or(false)
        });
        if all_done {
            ready.push(task.id.clone());
        }
    }

    for (task_id, failed_dep_id) in &failed_deps {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            entry.status = TaskStatus::Failed;
            entry.error = Some(format!("dependency {} failed", failed_dep_id.0));
        }
    }
    for task_id in &ready {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            entry.status = TaskStatus::Pending;
        }
    }

    ready
}
