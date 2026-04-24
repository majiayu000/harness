use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::{mutate_and_persist, CreateTaskRequest, TaskStatus};
use chrono::Utc;
use harness_core::{
    config::misc::RetrySchedulerConfig,
    types::{Decision, Event, EventFilters, SessionId},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Parse a canonical `external_id` into `(issue_num, pr_num)`.
fn parse_external_id(external_id: Option<&str>) -> (Option<u64>, Option<u64>) {
    match external_id {
        Some(eid) if eid.starts_with("issue:") => (
            eid.strip_prefix("issue:").and_then(|s| s.parse().ok()),
            None,
        ),
        Some(eid) if eid.starts_with("pr:") => {
            (None, eid.strip_prefix("pr:").and_then(|s| s.parse().ok()))
        }
        _ => (None, None),
    }
}

/// Spawn the periodic retry loop as a background task.
///
/// If retry scheduling is disabled in config the function returns immediately
/// without spawning anything; no resources are consumed.
pub fn start(state: Arc<AppState>, config: RetrySchedulerConfig) {
    if !config.enabled {
        tracing::debug!("scheduler: periodic retry disabled, retry loop not started");
        return;
    }

    tokio::spawn(async move {
        retry_loop(state, config).await;
    });
}

async fn retry_loop(state: Arc<AppState>, config: RetrySchedulerConfig) {
    let interval = Duration::from_secs(config.interval_secs);
    let stale_threshold = Duration::from_secs(config.stale_threshold_mins * 60);

    // Brief init delay to let the server fully come up before the first tick.
    sleep(Duration::from_secs(15)).await;

    loop {
        if let Err(e) = run_retry_tick(&state, &config, stale_threshold).await {
            tracing::error!("periodic_retry: tick failed: {e}");
        }
        sleep(interval).await;
    }
}

async fn run_retry_tick(
    state: &Arc<AppState>,
    config: &RetrySchedulerConfig,
    stale_threshold: Duration,
) -> anyhow::Result<()> {
    let stalled = state
        .core
        .tasks
        .list_stalled_tasks(stale_threshold, None)
        .await?;

    let cooldown = chrono::Duration::minutes(config.cooldown_mins as i64);

    let mut checked: u32 = 0;
    let mut retried: u32 = 0;
    let mut stuck: u32 = 0;
    let mut skipped: u32 = 0;

    for task in &stalled {
        checked += 1;

        // Key attempt history by project+external_id so counts persist across
        // task generations (each retry creates a new task_id) while remaining
        // isolated per repo on multi-repo servers.
        let project_prefix = task
            .project_root
            .as_ref()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        let raw_ext = task.external_id.as_deref().unwrap_or(task.id.0.as_str());
        let ext_key = if project_prefix.is_empty() {
            raw_ext.to_string()
        } else {
            format!("{project_prefix}:{raw_ext}")
        };
        let attempt_hook = format!("periodic_retry:attempt:{ext_key}");
        let attempts = match state
            .observability
            .events
            .query(&EventFilters {
                hook: Some(attempt_hook.clone()),
                ..EventFilters::default()
            })
            .await
        {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.id.0,
                    "periodic_retry: failed to query attempt history, skipping: {e}"
                );
                skipped += 1;
                continue;
            }
        };

        let attempt_count = attempts.len() as u32;
        let last_attempt_ts = attempts.iter().map(|e| e.ts).max();

        // Skip if still within the cooldown window.
        if let Some(last_ts) = last_attempt_ts {
            let elapsed = Utc::now().signed_duration_since(last_ts);
            if elapsed < cooldown {
                tracing::debug!(
                    task_id = %task.id.0,
                    elapsed_mins = elapsed.num_minutes(),
                    cooldown_mins = config.cooldown_mins,
                    "periodic_retry: within cooldown, skipping"
                );
                skipped += 1;
                continue;
            }
        }

        if attempt_count < config.max_retries {
            // Validate that the external_id is parseable before any destructive
            // operation so a legacy or malformed ID cannot burn all retry attempts
            // without ever producing a successor task.
            let (issue_num, pr_num) = parse_external_id(task.external_id.as_deref());
            if issue_num.is_none() && pr_num.is_none() {
                tracing::warn!(
                    task_id = %task.id.0,
                    external_id = ?task.external_id,
                    "periodic_retry: external_id is not parseable as issue/pr; \
                     cancelling task without consuming a retry counter"
                );
                if let Err(e) = mutate_and_persist(&state.core.tasks, &task.id, |s| {
                    s.status = TaskStatus::Cancelled;
                })
                .await
                {
                    tracing::warn!(
                        task_id = %task.id.0,
                        "periodic_retry: failed to cancel unparseable task: {e}"
                    );
                } else {
                    state.core.tasks.abort_task(&task.id);
                }
                stuck += 1;
                continue;
            }

            // Capture original status so we can restore it if enqueue fails.
            let original_status = task.status.clone();

            // Cancel the stalled task before re-enqueuing so the active-task
            // dedup in enqueue_task does not find it and return the same ID
            // instead of creating a new successor.
            if let Err(e) = mutate_and_persist(&state.core.tasks, &task.id, |s| {
                s.status = TaskStatus::Cancelled;
            })
            .await
            {
                tracing::warn!(
                    task_id = %task.id.0,
                    "periodic_retry: failed to cancel stalled task before retry: {e}; skipping"
                );
                continue;
            }
            state.core.tasks.abort_task(&task.id);

            if let (Some(wmgr), Some(project_root)) = (
                state.concurrency.workspace_mgr.as_ref(),
                task.project_root.as_ref(),
            ) {
                if let Err(e) = wmgr
                    .cleanup_workspace_for_retry(
                        &task.id,
                        project_root,
                        task.workspace_path.as_deref(),
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task.id.0,
                        "periodic_retry: workspace cleanup before retry failed: {e}; restoring task status"
                    );
                    if let Err(e2) = state
                        .core
                        .tasks
                        .restore_status_preserve_staleness(&task.id, original_status)
                        .await
                    {
                        tracing::error!(
                            task_id = %task.id.0,
                            "periodic_retry: failed to restore task status after workspace cleanup failure: {e2}"
                        );
                    }
                    skipped += 1;
                    continue;
                }
            }

            // Restore original execution limits so the retry honours the same
            // agent, budgets, timeouts, and prompt context as the original
            // request rather than silently falling back to server defaults.
            let mut req = CreateTaskRequest {
                issue: issue_num,
                pr: pr_num,
                repo: task.repo.clone(),
                // Preserve the original intake source so the completion
                // callback can route back to the correct intake (e.g. GitHub)
                // and unmark the issue from the dispatched set on failure/cancel.
                source: task
                    .source
                    .clone()
                    .or_else(|| Some("periodic-retry".to_string())),
                project: task.project_root.clone(),
                priority: task.priority,
                ..CreateTaskRequest::default()
            };
            if let Some(settings) = &task.request_settings {
                settings.apply_to_req(&mut req);
            }

            match task_routes::enqueue_task(state, req).await {
                Ok(new_id) => {
                    // Emit attempt watermark only after confirmed enqueue so that
                    // a transient enqueue failure does not consume a retry counter.
                    let session_id = SessionId::new();
                    let mut attempt_event =
                        Event::new(session_id, &attempt_hook, "RetryScheduler", Decision::Pass);
                    attempt_event.reason = Some(format!(
                        "attempt {}/{}",
                        attempt_count + 1,
                        config.max_retries
                    ));
                    attempt_event.detail = Some(new_id.to_string());
                    if let Err(e) = state.observability.events.log(&attempt_event).await {
                        tracing::warn!(
                            task_id = %task.id.0,
                            "periodic_retry: failed to log attempt event: {e}"
                        );
                    }
                    tracing::info!(
                        new_task_id = %new_id,
                        stalled_task_id = %task.id.0,
                        attempt = attempt_count + 1,
                        max = config.max_retries,
                        "periodic_retry: stalled task re-enqueued"
                    );
                    retried += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.id.0,
                        "periodic_retry: failed to enqueue retry: {e}; restoring task status"
                    );
                    // Restore the stalled task to its pre-cancel status WITHOUT
                    // updating updated_at so the next scheduler tick still sees it
                    // as stale and retries it promptly instead of waiting another
                    // full stale_threshold_mins window.
                    if let Err(e2) = state
                        .core
                        .tasks
                        .restore_status_preserve_staleness(&task.id, original_status)
                        .await
                    {
                        tracing::error!(
                            task_id = %task.id.0,
                            "periodic_retry: failed to restore task status after enqueue failure: {e2}"
                        );
                    }
                }
            }
        } else {
            // Retry cap reached.
            let stuck_hook = format!("periodic_retry:stuck:{ext_key}");

            // Once-only guard: if we already emitted a stuck event for this
            // task, skip re-escalation — prompt-only tasks have no external_id
            // so the normal dedup path would not stop duplicates.
            let prior_stuck = state
                .observability
                .events
                .query(&EventFilters {
                    hook: Some(stuck_hook.clone()),
                    ..EventFilters::default()
                })
                .await
                .unwrap_or_default();

            if !prior_stuck.is_empty() {
                tracing::debug!(
                    task_id = %task.id.0,
                    "periodic_retry: already escalated as stuck, re-attempting cancellation"
                );
                // Previous escalation may not have successfully cancelled the task.
                // Re-attempt cancellation so the task does not permanently occupy a
                // stalled-scan slot (LIMIT 100) and block progress for other tasks.
                if let Err(e) = mutate_and_persist(&state.core.tasks, &task.id, |s| {
                    s.status = TaskStatus::Cancelled;
                })
                .await
                {
                    tracing::warn!(
                        task_id = %task.id.0,
                        "periodic_retry: failed to cancel already-escalated stuck task: {e}"
                    );
                } else {
                    state.core.tasks.abort_task(&task.id);
                }
                stuck += 1;
                continue;
            }

            // Emit stuck event and ask an agent to apply label.
            let session_id = SessionId::new();
            let mut stuck_event =
                Event::new(session_id, &stuck_hook, "RetryScheduler", Decision::Warn);
            stuck_event.reason = Some(format!(
                "retry cap reached ({}/{})",
                attempt_count, config.max_retries
            ));
            stuck_event.detail = Some(task.id.0.clone());
            if let Err(e) = state.observability.events.log(&stuck_event).await {
                tracing::warn!(
                    task_id = %task.id.0,
                    "periodic_retry: failed to log stuck event: {e}"
                );
            }

            // Enqueue an agent task to apply the harness:stuck label.
            // Direct gh calls are forbidden inside harness crates (CLAUDE.md).
            let (issue_num, pr_num) = parse_external_id(task.external_id.as_deref());
            let stuck_prompt = match (issue_num, pr_num) {
                (Some(n), _) => Some(format!(
                    "Add the label `harness:stuck` to issue #{n}. \
                     This issue has reached the maximum automatic retry limit \
                     ({}/{}) and requires human attention.",
                    attempt_count, config.max_retries
                )),
                (_, Some(n)) => Some(format!(
                    "Add the label `harness:stuck` to PR #{n}. \
                     This pull request has reached the maximum automatic retry limit \
                     ({}/{}) and requires human attention.",
                    attempt_count, config.max_retries
                )),
                _ => None,
            };
            if let Some(prompt) = stuck_prompt {
                let req = CreateTaskRequest {
                    prompt: Some(prompt),
                    repo: task.repo.clone(),
                    source: Some("periodic-retry-stuck".to_string()),
                    project: task.project_root.clone(),
                    ..CreateTaskRequest::default()
                };
                if let Err(e) = task_routes::enqueue_task(state, req).await {
                    tracing::warn!(
                        task_id = %task.id.0,
                        "periodic_retry: failed to enqueue stuck-label task: {e}"
                    );
                }
            }

            // Transition the exhausted task out of active status so it no
            // longer occupies a slot in list_stalled_tasks (which has a
            // LIMIT 100), preventing it from starving newer stalled tasks.
            if let Err(e) = mutate_and_persist(&state.core.tasks, &task.id, |s| {
                s.status = TaskStatus::Cancelled;
            })
            .await
            {
                tracing::warn!(
                    task_id = %task.id.0,
                    "periodic_retry: failed to cancel stuck task: {e}"
                );
            } else {
                state.core.tasks.abort_task(&task.id);
            }

            stuck += 1;
        }
    }

    state
        .observability
        .events
        .persist_retry_summary(checked, retried, stuck, skipped)
        .await;

    tracing::info!(
        checked,
        retried,
        stuck,
        skipped,
        "periodic_retry: tick complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_db::TaskDb;
    use crate::task_runner::{TaskState, TaskStatus};
    use crate::test_helpers;
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
    use harness_core::types::{Capability, TaskId, TokenUsage};
    use harness_observe::event_store::EventStore;
    use std::sync::Arc;

    struct RetryTestAgent;

    #[async_trait::async_trait]
    impl CodeAgent for RetryTestAgent {
        fn name(&self) -> &str {
            "test"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
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
                model: "test".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            Ok(())
        }
    }

    fn stalled_task(id: &str, external_id: &str, project: &str) -> TaskState {
        TaskState {
            id: TaskId(id.to_string()),
            task_kind: crate::task_runner::TaskKind::Issue,
            status: TaskStatus::Implementing,
            failure_kind: None,
            turn: 1,
            pr_url: None,
            rounds: vec![],
            error: None,
            source: None,
            external_id: Some(external_id.to_string()),
            parent_id: None,
            depends_on: vec![],
            subtask_ids: vec![],
            project_root: Some(std::path::PathBuf::from(project)),
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            issue: None,
            repo: None,
            description: None,
            created_at: None,
            updated_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
            system_input: None,
        }
    }

    #[tokio::test]
    async fn no_stalled_tasks_returns_empty() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        // Insert a Done task — should never be returned.
        let mut done = stalled_task("t1", "issue:1", "/proj");
        done.status = TaskStatus::Done;
        db.insert(&done).await?;

        let results = db.list_stalled_tasks(Duration::from_secs(1), None).await?;
        assert!(results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn stalled_task_is_detected() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let task = stalled_task("t1", "issue:42", "/proj");
        db.insert(&task).await?;
        // Force updated_at into the past so the task qualifies as stalled.
        sqlx::query("UPDATE tasks SET updated_at = NOW() - INTERVAL '120 minutes' WHERE id = 't1'")
            .execute(db.pool_for_test())
            .await?;

        let results = db
            .list_stalled_tasks(Duration::from_secs(60 * 60), None)
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id.0, "t1");
        Ok(())
    }

    #[tokio::test]
    async fn terminal_tasks_excluded() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        for (id, status) in [
            ("t1", TaskStatus::Done),
            ("t2", TaskStatus::Failed),
            ("t3", TaskStatus::Cancelled),
        ] {
            let mut t = stalled_task(id, "issue:1", "/proj");
            t.status = status;
            db.insert(&t).await?;
            sqlx::query(
                "UPDATE tasks SET updated_at = NOW() - INTERVAL '120 minutes' WHERE id = $1",
            )
            .bind(id)
            .execute(db.pool_for_test())
            .await?;
        }

        let results = db
            .list_stalled_tasks(Duration::from_secs(60 * 60), None)
            .await?;
        assert!(results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn null_external_id_skipped() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = stalled_task("t1", "issue:1", "/proj");
        task.external_id = None;
        db.insert(&task).await?;
        sqlx::query("UPDATE tasks SET updated_at = NOW() - INTERVAL '120 minutes' WHERE id = 't1'")
            .execute(db.pool_for_test())
            .await?;

        let results = db
            .list_stalled_tasks(Duration::from_secs(60 * 60), None)
            .await?;
        assert!(results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn system_review_tasks_are_excluded_from_stalled_scan() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = stalled_task("t1", "issue:1", "/proj");
        task.task_kind = crate::task_runner::TaskKind::Review;
        task.status = TaskStatus::ReviewWaiting;
        task.external_id = None;
        db.insert(&task).await?;
        sqlx::query("UPDATE tasks SET updated_at = NOW() - INTERVAL '120 minutes' WHERE id = 't1'")
            .execute(db.pool_for_test())
            .await?;

        let results = db
            .list_stalled_tasks(Duration::from_secs(60 * 60), None)
            .await?;
        assert!(results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn project_filter_scopes_results() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let t1 = stalled_task("t1", "issue:1", "/proj-a");
        let t2 = stalled_task("t2", "issue:2", "/proj-b");
        db.insert(&t1).await?;
        db.insert(&t2).await?;
        for id in ["t1", "t2"] {
            sqlx::query(
                "UPDATE tasks SET updated_at = NOW() - INTERVAL '120 minutes' WHERE id = $1",
            )
            .bind(id)
            .execute(db.pool_for_test())
            .await?;
        }

        let results = db
            .list_stalled_tasks(Duration::from_secs(60 * 60), Some("/proj-a"))
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id.0, "t1");
        Ok(())
    }

    #[tokio::test]
    async fn config_disabled_flag() {
        let config = RetrySchedulerConfig {
            enabled: false,
            ..RetrySchedulerConfig::default()
        };
        assert!(!config.enabled);
    }

    #[tokio::test]
    async fn summary_event_is_persisted() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let events = EventStore::new(tmp.path()).await?;
        events.persist_retry_summary(5, 2, 1, 2).await;

        let stored = events
            .query(&EventFilters {
                hook: Some("periodic_retry:summary".to_string()),
                ..EventFilters::default()
            })
            .await?;
        assert_eq!(stored.len(), 1);
        let detail = stored[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("\"checked\":5"));
        assert!(detail.contains("\"retried\":2"));
        assert!(detail.contains("\"stuck\":1"));
        assert!(detail.contains("\"skipped\":2"));
        Ok(())
    }

    #[tokio::test]
    async fn summary_decision_is_warn_when_stuck_nonzero() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let events = EventStore::new(tmp.path()).await?;
        events.persist_retry_summary(1, 0, 1, 0).await;

        let stored = events
            .query(&EventFilters {
                hook: Some("periodic_retry:summary".to_string()),
                ..EventFilters::default()
            })
            .await?;
        assert_eq!(stored[0].decision, Decision::Warn);
        Ok(())
    }

    #[tokio::test]
    async fn summary_decision_is_pass_when_no_stuck() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let events = EventStore::new(tmp.path()).await?;
        events.persist_retry_summary(3, 2, 0, 1).await;

        let stored = events
            .query(&EventFilters {
                hook: Some("periodic_retry:summary".to_string()),
                ..EventFilters::default()
            })
            .await?;
        assert_eq!(stored[0].decision, Decision::Pass);
        Ok(())
    }

    #[tokio::test]
    async fn retry_tick_cleans_workspace_before_reenqueue() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        if !test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = test_helpers::tempdir_in_home("harness-test-retry-ws-")?;
        let mut registry = AgentRegistry::new("test");
        registry.register("test", Arc::new(RetryTestAgent));
        let mut state = test_helpers::make_test_state_with_registry(dir.path(), registry).await?;

        let workspace_root = dir.path().join("workspaces");
        let workspace_mgr = Arc::new(crate::workspace::WorkspaceManager::new(
            harness_core::config::misc::WorkspaceConfig {
                root: workspace_root.clone(),
                ..Default::default()
            },
        )?);
        state.concurrency.workspace_mgr = Some(workspace_mgr);
        let state = Arc::new(state);

        let task_id = TaskId("retry-task".to_string());
        let workspace_path = workspace_root.join("retry-task");
        std::fs::create_dir_all(&workspace_path)?;

        let mut task = stalled_task("retry-task", "issue:42", dir.path().to_str().unwrap());
        task.id = task_id.clone();
        task.source = Some("github".to_string());
        task.workspace_path = Some(workspace_path.clone());
        task.workspace_owner = Some("old-session".to_string());
        task.run_generation = 1;
        state.core.tasks.insert(&task).await;

        let config = RetrySchedulerConfig {
            enabled: true,
            interval_secs: 60,
            stale_threshold_mins: 0,
            cooldown_mins: 0,
            max_retries: 1,
        };
        run_retry_tick(&state, &config, Duration::from_secs(0)).await?;

        assert!(
            !workspace_path.exists(),
            "stale workspace should be removed before retry enqueue"
        );
        let tasks = state.core.tasks.list_all_with_terminal().await?;
        assert!(
            tasks
                .iter()
                .any(|t| t.id == task_id && t.status == TaskStatus::Cancelled),
            "original stalled task should be cancelled before retry"
        );
        assert!(
            tasks.iter().any(|t| t.id != task_id),
            "retry tick should enqueue a successor task after cleanup even if it finishes quickly"
        );
        Ok(())
    }
}
