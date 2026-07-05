use super::*;

/// Re-dispatch review/planner tasks that were recovered into their kind-specific
/// waiting states after a restart.
pub(in crate::http) fn spawn_system_task_recovery(state: &Arc<AppState>) {
    let recovered: Vec<_> = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .filter(|task| {
            matches!(
                task.status,
                task_runner::TaskStatus::ReviewWaiting | task_runner::TaskStatus::PlannerWaiting
            )
        })
        .collect();
    if !recovered.is_empty() {
        tracing::info!(
            count = recovered.len(),
            "startup: re-dispatching recovered review/planner task(s)"
        );
        for task in recovered {
            let state = state.clone();
            tokio::spawn(async move {
                let project_path = task
                    .project_root
                    .clone()
                    .or_else(|| task.repo.as_deref().map(std::path::PathBuf::from));
                let canonical = match task_runner::resolve_canonical_project(project_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to resolve project path: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
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

                let req = build_recovered_request(&task, canonical, None, None);
                if req.prompt.is_none() {
                    let reason = format!(
                        "startup recovery: {} task has no restart-safe input metadata",
                        task.task_kind.as_ref()
                    );
                    tracing::error!(task_id = ?task.id, "{reason}");
                    if let Err(pe) =
                        task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                            s.status = task_runner::TaskStatus::Failed;
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

                let agent = match task_routes::select_agent(
                    &req,
                    &state.core.server.agent_registry,
                    None,
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
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
                        let reason =
                            format!("startup recovery: failed to resolve reviewer: {error}");
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
}
