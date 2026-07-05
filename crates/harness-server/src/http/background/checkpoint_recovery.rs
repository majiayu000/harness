use super::*;

/// Re-dispatch tasks recovered from plan/triage checkpoints but without a PR.
/// Source A (spawn_pr_recovery) handles tasks with `pr_url` set. Source B (this
/// function) picks up remaining pending tasks that have a plan or triage
/// checkpoint - these are issue/prompt tasks interrupted during planning that
/// need to continue execution.
/// If the DB query fails, log a warning and skip (startup must not abort).
pub(in crate::http) async fn spawn_checkpoint_recovery(state: &Arc<AppState>) {
    let checkpoint_tasks = match state.core.tasks.pending_tasks_with_checkpoint().await {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::warn!(
                "startup: failed to query checkpoint tasks, \
                     skipping plan/triage redispatch: {e}"
            );
            vec![]
        }
    };
    if !checkpoint_tasks.is_empty() {
        tracing::info!(
            count = checkpoint_tasks.len(),
            "startup: re-dispatching recovered pending task(s) with plan/triage checkpoints"
        );
        for (task, _checkpoint) in checkpoint_tasks {
            let state = state.clone();
            tokio::spawn(async move {
                let Some(task) =
                    await_startup_recovery_ready_task(&state, &task.id, "checkpoint").await
                else {
                    return;
                };
                // Reconstruct request type: parse issue number from the
                // authoritative external_id ("issue:<n>") first, then fall
                // back to the human-readable description for older rows.
                // PR tasks are handled by Source A and never appear here.
                let issue_num = task
                    .external_id
                    .as_deref()
                    .and_then(|eid| eid.strip_prefix("issue:"))
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| {
                        task.description
                            .as_deref()
                            .and_then(|d| d.strip_prefix("issue #"))
                            .and_then(|s| s.split_whitespace().next())
                            .and_then(|s| s.parse::<u64>().ok())
                    });

                if issue_num.is_none() {
                    let reason = if matches!(task.task_kind, task_runner::TaskKind::Prompt) {
                        "prompt task cannot be recovered after restart: original prompt text is not persisted"
                    } else {
                        "checkpoint task has no parseable issue number — skipping"
                    };
                    tracing::warn!(task_id = ?task.id, "{reason}");
                    let mut failed = task.clone();
                    failed.status = task_runner::TaskStatus::Failed;
                    failed
                        .scheduler
                        .mark_terminal(&task_runner::TaskStatus::Failed);
                    failed.error = Some(reason.to_string());
                    state.core.tasks.cache.insert(failed.id.clone(), failed);
                    if let Err(e) = state.core.tasks.persist(&task.id).await {
                        tracing::warn!(
                            task_id = ?task.id,
                            "startup recovery: failed to persist failed status: {e}"
                        );
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }

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
                        let reason =
                            format!("startup recovery: failed to resolve project path: {e}");
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

                let permit = match state
                    .concurrency
                    .task_queue
                    .acquire(&project_id, task.priority)
                    .await
                {
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

                let Some(issue) = issue_num else { return };
                let req = build_recovered_request(&task, canonical, Some(issue), None);

                // Use the three-tier select_agent() so that an explicit agent
                // pin stored in request_settings (Tier 1) and project-level
                // defaults (Tier 2a) are honoured, not bypassed by a raw
                // complexity dispatch.
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
