use super::{helpers, task_routes, types::AppState};
use crate::task_runner;
use std::sync::Arc;

/// Spawn the background watcher that transitions AwaitingDeps tasks once their
/// dependencies reach terminal state.  Uses a weak reference so the loop exits
/// when AppState is dropped.
pub(crate) fn spawn_awaiting_deps_watcher(state: &Arc<AppState>) {
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

                    // Reconstruct the CreateTaskRequest from persisted task fields.
                    // Parse issue/pr numbers from the canonical external_id (e.g. "issue:42").
                    let (issue, pr) = task
                        .external_id
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
                        .unwrap_or((None, None));
                    let mut req = task_runner::CreateTaskRequest {
                        issue,
                        pr,
                        project: Some(canonical),
                        repo: task.repo.clone(),
                        source: task.source.clone(),
                        external_id: task.external_id.clone(),
                        parent_task_id: task.parent_id.clone(),
                        priority: task.priority,
                        ..Default::default()
                    };
                    // Restore execution limits and prompt from persisted settings.
                    if let Some(ref settings) = task.request_settings {
                        settings.apply_to_req(&mut req);
                    }

                    // Guard: prompt-only tasks store their prompt in memory only
                    // (#[serde(skip)]). After a server restart the prompt field is
                    // absent. If no issue/pr is present either, dispatching would
                    // call implement_from_prompt("") — a silent mis-execution.
                    // Fail the task explicitly so the caller can re-submit.
                    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
                        let reason = "dep-watcher: prompt-only task has no persisted \
                            prompt after server restart; please re-submit the task"
                            .to_string();
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
                    let (reviewer, _) = helpers::resolve_reviewer(
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
    });
}

