use super::*;

/// Spawn background watcher for AwaitingDeps tasks.
/// Uses Weak<AppState> to avoid a reference cycle; the loop exits when AppState is dropped.
pub(in crate::http) fn spawn_awaiting_deps_watcher(state: &Arc<AppState>) {
    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let state = match weak_state.upgrade() {
                Some(s) => s,
                None => break,
            };
            let (ready_ids, failed_ids) =
                crate::task_runner::check_awaiting_deps(&state.core.tasks).await;
            // Persist status changes for both ready and failed tasks.
            for task_id in ready_ids.iter().chain(failed_ids.iter()) {
                if let Err(e) = state.core.tasks.persist(task_id).await {
                    tracing::warn!(
                        "dep-watcher: failed to persist {} after transition: {e}",
                        task_id.0
                    );
                }
            }
            // Invoke completion_callback for dep-failed tasks so intake
            // sources (e.g. github_issues) can unmark them from the
            // dispatched map and allow retries on the next poll cycle.
            // Without this the issue stays permanently "dispatched" and
            // the intake poller never re-queues it.
            if let Some(ref cb) = state.intake.completion_callback {
                for task_id in &failed_ids {
                    if let Some(task) = state.core.tasks.get(task_id) {
                        let cb = cb.clone();
                        tokio::spawn(async move {
                            cb(task).await;
                        });
                    } else {
                        tracing::warn!(
                            "dep-watcher: task {} not found after dep-failed transition",
                            task_id.0
                        );
                    }
                }
            }
            // Spawn an agent for each task whose deps are now satisfied.
            for task_id in ready_ids {
                let state = state.clone();
                tokio::spawn(async move {
                    let task = match state.core.tasks.get(&task_id) {
                        Some(t) => t,
                        None => {
                            tracing::warn!(
                                "dep-watcher: task {} not found after Pending transition",
                                task_id.0
                            );
                            return;
                        }
                    };
                    // Reconstruct the project path: prefer stored project_root,
                    // fall back to repo slug so the agent runs in the right worktree.
                    let project_path = task
                        .project_root
                        .clone()
                        .or_else(|| task.repo.as_deref().map(std::path::PathBuf::from));
                    let canonical = match task_runner::resolve_canonical_project(project_path).await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            let reason =
                                format!("dep-watcher: failed to resolve project path: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };
                    let project_id = canonical.to_string_lossy().into_owned();

                    let (issue, pr) = parse_issue_pr(&task);
                    let req = build_recovered_request(&task, canonical, issue, pr);

                    // Guard: prompt-only tasks store their prompt in memory only
                    // (#[serde(skip)]). After a server restart the prompt field is
                    // absent. If no issue/pr is present either, dispatching would
                    // call implement_from_prompt("") — a silent mis-execution.
                    // Fail the task explicitly so the caller can re-submit.
                    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
                        let reason = format!(
                            "dep-watcher: {} task has no restart-safe input after server restart; \
                             please re-submit the task",
                            task.task_kind.as_ref()
                        );
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "dep-watcher: failed to persist failed status: {pe}"
                            );
                        }
                        // Fire completion callback so intake sources (e.g. GitHub Issues
                        // poller) remove this task from their `dispatched` map. Without
                        // this the issue stays marked as dispatched forever and will never
                        // be re-queued, causing a silent production deadlock.
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }

                    let permit = match state
                        .concurrency
                        .task_queue
                        .acquire(&project_id, task.priority)
                        .await
                    {
                        Ok(p) => p,
                        Err(e) => {
                            let reason =
                                format!("dep-watcher: failed to acquire concurrency permit: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };

                    let agent = match task_routes::select_agent(
                        &req,
                        &state.core.server.agent_registry,
                        None,
                    ) {
                        Ok(a) => a,
                        Err(e) => {
                            let reason = format!("dep-watcher: failed to select agent: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };
                    let reviewer = match resolve_reviewer_for_request(&state, &req, agent.name()) {
                        Ok(reviewer) => reviewer,
                        Err(error) => {
                            let reason =
                                format!("dep-watcher: failed to resolve reviewer: {error}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            fail_background_dispatch_task(&state, &task.id, reason).await;
                            return;
                        }
                    };
                    state.core.tasks.register_task_stream(&task.id);
                    task_runner::spawn_preregistered_task(
                        task.id,
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
                        state.core.issue_workflow_store.clone(),
                        state.core.workflow_runtime_store.clone(),
                        state
                            .core
                            .server
                            .config
                            .server
                            .allowed_project_roots
                            .clone(),
                        None,
                    )
                    .await;
                });
            }
        }
    });
}
