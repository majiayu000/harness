use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use chrono::{DateTime, Utc};
use harness_core::{
    config::misc::ReviewConfig, config::misc::ReviewStrategy, types::Decision, types::Event,
    types::EventFilters, types::SessionId,
};
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

    // Brief delay to let the server fully initialize before checking.
    sleep(Duration::from_secs(5)).await;

    // On startup, check how long since the last review (from DB watermark).
    // If overdue, trigger immediately instead of waiting a full interval.
    let initial_delay = match last_review_timestamp(&state).await {
        Some(last_ts) => {
            let elapsed = Utc::now().signed_duration_since(last_ts);
            let interval_chrono = chrono::Duration::from_std(interval)
                .unwrap_or_else(|_| chrono::Duration::hours(24));
            if elapsed >= interval_chrono {
                tracing::info!(
                    last_review = %last_ts,
                    elapsed_hours = elapsed.num_hours(),
                    "scheduler: periodic review overdue, triggering now"
                );
                Duration::from_secs(0)
            } else {
                let remaining = (interval_chrono - elapsed).to_std().unwrap_or(interval);
                tracing::info!(
                    last_review = %last_ts,
                    next_in_secs = remaining.as_secs(),
                    "scheduler: periodic review not yet due, sleeping"
                );
                remaining
            }
        }
        None => {
            tracing::info!("scheduler: no prior periodic review found, triggering now");
            Duration::from_secs(0)
        }
    };

    if !initial_delay.is_zero() {
        sleep(initial_delay).await;
    }

    // First tick (may be immediate if overdue).
    if let Err(err) = run_review_tick(&state, &config, &review_state).await {
        tracing::error!("scheduler: periodic review tick failed: {err}");
    }

    loop {
        sleep(interval).await;
        if let Err(err) = run_review_tick(&state, &config, &review_state).await {
            tracing::error!("scheduler: periodic review tick failed: {err}");
        }
    }
}

