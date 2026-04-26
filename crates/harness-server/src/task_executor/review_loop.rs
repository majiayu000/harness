use super::agent_review::jaccard_word_similarity;
use super::helpers::{
    build_task_event, run_agent_streaming, run_on_error, run_post_execute, run_pre_execute,
    telemetry_for_timeout, update_status,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{Decision, ExecutionPhase, TurnFailure, TurnFailureKind};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{sleep, Duration, Instant};

/// External state of a PR as observed via `gh`. Used to short-circuit the
/// review loop when a PR has been merged or closed outside of this task so the
/// loop does not keep invoking the reviewer agent against stale work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrExternalState {
    Open,
    Merged,
    Closed,
    Unknown,
}

/// Query `gh pr view <pr> --json state,mergedAt` with a 10 s timeout and
/// classify the result. Returns [`PrExternalState::Unknown`] on any transient
/// failure so callers do not abort a healthy review loop because of a flaky
/// network call.
async fn fetch_pr_external_state(pr_num: u64, project: &Path) -> PrExternalState {
    // kill_on_drop(true) so the gh subprocess is reaped if the timeout
    // future is dropped — without it, hung gh invocations (network/auth
    // stalls) accumulate as orphaned children across repeated review
    // rounds and degrade the server. Mirrors the pattern used by
    // task_runner/store.rs::validate_recovered_tasks.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("gh")
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "state,mergedAt",
                "--jq",
                ".state + \"|\" + ((.mergedAt // \"\")|tostring)",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let output = match result {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            tracing::debug!(pr = pr_num, exit = ?o.status.code(), "gh pr state check failed");
            return PrExternalState::Unknown;
        }
        Ok(Err(e)) => {
            tracing::debug!(pr = pr_num, error = %e, "gh pr state check invocation error");
            return PrExternalState::Unknown;
        }
        Err(_) => {
            tracing::debug!(pr = pr_num, "gh pr state check timed out after 10s");
            return PrExternalState::Unknown;
        }
    };
    let raw = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();
    let (state, merged_at) = raw.split_once('|').unwrap_or((raw.as_str(), ""));
    match (state.trim(), merged_at.trim().is_empty()) {
        ("OPEN", _) => PrExternalState::Open,
        ("MERGED", _) => PrExternalState::Merged,
        // GitHub occasionally reports merged PRs as CLOSED with a non-empty
        // `mergedAt`; treat that as a merge so we do not cancel a completed
        // task by mistake.
        ("CLOSED", false) => PrExternalState::Merged,
        ("CLOSED", true) => PrExternalState::Closed,
        _ => PrExternalState::Unknown,
    }
}

/// Returns `true` when the trailing three entries of `counts` are all `Some`
/// and the issue count is not decreasing (`c >= a && c >= b`).
///
/// Used to detect convergence failure in the review loop so the caller can
/// switch to impasse (critical-only) mode.
fn issue_count_not_decreasing(counts: &[Option<u32>]) -> bool {
    if counts.len() < 3 {
        return false;
    }
    let tail = &counts[counts.len() - 3..];
    matches!(tail, [Some(a), Some(b), Some(c)] if c >= a && c >= b)
}

