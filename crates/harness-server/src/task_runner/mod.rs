mod scheduling;
mod store;
mod types;

pub use scheduling::{check_awaiting_deps, spawn_task_awaiting_deps};
pub use store::TaskStore;
pub(crate) use types::{
    effective_turn_timeout, fill_missing_repo_from_project, is_non_decomposable_prompt_source,
    is_transient_error, prompt_requires_plan, resolve_canonical_project, resolve_project_root_with,
    summarize_request_description,
};
pub use types::{
    CompletionCallback, CreateTaskRequest, DashboardCounts, ProjectCounts, RoundResult, TaskPhase,
    TaskState, TaskStatus, TaskSummary,
};
pub use types::{TaskId, MAX_TASK_PRIORITY};

use harness_core::agent::CodeAgent;
use harness_core::types::{Decision, Event, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use types::{
    detect_main_worktree, TaskPhase as Phase, ACCOUNT_LIMIT_PATTERN, MAX_TRANSIENT_RETRIES,
};

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
    state.priority = req.priority;
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

async fn spawn_task_with_worktree_detector<F>(
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
        // Task was pre-registered (e.g. by register_pending_task for batch submission);
        // store.insert and register_task_stream were already called.
        id
    } else {
        let task_id = TaskId::new();
        let mut state = TaskState::new(task_id.clone());
        state.source = req.source.clone();
        state.external_id = req.external_id.clone();
        state.repo = req.repo.clone();
        state.priority = req.priority;
        store.insert(&state).await;
        // Register stream channel before spawning so SSE clients can subscribe immediately.
        store.register_task_stream(&task_id);
        task_id
    };

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);
    // Clones used to store the abort handle after the main future is spawned
    // (store and id are moved into the spawn closure).
    let store_for_abort = store.clone();
    let id_for_abort = id.clone();

    let handle = tokio::spawn(async move {
        // Hold both permits for the task's lifetime so that the group serialisation
        // semaphore is not released until actual execution completes (not just until
        // spawn_preregistered_task returns, which happens almost immediately).
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
        let is_non_decomposable_source = is_non_decomposable_prompt_source(req.source.as_deref());
        if req.issue.is_none() && req.pr.is_none() && is_complex && !is_non_decomposable_source {
            if let Some(ref wmgr) = workspace_mgr {
                let mut subtask_specs = match crate::parallel_dispatch::decompose(
                    req.prompt.as_deref().unwrap_or_default(),
                ) {
                    Ok(specs) => specs,
                    Err(msg) => {
                        tracing::warn!(task_id = %id.0, "parallel_dispatch rejected: {}", msg);
                        mutate_and_persist(&store, &id, |s| {
                            s.status = TaskStatus::Failed;
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

                    // Both sequential and parallel tasks require ALL subtasks to succeed.
                    // With up to MAX_PARALLEL (8) chunks, any() would mark Done on 1/8
                    // success, silently dropping failed work.
                    let succeeded = run_result.results.iter().all(|r| r.response.is_some());
                    mutate_and_persist(&store, &id, |s| {
                        for r in &run_result.results {
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
                        if succeeded {
                            s.status = TaskStatus::Done;
                        } else {
                            let failed_count = run_result
                                .results
                                .iter()
                                .filter(|r| r.response.is_none())
                                .count();
                            let total_count = run_result.results.len();
                            s.status = TaskStatus::Failed;
                            s.error = Some(if run_result.is_sequential {
                                format!(
                                    "{}/{} sequential subtasks failed; remaining steps were skipped",
                                    failed_count, total_count
                                )
                            } else {
                                format!(
                                    "{}/{} parallel subtasks failed",
                                    failed_count, total_count
                                )
                            });
                        }
                    })
                    .await?;
                    return Ok(());
                }
            }
        }

        // Planning gate: complex prompt-only tasks must go through the Plan phase
        // before implementation to reduce drift.
        // Heuristic: prompt longer than 200 words OR containing 3+ file path tokens.
        if req.issue.is_none()
            && req.pr.is_none()
            && !is_non_decomposable_prompt_source(req.source.as_deref())
        {
            if let Some(ref prompt) = req.prompt {
                if prompt_requires_plan(prompt) {
                    mutate_and_persist(&store, &id, |s| {
                        if s.phase == Phase::Implement {
                            s.phase = Phase::Plan;
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

        // Retry loop: on transient errors, back off and retry up to MAX_TRANSIENT_RETRIES.
        // Retrying inside the spawn means the stream stays open and the task actually re-runs.
        let mut transient_attempts = 0u32;
        // Track total turns used across all transient-retry attempts so the
        // max_turns budget is enforced globally over the full task lifecycle.
        let mut total_turns_used: u32 = 0;
        let task_result = loop {
            // Wait if global rate-limit circuit breaker is active (another task hit the limit).
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
                        s.rounds.push(RoundResult {
                            turn: s.turn,
                            action: "transient_retry".into(),
                            result: format!(
                                "attempt {transient_attempts}/{MAX_TRANSIENT_RETRIES}"
                            ),
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

        // Cleanup workspace when task ends (Done or Failed) if auto_cleanup is set.
        if let Some(wmgr) = workspace_mgr {
            if wmgr.config.auto_cleanup {
                if let Err(e) = wmgr.remove_workspace(&id).await {
                    tracing::warn!("workspace cleanup failed for {id:?}: {e}");
                }
            }
        }

        task_result
    });

    // Store abort handle before the watcher consumes the JoinHandle, so the
    // cancel endpoint can abort the task's Tokio future (which also kills the
    // child process via kill_on_drop(true)).
    store_for_abort.store_abort_handle(&id_for_abort, handle.abort_handle());

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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
    use harness_core::types::{Capability, ContextItem, EventFilters, ExecutionPhase, TokenUsage};
    use tokio::time::Duration;
    use types::TASK_STREAM_CAPACITY;

    #[tokio::test]
    async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("stream-test".to_string());

        // No stream registered yet.
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe before register should return None"
        );

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

        store.publish_stream_item(
            &id,
            StreamItem::MessageDelta {
                text: "hello\n".into(),
            },
        );
        store.publish_stream_item(&id, StreamItem::Done);

        let item1 = rx.recv().await?;
        let item2 = rx.recv().await?;
        assert!(matches!(item1, StreamItem::MessageDelta { .. }));
        assert!(matches!(item2, StreamItem::Done));

        // After close_task_stream the channel sender is dropped.
        store.close_task_stream(&id);
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe after close should return None"
        );
        Ok(())
    }

    #[tokio::test]
    async fn task_stream_backpressure_drops_oldest_on_lag() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("backpressure-test".to_string());

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe should succeed after register"))?;

        // Publish more items than TASK_STREAM_CAPACITY to trigger lag.
        for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
            store.publish_stream_item(
                &id,
                StreamItem::MessageDelta {
                    text: format!("line {i}\n"),
                },
            );
        }

        // Receiver should see RecvError::Lagged on overflow.
        let result = rx.recv().await;
        assert!(
            result.is_err(),
            "expected Lagged error after overflow, got: {:?}",
            result
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_children_returns_subtasks_for_parent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let parent_id = harness_core::types::TaskId("parent-task".to_string());
        let parent = TaskState::new(parent_id.clone());
        store.insert(&parent).await;

        let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
        child1.parent_id = Some(parent_id.clone());
        store.insert(&child1).await;

        let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
        child2.parent_id = Some(parent_id.clone());
        store.insert(&child2).await;

        // Unrelated task.
        store
            .insert(&TaskState::new(harness_core::types::TaskId(
                "other".to_string(),
            )))
            .await;

        let children = store.list_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

        let no_children =
            store.list_children(&harness_core::types::TaskId("nonexistent".to_string()));
        assert!(no_children.is_empty());
        Ok(())
    }

    #[test]
    fn test_task_state_new() {
        let id = TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
        assert!(state.project_root.is_none());
        assert!(state.issue.is_none());
        assert!(state.description.is_none());
    }

    #[tokio::test]
    async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project = PathBuf::from("/repo/project");
        let other_project = PathBuf::from("/repo/other");

        let current_id = harness_core::types::TaskId("current".to_string());
        let mut current = TaskState::new(current_id.clone());
        current.project_root = Some(project.clone());
        current.status = TaskStatus::Implementing;
        store.insert(&current).await;

        // Sibling on same project in Implementing status.
        let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
        sibling1.project_root = Some(project.clone());
        sibling1.status = TaskStatus::Implementing;
        sibling1.issue = Some(77);
        sibling1.description = Some("fix unwrap in s3.rs".to_string());
        store.insert(&sibling1).await;

        // Sibling on same project in Pending status.
        let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
        sibling2.project_root = Some(project.clone());
        sibling2.status = TaskStatus::Pending;
        store.insert(&sibling2).await;

        // Task on a different project — must not appear.
        let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
        other.project_root = Some(other_project.clone());
        other.status = TaskStatus::Implementing;
        store.insert(&other).await;

        // Done task on same project — must not appear.
        let mut done = TaskState::new(harness_core::types::TaskId("done-task".to_string()));
        done.project_root = Some(project.clone());
        done.status = TaskStatus::Done;
        store.insert(&done).await;

        let siblings = store.list_siblings(&project, &current_id);
        let sibling_ids: Vec<&str> = siblings.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(
            siblings.len(),
            2,
            "expected 2 siblings, got: {sibling_ids:?}"
        );
        assert!(
            siblings.iter().all(|s| s.id != current_id),
            "current task must be excluded"
        );
        assert!(siblings
            .iter()
            .all(|s| s.project_root.as_deref() == Some(project.as_path())));

        // One sibling on `other_project`.
        let other_project_siblings = store.list_siblings(&other_project, &current_id);
        assert_eq!(other_project_siblings.len(), 1);

        Ok(())
    }

    struct CapturingAgent {
        captured: tokio::sync::Mutex<Vec<ContextItem>>,
    }

    impl CapturingAgent {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                captured: tokio::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl harness_core::agent::CodeAgent for CapturingAgent {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
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
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            // Mirror execute(): capture context on first call so tests that
            // verify skill injection work whether execute or execute_stream is called.
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut skill_store = harness_skills::store::SkillStore::new();
        skill_store.create(
            "test-skill".to_string(),
            "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
        );
        let skills = Arc::new(RwLock::new(skill_store));

        let agent = CapturingAgent::new();
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("test task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let captured = agent.captured.lock().await;
        assert!(
            !captured.is_empty(),
            "expected skills to be injected into AgentRequest.context"
        );
        assert!(
            captured
                .iter()
                .any(|item| matches!(item, ContextItem::Skill { .. })),
            "expected at least one ContextItem::Skill"
        );
        Ok(())
    }

    struct BlockingInterceptor;

    #[async_trait]
    impl harness_core::interceptor::TurnInterceptor for BlockingInterceptor {
        fn name(&self) -> &str {
            "blocking-test"
        }

        async fn pre_execute(
            &self,
            _req: &AgentRequest,
        ) -> harness_core::interceptor::InterceptResult {
            harness_core::interceptor::InterceptResult::block("test block")
        }
    }

    #[tokio::test]
    async fn blocking_interceptor_fails_task() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
            vec![Arc::new(BlockingInterceptor)];

        let req = CreateTaskRequest {
            prompt: Some("blocked task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        let task_id = spawn_task(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events,
            interceptors,
            req,
            None,
            permit,
            None,
        )
        .await;

        // Allow async task to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Blocked by interceptor"),
            "error message should mention blocked: {:?}",
            state.error
        );
        Ok(())
    }

    /// Verify that a local u32 counter correctly tracks waiting rounds without any store query.
    /// Task execution is sequential within a single tokio task, so a plain local counter suffices.
    #[test]
    fn local_waiting_counter_increments_on_each_waiting_response() {
        let max_rounds = 5u32;
        let mut waiting_count: u32 = 0;
        let mut observed: Vec<u32> = Vec::new();

        // Simulate the initial wait before the review loop.
        waiting_count += 1;
        observed.push(waiting_count);

        // Simulate inter-round waits (max_rounds - 1 additional waits).
        for _ in 1..max_rounds {
            waiting_count += 1;
            observed.push(waiting_count);
        }

        let expected: Vec<u32> = (1..=max_rounds).collect();
        assert_eq!(
            observed, expected,
            "waiting_count must increment monotonically on each waiting response"
        );
    }

    #[tokio::test]
    async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let req = CreateTaskRequest {
            prompt: Some("panic path".into()),
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        let task_id = spawn_task_with_worktree_detector(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events.clone(),
            vec![],
            req,
            || -> PathBuf {
                panic!("forced detect_main_worktree panic");
            },
            None,
            permit,
            None,
            None,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("detect_main_worktree panicked"),
            "expected panic reason in task error, got: {error}"
        );
        assert!(
            error.contains("forced detect_main_worktree panic"),
            "expected panic payload in task error, got: {error}"
        );

        let expected_detail = format!("task_id={}", task_id.0);
        let failure_events = events
            .query(&EventFilters {
                hook: Some("task_failure".to_string()),
                ..Default::default()
            })
            .await?;
        assert!(
            failure_events.iter().any(|event| {
                event.detail.as_deref() == Some(expected_detail.as_str())
                    && event
                        .reason
                        .as_deref()
                        .unwrap_or_default()
                        .contains("forced detect_main_worktree panic")
            }),
            "expected task_failure event containing panic payload, got: {:?}",
            failure_events
        );
        Ok(())
    }

    #[tokio::test]
    async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let req = CreateTaskRequest {
            issue: Some(20),
            source: Some("github".to_string()),
            external_id: Some("20".to_string()),
            repo: Some("acme/harness".to_string()),
            ..Default::default()
        };

        let task_id = register_pending_task(store.clone(), &req).await;
        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;

        assert_eq!(state.source.as_deref(), Some("github"));
        assert_eq!(state.external_id.as_deref(), Some("20"));
        assert_eq!(state.repo.as_deref(), Some("acme/harness"));
        assert_eq!(state.description.as_deref(), Some("issue #20"));
        Ok(())
    }

    /// Mock agent that records the `execution_phase` from every call and
    /// returns pre-configured responses in order.
    struct PhaseCapturingAgent {
        phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
        responses: tokio::sync::Mutex<Vec<String>>,
    }

    impl PhaseCapturingAgent {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                phases: tokio::sync::Mutex::new(Vec::new()),
                responses: tokio::sync::Mutex::new(responses),
            })
        }

        async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
            self.phases.lock().await.clone()
        }

        async fn next_response(&self) -> String {
            let mut guard = self.responses.lock().await;
            if guard.is_empty() {
                String::new()
            } else {
                guard.remove(0)
            }
        }
    }

    #[async_trait]
    impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
        fn name(&self) -> &str {
            "phase-capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            Ok(AgentResponse {
                output,
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            if !output.is_empty() {
                if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                    tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
                }
            }
            if let Err(e) = tx.send(StreamItem::Done).await {
                tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn planning_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Agent returns empty output (no PR URL) → task completes after implementation.
        let agent = PhaseCapturingAgent::new(vec![String::new()]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            !phases.is_empty(),
            "expected at least one agent call, got none"
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "initial implementation turn must use Planning phase"
        );
        Ok(())
    }

    #[tokio::test]
    async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Call 1 (execute_stream): return a PR URL to trigger the review loop.
        // Call 2 (execute): return LGTM to complete the review loop.
        let agent = PhaseCapturingAgent::new(vec![
            "PR_URL=https://github.com/owner/repo/pull/1".into(),
            "LGTM".into(),
        ]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(1),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            phases.len() >= 2,
            "expected at least 2 agent calls (implementation + review check), got {}",
            phases.len()
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "implementation turn must use Planning phase"
        );
        assert_eq!(
            phases[1],
            Some(ExecutionPhase::Execution),
            "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
        );
        Ok(())
    }

    #[test]
    fn transient_error_detection() {
        // Positive cases — should match transient patterns.
        assert!(is_transient_error(
            "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
        ));
        assert!(is_transient_error("rate limit exceeded, retry after 30s"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("502 Bad Gateway"));
        assert!(is_transient_error(
            "Agent stream stalled: no output for 300s"
        ));
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error(
            "stream idle timeout after 300s: zombie connection terminated"
        ));
        assert!(is_transient_error(
            "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
        ));

        // Negative cases — permanent errors should not match.
        assert!(!is_transient_error(
            "Task did not receive LGTM after 5 review rounds."
        ));
        assert!(!is_transient_error(
            "triage output unparseable — agent did not produce TRIAGE=<decision>"
        ));
        assert!(!is_transient_error("all parallel subtasks failed"));
        assert!(!is_transient_error(
            "task failed unexpectedly: task 102 panicked"
        ));
        assert!(!is_transient_error(
            "budget exceeded: spent $5.00, limit $3.00"
        ));
    }

    #[test]
    fn parse_pr_url_standard() {
        let Some((owner, repo, number)) =
            scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/42")
        else {
            panic!("expected Some for standard GitHub PR URL");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 42);
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let Some((owner, repo, number)) =
            scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
        else {
            panic!("expected Some for PR URL with fragment");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let Some((owner, repo, number)) =
            scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/7/")
        else {
            panic!("expected Some for PR URL with trailing slash");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_invalid_returns_none() {
        assert!(scheduling::parse_pr_url("https://github.com/acme/myrepo").is_none());
        assert!(scheduling::parse_pr_url("not-a-url").is_none());
        assert!(scheduling::parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
    }

    // --- planning gate ---

    #[test]
    fn short_prompt_does_not_require_plan() {
        let prompt = "Fix the typo in README.md";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn long_prompt_over_200_words_requires_plan() {
        let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
        let prompt = words.join(" ");
        assert!(prompt_requires_plan(&prompt));
    }

    #[test]
    fn prompt_with_three_file_paths_requires_plan() {
        let prompt =
            "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
        assert!(prompt_requires_plan(prompt));
    }

    #[test]
    fn prompt_with_two_file_paths_does_not_require_plan() {
        let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn xml_closing_tags_are_not_counted_as_file_paths() {
        // `wrap_external_data` wraps content in <external_data>...</external_data>.
        // Three `</external_data>` closing tags contain '/' but must NOT trigger
        // the planning gate — they are markup, not file paths.
        let prompt = "GC applied files:\n\
                      <external_data>test-guard.sh</external_data>\n\
                      Rationale:\n<external_data>test</external_data>\n\
                      Validation:\n<external_data>test</external_data>";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn non_decomposable_source_list_includes_periodic_and_sprint() {
        assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
        assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
        assert!(!is_non_decomposable_prompt_source(Some("github")));
        assert!(!is_non_decomposable_prompt_source(None));
    }

    #[test]
    fn task_status_semantics_are_centralized() {
        let cases = [
            (
                TaskStatus::Pending,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AwaitingDeps,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::Implementing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AgentReview,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Waiting, false, true, true, false, false, false),
            (
                TaskStatus::Reviewing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Done, true, false, false, true, false, false),
            (TaskStatus::Failed, true, false, false, false, true, false),
            (
                TaskStatus::Cancelled,
                true,
                false,
                false,
                false,
                false,
                true,
            ),
        ];

        for (status, terminal, inflight, resumable, success, failure, cancelled) in cases {
            assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
            assert_eq!(status.is_inflight(), inflight, "{status:?} inflight");
            assert_eq!(
                status.is_resumable_after_restart(),
                resumable,
                "{status:?} resumable"
            );
            assert_eq!(status.is_success(), success, "{status:?} success");
            assert_eq!(status.is_failure(), failure, "{status:?} failure");
            assert_eq!(status.is_cancelled(), cancelled, "{status:?} cancelled");
        }

        assert_eq!(
            TaskStatus::terminal_statuses(),
            &["done", "failed", "cancelled"]
        );
        assert_eq!(
            TaskStatus::resumable_statuses(),
            &["implementing", "agent_review", "waiting", "reviewing"]
        );
    }

    #[tokio::test]
    async fn count_by_project_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        assert!(store.count_for_dashboard().await.by_project.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_none_root_excluded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut task = TaskState::new(harness_core::types::TaskId("no-root".to_string()));
        task.status = TaskStatus::Done;
        // project_root stays None
        store.insert(&task).await;

        assert!(
            store.count_for_dashboard().await.by_project.is_empty(),
            "tasks with no project_root must not appear in per-project counts"
        );
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_groups_correctly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root_a = std::path::PathBuf::from("/projects/alpha");
        let root_b = std::path::PathBuf::from("/projects/beta");

        for (id, root, status) in [
            ("a1", &root_a, TaskStatus::Done),
            ("a2", &root_a, TaskStatus::Done),
            ("a3", &root_a, TaskStatus::Failed),
            ("a4", &root_a, TaskStatus::Cancelled),
            ("b1", &root_b, TaskStatus::Done),
            ("b2", &root_b, TaskStatus::Failed),
            ("b3", &root_b, TaskStatus::Failed),
            ("b4", &root_b, TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await.by_project;
        let key_a = root_a.to_string_lossy().into_owned();
        let key_b = root_b.to_string_lossy().into_owned();

        assert!(counts.contains_key(&key_a), "alpha counts missing");
        assert_eq!(counts[&key_a].done, 2, "alpha done");
        assert_eq!(counts[&key_a].failed, 1, "alpha failed");

        assert!(counts.contains_key(&key_b), "beta counts missing");
        assert_eq!(counts[&key_b].done, 1, "beta done");
        assert_eq!(counts[&key_b].failed, 2, "beta failed");
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_excludes_cancelled_from_failed_totals() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root = std::path::PathBuf::from("/projects/alpha");
        for (id, status) in [
            ("done", TaskStatus::Done),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await;
        assert_eq!(counts.global_done, 1);
        assert_eq!(counts.global_failed, 1);
        let key = root.to_string_lossy().into_owned();
        assert_eq!(counts.by_project[&key].done, 1);
        assert_eq!(counts.by_project[&key].failed, 1);
        Ok(())
    }
}