/// Query the most recent periodic_review watermark from the EventStore.
async fn last_review_timestamp(state: &Arc<AppState>) -> Option<DateTime<Utc>> {
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("periodic_review".to_string()),
            ..EventFilters::default()
        })
        .await
        .ok()?;
    events.iter().map(|e| e.ts).max()
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
    let project_cfg = harness_core::config::project::load_project_config(project_root)
        .map_err(|e| anyhow::anyhow!("failed to load periodic review config: {e}"))?;

    // Run the guard scan on the source repo before spawning the agent.  The
    // agent runs inside a worktree that may contain nested `.harness/worktrees/`
    // from previous runs; scanning there inflates violation counts ~3x.
    //
    // Only scan repos that have opted in via `.harness/guards`.  The shared
    // RuleEngine holds guards from all registered projects; scanning an unrelated
    // repo with those guards produces false positives (mirrors the opt-in check
    // in rule_enforcer.rs).
    let guard_scan_output: Option<String> = if project_root.join(".harness").join("guards").is_dir()
    {
        let rules = state.engines.rules.read().await;
        match rules.scan(project_root).await {
            Ok(violations) => {
                let text = format_violations_for_prompt(&violations);
                Some(text)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "scheduler: guard scan on source repo failed; agent will run guards itself"
                );
                None
            }
        }
    } else {
        tracing::debug!(
            project_root = %project_root.display(),
            "scheduler: no .harness/guards directory, skipping pre-scan"
        );
        None
    };

    let base_prompt = harness_core::prompts::periodic_review_prompt_with_guard_scan(
        &project_str,
        &since_arg,
        project_cfg.review_type.as_str(),
        guard_scan_output.as_deref(),
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

    // Secondary task is intentionally NOT enqueued here — it is deferred until
    // the primary confirms new commits exist (REVIEW_SKIPPED check inside the
    // poll loop below).  Enqueueing before the check would spawn real agent
    // work on every idle tick, exhausting queue capacity and quota.

    // NOTE: the watermark (fallback_ts + periodic_review event) is NOT advanced
    // here at enqueue time.  Advancing early creates a duplicate-review window:
    // the agent runs with `--since=<old_watermark>` (no --until bound), so any
    // commit that arrives between enqueue and execution is reviewed by this agent
    // AND falls inside the next tick's `--since=<enqueue_time>` window, producing
    // duplicate issues.  The watermark is advanced inside the async poll task
    // below, only after the agent confirms new commits exist.

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
    let fallback_ts_for_poll = review_state.clone();
    let handle = tokio::spawn(async move {
        let primary_output = poll_task_output(&store, &primary_review_id, timeout_secs).await;
        tracing::info!(
            task_id = %primary_review_id,
            output_len = primary_output.as_ref().map(|s| s.len()).unwrap_or(0),
            "scheduler: primary periodic review completed"
        );

        // Agent may signal that no commits landed since last review.
        // Require the ENTIRE output (trimmed) to equal "REVIEW_SKIPPED".
        // Line-by-line matching is a false positive when the agent quotes the
        // sentinel in an explanation, code block, or constant reference — in
        // those cases the agent still produced a real review and the secondary/
        // synthesis tasks must not be silently dropped.
        if primary_output
            .as_deref()
            .map(|s| s.trim() == "REVIEW_SKIPPED")
            .unwrap_or(false)
        {
            tracing::debug!(
                task_id = %primary_review_id,
                "scheduler: agent reported REVIEW_SKIPPED — no new commits"
            );
            return;
        }

        // Primary confirmed new commits exist — advance the watermark now.
        // Advancing at enqueue time creates a duplicate-review gap: commits
        // arriving between enqueue and execution are covered by this agent
        // (--since has no --until bound) but would also fall inside the next
        // tick's --since window, producing duplicate issues.  By advancing here
        // we set the boundary to the agent's execution completion time, ensuring
        // the next tick's --since is always ≥ all commits this agent reviewed.
        let review_complete_ts = Utc::now();
        fallback_ts_for_poll.lock().await.fallback_ts = Some(review_complete_ts);
        let watermark_event = Event::new(
            SessionId::new(),
            "periodic_review",
            "scheduler",
            Decision::Pass,
        );
        if let Err(err) = state_for_synthesis
            .observability
            .events
            .log(&watermark_event)
            .await
        {
            tracing::error!("scheduler: failed to log periodic_review event (continuing): {err}");
        }

        // Now it is safe to enqueue the secondary reviewer (Cross strategy only).
        let secondary_review_id: Option<harness_core::types::TaskId> = if let Some(agent) =
            secondary_agent_name.as_ref()
        {
            let req = CreateTaskRequest {
                prompt: Some(base_prompt.clone()),
                agent: Some(agent.clone()),
                turn_timeout_secs: timeout_secs,
                source: Some("periodic_review".to_string()),
                ..CreateTaskRequest::default()
            };
            match task_routes::enqueue_task(&state_for_synthesis, req).await {
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
                            tracing::info!(
                                new_findings = n,
                                "scheduler: review findings persisted"
                            );
                            // Auto-spawn fix tasks for P1/P2 open findings that have
                            // no existing task yet (task_id IS NULL = dedup guard).
                            // P0 excluded: critical issues require human judgment.
                            // P3 excluded: informational only, too low priority.
                            match rs
                                .list_spawnable_findings(&final_task_id.0, &["P1", "P2"])
                                .await
                            {
                                Ok(spawnable) => {
                                    for finding in spawnable {
                                        let prompt = format!(
                                            "Fix finding '{}' ({}, {}:{}): {}\n\nRequired action: {}",
                                            finding.title,
                                            finding.rule_id,
                                            finding.file,
                                            finding.line,
                                            finding.description,
                                            finding.action
                                        );
                                        let req = CreateTaskRequest {
                                            prompt: Some(prompt),
                                            source: Some("auto-fix".into()),
                                            ..CreateTaskRequest::default()
                                        };
                                        match task_routes::enqueue_task(&state_for_synthesis, req)
                                            .await
                                        {
                                            Ok(fix_task_id) => {
                                                if let Err(e) = rs
                                                    .mark_task_spawned(
                                                        &final_task_id.0,
                                                        &finding.id,
                                                        &fix_task_id.0,
                                                    )
                                                    .await
                                                {
                                                    tracing::warn!(
                                                        finding_id = %finding.id,
                                                        "scheduler: failed to mark task spawned: {e}"
                                                    );
                                                } else {
                                                    tracing::info!(
                                                        finding_id = %finding.id,
                                                        task_id = %fix_task_id,
                                                        "scheduler: auto-fix task spawned"
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    finding_id = %finding.id,
                                                    "scheduler: failed to spawn fix task: {e}"
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "scheduler: failed to list spawnable findings: {e}"
                                    );
                                }
                            }
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

/// Maximum number of violations inlined into the prompt.
///
/// The full list may contain hundreds of entries; inlining all of them can
/// exceed OS ARG_MAX or the model's context window before the agent even
/// starts.  Only the first N are embedded; the total count is always reported
/// so the agent knows additional findings exist.
const MAX_INLINE_VIOLATIONS: usize = 20;

fn format_violations_for_prompt(violations: &[harness_core::types::Violation]) -> String {
    if violations.is_empty() {
        return "No violations found.".to_string();
    }
    let total = violations.len();
    let shown = violations.len().min(MAX_INLINE_VIOLATIONS);
    let lines: Vec<String> = violations[..shown]
        .iter()
        .map(|v| {
            let loc = match v.line {
                Some(l) => format!("{}:{l}", v.file.display()),
                None => v.file.display().to_string(),
            };
            format!("[{:?}] {}: {} ({})", v.severity, v.rule_id, v.message, loc)
        })
        .collect();
    let mut out = format!(
        "{total} violation(s) (showing {shown}):\n{}",
        lines.join("\n")
    );
    if total > shown {
        out.push_str(&format!(
            "\n... and {} more violation(s) not shown. Run guard scripts locally for the full list.",
            total - shown
        ));
    }
    out
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
    task_id: &harness_core::types::TaskId,
    timeout_secs: u64,
) -> Option<String> {
    let poll_interval = Duration::from_secs(15);
    let max_wait = if timeout_secs == 0 {
        Duration::from_secs(999_999)
    } else {
        Duration::from_secs(timeout_secs + 120)
    };
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

    #[test]
    fn format_violations_truncates_at_max_inline() {
        use harness_core::{types::RuleId, types::Severity, types::Violation};
        use std::path::PathBuf;

        let make_v = |i: usize| Violation {
            rule_id: RuleId::from_str(&format!("RS-{i:02}")),
            file: PathBuf::from(format!("src/file{i}.rs")),
            line: Some(i),
            message: format!("violation {i}"),
            severity: Severity::Medium,
        };

        // Exactly at the limit — all shown, no truncation suffix.
        let at_limit: Vec<_> = (0..MAX_INLINE_VIOLATIONS).map(make_v).collect();
        let out = format_violations_for_prompt(&at_limit);
        assert!(out.contains(&format!(
            "{} violation(s) (showing {})",
            MAX_INLINE_VIOLATIONS, MAX_INLINE_VIOLATIONS
        )));
        assert!(!out.contains("more violation(s) not shown"));

        // One over the limit — truncation suffix must appear.
        let over_limit: Vec<_> = (0..=MAX_INLINE_VIOLATIONS).map(make_v).collect();
        let out = format_violations_for_prompt(&over_limit);
        assert!(out.contains("1 more violation(s) not shown"));
    }

    /// Fallback is updated atomically before the EventStore write; verify the
    /// Arc<Mutex<Option<DateTime<Utc>>>> can be written and read correctly.
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

    /// Structural check: run_review_tick no longer spawns git as a child process.
    /// The source file must not contain the forbidden invocation pattern.
    #[test]
    fn test_git_guard_removed() {
        // The forbidden pattern is split across two literals so that this very
        // test does not trigger the assertion it is enforcing.
        let forbidden = ["Command::new(", "\"git\")"].concat();
        let source = include_str!("periodic_reviewer.rs");
        assert!(
            !source.contains(&forbidden),
            "periodic_reviewer.rs must not spawn git directly"
        );
    }

    /// REVIEW_SKIPPED is detected only when the ENTIRE output (trimmed) equals
    /// the sentinel.  Line-by-line matching is too permissive: an agent that
    /// quotes the sentinel in an explanation or code block would trigger a false
    /// skip, silently dropping the real review/secondary/synthesis results.
    #[test]
    fn test_review_skipped_detection() {
        let is_skipped = |s: Option<&str>| s.map(|s| s.trim() == "REVIEW_SKIPPED").unwrap_or(false);

        // True positive: entire output is exactly the sentinel.
        assert!(is_skipped(Some("REVIEW_SKIPPED")));
        // True positive: trailing newline stripped by trim.
        assert!(is_skipped(Some("REVIEW_SKIPPED\n")));
        // True positive: leading/trailing whitespace stripped by trim.
        assert!(is_skipped(Some("  REVIEW_SKIPPED  ")));

        // False-positive guard: sentinel on its own line but with other content —
        // the entire output is not the sentinel, so this must NOT trigger.
        assert!(!is_skipped(Some(
            "some preamble\nREVIEW_SKIPPED\nmore text"
        )));
        assert!(!is_skipped(Some("No commits found.\nREVIEW_SKIPPED")));
        // False-positive guard: literal appears inside JSON content.
        assert!(!is_skipped(Some(
            r#"{"title":"REVIEW_SKIPPED check removed","action":"LGTM"}"#
        )));
        // Normal review output: no sentinel.
        assert!(!is_skipped(Some("REVIEW_JSON_START\n{}\nREVIEW_JSON_END")));
        // No output at all.
        assert!(!is_skipped(None));
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
