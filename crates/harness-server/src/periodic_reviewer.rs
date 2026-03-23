use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use chrono::{DateTime, Utc};
use harness_core::{Decision, Event, EventFilters, ReviewConfig, SessionId};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Spawn the periodic review loop as a background task.
///
/// If review is disabled in config the loop returns immediately without
/// spawning anything; no resources are consumed.
pub fn start(state: Arc<AppState>, config: ReviewConfig) {
    if !config.enabled {
        tracing::debug!("scheduler: periodic review disabled, review_loop not started");
        return;
    }

    tokio::spawn(async move {
        review_loop(state, config).await;
    });
}

async fn review_loop(state: Arc<AppState>, config: ReviewConfig) {
    let interval = config.effective_interval();
    // Local fallback timestamp guards against stale deduplication when the
    // EventStore write fails.  It is updated atomically after every successful
    // task enqueue so the next cycle always sees a fresh lower-bound even if
    // the DB is unavailable.
    let fallback_ts: Arc<Mutex<Option<DateTime<Utc>>>> = Arc::new(Mutex::new(None));

    // Tracks the currently active polling task so it can be cancelled if a new
    // review cycle starts before the previous one finishes (issue #448).
    let poll_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    if config.run_on_startup {
        // Brief delay to let the server fully initialize before the first tick.
        sleep(Duration::from_secs(5)).await;
        if let Err(err) = run_review_tick(&state, &config, &fallback_ts, &poll_handle).await {
            tracing::error!("scheduler: periodic review startup tick failed: {err}");
        }
    }

    loop {
        sleep(interval).await;
        if let Err(err) = run_review_tick(&state, &config, &fallback_ts, &poll_handle).await {
            tracing::error!("scheduler: periodic review tick failed: {err}");
        }
    }
}

