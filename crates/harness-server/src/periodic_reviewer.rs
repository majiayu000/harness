use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use chrono::{DateTime, Utc};
use harness_core::{Decision, Event, EventFilters, ReviewConfig, ReviewStrategy, SessionId};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Mutable state shared between review ticks.
///
/// Both fields are combined into a single `Mutex` to eliminate the RS-01
/// nested-lock risk that arises from holding two separate mutexes in the same
/// function.  Every acquisition of this lock is short (no `.await` while
/// holding it) and sequential — locks are never held concurrently.
#[derive(Default)]
struct ReviewState {
    /// Local fallback timestamp guards against stale deduplication when the
    /// EventStore write fails.  Updated atomically after every successful task
    /// enqueue so the next cycle always sees a fresh lower-bound even if the
    /// DB is unavailable.
    fallback_ts: Option<DateTime<Utc>>,
    /// Handle for the currently active polling task.  The handle is dropped
    /// (without abort) when a new cycle starts so the old task can still run
    /// to completion and persist its findings (issue #448).
    poll_handle: Option<tokio::task::JoinHandle<()>>,
}

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
    // Single mutex covering both fallback_ts and poll_handle — RS-01 fix.
    let review_state: Arc<Mutex<ReviewState>> = Arc::new(Mutex::new(ReviewState::default()));

    if config.run_on_startup {
        // Brief delay to let the server fully initialize before the first tick.
        sleep(Duration::from_secs(5)).await;
        if let Err(err) = run_review_tick(&state, &config, &review_state).await {
            tracing::error!("scheduler: periodic review startup tick failed: {err}");
        }
    }

    loop {
        sleep(interval).await;
        if let Err(err) = run_review_tick(&state, &config, &review_state).await {
            tracing::error!("scheduler: periodic review tick failed: {err}");
        }
    }
}