/// Execute the external review bot wait loop.
///
/// Polls the PR for review bot feedback, handles LGTM/FIXED/WAITING responses,
/// runs the test gate on LGTM acceptance, and manages convergence tracking.
/// Returns `Ok(())` when the task is complete (status already persisted).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_review_loop(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    review_config: &harness_core::config::agents::AgentReviewConfig,
    project_config: &harness_core::config::project::ProjectConfig,
    req: &CreateTaskRequest,
    events: &Arc<harness_observe::event_store::EventStore>,
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    context_items: &[harness_core::types::ContextItem],
    project: &Path,
    cargo_env: &HashMap<String, String>,
    pr_url: Option<String>,
    pr_num: u64,
    effective_max_turns: Option<u32>,
    _effective_max_rounds: u32,
    wait_secs: u64,
    max_rounds: u32,
    agent_pushed_commit: bool,
    rebase_pushed: bool,
    turn_timeout: Duration,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
    repo_slug: String,
    jaccard_threshold: f64,
) -> anyhow::Result<()> {
    let review_phase_start = Instant::now();

    // `prev_fixed` tracks whether a commit was pushed that hasn't been re-reviewed yet.
    // Starts true if the implementation phase pushed a commit, and is set to true again
    // whenever a review round produces FIXED (agent commits + pushes). This drives the
    // freshness check in every round after a fix commit, not just round 1.
    let mut prev_fixed = agent_pushed_commit || rebase_pushed;

    // Convergence tracking: detect when issue count stops decreasing.
    let mut issue_counts: Vec<Option<u32>> = Vec::new();
    let mut impasse = false;

    // Carries test gate failure output from a rejected LGTM into the next review round.
    let mut pending_test_failure: Option<String> = None;
    // Issue 2 fix: track whether any LGTM was ever rejected by the test gate
    // without a subsequent clean LGTM+pass. `pending_test_failure.take()` clears
    // the Option at the start of each round (to avoid re-showing the same output),
    // but the graduation check must still know a test gate rejection occurred.
    let mut lgtm_test_gate_rejected = false;

    // Tracks the most recent non-waiting review output for Jaccard loop detection.
    let mut prev_review_output: Option<String> = None;

    const QUOTA_EXHAUSTED_THRESHOLD: u32 = 3;
    let mut quota_exhausted_rounds: u32 = 0;

    // Review loop.
    // Use an explicit counter so WAITING responses don't consume a round — `continue`
    // inside a `for` loop would silently advance the iterator even without a real review.
    let mut round: u32 = 1;
    let mut waiting_count: u32 = 1;
    while round <= max_rounds {
        // Cheap pre-flight: check the PR's external state before invoking the
        // reviewer agent. If the PR has already been merged or closed outside
        // of this task (operator merged manually, another agent closed it,
        // webhook finalised it) the remaining rounds would only burn tokens
        // against stale work — short-circuit to the appropriate terminal
        // status. Unknown/transient failures fall through and continue the
        // normal review flow.
        match fetch_pr_external_state(pr_num, project).await {
            PrExternalState::Merged => {
                tracing::info!(
                    task_id = %task_id,
                    pr = pr_num,
                    round,
                    "review loop exit: PR merged externally"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Done;
                    s.turn = round;
                    s.error = Some(
                        "PR merged externally; review loop exited without waiting for LGTM"
                            .to_string(),
                    );
                })
                .await?;
                store.log_event(crate::event_replay::TaskEvent::Completed {
                    task_id: task_id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                });
                tracing::info!(
                    task_id = %task_id,
                    phase = "reviewing",
                    elapsed_secs = review_phase_start.elapsed().as_secs(),
                    "phase_completed"
                );
                tracing::info!(
                    task_id = %task_id,
                    status = "done",
                    turns = round,
                    pr_url = pr_url.as_deref().unwrap_or(""),
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(());
            }
            PrExternalState::Closed => {
                tracing::info!(
                    task_id = %task_id,
                    pr = pr_num,
                    round,
                    "review loop exit: PR closed externally without merge"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Cancelled;
                    s.turn = round;
                    s.error =
                        Some("PR closed externally without merge; review loop exited".to_string());
                })
                .await?;
                return Ok(());
            }
            PrExternalState::Open | PrExternalState::Unknown => {}
        }

        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let base_prompt = prompts::review_prompt(
            req.issue,
            pr_num,
            round,
            prev_fixed,
            &review_config.review_bot_command,
            &review_config.reviewer_name,
            &repo_slug,
            impasse,
        );
        // If the previous LGTM was rejected by the test gate, prepend the failure
        // output so the agent has context for why re-work is needed.
        let round_prompt = if let Some(failure) = pending_test_failure.take() {
            prompts::test_gate_failure_prompt(&failure, &base_prompt)
        } else {
            base_prompt
        };

        let prompt_built_at = Utc::now();
        let check_req = AgentRequest {
            prompt: round_prompt,
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let check_req = run_pre_execute(interceptors, check_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                let msg = format!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used, max
                );
                tracing::warn!(task_id = %task_id, turns_used, max, "turn budget exhausted in review loop");
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(msg.clone());
                })
                .await?;
                return Ok(());
            }
        }
        let review_started_at = Utc::now();
        let resp = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(
                agent,
                check_req.clone(),
                task_id,
                store,
                round,
                prompt_built_at,
                review_started_at,
            ),
        )
        .await;
        *turns_used += 1;
        *turns_used_acc = *turns_used;
        let resp = match resp {
            Ok(Ok(success)) => {
                let r = success.response;
                let tool_violations = validate_tool_usage(
                    &r.output,
                    check_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        round,
                        ?tool_violations,
                        "review check: agent used tools outside allowed list"
                    );
                    run_on_error(interceptors, &check_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(interceptors, &check_req, &r).await {
                    tracing::error!(
                        round,
                        error = %val_err,
                        "post-execute validation failed in review check; will re-enter review"
                    );
                    pending_test_failure =
                        Some(format!("Post-execution validation failed:\n{val_err}"));
                }
                (r, success.telemetry)
            }
            Ok(Err(failure)) => {
                // Quota/billing failures are not retryable — break immediately instead of
                // burning remaining review rounds on repeated errors.
                // Do NOT activate the global rate-limit circuit breaker: the reviewer
                // agent is configured independently from the implementation agent and a
                // depleted reviewer account must not stall unrelated implementation tasks.
                if matches!(
                    failure.failure.kind,
                    TurnFailureKind::Quota | TurnFailureKind::Billing
                ) {
                    tracing::error!(round, error = %failure.error, "quota/billing failure during review — aborting review loop");
                    run_on_error(interceptors, &check_req, &failure.error.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(failure.error.to_string());
                        s.rounds.push(RoundResult::new(
                            round,
                            "review",
                            match failure.failure.kind {
                                TurnFailureKind::Quota => "quota_exhausted",
                                TurnFailureKind::Billing => "billing_failed",
                                TurnFailureKind::Upstream => "upstream_failure",
                                _ => "failed",
                            },
                            None,
                            Some(failure.telemetry.clone()),
                            Some(failure.failure.clone()),
                        ));
                    })
                    .await?;
                    let event = build_task_event(
                        task_id,
                        round,
                        "review",
                        "pr_review",
                        Decision::Block,
                        Some("review round failed".to_string()),
                        Some(format!("pr={pr_num}")),
                        Some(failure.telemetry),
                        Some(failure.failure),
                        None,
                    );
                    if let Err(error) = events.log(&event).await {
                        tracing::warn!("failed to log pr_review event: {error}");
                    }
                    return Ok(());
                }
                run_on_error(interceptors, &check_req, &failure.error.to_string()).await;
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        round,
                        "review",
                        "failed",
                        None,
                        Some(failure.telemetry.clone()),
                        Some(failure.failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    round,
                    "review",
                    "pr_review",
                    Decision::Block,
                    Some("review round failed".to_string()),
                    Some(format!("pr={pr_num}")),
                    Some(failure.telemetry),
                    Some(failure.failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log pr_review event: {error}");
                }
                return Err(failure.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &check_req, &msg).await;
                let telemetry =
                    telemetry_for_timeout(prompt_built_at, review_started_at, Utc::now(), None);
                let failure = TurnFailure {
                    kind: TurnFailureKind::Timeout,
                    provider: Some(agent.name().to_string()),
                    upstream_status: None,
                    message: Some(msg.clone()),
                    body_excerpt: None,
                };
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        round,
                        "review",
                        "timeout",
                        None,
                        Some(telemetry.clone()),
                        Some(failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    round,
                    "review",
                    "pr_review",
                    Decision::Block,
                    Some("review round timed out".to_string()),
                    Some(format!("pr={pr_num}")),
                    Some(telemetry),
                    Some(failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log pr_review event: {error}");
                }
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let (AgentResponse { output, stderr, .. }, review_telemetry) = resp;

        if !stderr.is_empty() {
            tracing::warn!(round, stderr = %stderr, "agent stderr during review check");
        }

        let raw_lgtm = prompts::is_lgtm(&output);
        let waiting = prompts::is_waiting(&output);
        let quota_exhausted = !raw_lgtm && !waiting && prompts::is_quota_exhausted(&output);
        // If post-execute validation failed this round, block LGTM acceptance even
        // if the reviewer approved — the local validator caught an issue that must be
        // fixed before the PR can be marked done.
        let lgtm = raw_lgtm && pending_test_failure.is_none();
        let fixed = !lgtm && !waiting && !quota_exhausted;

        // Parse issue count before the Jaccard check so loop detection can distinguish
        // genuine forward progress (decreasing issue count) from true stuck loops.
        let current_issues = prompts::parse_issue_count(&output);

        // Jaccard loop detection: two consecutive non-waiting, non-LGTM outputs that are
        // too similar indicate the reviewer is stuck repeating itself without making progress.
        // Skip when the raw output is an approval: repeated LGTM is legitimate convergence
        // (e.g., reviewer approves again after a test-gate previously blocked acceptance).
        // Quota-exhausted rounds are also skipped — they contain no real review content.
        if !waiting && !raw_lgtm && !quota_exhausted {
            if let Some(ref prev) = prev_review_output {
                let score = jaccard_word_similarity(prev, &output);
                if score >= jaccard_threshold {
                    // If the issue count decreased since the previous round, the agent is making
                    // genuine progress despite similar output structure (e.g., one fix per round
                    // with a consistent report format). Only abort on true stagnation.
                    let last_count = issue_counts.last().and_then(|x| *x);
                    let progressing = matches!(
                        (last_count, current_issues),
                        (Some(prev_n), Some(curr_n)) if curr_n < prev_n
                    );
                    if !progressing {
                        let msg = format!(
                            "review loop detected: output similarity {score:.2} >= threshold {jaccard_threshold:.2}"
                        );
                        tracing::warn!(task_id = %task_id, round, score, jaccard_threshold, "jaccard loop detected in review");
                        mutate_and_persist(store, task_id, |s| {
                            s.status = TaskStatus::Failed;
                            s.error = Some(msg.clone());
                        })
                        .await?;
                        return Ok(());
                    }
                }
            }
            prev_review_output = Some(output.clone());
        } else if raw_lgtm {
            // A test-gate-rejected LGTM still resets the comparison baseline so the
            // next genuine non-LGTM review is not incorrectly compared against a
            // pre-LGTM review and falsely flagged as a loop.
            prev_review_output = None;
        }

        if !waiting && !quota_exhausted {
            issue_counts.push(current_issues);
        }

        // Convergence check: if the last 3 rounds all reported issues and the
        // count is not decreasing, enter impasse mode (critical-only fixes).
        if !impasse && issue_count_not_decreasing(&issue_counts) {
            impasse = true;
            let tail = &issue_counts[issue_counts.len() - 3..];
            if let [Some(a), Some(b), Some(c)] = tail {
                tracing::warn!(
                    task_id = %task_id,
                    round,
                    issues = %format_args!("[{a}, {b}, {c}]"),
                    "review loop impasse detected: issue count not decreasing"
                );
            }
        }

        // WAITING means review bot hasn't posted yet (e.g., quota exhausted).
        // Don't consume a round — just sleep and retry without incrementing.
        // Cap consecutive waits to prevent infinite loops when the bot never responds.
        const MAX_CONSECUTIVE_WAITS: u32 = 10;
        if waiting {
            if waiting_count >= MAX_CONSECUTIVE_WAITS {
                tracing::warn!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded after {MAX_CONSECUTIVE_WAITS} waits; \
                     consuming a round to prevent infinite loop"
                );
                round += 1;
                waiting_count = 0;
            } else {
                tracing::info!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded yet; sleeping without consuming round"
                );
                waiting_count += 1;
                update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
                sleep(Duration::from_secs(wait_secs)).await;
                continue;
            }
        }

        // Quota-exhausted: reviewer posted a quota warning instead of a real review.
        // Don't consume a round — sleep and retry. After K consecutive quota rounds,
        // run the test gate as a heuristic graduation check.
        if quota_exhausted {
            quota_exhausted_rounds += 1;
            tracing::info!(
                round,
                quota_exhausted_rounds,
                "PR #{pr_num} reviewer quota exhausted; not consuming a review round"
            );
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(RoundResult::new(
                    round,
                    "review",
                    "quota_exhausted",
                    None,
                    Some(review_telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = build_task_event(
                task_id,
                round,
                "review",
                "pr_review",
                Decision::Warn,
                Some(format!("round {round}: quota exhausted")),
                Some(format!("pr={pr_num}")),
                Some(review_telemetry.clone()),
                None,
                Some(output.clone()),
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_review event: {error}");
            }

            if quota_exhausted_rounds >= QUOTA_EXHAUSTED_THRESHOLD && !lgtm_test_gate_rejected {
                tracing::info!(
                    task_id = %task_id,
                    quota_exhausted_rounds,
                    "quota-heuristic graduation: running test gate after {} quota-exhausted rounds",
                    quota_exhausted_rounds
                );
                match super::run_test_gate(
                    project,
                    &project_config.validation.pre_push,
                    project_config.validation.test_gate_timeout_secs,
                    cargo_env,
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!(
                            task_id = %task_id,
                            quota_exhausted_rounds,
                            "quota-heuristic graduation: tests passed — marking done"
                        );
                        mutate_and_persist(store, task_id, |s| {
                            s.status = TaskStatus::Done;
                            s.turn = round.saturating_add(1);
                            s.error = Some(format!(
                                "LGTM via quota-heuristic: external reviewer quota exhausted after {} rounds, tests passed",
                                quota_exhausted_rounds
                            ));
                        })
                        .await?;
                        return Ok(());
                    }
                    Err(_test_output) => {
                        tracing::warn!(
                            task_id = %task_id,
                            quota_exhausted_rounds,
                            "quota-heuristic graduation: tests failed — marking failed"
                        );
                        mutate_and_persist(store, task_id, |s| {
                            s.status = TaskStatus::Failed;
                            s.turn = round.saturating_add(1);
                            s.error = Some(format!(
                                "Quota-heuristic: tests failed after {} quota-exhausted rounds",
                                quota_exhausted_rounds
                            ));
                        })
                        .await?;
                        return Ok(());
                    }
                }
            }

            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
            continue; // Don't increment round — quota rounds are free
        }

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult::new(
                round,
                "review",
                result_label,
                None,
                Some(review_telemetry.clone()),
                None,
            ));
        })
        .await?;

        // Emit RoundCompleted for crash recovery.
        store.log_event(crate::event_replay::TaskEvent::RoundCompleted {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            round,
            result: result_label.to_string(),
        });

        // Log pr_review event for observability and GC signal detection.
        let event = build_task_event(
            task_id,
            round,
            "review",
            "pr_review",
            if lgtm {
                Decision::Complete
            } else {
                Decision::Warn
            },
            Some(format!("round {round}: {result_label}")),
            Some(format!("pr={pr_num}")),
            Some(review_telemetry.clone()),
            None,
            Some(output.clone()),
        );
        if let Err(e) = events.log(&event).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            // Hard gate: run the project's tests before accepting LGTM.
            // This prevents agents from gaming the review by manipulating tests
            // rather than fixing code (OpenAI "Monitoring Reasoning Models", 2026).
            match super::run_test_gate(
                project,
                &project_config.validation.pre_push,
                project_config.validation.test_gate_timeout_secs,
                cargo_env,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!("PR #{pr_num} approved at round {round}");
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Done;
                        s.turn = round.saturating_add(1);
                    })
                    .await?;
                    store.log_event(crate::event_replay::TaskEvent::Completed {
                        task_id: task_id.0.clone(),
                        ts: crate::event_replay::now_ts(),
                    });
                    tracing::info!(
                        task_id = %task_id,
                        phase = "reviewing",
                        elapsed_secs = review_phase_start.elapsed().as_secs(),
                        "phase_completed"
                    );
                    tracing::info!(
                        task_id = %task_id,
                        status = "done",
                        turns = round.saturating_add(1),
                        pr_url = pr_url.as_deref().unwrap_or(""),
                        total_elapsed_secs = task_start.elapsed().as_secs(),
                        "task_completed"
                    );
                    return Ok(());
                }
                Err(output) => {
                    tracing::warn!(
                        task_id = %task_id,
                        round,
                        "LGTM rejected: tests failed in test gate, re-entering review round {}",
                        round.saturating_add(1)
                    );
                    lgtm_test_gate_rejected = true;
                    pending_test_failure = Some(output);
                }
            }
        }

        // Track whether this round produced a fix so the next round enforces
        // freshness checks against new reviewer feedback.
        // Also treat a test gate rejection as a "fixed" round — the agent needs
        // to push a fix and get a fresh review before LGTM can be accepted.
        prev_fixed = fixed || pending_test_failure.is_some();
        if fixed {
            quota_exhausted_rounds = 0;
        }
        tracing::info!("PR #{pr_num} fixed at round {round}; waiting for bot re-review");
        if round < max_rounds {
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
        }
        round += 1;
    }

    // Graduated exit: if the last round had few issues remaining, mark as done
    // with a warning instead of outright failure — the PR is likely mergeable.
    // But never graduate when the test gate was still failing on the last round:
    // a PR whose tests consistently fail must not be approved.
    let last_issue_count = issue_counts.iter().rev().find_map(|c| *c);
    // Never graduate when a LGTM was rejected by the test gate — the pending_test_failure
    // Option was already cleared by take() at round start, so we track rejection separately.
    let graduated = last_issue_count.is_some_and(|n| n <= 2) && !lgtm_test_gate_rejected;

    if graduated {
        tracing::info!(
            task_id = %task_id,
            remaining_issues = last_issue_count.unwrap_or(0),
            "review loop exhausted but few issues remain — marking done with warning"
        );
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = max_rounds.saturating_add(1);
            s.error = Some(format!(
                "Graduated: LGTM not received after {} rounds but only {} issues remain.",
                max_rounds,
                last_issue_count.unwrap_or(0),
            ));
        })
        .await?;
    } else {
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = max_rounds.saturating_add(1);
            s.error = Some(format!(
                "Task did not receive LGTM after {} review rounds (last issues: {}).",
                max_rounds,
                last_issue_count.map_or("unknown".to_string(), |n| n.to_string()),
            ));
        })
        .await?;
    }
    tracing::info!(
        task_id = %task_id,
        phase = "reviewing",
        elapsed_secs = review_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    tracing::info!(
        task_id = %task_id,
        status = if graduated { "done" } else { "failed" },
        turns = max_rounds.saturating_add(1),
        pr_url = pr_url.as_deref().unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}
