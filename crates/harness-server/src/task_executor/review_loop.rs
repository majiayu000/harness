/// External review bot loop module.
///
/// Waits for the external review bot (e.g. Gemini) to post review comments,
/// dispatches fix agents when issues are found, and loops until LGTM is
/// received, the round budget is exhausted, or a loop is detected.
use crate::task_runner::{mutate_and_persist, RoundResult, TaskStatus};
use harness_core::agent::AgentRequest;
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Decision, Event, ExecutionPhase, SessionId};
use tokio::time::{sleep, timeout, Duration, Instant};

use super::helpers::{run_on_error, run_post_execute, run_pre_execute, update_status};
use super::TaskContext;

/// Run the external review bot wait loop.
///
/// Waits for the external review bot to post review feedback, dispatches fix
/// agents when issues are found, and terminates when:
/// - The bot posts LGTM and the test gate passes → task marked Done.
/// - Round budget exhausted → task marked Done (graduated) or Failed.
/// - Jaccard loop detected → task marked Failed.
/// - Turn budget exhausted → task marked Failed.
///
/// `pr_url` and `pr_num` identify the PR being reviewed.
/// `context_items` are passed to each fix agent for context.
/// `effective_max_rounds` is the triage-derived (or caller-overridden) round cap.
/// `prev_fixed` is true when the implementation or agent review pushed a commit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    ctx: &TaskContext<'_>,
    pr_url: Option<String>,
    pr_num: u64,
    context_items: Vec<ContextItem>,
    effective_max_rounds: u32,
    effective_max_turns: Option<u32>,
    prev_fixed: bool,
    turns_used: &mut u32,
) -> anyhow::Result<()> {
    let mut waiting_count: u32 = 1;
    update_status(ctx.store, ctx.task_id, TaskStatus::Waiting, waiting_count).await?;

    let wait_secs = ctx.resolved_review_wait_secs.unwrap_or(ctx.req.wait_secs);
    let max_rounds = ctx
        .resolved_review_max_rounds
        .unwrap_or(effective_max_rounds);
    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    let review_phase_start = Instant::now();
    let jaccard_threshold = ctx.jaccard_threshold;

    let mut prev_fixed = prev_fixed;
    let repo_slug = prompts::repo_slug_from_pr_url(pr_url.as_deref());

    let mut issue_counts: Vec<Option<u32>> = Vec::new();
    let mut impasse = false;
    let mut pending_test_failure: Option<String> = None;
    let mut lgtm_test_gate_rejected = false;
    let mut prev_review_output: Option<String> = None;

    let mut round: u32 = 1;
    while round <= max_rounds {
        update_status(ctx.store, ctx.task_id, TaskStatus::Reviewing, round).await?;

        let base_prompt = prompts::review_prompt(
            ctx.req.issue,
            pr_num,
            round,
            prev_fixed,
            &ctx.review_config.review_bot_command,
            &ctx.review_config.reviewer_name,
            &repo_slug,
            impasse,
        );
        let round_prompt = if let Some(failure) = pending_test_failure.take() {
            prompts::test_gate_failure_context(&failure, &base_prompt)
        } else {
            base_prompt
        };

        let check_req = AgentRequest {
            prompt: round_prompt,
            project_root: ctx.project.clone(),
            context: context_items.clone(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: ctx.cargo_env.clone(),
            ..Default::default()
        };
        let check_req = run_pre_execute(&ctx.interceptors, check_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                let msg = format!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used, max
                );
                tracing::warn!(
                    task_id = %ctx.task_id,
                    turns_used,
                    max,
                    "turn budget exhausted in review loop"
                );
                mutate_and_persist(ctx.store, ctx.task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(msg.clone());
                })
                .await?;
                return Ok(());
            }
        }
        let resp = timeout(ctx.turn_timeout, ctx.agent.execute(check_req.clone())).await;
        *turns_used += 1;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(
                    &r.output,
                    check_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: \
                         agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        round,
                        ?tool_violations,
                        "review check: agent used tools outside allowed list"
                    );
                    run_on_error(&ctx.interceptors, &check_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(&ctx.interceptors, &check_req, &r).await {
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
                if matches!(e, HarnessError::QuotaExhausted(_)) {
                    tracing::error!(
                        round,
                        error = %e,
                        "quota exhausted during review — aborting review loop"
                    );
                    run_on_error(&ctx.interceptors, &check_req, &e.to_string()).await;
                    mutate_and_persist(ctx.store, ctx.task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok(());
                }
                run_on_error(&ctx.interceptors, &check_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    ctx.turn_timeout.as_secs()
                );
                run_on_error(&ctx.interceptors, &check_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let harness_core::agent::AgentResponse { output, stderr, .. } = resp;

        if !stderr.is_empty() {
            tracing::warn!(round, stderr = %stderr, "agent stderr during review check");
        }

        let raw_lgtm = prompts::is_lgtm(&output);
        let waiting = prompts::is_waiting(&output);
        let lgtm = raw_lgtm && pending_test_failure.is_none();
        let fixed = !lgtm && !waiting;

        let current_issues = prompts::parse_issue_count(&output);

        // Jaccard loop detection: two consecutive non-waiting, non-LGTM outputs
        // that are too similar indicate the reviewer is stuck.
        if !waiting && !raw_lgtm {
            if let Some(ref prev) = prev_review_output {
                let score = super::jaccard_word_similarity(prev, &output);
                if score >= jaccard_threshold {
                    let last_count = issue_counts.last().and_then(|x| *x);
                    let progressing = matches!(
                        (last_count, current_issues),
                        (Some(prev_n), Some(curr_n)) if curr_n < prev_n
                    );
                    if !progressing {
                        let msg = format!(
                            "review loop detected: output similarity {score:.2} >= \
                             threshold {jaccard_threshold:.2}"
                        );
                        tracing::warn!(
                            task_id = %ctx.task_id,
                            round,
                            score,
                            jaccard_threshold,
                            "jaccard loop detected in review"
                        );
                        mutate_and_persist(ctx.store, ctx.task_id, |s| {
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
            prev_review_output = None;
        }

        if !waiting {
            issue_counts.push(current_issues);
        }

        // Convergence check: if the last 3 rounds all reported issues and the
        // count is not decreasing, enter impasse mode.
        if issue_counts.len() >= 3 && !impasse {
            let tail = &issue_counts[issue_counts.len() - 3..];
            if let [Some(a), Some(b), Some(c)] = tail {
                if c >= a && c >= b {
                    impasse = true;
                    tracing::warn!(
                        task_id = %ctx.task_id,
                        round,
                        issues = %format_args!("[{a}, {b}, {c}]"),
                        "review loop impasse detected: issue count not decreasing"
                    );
                }
            }
        }

        // WAITING: review bot hasn't posted yet.
        const MAX_CONSECUTIVE_WAITS: u32 = 10;
        if waiting {
            if waiting_count >= MAX_CONSECUTIVE_WAITS {
                tracing::warn!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded after \
                     {MAX_CONSECUTIVE_WAITS} waits; consuming a round"
                );
                round += 1;
                waiting_count = 0;
            } else {
                tracing::info!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded yet; sleeping"
                );
                waiting_count += 1;
                update_status(ctx.store, ctx.task_id, TaskStatus::Waiting, waiting_count).await?;
                sleep(Duration::from_secs(wait_secs)).await;
                continue;
            }
        }

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: result_label.into(),
                detail: None,
                first_token_latency_ms: None,
            });
        })
        .await?;

        ctx.store
            .log_event(crate::event_replay::TaskEvent::RoundCompleted {
                task_id: ctx.task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                round,
                result: result_label.to_string(),
            });

        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                Decision::Complete
            } else {
                Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(format!("round {round}: {result_label}"));
        if let Err(e) = ctx.events.log(&ev).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            // Hard gate: run the project's tests before accepting LGTM.
            match super::run_test_gate(
                &ctx.project,
                &ctx.project_config.validation.pre_push,
                ctx.project_config.validation.test_gate_timeout_secs,
                &ctx.cargo_env,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!("PR #{pr_num} approved at round {round}");
                    mutate_and_persist(ctx.store, ctx.task_id, |s| {
                        s.status = TaskStatus::Done;
                        s.turn = round.saturating_add(1);
                    })
                    .await?;
                    ctx.store
                        .log_event(crate::event_replay::TaskEvent::Completed {
                            task_id: ctx.task_id.0.clone(),
                            ts: crate::event_replay::now_ts(),
                        });
                    tracing::info!(
                        task_id = %ctx.task_id,
                        phase = "reviewing",
                        elapsed_secs = review_phase_start.elapsed().as_secs(),
                        "phase_completed"
                    );
                    tracing::info!(
                        task_id = %ctx.task_id,
                        status = "done",
                        turns = round.saturating_add(1),
                        pr_url = pr_url.as_deref().unwrap_or(""),
                        total_elapsed_secs = ctx.task_start.elapsed().as_secs(),
                        "task_completed"
                    );
                    return Ok(());
                }
                Err(gate_output) => {
                    tracing::warn!(
                        task_id = %ctx.task_id,
                        round,
                        "LGTM rejected: tests failed in test gate, re-entering review round {}",
                        round.saturating_add(1)
                    );
                    lgtm_test_gate_rejected = true;
                    pending_test_failure = Some(gate_output);
                }
            }
        }

        prev_fixed = fixed || pending_test_failure.is_some();
        tracing::info!("PR #{pr_num} fixed at round {round}; waiting for bot re-review");
        if round < max_rounds {
            waiting_count += 1;
            update_status(ctx.store, ctx.task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
        }
        round += 1;
    }

    // Graduated exit: if the last round had few issues remaining, mark as done.
    let last_issue_count = issue_counts.iter().rev().find_map(|c| *c);
    let graduated = last_issue_count.is_some_and(|n| n <= 2) && !lgtm_test_gate_rejected;

    if graduated {
        tracing::info!(
            task_id = %ctx.task_id,
            remaining_issues = last_issue_count.unwrap_or(0),
            "review loop exhausted but few issues remain — marking done with warning"
        );
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
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
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
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
        task_id = %ctx.task_id,
        phase = "reviewing",
        elapsed_secs = review_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    tracing::info!(
        task_id = %ctx.task_id,
        status = if graduated { "done" } else { "failed" },
        turns = max_rounds.saturating_add(1),
        pr_url = pr_url.as_deref().unwrap_or(""),
        total_elapsed_secs = ctx.task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}