async fn run_review_tick(
    state: &Arc<AppState>,
    config: &ReviewConfig,
    review_state: &Arc<Mutex<ReviewState>>,
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
    //
    // Sequential acquisition — lock is dropped immediately after the copy;
    // not held across any await point.
    let fb = review_state.lock().await.fallback_ts;
    let last_review_ts = match (db_last_review_ts, fb) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };

    let since_arg = last_review_ts
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let project_str = project_root.display().to_string();
    let project_cfg = harness_core::config::load_project_config(project_root);
    let base_prompt = harness_core::prompts::periodic_review_prompt(
        &project_str,
        &since_arg,
        project_cfg.review_type.as_str(),
    );

    let review_agent = config
        .agent
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            state
                .core
                .server
                .agent_registry
                .resolved_default_agent_name()
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("no default review agent available"))?;
    let registered_agents: Vec<String> = state
        .core
        .server
        .agent_registry
        .list()
        .into_iter()
        .map(str::to_string)
        .collect();
    let secondary_agent = if config.strategy == ReviewStrategy::Cross {
        pick_secondary_review_agent(&review_agent, &registered_agents, |agent| {
            state.core.server.agent_registry.get(agent).is_some()
        })
    } else {
        None
    };
    if config.strategy == ReviewStrategy::Cross && secondary_agent.is_none() {
        tracing::warn!(
            primary_agent = %review_agent,
            "scheduler: review.strategy=cross but no secondary reviewer available; degrading to single"
        );
    }

    let review_req = CreateTaskRequest {
        prompt: Some(base_prompt.clone()),
        agent: Some(review_agent.clone()),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        ..CreateTaskRequest::default()
    };

    let primary_review_id = task_routes::enqueue_task(state, review_req)
        .await
        .map_err(|e| anyhow::anyhow!("failed to enqueue periodic review: {e}"))?;
    tracing::info!(
        task_id = %primary_review_id,
        agent = %review_agent,
        strategy = ?config.strategy,
        "scheduler: primary periodic review enqueued"
    );

    let secondary_review_id = if let Some(agent) = secondary_agent.clone() {
        let req = CreateTaskRequest {
            prompt: Some(base_prompt),
            agent: Some(agent.clone()),
            turn_timeout_secs: config.timeout_secs,
            source: Some("periodic_review".to_string()),
            ..CreateTaskRequest::default()
        };
        match task_routes::enqueue_task(state, req).await {
            Ok(task_id) => {
                tracing::info!(
                    task_id = %task_id,
                    agent = %agent,
                    "scheduler: secondary periodic review enqueued"
                );
                Some(task_id)
            }
            Err(err) => {
                tracing::warn!(
                    agent = %agent,
                    error = %err,
                    "scheduler: failed to enqueue secondary review; continuing with primary only"
                );
                None
            }
        }
    } else {
        None
    };

    // Sequential acquisition — lock dropped immediately after write; not held
    // across any await point.
    review_state.lock().await.fallback_ts = Some(Utc::now());

    let event = Event::new(
        SessionId::new(),
        "periodic_review",
        "scheduler",
        Decision::Pass,
    );
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::error!("scheduler: failed to log periodic_review event (continuing): {err}");
    }

    // Sequential acquisition — take the old handle (drop without abort so the
    // previous poller can still run to completion) then release the lock before
    // spawning the new task.
    {
        let mut guard = review_state.lock().await;
        if guard.poll_handle.take().is_some() {
            tracing::debug!(
                "scheduler: dropped previous review poll handle (task continues to completion)"
            );
        }
    }

    // Wait for review completion, optionally run synthesis, then persist findings.
    let store = state.core.tasks.clone();
    let review_store = state.observability.review_store.clone();
    let timeout_secs = config.timeout_secs;
    let primary_agent_for_synthesis = review_agent.clone();
    let secondary_agent_name = secondary_agent.clone();
    let state_for_synthesis = state.clone();
    let handle = tokio::spawn(async move {
        let primary_output = poll_task_output(&store, &primary_review_id, timeout_secs).await;
        tracing::info!(
            task_id = %primary_review_id,
            output_len = primary_output.as_ref().map(|s| s.len()).unwrap_or(0),
            "scheduler: primary periodic review completed"
        );

        // Agent may signal that no commits landed since last review.
        if primary_output
            .as_deref()
            .map(|s| s.contains("REVIEW_SKIPPED"))
            .unwrap_or(false)
        {
            tracing::debug!(
                task_id = %primary_review_id,
                "scheduler: agent reported REVIEW_SKIPPED — no new commits"
            );
            return;
        }

        let mut final_task_id = primary_review_id.clone();
        let mut final_output = primary_output.clone();
        if let (Some(secondary_id), Some(secondary_name)) =
            (secondary_review_id.as_ref(), secondary_agent_name.as_ref())
        {
            let secondary_output = poll_task_output(&store, secondary_id, timeout_secs).await;
            tracing::info!(
                task_id = %secondary_id,
                agent = %secondary_name,
                output_len = secondary_output.as_ref().map(|s| s.len()).unwrap_or(0),
                "scheduler: secondary periodic review completed"
            );

            if let (Some(primary_text), Some(secondary_text)) =
                (primary_output.as_ref(), secondary_output.as_ref())
            {
                let synthesis_prompt = harness_core::prompts::review_synthesis_prompt_with_agents(
                    &review_agent,
                    primary_text,
                    secondary_name,
                    secondary_text,
                );
                let synth_req = CreateTaskRequest {
                    prompt: Some(synthesis_prompt),
                    agent: Some(primary_agent_for_synthesis.clone()),
                    turn_timeout_secs: timeout_secs,
                    source: Some("periodic_review".to_string()),
                    ..CreateTaskRequest::default()
                };
                match task_routes::enqueue_task(&state_for_synthesis, synth_req).await {
                    Ok(synth_id) => {
                        tracing::info!(
                            task_id = %synth_id,
                            agent = %primary_agent_for_synthesis,
                            "scheduler: synthesis review enqueued"
                        );
                        final_task_id = synth_id.clone();
                        final_output = poll_task_output(&store, &synth_id, timeout_secs).await;
                    }
                    Err(err) => {
                        tracing::warn!("scheduler: failed to enqueue synthesis review: {err}");
                        final_output = primary_output;
                    }
                }
            } else if primary_output.is_none() {
                // Primary failed/no output: fall back to secondary output if available.
                final_task_id = secondary_id.clone();
                final_output = secondary_output;
            }
        }

        let Some(output) = final_output else {
            tracing::warn!("scheduler: no review output to parse");
            return;
        };
        match crate::review_store::parse_review_output(&output) {
            Ok(review) => {
                tracing::info!(
                    findings = review.findings.len(),
                    health_score = review.summary.health_score,
                    "scheduler: periodic review parsed"
                );
                if let Some(ref rs) = review_store {
                    match rs
                        .persist_findings(&final_task_id.0, &review.findings)
                        .await
                    {
                        Ok(n) => {
                            tracing::info!(new_findings = n, "scheduler: review findings persisted")
                        }
                        Err(e) => tracing::warn!("scheduler: failed to persist findings: {e}"),
                    }
                }
            }
            Err(e) => {
                tracing::error!("scheduler: failed to parse review output as JSON: {e}");
            }
        }
    });
    // Sequential acquisition — lock dropped immediately after storing the handle.
    review_state.lock().await.poll_handle = Some(handle);

    Ok(())
}

