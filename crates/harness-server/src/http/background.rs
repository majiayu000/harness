use std::sync::Arc;

use super::{state::AppState, task_routes};
use crate::task_runner;

fn parse_issue_pr(task: &task_runner::TaskState) -> (Option<u64>, Option<u64>) {
    task.external_id
        .as_deref()
        .map(|eid| {
            if let Some(n) = eid.strip_prefix("issue:") {
                (n.parse::<u64>().ok(), None)
            } else if let Some(n) = eid.strip_prefix("pr:") {
                (None, n.parse::<u64>().ok())
            } else {
                (None, None)
            }
        })
        .unwrap_or((None, None))
}

fn build_recovered_request(
    task: &task_runner::TaskState,
    canonical: std::path::PathBuf,
    issue: Option<u64>,
    pr: Option<u64>,
) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest {
        issue,
        pr,
        project: Some(canonical),
        repo: task.repo.clone(),
        source: task.source.clone(),
        external_id: task.external_id.clone(),
        parent_task_id: task.parent_id.clone(),
        priority: task.priority,
        system_input: task.system_input.clone(),
        ..Default::default()
    };
    if let Some(ref settings) = task.request_settings {
        settings.apply_to_req(&mut req);
    }
    if req.prompt.is_none() {
        if let Some(system_input) = req.system_input.as_ref() {
            req.prompt = Some(system_input.prompt().to_string());
        }
    }
    req
}

pub(super) fn recovery_queue_domain(task_kind: task_runner::TaskKind) -> task_routes::QueueDomain {
    match task_kind {
        task_runner::TaskKind::Review => task_routes::QueueDomain::Review,
        task_runner::TaskKind::Issue
        | task_runner::TaskKind::Pr
        | task_runner::TaskKind::Prompt
        | task_runner::TaskKind::Planner => task_routes::QueueDomain::Primary,
    }
}

/// Spawn background watcher for AwaitingDeps tasks.
/// Uses Weak<AppState> to avoid a reference cycle; the loop exits when AppState is dropped.
pub(super) fn spawn_awaiting_deps_watcher(state: &Arc<AppState>) {
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
                    let (reviewer, _) = super::resolve_reviewer(
                        &state.core.server.agent_registry,
                        &state.core.server.config.agents.review,
                        agent.name(),
                    );
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
                        None,
                    )
                    .await;
                });
            }
        }
    });
}

/// Spawn a background sweeper that turns `pr_open` / `awaiting_feedback`
/// issue workflows into `pr:N` review tasks.
///
/// This reuses the existing PR review loop instead of adding another GitHub API
/// interpretation layer in the server. The workflow store decides *which* PRs
/// need attention; the existing `pr:N` task path decides *what* to do.
pub(super) fn spawn_issue_workflow_feedback_sweeper(state: &Arc<AppState>) {
    if state.core.issue_workflow_store.is_none() {
        tracing::debug!("workflow feedback sweeper disabled: issue workflow store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(s) => s,
                None => break,
            };
            let workflow_cfg =
                harness_core::config::workflow::load_workflow_config(&state.core.project_root)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            "workflow feedback sweeper: failed to load WORKFLOW.md, using default config: {e}"
                        );
                        harness_core::config::workflow::WorkflowConfig::default()
                    });
            let interval =
                std::time::Duration::from_secs(workflow_cfg.pr_feedback.sweep_interval_secs);
            tokio::time::sleep(interval).await;

            if !workflow_cfg.pr_feedback.enabled {
                tracing::debug!("workflow feedback sweeper: disabled by WORKFLOW.md");
                continue;
            }

            let Some(issue_workflows) = state.core.issue_workflow_store.as_ref() else {
                continue;
            };

            let candidates = match issue_workflows.list_feedback_candidates().await {
                Ok(candidates) => candidates,
                Err(e) => {
                    tracing::warn!("workflow feedback sweep: failed to list candidates: {e}");
                    continue;
                }
            };
            if candidates.is_empty() {
                continue;
            }

            let mut touched_projects = std::collections::HashSet::new();
            let mut incomplete_projects = std::collections::HashSet::new();
            for workflow in candidates {
                let Some(pr_number) = workflow.pr_number else {
                    continue;
                };
                let project_key = (workflow.project_id.clone(), workflow.repo.clone());
                if touched_projects.insert(project_key.clone()) {
                    if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                        if let Err(e) = project_store
                            .record_feedback_sweep_started(
                                &workflow.project_id,
                                workflow.repo.as_deref(),
                            )
                            .await
                        {
                            tracing::warn!(
                                project_id = %workflow.project_id,
                                "workflow feedback sweep: failed to mark project sweep start: {e}"
                            );
                        }
                    }
                }

                let req = crate::task_runner::CreateTaskRequest {
                    pr: Some(pr_number),
                    project: Some(std::path::PathBuf::from(&workflow.project_id)),
                    repo: workflow.repo.clone(),
                    source: Some("workflow_feedback".to_string()),
                    ..Default::default()
                };

                match task_routes::enqueue_task_background(state.clone(), req, None).await {
                    Ok(task_id) => {
                        tracing::info!(
                            project_id = %workflow.project_id,
                            pr = pr_number,
                            task_id = %task_id.0,
                            "workflow feedback sweep: PR task enqueued"
                        );
                    }
                    Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow {
                        retry_after_secs,
                    }) => {
                        incomplete_projects.insert(project_key.clone());
                        if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                            let _ = project_store
                                .record_paused(
                                    &workflow.project_id,
                                    workflow.repo.as_deref(),
                                    &format!(
                                        "feedback sweep paused by maintenance window; retry after {retry_after_secs}s"
                                    ),
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        incomplete_projects.insert(project_key.clone());
                        tracing::warn!(
                            project_id = %workflow.project_id,
                            pr = pr_number,
                            "workflow feedback sweep: failed to enqueue PR task: {e}"
                        );
                        if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                            let _ = project_store
                                .record_degraded(
                                    &workflow.project_id,
                                    workflow.repo.as_deref(),
                                    &format!(
                                        "feedback sweep enqueue failed for pr:{pr_number}: {e}"
                                    ),
                                )
                                .await;
                        }
                    }
                }
            }

            if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                for (project_id, repo) in touched_projects {
                    if incomplete_projects.contains(&(project_id.clone(), repo.clone())) {
                        continue;
                    }
                    if let Err(e) = project_store
                        .record_feedback_sweep_completed(&project_id, repo.as_deref())
                        .await
                    {
                        tracing::warn!(
                            project_id = %project_id,
                            "workflow feedback sweep: failed to mark project sweep completion: {e}"
                        );
                    }
                }
            }
        }
    });
}

