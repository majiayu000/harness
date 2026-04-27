use super::agent_review::jaccard_word_similarity;
use super::helpers::{run_on_error, run_post_execute, run_pre_execute, update_status};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{Event, ExecutionPhase, SessionId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{sleep, Duration, Instant};

/// Tracks which review bot tier is active in the review loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewTier {
    /// Gemini (or configured primary bot) is active.
    Primary,
    /// Codex fallback is active after primary quota exhaustion.
    /// A second quota exhaustion from this tier triggers the human gate (tier C).
    Codex,
}

/// External state of a PR as observed via the GitHub API.
///
/// Used to short-circuit the review loop when the PR was already merged or
/// closed while the server was processing other tasks, avoiding unnecessary
/// agent turns on stale work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrExternalState {
    Open,
    Merged,
    Closed,
    /// `gh` call failed or returned an unrecognised value; proceed normally.
    Unknown,
}

/// Query GitHub for the current state of a PR via the `gh` CLI.
///
/// The `gh_bin` parameter is injectable so that tests can substitute a mock
/// shell script without requiring a real GitHub token or network access.
///
/// All failure paths return [`PrExternalState::Unknown`] so that a transient
/// `gh` error never blocks the review loop — unknown state is treated as
/// "proceed with normal review" (graceful degradation).
async fn fetch_pr_external_state(pr_num: u64, project: &Path, gh_bin: &str) -> PrExternalState {
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new(gh_bin)
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    {
        Ok(Ok(out)) if out.status.success() => out,
        Ok(Ok(out)) => {
            tracing::debug!(
                pr = pr_num,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "fetch_pr_external_state: gh returned non-zero"
            );
            return PrExternalState::Unknown;
        }
        Ok(Err(e)) => {
            tracing::debug!(pr = pr_num, error = %e, "fetch_pr_external_state: gh failed");
            return PrExternalState::Unknown;
        }
        Err(_) => {
            tracing::debug!(pr = pr_num, "fetch_pr_external_state: gh timed out");
            return PrExternalState::Unknown;
        }
    };
    let raw = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_matches('"')
        .to_uppercase();
    match raw.as_str() {
        "OPEN" => PrExternalState::Open,
        "MERGED" => PrExternalState::Merged,
        "CLOSED" => PrExternalState::Closed,
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
    gh_bin: &str,
) -> anyhow::Result<()> {
    let review_phase_start = Instant::now();

    // Fast-path: if the PR is already merged or closed on GitHub, skip the
    // review loop entirely to avoid burning agent turns on stale work.
    match fetch_pr_external_state(pr_num, project, gh_bin).await {
        PrExternalState::Merged => {
            tracing::info!(
                task_id = %task_id,
                pr_num,
                "review loop: PR already merged on GitHub — marking done"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = 1;
                s.error = Some("PR merged on GitHub before review loop started".to_string());
            })
            .await?;
            return Ok(());
        }
        PrExternalState::Closed => {
            tracing::info!(
                task_id = %task_id,
                pr_num,
                "review loop: PR closed on GitHub without merge — marking cancelled"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Cancelled;
                s.turn = 1;
                s.error = Some(
                    "PR closed on GitHub without merging before review loop started".to_string(),
                );
            })
            .await?;
            return Ok(());
        }
        // Open or Unknown: proceed with normal review loop.
        PrExternalState::Open | PrExternalState::Unknown => {}
    }

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

    // Tiered fallback: tracks which review bot is active.
    let mut tier = ReviewTier::Primary;
    // These may be swapped to Codex constants when tier B activates.
    let mut active_review_bot_command: &str = &review_config.review_bot_command;
    let mut active_reviewer_name: &str = &review_config.reviewer_name;

    let quota_handoff_threshold = review_config.quota_handoff_threshold;
    let silence_handoff_rounds = review_config.silence_handoff_rounds;
    let mut quota_exhausted_rounds: u32 = 0;

    // Review loop.
    // Use an explicit counter so WAITING responses don't consume a round — `continue`
    // inside a `for` loop would silently advance the iterator even without a real review.
    let mut round: u32 = 1;
    let mut waiting_count: u32 = 1;
    while round <= max_rounds {
        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let base_prompt = prompts::review_prompt(
            req.issue,
            pr_num,
            round,
            prev_fixed,
            active_review_bot_command,
            active_reviewer_name,
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
        let resp = tokio::time::timeout(turn_timeout, agent.execute(check_req.clone())).await;
        *turns_used += 1;
        *turns_used_acc = *turns_used;
        let resp = match resp {
            Ok(Ok(r)) => {
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
                r
            }
            Ok(Err(e)) => {
                // Quota/billing failures are not retryable — break immediately instead of
                // burning remaining review rounds on repeated errors.
                // Do NOT activate the global rate-limit circuit breaker: the reviewer
                // agent is configured independently from the implementation agent and a
                // depleted reviewer account must not stall unrelated implementation tasks.
                if matches!(
                    e,
                    HarnessError::QuotaExhausted(_) | HarnessError::BillingFailed(_)
                ) {
                    tracing::error!(round, error = %e, "quota/billing failure during review — aborting review loop");
                    run_on_error(interceptors, &check_req, &e.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok(());
                }
                run_on_error(interceptors, &check_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &check_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let AgentResponse { output, stderr, .. } = resp;

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

        // WAITING: review bot hasn't posted yet.
        // Don't consume a round — just sleep and retry. Cap consecutive waits via
        // `silence_handoff_rounds`. When that cap is reached, check the five
        // silence-as-handoff preconditions before deciding whether to park at
        // ReadyToMerge or consume a round.
        if waiting {
            if waiting_count >= silence_handoff_rounds {
                // Five preconditions for silence-as-handoff (tier C via silence path):
                // 1. No pending test failure from prior LGTM rejection.
                // 2. No LGTM was ever rejected by the test gate.
                // 3. At least one real review round completed (round > 1).
                // 4. Last recorded issue count is <= 2, or bot never posted (issue_counts empty).
                // 5. Test gate passes.
                let issues_ok = issue_counts.is_empty()
                    || issue_counts
                        .iter()
                        .rev()
                        .find_map(|c| *c)
                        .is_some_and(|n| n <= 2);
                let preconditions_met = pending_test_failure.is_none()
                    && !lgtm_test_gate_rejected
                    && round > 1
                    && issues_ok;

                if preconditions_met {
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
                                round,
                                waiting_count,
                                "silence-as-handoff: all preconditions met — marking ReadyToMerge"
                            );
                            mutate_and_persist(store, task_id, |s| {
                                s.status = TaskStatus::ReadyToMerge;
                                s.turn = round.saturating_add(1);
                                s.error = Some(format!(
                                    "ReadyToMerge: review bot silent after {} rounds, tests passed",
                                    waiting_count
                                ));
                            })
                            .await?;
                            return Ok(());
                        }
                        Err(_) => {
                            tracing::warn!(
                                task_id = %task_id,
                                round,
                                waiting_count,
                                "silence-as-handoff: tests failed — consuming a round"
                            );
                        }
                    }
                }

                tracing::warn!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded after {silence_handoff_rounds} waits; \
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
        // Don't consume a round — sleep and retry. After `quota_handoff_threshold`
        // consecutive quota rounds, escalate through tiers:
        //   Tier A (primary) -> Tier B (Codex) when codex_fallback_enabled, else Done/Failed.
        //   Tier B (Codex)   -> Tier C (human gate) -> ReadyToMerge/Failed.
        if quota_exhausted {
            quota_exhausted_rounds += 1;
            tracing::info!(
                round,
                quota_exhausted_rounds,
                tier = ?tier,
                "PR #{pr_num} reviewer quota exhausted; not consuming a review round"
            );
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(RoundResult {
                    turn: round,
                    action: "review".into(),
                    result: "quota_exhausted".into(),
                    detail: None,
                    first_token_latency_ms: None,
                });
            })
            .await?;

            if quota_exhausted_rounds >= quota_handoff_threshold && !lgtm_test_gate_rejected {
                match tier {
                    ReviewTier::Primary if review_config.codex_fallback_enabled => {
                        // Escalate to Tier B: switch active bot to Codex.
                        tier = ReviewTier::Codex;
                        quota_exhausted_rounds = 0;
                        active_review_bot_command = &review_config.codex_review_bot_command;
                        active_reviewer_name = &review_config.codex_reviewer_name;
                        tracing::info!(
                            task_id = %task_id,
                            quota_exhausted_rounds,
                            "quota exhausted: escalating to Codex tier (B)"
                        );
                    }
                    ReviewTier::Primary => {
                        // Backward compat: no Codex fallback configured; run test gate.
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
                    ReviewTier::Codex => {
                        // Tier C: both primary and Codex exhausted -> human gate.
                        tracing::info!(
                            task_id = %task_id,
                            quota_exhausted_rounds,
                            "Codex tier also quota-exhausted: escalating to human gate (tier C)"
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
                                    "tier C: tests passed — marking ReadyToMerge"
                                );
                                mutate_and_persist(store, task_id, |s| {
                                    s.status = TaskStatus::ReadyToMerge;
                                    s.turn = round.saturating_add(1);
                                    s.error = Some(format!(
                                        "ReadyToMerge: both primary and Codex quota exhausted after {} rounds, tests passed",
                                        quota_exhausted_rounds
                                    ));
                                })
                                .await?;
                                return Ok(());
                            }
                            Err(_) => {
                                tracing::warn!(
                                    task_id = %task_id,
                                    "tier C: tests failed — marking failed"
                                );
                                mutate_and_persist(store, task_id, |s| {
                                    s.status = TaskStatus::Failed;
                                    s.turn = round.saturating_add(1);
                                    s.error = Some(format!(
                                        "ReadyToMerge blocked: tests failed after all review tiers exhausted ({} quota rounds)",
                                        quota_exhausted_rounds
                                    ));
                                })
                                .await?;
                                return Ok(());
                            }
                        }
                    }
                }
            }

            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
            continue; // Don't increment round — quota rounds are free
        }

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: result_label.into(),
                detail: None,
                first_token_latency_ms: None,
            });
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
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                harness_core::types::Decision::Complete
            } else {
                harness_core::types::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        let result_label = if lgtm { "lgtm" } else { "fixed" };
        ev.reason = Some(format!("round {round}: {result_label}"));
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        // Log pr_review_fallback event when a non-primary tier was active.
        if tier != ReviewTier::Primary {
            let mut fallback_ev = Event::new(
                SessionId::new(),
                "pr_review_fallback",
                "task_runner",
                harness_core::types::Decision::Warn,
            );
            fallback_ev.detail = Some(format!("pr={pr_num} tier={tier:?}"));
            fallback_ev.reason = Some(format!("round {round}: {result_label} via {:?}", tier));
            if let Err(e) = events.log(&fallback_ev).await {
                tracing::warn!("failed to log pr_review_fallback event: {e}");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── TaskStatus::ReadyToMerge serialization ────────────────────────────────

    #[test]
    fn taskstatus_readytomerge_serializes_correctly() {
        let s = serde_json::to_string(&TaskStatus::ReadyToMerge).unwrap();
        assert_eq!(s, r#""ready_to_merge""#);
        let rt: TaskStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(rt, TaskStatus::ReadyToMerge);
    }

    #[test]
    fn taskstatus_readytomerge_is_not_terminal() {
        // ReadyToMerge is not terminal so the reconciliation loop can
        // transition it to Done/Cancelled when the PR is merged/closed.
        assert!(!TaskStatus::ReadyToMerge.is_terminal());
        assert!(!TaskStatus::ReadyToMerge.is_inflight());
        assert!(!TaskStatus::ReadyToMerge.is_success());
        assert!(!TaskStatus::ReadyToMerge.is_failure());
    }

    #[test]
    fn taskstatus_readytomerge_from_str() {
        let s: TaskStatus = "ready_to_merge".parse().unwrap();
        assert_eq!(s, TaskStatus::ReadyToMerge);
    }

    // ── issue_count_not_decreasing ────────────────────────────────────────────

    #[test]
    fn issue_count_not_decreasing_requires_three_entries() {
        assert!(!issue_count_not_decreasing(&[]));
        assert!(!issue_count_not_decreasing(&[Some(5)]));
        assert!(!issue_count_not_decreasing(&[Some(5), Some(5)]));
    }

    #[test]
    fn issue_count_not_decreasing_detects_stagnation() {
        assert!(issue_count_not_decreasing(&[Some(5), Some(5), Some(5)]));
        assert!(issue_count_not_decreasing(&[Some(3), Some(4), Some(5)]));
    }

    #[test]
    fn issue_count_not_decreasing_false_when_decreasing() {
        assert!(!issue_count_not_decreasing(&[Some(5), Some(4), Some(3)]));
        assert!(!issue_count_not_decreasing(&[Some(5), Some(5), Some(4)]));
    }

    #[test]
    fn issue_count_not_decreasing_false_when_none_in_tail() {
        assert!(!issue_count_not_decreasing(&[Some(5), None, Some(5)]));
        assert!(!issue_count_not_decreasing(&[None, None, None]));
    }

    // ── ReviewTier ────────────────────────────────────────────────────────────

    #[test]
    fn review_tier_primary_is_not_codex() {
        let tier = ReviewTier::Primary;
        assert_eq!(tier, ReviewTier::Primary);
        assert_ne!(tier, ReviewTier::Codex);
    }

    #[test]
    fn codex_config_defaults_are_nonempty() {
        let cfg = harness_core::config::agents::AgentReviewConfig::default();
        assert!(!cfg.codex_reviewer_name.is_empty());
        assert!(!cfg.codex_review_bot_command.is_empty());
    }
}
