use super::*;

/// Re-dispatch tasks that were recovered to pending after server restart.
/// These had PRs when the server crashed and need their review loop re-started.
/// Without this, recovered tasks silently hang in pending forever.
///
/// Each task is re-dispatched in a background tokio task so that permit
/// acquisition never blocks serve() - if more tasks exist than available
/// concurrency slots, the background futures will simply wait in queue.
pub(in crate::http) fn spawn_pr_recovery(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        let recovered = collect_pr_recovery_tasks(&state).await;
        if !recovered.is_empty() {
            tracing::info!(
                count = recovered.len(),
                "startup: re-dispatching recovered pending task(s) with PRs"
            );
        }
        for task in recovered {
            let state = state.clone();
            tokio::spawn(async move {
                let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "pr").await
                else {
                    return;
                };
                let pr_url = task.pr_url.as_deref().unwrap_or("");
                // Issue 4: robust parsing handles /pull/42/files and #fragment suffixes
                let pr_num = match crate::http::parse_pr_num_from_url(pr_url) {
                    Some(n) => n,
                    None => {
                        // pr_url is present but unparseable (empty, corrupted, or
                        // non-standard).  Simply returning would leave the task stuck in
                        // 'pending' forever — fail-close it instead so operators can see
                        // it and the re-dispatch filter never picks it up again.
                        tracing::error!(
                            task_id = ?task.id,
                            pr_url,
                            "startup recovery: cannot parse PR number from URL — marking task failed"
                        );
                        let bad_url = pr_url.to_owned();
                        if let Err(e) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(format!(
                                    "startup recovery: unparseable pr_url: {bad_url}"
                                ));
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {e}"
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
                };

                // Issues 2 & 3: resolve canonical project path. Prefer the
                // persisted absolute project_root (set at dispatch time); fall
                // back to registry/repo lookup only when it is absent.
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
                                            "startup recovery: registry lookup failed: {e}, using repo as path"
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

                // Issue 1: acquire permit here inside the spawned future so serve()
                // is never blocked waiting for a concurrency slot.
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

                let req = build_recovered_request(&task, canonical, None, Some(pr_num));
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
    });
}

fn workflow_state_needs_pr_recovery(state: IssueLifecycleState) -> bool {
    matches!(state, IssueLifecycleState::AddressingFeedback)
}

pub(super) fn task_is_pr_recovery_candidate(task: &task_runner::TaskState) -> bool {
    matches!(task.status, task_runner::TaskStatus::Pending) && task.pr_url.is_some()
}

pub(super) fn workflow_recovery_task_ids(
    workflows: &[IssueWorkflowInstance],
) -> std::collections::HashSet<task_runner::TaskId> {
    workflows
        .iter()
        .filter(|workflow| workflow_state_needs_pr_recovery(workflow.state))
        .filter_map(|workflow| {
            workflow
                .active_task_id
                .as_deref()
                .filter(|task_id| !is_feedback_claim_placeholder(task_id))
                .map(|task_id| harness_core::types::TaskId(task_id.to_string()))
        })
        .collect()
}

async fn collect_pr_recovery_tasks(state: &Arc<AppState>) -> Vec<task_runner::TaskState> {
    let tasks = state.core.tasks.list_all();
    let mut recovered = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(workflows) = state.core.issue_workflow_store.as_ref() {
        match workflows.list().await {
            Ok(workflows) => {
                let workflow_task_ids = workflow_recovery_task_ids(&workflows);
                for task in &tasks {
                    if workflow_task_ids.contains(&task.id) && task_is_pr_recovery_candidate(task) {
                        seen.insert(task.id.clone());
                        recovered.push(task.clone());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "startup: failed to list issue workflows for PR recovery, falling back to task-only recovery: {e}"
                );
            }
        }
    }

    for task in tasks {
        if seen.contains(&task.id) {
            continue;
        }
        if task_is_pr_recovery_candidate(&task) {
            recovered.push(task);
        }
    }

    recovered
}