/// Re-dispatch tasks that were recovered to pending after server restart.
/// These had PRs when the server crashed and need their review loop re-started.
/// Without this, recovered tasks silently hang in pending forever.
///
/// Each task is re-dispatched in a background tokio task so that permit
/// acquisition never blocks serve() — if more tasks exist than available
/// concurrency slots, the background futures will simply wait in queue.
pub(super) fn spawn_pr_recovery(state: &Arc<AppState>) {
    let recovered: Vec<_> = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .filter(|t| matches!(t.status, task_runner::TaskStatus::Pending) && t.pr_url.is_some())
        .collect();
    if !recovered.is_empty() {
        tracing::info!(
            count = recovered.len(),
            "startup: re-dispatching recovered pending task(s) with PRs"
        );
        for task in recovered {
            let state = state.clone();
            tokio::spawn(async move {
                let pr_url = task.pr_url.as_deref().unwrap_or("");
                // Issue 4: robust parsing handles /pull/42/files and #fragment suffixes
                let pr_num = match super::parse_pr_num_from_url(pr_url) {
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
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
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
                    None,
                )
                .await;
            });
        }
    }
}

/// Re-dispatch review/planner tasks that were recovered into their kind-specific
/// waiting states after a restart.
pub(super) fn spawn_system_task_recovery(state: &Arc<AppState>) {
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
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
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
                    None,
                )
                .await;
            });
        }
    }
}

/// Re-dispatch tasks recovered from plan/triage checkpoints but without a PR.
/// Source A (spawn_pr_recovery) handles tasks with `pr_url` set. Source B (this
/// function) picks up remaining pending tasks that have a plan or triage
/// checkpoint — these are issue/prompt tasks interrupted during planning that
/// need to continue execution.
/// If the DB query fails, log a warning and skip (startup must not abort).
pub(super) async fn spawn_checkpoint_recovery(state: &Arc<AppState>) {
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
                // Reconstruct request type using the persisted task kind.
                let issue_num = match task.task_kind {
                    task_runner::TaskKind::Issue => task
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
                        }),
                    task_runner::TaskKind::Prompt => None,
                    task_runner::TaskKind::Pr
                    | task_runner::TaskKind::Review
                    | task_runner::TaskKind::Planner => None,
                };

                if issue_num.is_none() {
                    let reason = if matches!(task.task_kind, task_runner::TaskKind::Prompt) {
                        "prompt task cannot be recovered after restart: original prompt text is not persisted"
                    } else {
                        "checkpoint task has no parseable issue number — skipping"
                    };
                    tracing::warn!(task_id = ?task.id, "{reason}");
                    let mut failed = task.clone();
                    failed.status = task_runner::TaskStatus::Failed;
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
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
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
                    None,
                )
                .await;
            });
        }
    }
}