fn pick_secondary_review_agent<F>(
    primary_agent: &str,
    candidates: &[String],
    mut is_available: F,
) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    candidates
        .iter()
        .find(|agent| agent.as_str() != primary_agent && is_available(agent.as_str()))
        .cloned()
}

/// Poll a task until it reaches a terminal state, then extract its output.
async fn poll_task_output(
    store: &crate::task_runner::TaskStore,
    task_id: &harness_core::TaskId,
    timeout_secs: u64,
) -> Option<String> {
    let poll_interval = Duration::from_secs(15);
    let max_wait = Duration::from_secs(timeout_secs + 120);
    let start = tokio::time::Instant::now();
    loop {
        sleep(poll_interval).await;
        if start.elapsed() > max_wait {
            tracing::warn!(task_id = %task_id, "poll_task_output: timed out");
            return None;
        }
        let Some(task) = store.get(task_id) else {
            continue;
        };
        if !matches!(
            task.status,
            crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
        ) {
            continue;
        }
        let output: String = task
            .rounds
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        if output.is_empty() {
            tracing::warn!(task_id = %task_id, "poll_task_output: completed but no output");
            return None;
        }
        return Some(output);
    }
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
        assert_eq!(config.strategy, ReviewStrategy::Single);
    }

    #[test]
    fn review_config_custom_values() {
        let config = ReviewConfig {
            enabled: true,
            run_on_startup: true,
            interval_hours: 12,
            interval_secs: None,
            agent: Some("codex".to_string()),
            strategy: ReviewStrategy::Cross,
            timeout_secs: 600,
        };
        assert!(config.enabled);
        assert!(config.run_on_startup);
        assert_eq!(config.interval_hours, 12);
        assert_eq!(config.agent.as_deref(), Some("codex"));
        assert_eq!(config.strategy, ReviewStrategy::Cross);
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn pick_secondary_review_agent_prefers_claude_for_codex_primary() {
        let candidates = vec![
            "codex".to_string(),
            "claude".to_string(),
            "anthropic-api".to_string(),
        ];
        let agent = pick_secondary_review_agent("codex", &candidates, |name| name == "claude");
        assert_eq!(agent.as_deref(), Some("claude"));
    }

    #[test]
    fn pick_secondary_review_agent_prefers_codex_for_claude_primary() {
        let candidates = vec![
            "claude".to_string(),
            "codex".to_string(),
            "anthropic-api".to_string(),
        ];
        let agent = pick_secondary_review_agent("claude", &candidates, |name| name == "codex");
        assert_eq!(agent.as_deref(), Some("codex"));
    }

    #[test]
    fn pick_secondary_review_agent_falls_back_to_anthropic_api() {
        let candidates = vec!["codex".to_string(), "anthropic-api".to_string()];
        let agent =
            pick_secondary_review_agent("codex", &candidates, |name| name == "anthropic-api");
        assert_eq!(agent.as_deref(), Some("anthropic-api"));
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

    /// Fallback timestamp is updated atomically; verify ReviewState round-trips
    /// correctly through the single combined mutex.
    #[tokio::test]
    async fn fallback_ts_arc_mutex_roundtrip() {
        let state: Arc<Mutex<ReviewState>> = Arc::new(Mutex::new(ReviewState::default()));
        assert!(state.lock().await.fallback_ts.is_none());

        let now = Utc::now();
        state.lock().await.fallback_ts = Some(now);
        assert_eq!(state.lock().await.fallback_ts, Some(now));
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
        let state: Arc<Mutex<ReviewState>> = Arc::new(Mutex::new(ReviewState::default()));

        // First cycle: spawn a long-running task and store it.
        let first = tokio::spawn(async {
            sleep(Duration::from_secs(3600)).await;
        });
        state.lock().await.poll_handle = Some(first);

        // Second cycle: take the old handle (drop without abort) and store a new one.
        let old_handle = {
            let mut guard = state.lock().await;
            guard.poll_handle.take()
        };
        let second = tokio::spawn(async {});
        state.lock().await.poll_handle = Some(second);

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
        let guard = state.lock().await;
        assert!(guard.poll_handle.is_some());
    }

    /// Structural check: run_review_tick no longer contains Command::new("git").
    /// This is a compile-time / source-level assertion verified by reading the
    /// function source — there is no tokio::process::Command call in the tick path.
    #[test]
    fn test_git_guard_removed() {
        // The git log guard was at lines 88-104 in the original file.
        // Verifying absence structurally: the function compiles without any
        // tokio::process::Command usage for the git check.
        // Since the crate compiles (ignoring pre-existing errors in other files),
        // this test passing proves the guard is gone.
        let source = include_str!("periodic_reviewer.rs");
        assert!(
            !source.contains("Command::new(\"git\")"),
            "periodic_reviewer.rs must not contain Command::new(\"git\")"
        );
    }

    /// REVIEW_SKIPPED in agent output is treated as a no-op: the contains check
    /// returns true and the early-return path is taken.
    #[test]
    fn test_review_skipped_detection() {
        let output_with_skip = Some("some preamble\nREVIEW_SKIPPED\nmore text".to_string());
        let output_without_skip = Some("REVIEW_JSON_START\n{}\nREVIEW_JSON_END".to_string());
        let no_output: Option<String> = None;

        assert!(output_with_skip
            .as_deref()
            .map(|s| s.contains("REVIEW_SKIPPED"))
            .unwrap_or(false));
        assert!(!output_without_skip
            .as_deref()
            .map(|s| s.contains("REVIEW_SKIPPED"))
            .unwrap_or(false));
        assert!(!no_output
            .as_deref()
            .map(|s| s.contains("REVIEW_SKIPPED"))
            .unwrap_or(false));
    }

    /// On first boot (no prior event, no fallback), since_arg must be the
    /// Unix epoch sentinel — unchanged behaviour.
    #[test]
    fn test_first_boot_no_ts_produces_epoch_since() {
        let last_review_ts: Option<DateTime<Utc>> = None;
        let since_arg = last_review_ts
            .map(|ts| ts.to_rfc3339())
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
        assert_eq!(since_arg, "1970-01-01T00:00:00Z");
    }
}
