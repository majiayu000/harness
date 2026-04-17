use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use chrono::Utc;
use harness_core::{
    config::misc::RetrySchedulerConfig,
    types::{Decision, Event, EventFilters, SessionId},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

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

        // Query attempt history for this specific task.
        let attempt_hook = format!("periodic_retry:attempt:{}", task.id.0);
        let attempts = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some(attempt_hook.clone()),
                ..EventFilters::default()
            })
            .await
            .unwrap_or_default();

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
            // Emit attempt watermark event before enqueuing so that a
            // scheduler crash between the two does not cause a silent retry
            // without an event record.
            let session_id = SessionId::new();
            let mut attempt_event =
                Event::new(session_id, &attempt_hook, "RetryScheduler", Decision::Pass);
            attempt_event.reason = Some(format!(
                "attempt {}/{}",
                attempt_count + 1,
                config.max_retries
            ));
            attempt_event.detail = Some(task.id.0.clone());
            if let Err(e) = state.observability.events.log(&attempt_event).await {
                tracing::warn!(
                    task_id = %task.id.0,
                    "periodic_retry: failed to log attempt event: {e}"
                );
            }

            let issue_num = task
                .external_id
                .as_deref()
                .and_then(|eid| eid.strip_prefix("issue:"))
                .and_then(|s| s.parse::<u64>().ok());

            let req = CreateTaskRequest {
                issue: issue_num,
                repo: task.repo.clone(),
                source: Some("periodic-retry".to_string()),
                project: task.project_root.clone(),
                ..CreateTaskRequest::default()
            };

            match task_routes::enqueue_task(state, req).await {
                Ok(new_id) => {
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
                        "periodic_retry: failed to enqueue retry: {e}"
                    );
                }
            }
        } else {
            // Retry cap reached — emit stuck event and ask an agent to apply label.
            let session_id = SessionId::new();
            let stuck_hook = format!("periodic_retry:stuck:{}", task.id.0);
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
            if let Some(issue_num) = task
                .external_id
                .as_deref()
                .and_then(|eid| eid.strip_prefix("issue:"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                let prompt = format!(
                    "Add the label `harness:stuck` to issue #{issue_num}. \
                     This issue has reached the maximum automatic retry limit \
                     ({}/{}) and requires human attention.",
                    attempt_count, config.max_retries
                );
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
                        issue = issue_num,
                        "periodic_retry: failed to enqueue stuck-label task: {e}"
                    );
                }
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
    use harness_core::types::TaskId;
    use harness_observe::event_store::EventStore;

    fn stalled_task(id: &str, external_id: &str, project: &str) -> TaskState {
        TaskState {
            id: TaskId(id.to_string()),
            status: TaskStatus::Implementing,
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
            issue: None,
            repo: None,
            description: None,
            created_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
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
        sqlx::query(
            "UPDATE tasks SET updated_at = datetime('now', '-120 minutes') WHERE id = 't1'",
        )
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
                "UPDATE tasks SET updated_at = datetime('now', '-120 minutes') WHERE id = ?",
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
        sqlx::query(
            "UPDATE tasks SET updated_at = datetime('now', '-120 minutes') WHERE id = 't1'",
        )
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
                "UPDATE tasks SET updated_at = datetime('now', '-120 minutes') WHERE id = ?",
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
}