/// Re-dispatch tasks that were recovered to pending after server restart and
/// already have a PR URL.  Each task is spawned in a background future so that
/// permit acquisition never blocks the caller.
pub(crate) fn redispatch_recovered_pr_tasks(state: &Arc<AppState>) {
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
                let pr_num = match helpers::parse_pr_num_from_url(pr_url) {
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

                // Issues 2 & 3: resolve canonical project path from repo name so that
                // (a) the correct per-project concurrency bucket is used, and
                // (b) req.project is populated so the agent runs in the right worktree.
                let project_path = match task.repo.as_deref() {
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

                let mut req = task_runner::CreateTaskRequest {
                    pr: Some(pr_num),
                    project: Some(canonical),
                    repo: task.repo.clone(),
                    source: task.source.clone(),
                    external_id: task.external_id.clone(),
                    ..Default::default()
                };
                // Restore persisted execution limits so the recovered task resumes
                // with the same budget / timeout guardrails as originally requested.
                if let Some(ref settings) = task.request_settings {
                    settings.apply_to_req(&mut req);
                }
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
                let (reviewer, _) = helpers::resolve_reviewer(
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

/// Re-dispatch tasks recovered from plan/triage checkpoints that do not yet
/// have a PR URL.  The DB query is fallible; on failure a warning is logged and
/// the function returns without aborting startup.
pub(crate) async fn redispatch_checkpoint_tasks(state: &Arc<AppState>) {
    let checkpoint_tasks = match state.core.tasks.pending_tasks_with_checkpoint().await {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::warn!(
                "startup: failed to query checkpoint tasks, \
                     skipping plan/triage redispatch: {e}"
            );
            return;
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
                // Reconstruct request type from the persisted description.
                // Description format: "issue #N" → issue task; other → prompt task.
                // PR tasks are handled by redispatch_recovered_pr_tasks and never appear here.
                let issue_num = task
                    .description
                    .as_deref()
                    .and_then(|d| d.strip_prefix("issue #"))
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<u64>().ok());

                if issue_num.is_none() {
                    // Prompt tasks store the placeholder "prompt task" in description
                    // by design — the original prompt text is never persisted for
                    // privacy.  Re-dispatching with that placeholder would execute
                    // against the wrong prompt, so mark the task failed instead to
                    // prevent the same broken recovery on every subsequent restart.
                    let reason = if task.description.as_deref() == Some("prompt task") {
                        "prompt task cannot be recovered after restart: \
                         original prompt text is not persisted"
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

                let project_path = match task.repo.as_deref() {
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

                let Some(issue) = issue_num else {
                    return;
                };
                let mut req = task_runner::CreateTaskRequest {
                    issue: Some(issue),
                    project: Some(canonical),
                    repo: task.repo.clone(),
                    source: task.source.clone(),
                    external_id: task.external_id.clone(),
                    priority: task.priority,
                    ..Default::default()
                };
                // Restore persisted execution limits and any additional prompt
                // context so the recovered task resumes with the same settings
                // and caller-supplied context as originally requested.
                if let Some(ref settings) = task.request_settings {
                    settings.apply_to_req(&mut req);
                }

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
                let (reviewer, _) = helpers::resolve_reviewer(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        task_runner::{self, TaskState, TaskStatus},
        test_helpers::{HomeGuard, HOME_LOCK},
    };
    use harness_core::types::TaskId;
    use std::sync::Arc;

    // ── redispatch_recovered_pr_tasks: unparseable pr_url → fail-close ───────

    #[tokio::test]
    async fn redispatch_recovered_pr_tasks_bad_url_marks_failed_fires_callback() {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();
        // SAFETY: HOME_LOCK is held; HomeGuard::drop restores HOME unconditionally.
        let _env = unsafe { HomeGuard::set(&fake_home) };

        // Oneshot channel used to detect callback invocation without polling.
        let (tx, rx) = tokio::sync::oneshot::channel::<TaskState>();
        let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let cb: task_runner::CompletionCallback = Arc::new(move |s| {
            let sender = tx.clone();
            Box::pin(async move {
                if let Some(ch) = sender.lock().await.take() {
                    let _ = ch.send(s);
                }
            })
        });

        let mut state = crate::test_helpers::make_test_state(dir.path())
            .await
            .unwrap();
        state.intake.completion_callback = Some(cb);
        let state = Arc::new(state);

        // Insert a Pending task with an unparseable pr_url into the cache only
        // (list_all() reads the cache, so no DB row is needed for this path).
        let mut task = TaskState::new(TaskId("t-bad-pr".to_string()));
        task.pr_url = Some("not-a-github-url".to_string());
        let task_id = task.id.clone();
        state.core.tasks.cache.insert(task_id.clone(), task);

        redispatch_recovered_pr_tasks(&state);

        // The fail-close branch fires the callback; await it with a 5-second guard.
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("completion callback must fire within 5 s for a bad-URL task")
            .expect("oneshot sender must not be dropped before firing");

        assert!(
            matches!(completed.status, TaskStatus::Failed),
            "task with unparseable pr_url must be marked Failed; got {:?}",
            completed.status
        );
        assert!(
            completed
                .error
                .as_deref()
                .unwrap_or("")
                .contains("unparseable"),
            "error must mention 'unparseable'; got: {:?}",
            completed.error
        );
    }

    // ── redispatch_checkpoint_tasks: prompt-only task → fail-close ───────────

    #[tokio::test]
    async fn redispatch_checkpoint_tasks_prompt_task_marks_failed_fires_callback() {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();
        // SAFETY: HOME_LOCK is held; HomeGuard::drop restores HOME unconditionally.
        let _env = unsafe { HomeGuard::set(&fake_home) };

        let (tx, rx) = tokio::sync::oneshot::channel::<TaskState>();
        let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
        let cb: task_runner::CompletionCallback = Arc::new(move |s| {
            let sender = tx.clone();
            Box::pin(async move {
                if let Some(ch) = sender.lock().await.take() {
                    let _ = ch.send(s);
                }
            })
        });

        let mut state = crate::test_helpers::make_test_state(dir.path())
            .await
            .unwrap();
        state.intake.completion_callback = Some(cb);
        let state = Arc::new(state);

        // Build a Pending task whose description marks it as a prompt-only task.
        // pending_tasks_with_checkpoint() JOINs tasks + task_checkpoints in the DB,
        // so both rows must be present before calling redispatch_checkpoint_tasks.
        let mut task = TaskState::new(TaskId("t-prompt".to_string()));
        task.description = Some("prompt task".to_string());
        state.core.tasks.insert_task_for_test(&task).await.unwrap();
        // A non-null triage_output is required for the JOIN filter to match.
        state
            .core
            .tasks
            .write_checkpoint(&task.id, Some("triage"), None, None, "triage")
            .await
            .unwrap();

        redispatch_checkpoint_tasks(&state).await;

        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("completion callback must fire within 5 s for a prompt task")
            .expect("oneshot sender must not be dropped before firing");

        assert!(
            matches!(completed.status, TaskStatus::Failed),
            "prompt-only checkpoint task must be marked Failed; got {:?}",
            completed.status
        );
        assert!(
            completed.error.as_deref().unwrap_or("").contains("prompt"),
            "error must mention prompt recovery failure; got: {:?}",
            completed.error
        );
    }
}
