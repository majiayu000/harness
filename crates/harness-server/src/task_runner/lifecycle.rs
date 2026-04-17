use super::helpers::{
    detect_main_worktree, effective_turn_timeout, is_non_decomposable_prompt_source,
    is_transient_error, log_task_failure_event, prompt_requires_plan, resolve_project_root_with,
    summarize_request_description, ACCOUNT_LIMIT_PATTERN, MAX_TRANSIENT_RETRIES,
};
use super::store::TaskStore;
use super::types::{
    CompletionCallback, CreateTaskRequest, PersistedRequestSettings, RoundResult, TaskId,
    TaskPhase, TaskState, TaskStatus,
};
use harness_core::agent::CodeAgent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

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
                                first_token_latency_ms: None,
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
                            first_token_latency_ms: None,
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
#[path = "lifecycle_tests.rs"]
mod tests;
