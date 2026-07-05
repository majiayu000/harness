use super::*;

/// Re-dispatch leftover pending tasks that have no PR URL and no checkpoint.
///
/// This recovery runs last so the PR/system/checkpoint-specific startup paths
/// can claim their own rows first. The remaining bucket represents tasks that
/// crashed before their first checkpoint was written.
pub(in crate::http) async fn spawn_orphan_pending_recovery(state: &Arc<AppState>) {
    let orphan_tasks = match state.core.tasks.pending_orphan_tasks().await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                "startup: failed to query orphan pending tasks, skipping orphan redispatch: {e}"
            );
            return;
        }
    };
    if orphan_tasks.is_empty() {
        return;
    }

    let mut recoverable = Vec::new();
    let mut failed = Vec::new();
    for task in orphan_tasks {
        let (issue, pr) = parse_issue_pr(&task);
        if issue.is_some() || pr.is_some() || task_allows_prompt_orphan_recovery(&task) {
            recoverable.push((task, issue, pr));
        } else {
            failed.push(task);
        }
    }

    tracing::info!(
        recovered = recoverable.len(),
        failed = failed.len(),
        "startup: orphan pending recovery summary"
    );

    for task in failed {
        let state = state.clone();
        tokio::spawn(async move {
            let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "orphan").await
            else {
                return;
            };
            let reason = orphan_recovery_failure_reason(&task).to_string();
            tracing::warn!(task_id = ?task.id, "{reason}");
            let task_id = task.id.clone();
            if let Err(pe) =
                task_runner::mutate_and_persist(&state.core.tasks, &task_id, move |s| {
                    s.status = task_runner::TaskStatus::Failed;
                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                    s.error = Some(reason);
                })
                .await
            {
                tracing::error!(
                    task_id = ?task.id,
                    "startup recovery: failed to persist failed status: {pe}; \
                     skipping completion callback to avoid state split"
                );
                return;
            }
            if let Some(cb) = &state.intake.completion_callback {
                if let Some(final_state) = state.core.tasks.get(&task_id) {
                    cb(final_state).await;
                }
            }
        });
    }

    for (task, issue, pr) in recoverable {
        let state = state.clone();
        tokio::spawn(async move {
            let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "orphan").await
            else {
                return;
            };
            let project_path = if let Some(root) = task.project_root.clone() {
                Some(root)
            } else {
                match task.repo.as_deref() {
                    Some(repo) => {
                        if let Some(registry) = state.core.project_registry.as_deref() {
                            match registry.resolve_path(repo).await {
                                Ok(Some(p)) => Some(p),
                                Ok(None) => Some(std::path::PathBuf::from(repo)),
                                Err(e) => {
                                    tracing::warn!(
                                        task_id = ?task.id,
                                        repo,
                                        "startup recovery: registry lookup failed: \
                                         {e}, using repo as path"
                                    );
                                    Some(std::path::PathBuf::from(repo))
                                }
                            }
                        } else {
                            Some(std::path::PathBuf::from(repo))
                        }
                    }
                    None => None,
                }
            };

            let canonical = match task_runner::resolve_canonical_project(project_path).await {
                Ok(c) => c,
                Err(e) => {
                    let reason = format!("startup recovery: failed to resolve project path: {e}");
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
                            "startup recovery: failed to persist failed status: {pe}; \
                             skipping completion callback to avoid state split"
                        );
                        return;
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }
            };
            let project_id = canonical.to_string_lossy().into_owned();

            let queue = match recovery_queue_domain(task.task_kind) {
                task_routes::QueueDomain::Primary => state.concurrency.task_queue.clone(),
                task_routes::QueueDomain::Review => state.concurrency.review_task_queue.clone(),
            };
            let permit = match queue.acquire(&project_id, task.priority).await {
                Ok(p) => p,
                Err(e) => {
                    let reason =
                        format!("startup recovery: failed to acquire concurrency permit: {e}");
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
                            "startup recovery: failed to persist failed status: {pe}; \
                             skipping completion callback to avoid state split"
                        );
                        return;
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }
            };

            let req = build_recovered_request(&task, canonical, issue, pr);
            let agent =
                match task_routes::select_agent(&req, &state.core.server.agent_registry, None) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
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
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
            let reviewer = match resolve_reviewer_for_request(&state, &req, agent.name()) {
                Ok(reviewer) => reviewer,
                Err(error) => {
                    let reason = format!("startup recovery: failed to resolve reviewer: {error}");
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