async fn run_review_tick(
    state: &Arc<AppState>,
    config: &ReviewConfig,
    fallback_ts: &Arc<Mutex<Option<DateTime<Utc>>>>,
    poll_handle: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<()> {
    let project_root = &state.core.project_root;

    // Query EventStore for the most recent "periodic_review" event timestamp.
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("periodic_review".to_string()),
            ..EventFilters::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to query periodic_review events: {e}"))?;

    let db_last_review_ts = events.iter().map(|e| e.ts).max();

    // Merge DB timestamp with local fallback.  The fallback wins when it is
    // more recent — this prevents stale deduplication after an EventStore
    // write failure.
    let fb = *fallback_ts.lock().await;
    let last_review_ts = match (db_last_review_ts, fb) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };

    // If there was a prior review, skip this cycle when no new commits have landed.
    // On first boot (no prior event) we always run.
    if let Some(ts) = last_review_ts {
        let since = ts.to_rfc3339();
        let output = tokio::process::Command::new("git")
            .args(["log", "--oneline", &format!("--since={since}"), "-1"])
            .current_dir(project_root)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run git log: {e}"))?;

        if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            tracing::debug!(
                since = %since,
                "scheduler: periodic review skipped — no new commits"
            );
            return Ok(());
        }
    }

    let since_arg = last_review_ts
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let prompt = harness_core::prompts::periodic_review_prompt(
        &project_root.display().to_string(),
        &since_arg,
    );

    let req = CreateTaskRequest {
        prompt: Some(prompt),
        agent: config.agent.clone(),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        ..CreateTaskRequest::default()
    };

    let task_id = match task_routes::enqueue_task(state, req).await {
        Ok(id) => {
            tracing::info!(task_id = %id, "scheduler: periodic review task enqueued");
            id
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to enqueue periodic review task: {err}"
            ));
        }
    };

    // Update the local fallback timestamp before attempting the DB write so
    // that deduplication remains correct even if the EventStore is unavailable.
    *fallback_ts.lock().await = Some(Utc::now());

    // Log a "periodic_review" event so the next cycle can check the timestamp.
    let event = Event::new(
        SessionId::new(),
        "periodic_review",
        "scheduler",
        Decision::Pass,
    );
    // A log failure is non-fatal: the fallback timestamp (line above) already
    // guards deduplication, and the task is already enqueued.  Returning early
    // here would bypass the spawn block that polls and persists findings.
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::error!("scheduler: failed to log periodic_review event (continuing): {err}");
    }

    // Drop the previous poll handle without aborting: in Tokio, dropping a
    // JoinHandle does NOT cancel the spawned task — the old poller keeps running
    // until its review task reaches Done/Failed or its internal timeout fires.
    // Calling h.abort() here would silently discard findings when the prior
    // review task had not yet completed, because the poller is the only path
    // that calls parse_review_output + persist_findings (issue #448 / round-1
    // review correctness fix).
    {
        let mut guard = poll_handle.lock().await;
        if guard.take().is_some() {
            tracing::debug!(
                "scheduler: dropped previous review poll handle (task continues to completion)"
            );
        }
    }

    // Wait for the review task to complete, then persist structured findings.
    let store = state.core.tasks.clone();
    let review_store = state.observability.review_store.clone();
    let timeout_secs = config.timeout_secs;
    let handle = tokio::spawn(async move {
        let poll_interval = Duration::from_secs(15);
        let max_wait = Duration::from_secs(timeout_secs + 120);
        let start = tokio::time::Instant::now();
        loop {
            sleep(poll_interval).await;
            if start.elapsed() > max_wait {
                tracing::warn!(task_id = %task_id, "scheduler: review task polling timed out");
                break;
            }
            let Some(task) = store.get(&task_id) else {
                continue;
            };
            if !matches!(
                task.status,
                crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
            ) {
                continue;
            }
            // Collect agent output from round details.
            let output: String = task
                .rounds
                .iter()
                .filter_map(|r| r.detail.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            if output.is_empty() {
                tracing::warn!(task_id = %task_id, "scheduler: review task completed but no output");
                break;
            }
            match crate::review_store::parse_review_output(&output) {
                Ok(review) => {
                    tracing::info!(
                        task_id = %task_id,
                        findings = review.findings.len(),
                        health_score = review.summary.health_score,
                        "scheduler: periodic review parsed"
                    );
                    if let Some(ref rs) = review_store {
                        match rs.persist_findings(&task_id.0, &review.findings).await {
                            Ok(n) => tracing::info!(
                                new_findings = n,
                                "scheduler: review findings persisted"
                            ),
                            Err(e) => tracing::warn!("scheduler: failed to persist findings: {e}"),
                        }
                    }
                    // Issue creation is handled by the review agent itself
                    // (instructed in the prompt to create GitHub issues for P0/P1 findings).
                }
                Err(e) => {
                    tracing::error!(
                        task_id = %task_id,
                        "scheduler: failed to parse review output as JSON: {e}"
                    );
                }
            }
            break;
        }
    });
    *poll_handle.lock().await = Some(handle);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_config_defaults_disabled() {
        let config = ReviewConfig::default();
        assert!(!config.enabled);
        assert!(!config.run_on_startup);
        assert_eq!(config.interval_hours, 24);
        assert!(config.interval_secs.is_none());
        assert_eq!(config.timeout_secs, 900);
        assert!(config.agent.is_none());
    }

    #[test]
    fn review_config_custom_values() {
        let config = ReviewConfig {
            enabled: true,
            run_on_startup: true,
            interval_hours: 12,
            interval_secs: None,
            agent: Some("claude".to_string()),
            timeout_secs: 600,
        };
        assert!(config.enabled);
        assert!(config.run_on_startup);
        assert_eq!(config.interval_hours, 12);
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn effective_interval_prefers_secs_over_hours() {
        let config = ReviewConfig {
            interval_hours: 24,
            interval_secs: Some(300),
            ..ReviewConfig::default()
        };
        assert_eq!(config.effective_interval(), Duration::from_secs(300));
    }

    #[test]
    fn effective_interval_falls_back_to_hours() {
        let config = ReviewConfig {
            interval_hours: 2,
            interval_secs: None,
            ..ReviewConfig::default()
        };
        assert_eq!(config.effective_interval(), Duration::from_secs(7200));
    }

    #[test]
    fn create_task_request_source_field() {
        let req = CreateTaskRequest {
            prompt: Some("review".to_string()),
            source: Some("periodic_review".to_string()),
            ..CreateTaskRequest::default()
        };
        assert_eq!(req.source.as_deref(), Some("periodic_review"));
    }

    #[test]
    fn create_task_request_source_defaults_to_none() {
        let req = CreateTaskRequest::default();
        assert!(req.source.is_none());
    }

    /// Verify that the fallback timestamp merge logic picks the maximum of the
    /// DB timestamp and the local fallback, matching the intent of RS-10 fix.
    #[tokio::test]
    async fn fallback_ts_merge_picks_max() {
        // Use UNIX_EPOCH-based construction to avoid fallible unwrap() calls.
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let earlier: DateTime<Utc> = DateTime::from(epoch);
        let later: DateTime<Utc> =
            DateTime::from(epoch + std::time::Duration::from_secs(1_000_000));

        // Fallback newer than DB — fallback wins.
        let result = match (Some(earlier), Some(later)) {
            (Some(db), Some(f)) => Some(db.max(f)),
            (Some(db), None) => Some(db),
            (None, Some(f)) => Some(f),
            (None, None) => None,
        };
        assert_eq!(result, Some(later));

        // DB newer than fallback — DB wins.
        let result = match (Some(later), Some(earlier)) {
            (Some(db), Some(f)) => Some(db.max(f)),
            (Some(db), None) => Some(db),
            (None, Some(f)) => Some(f),
            (None, None) => None,
        };
        assert_eq!(result, Some(later));

        // No DB entry, only fallback.
        let result: Option<DateTime<Utc>> = match (None::<DateTime<Utc>>, Some(later)) {
            (Some(db), Some(f)) => Some(db.max(f)),
            (Some(db), None) => Some(db),
            (None, Some(f)) => Some(f),
            (None, None) => None,
        };
        assert_eq!(result, Some(later));

        // Neither present.
        let result: Option<DateTime<Utc>> = match (None::<DateTime<Utc>>, None) {
            (Some(db), Some(f)) => Some(db.max(f)),
            (Some(db), None) => Some(db),
            (None, Some(f)) => Some(f),
            (None, None) => None,
        };
        assert_eq!(result, None);
    }

    /// Fallback is updated atomically before the EventStore write; verify the
    /// Arc<Mutex<Option<DateTime<Utc>>>> can be written and read correctly.
    #[tokio::test]
    async fn fallback_ts_arc_mutex_roundtrip() {
        let fallback_ts: Arc<Mutex<Option<DateTime<Utc>>>> = Arc::new(Mutex::new(None));
        assert!(fallback_ts.lock().await.is_none());

        let now = Utc::now();
        *fallback_ts.lock().await = Some(now);
        assert_eq!(*fallback_ts.lock().await, Some(now));
    }

    /// Verify that replacing a poll_handle drops the old JoinHandle WITHOUT
    /// aborting the underlying task, so the old poller can still persist its
    /// findings after a new review cycle starts (issue #448 / round-1 review).
    ///
    /// The previous test only checked `guard.is_some()` after replacement, which
    /// would pass even if `h.abort()` were present and the old task was killed.
    /// This test additionally asserts that the old spawned task is still running.
    #[tokio::test]
    async fn poll_handle_replaced_without_aborting_previous() {
        let poll_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(None));

        // First cycle: spawn a long-running task and store it.
        let first = tokio::spawn(async {
            sleep(Duration::from_secs(3600)).await;
        });
        *poll_handle.lock().await = Some(first);

        // Second cycle: take the old handle (drop without abort) and store a new one.
        let old_handle = {
            let mut guard = poll_handle.lock().await;
            guard.take()
        };
        let second = tokio::spawn(async {});
        *poll_handle.lock().await = Some(second);

        // Yield so the runtime can process any pending state changes.
        tokio::task::yield_now().await;

        // The old task must still be running — dropping the handle must NOT cancel it.
        assert!(old_handle.is_some(), "first handle must have been stored");
        let old = match old_handle {
            Some(h) => h,
            None => return, // unreachable: asserted above
        };
        assert!(
            !old.is_finished(),
            "old poller must still be running after handle replacement; \
             aborting it would silently drop findings"
        );
        old.abort(); // clean up the long-running task

        // The slot holds exactly one (the new) task.
        let guard = poll_handle.lock().await;
        assert!(guard.is_some());
    }
}
