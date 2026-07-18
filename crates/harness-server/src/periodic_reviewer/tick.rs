use super::{commit_gate, project_hook_key, ProjectInfo, ReviewState};
use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use chrono::{DateTime, Utc};
use harness_core::{
    config::misc::{ReviewConfig, ReviewStrategy},
    types::{Decision, Event, EventFilters, SessionId},
};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::tick_helpers::{
    ensure_review_queue_limit, format_violations_for_prompt, parse_review_output,
    pick_secondary_review_agent, poll_task_output,
};

pub(super) async fn run_review_tick(
    state: &Arc<AppState>,
    config: &ReviewConfig,
    review_state: &Arc<Mutex<ReviewState>>,
    project: &ProjectInfo,
) -> anyhow::Result<()> {
    let project_root = &project.root;
    let hook_key = project_hook_key(&project.name, project_root);

    // Query EventStore for the most recent watermark for this project.
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some(hook_key.clone()),
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
        .as_ref()
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let project_str = project_root.display().to_string();
    // review_type already resolved by collect_projects — no second load_project_config needed.

    if commit_gate::should_skip_review(state, project, &hook_key, last_review_ts, &since_arg).await
    {
        return Ok(());
    }

    // Run the guard scan on the source repo before spawning the agent.  The
    // agent runs inside a worktree that may contain nested `.harness/worktrees/`
    // from previous runs; scanning there inflates violation counts ~3x.
    //
    // Only scan repos that have opted in via `.harness/guards`. The shared
    // RuleEngine holds guards from all registered projects; scanning an unrelated
    // repo with those guards produces false positives.
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
        project.review_type.as_str(),
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

    ensure_review_queue_limit(state, project_root);

    let review_req = CreateTaskRequest {
        prompt: Some(base_prompt.clone()),
        agent: Some(review_agent.clone()),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        project: Some(project_root.clone()),
        system_input: Some(crate::task_runner::SystemTaskInput::PeriodicReview {
            prompt: base_prompt.clone(),
        }),
        ..CreateTaskRequest::default()
    };

    let primary_review_id = task_routes::enqueue_task_background_in_domain(
        state.clone(),
        review_req,
        None,
        task_routes::QueueDomain::Review,
    )
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

    // Wait for review completion, optionally run synthesis, then advance the watermark.
    let timeout_secs = config.timeout_secs;
    let primary_agent_for_synthesis = review_agent.clone();
    let secondary_agent_name = secondary_agent.clone();
    let state_for_synthesis = state.clone();
    let fallback_ts_for_poll = review_state.clone();
    // Capture project root so secondary/synthesis/auto-fix tasks target the
    // correct repository — without this they fall back to main-worktree
    // detection and can execute against the wrong project.
    let project_root_for_poll = project_root.clone();
    // Capture the scan boundary before the review agents run. Using this
    // timestamp (rather than Utc::now() after synthesis completes) as the
    // watermark ensures that commits arriving while secondary/synthesis agents
    // are executing are NOT silently skipped on the next tick.
    let scan_ts = Utc::now();
    let handle = tokio::spawn(async move {
        let primary_output =
            poll_task_output(&state_for_synthesis, &primary_review_id, timeout_secs).await;
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

        // Only enqueue secondary (Cross strategy) when primary produced output.
        // Synthesis requires both outputs; running secondary when primary timed
        // out / OOM wastes queue/quota because its output will be discarded.
        let secondary_review_id: Option<harness_core::types::TaskId> = if primary_output.is_none() {
            if secondary_agent_name.is_some() {
                tracing::warn!(
                    task_id = %primary_review_id,
                    "scheduler: primary produced no output — skipping secondary enqueue \
                     to avoid queue/quota exhaustion (synthesis requires both outputs)"
                );
            }
            None
        } else if let Some(agent) = secondary_agent_name.as_ref() {
            let req = CreateTaskRequest {
                prompt: Some(base_prompt.clone()),
                agent: Some(agent.clone()),
                turn_timeout_secs: timeout_secs,
                source: Some("periodic_review".to_string()),
                project: Some(project_root_for_poll.clone()),
                system_input: Some(crate::task_runner::SystemTaskInput::PeriodicReview {
                    prompt: base_prompt.clone(),
                }),
                ..CreateTaskRequest::default()
            };
            match task_routes::enqueue_task_background_in_domain(
                state_for_synthesis.clone(),
                req,
                None,
                task_routes::QueueDomain::Review,
            )
            .await
            {
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
        // When synthesis is used (Cross mode) we record the timestamp captured
        // just before polling synthesis.  Advancing the watermark to this bound
        // ensures commits that arrive *during* synthesis latency are caught by
        // the next tick rather than permanently skipped (issue-2 fix).
        let mut review_bound_ts: Option<DateTime<Utc>> = None;

        // Cross mode: primary is confirmed non-None and non-REVIEW_SKIPPED.
        // Advance the in-memory watermark speculatively to scan_ts now so that
        // concurrent scheduler ticks do not re-enqueue the same commit window
        // while secondary/synthesis agents are in flight.  The DB-backed
        // periodic_review event is only written after a successful parse (below),
        // so a server restart resets fallback_ts and the DB remains authoritative.
        // We capture the pre-advance value so it can be restored if
        // parse_review_output later fails (prevents silent commit-window loss).
        let pre_speculative_fallback_ts: Option<Option<DateTime<Utc>>> =
            if secondary_review_id.is_some() {
                let mut guard = fallback_ts_for_poll.lock().await;
                let prev = guard.fallback_ts;
                guard.fallback_ts = Some(scan_ts);
                Some(prev)
            } else {
                None
            };

        if let (Some(secondary_id), Some(secondary_name)) =
            (secondary_review_id.as_ref(), secondary_agent_name.as_ref())
        {
            let secondary_output =
                poll_task_output(&state_for_synthesis, secondary_id, timeout_secs).await;
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
                    prompt: Some(synthesis_prompt.clone()),
                    agent: Some(primary_agent_for_synthesis.clone()),
                    turn_timeout_secs: timeout_secs,
                    source: Some("periodic_review".to_string()),
                    project: Some(project_root_for_poll.clone()),
                    system_input: Some(crate::task_runner::SystemTaskInput::PeriodicReview {
                        prompt: synthesis_prompt,
                    }),
                    ..CreateTaskRequest::default()
                };
                match task_routes::enqueue_task_background_in_domain(
                    state_for_synthesis.clone(),
                    synth_req,
                    None,
                    task_routes::QueueDomain::Review,
                )
                .await
                {
                    Ok(synth_id) => {
                        tracing::info!(
                            task_id = %synth_id,
                            agent = %primary_agent_for_synthesis,
                            "scheduler: synthesis review enqueued"
                        );
                        final_task_id = synth_id.clone();
                        // Capture bound before polling synthesis so the watermark
                        // covers only commits primary/secondary actually reviewed;
                        // commits arriving during synthesis latency are caught next
                        // tick (issue-2 fix).
                        let pre_synthesis_ts = Utc::now();
                        let synth_out =
                            poll_task_output(&state_for_synthesis, &synth_id, timeout_secs).await;
                        if synth_out.is_none() {
                            // Synthesis timed out / OOM / rate-limited.  Fall back to
                            // primary output.  Do NOT advance the watermark to
                            // pre_synthesis_ts here — secondary may have reviewed
                            // commits that primary never saw, and advancing past them
                            // would skip those commits forever.  Leave review_bound_ts
                            // as None so the watermark falls back to scan_ts (primary's
                            // boundary) at the merge site below.
                            tracing::warn!(
                                task_id = %synth_id,
                                "scheduler: synthesis produced no output \
                                 (timeout/OOM/rate-limit); falling back to primary \
                                 review output; watermark held at scan_ts to avoid \
                                 skipping commits only seen by secondary"
                            );
                            final_output = primary_output.clone();
                        } else {
                            final_output = synth_out;
                            // Only advance to pre_synthesis_ts when synthesis actually
                            // succeeded — it is safe because both primary and secondary
                            // outputs are captured in the synthesis result.
                            review_bound_ts = Some(pre_synthesis_ts);
                        }
                    }
                    Err(err) => {
                        tracing::warn!("scheduler: failed to enqueue synthesis review: {err}");
                        final_output = primary_output;
                    }
                }
            }
        }

        let Some(output) = final_output else {
            tracing::error!(
                task_id = %final_task_id,
                "scheduler: review produced no output (agent may have OOM/rate-limited/timed out) \
                 — watermark NOT advanced; will retry next tick"
            );
            return;
        };

        match parse_review_output(&output) {
            Ok(review) => {
                // Advance the watermark only after parse succeeds.
                // Doing this here (rather than before the match) prevents a
                // non-empty but malformed JSON response from permanently
                // dropping its commit window — on parse failure the watermark
                // stays put and the next tick will re-review the same window.
                // `scan_ts` is the boundary captured before agents were
                // enqueued, so commits arriving during secondary/synthesis are
                // NOT included in the advanced watermark and will be reviewed
                // on the next tick (fixes the Cross-strategy skip-forever bug).
                // Use pre_synthesis_ts when available (Cross mode) so the
                // watermark boundary does not jump past commits that arrived
                // during synthesis latency.  Falls back to scan_ts for
                // Single-strategy paths where no synthesis step exists.
                let watermark_ts = review_bound_ts.unwrap_or(scan_ts);
                fallback_ts_for_poll.lock().await.fallback_ts = Some(watermark_ts);
                // Set the event's ts to watermark_ts (not Utc::now()) so that next
                // tick's db_last_review_ts = max(event.ts) == watermark_ts.  If we
                // used Utc::now() here, db_last_review_ts >> watermark_ts, and the
                // max(db_ts, fallback_ts) merge would always pick db_ts, nullifying
                // the intended bound and allowing commit-window skips during long
                // secondary/synthesis latency (issue-2 fix).
                let mut watermark_event =
                    Event::new(SessionId::new(), &hook_key, "scheduler", Decision::Pass);
                watermark_event.ts = watermark_ts;
                if let Err(err) = state_for_synthesis
                    .observability
                    .events
                    .log(&watermark_event)
                    .await
                {
                    tracing::error!(
                        "scheduler: failed to log periodic_review event (continuing): {err}"
                    );
                }
                tracing::info!(
                    findings = review.findings.len(),
                    health_score = review.summary.health_score,
                    "scheduler: periodic review parsed"
                );
            }
            Err(e) => {
                tracing::error!("scheduler: failed to parse review output as JSON: {e}");
                // Rollback the speculative in-memory watermark advance (Cross
                // mode) so the next tick retries this commit window instead of
                // permanently skipping it due to a parse failure.
                if let Some(prev_ts) = pre_speculative_fallback_ts {
                    fallback_ts_for_poll.lock().await.fallback_ts = prev_ts;
                    tracing::warn!(
                        "scheduler: rolled back speculative fallback_ts after parse failure; \
                         next tick will retry this commit window"
                    );
                }
            }
        }
    });
    // Sequential acquisition — lock dropped immediately after storing the handle.
    review_state.lock().await.poll_handle = Some(handle);

    Ok(())
}
