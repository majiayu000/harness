use crate::task_executor::helpers::{
    run_on_error, run_post_execute, run_pre_execute, update_status,
};
use crate::task_executor::pipeline::run_test_gate;
use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStatus, TaskStore};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Event, ExecutionPhase, SessionId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};

/// Parameters for the external review bot loop.
pub(crate) struct ExternalReviewParams<'a> {
    pub store: &'a TaskStore,
    pub task_id: &'a TaskId,
    pub agent: &'a dyn CodeAgent,
    pub interceptors: &'a [Arc<dyn harness_core::interceptor::TurnInterceptor>],
    pub context_items: &'a [ContextItem],
    pub project: &'a Path,
    pub cargo_env: &'a HashMap<String, String>,
    pub pr_url: Option<&'a str>,
    pub pr_num: u64,
    pub agent_pushed_commit: bool,
    pub max_rounds: u32,
    pub wait_secs: u64,
    pub issue: Option<u64>,
    pub review_bot_command: &'a str,
    pub reviewer_name: &'a str,
    pub pre_push_cmds: &'a [String],
    pub test_gate_timeout_secs: u64,
    pub events: &'a harness_observe::event_store::EventStore,
    pub turn_timeout: Duration,
    pub task_start: Instant,
    pub jaccard_threshold: f64,
}

/// Run the external review bot wait-and-respond loop.
///
/// Returns `Ok(())` when the PR is approved (LGTM + test gate passes) or
/// when the loop exits via graduated exit. Returns `Err` on agent/timeout
/// failures. Marks the task Done/Failed internally before returning.
pub(crate) async fn run_external_review_loop(p: ExternalReviewParams<'_>) -> anyhow::Result<()> {
    let ExternalReviewParams {
        store,
        task_id,
        agent,
        interceptors,
        context_items,
        project,
        cargo_env,
        pr_url,
        pr_num,
        agent_pushed_commit,
        max_rounds,
        wait_secs,
        issue,
        review_bot_command,
        reviewer_name,
        pre_push_cmds,
        test_gate_timeout_secs,
        events,
        turn_timeout,
        task_start,
        jaccard_threshold,
    } = p;

    let mut waiting_count: u32 = 1;
    let mut prev_review_output: Option<String> = None;
    update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;

    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    let review_phase_start = Instant::now();

    let mut prev_fixed = agent_pushed_commit;
    let repo_slug = prompts::repo_slug_from_pr_url(pr_url);

    let mut issue_counts: Vec<Option<u32>> = Vec::new();
    let mut impasse = false;
    let mut pending_test_failure: Option<String> = None;
    let mut lgtm_test_gate_rejected = false;

    let mut round: u32 = 1;
    while round <= max_rounds {
        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let base_prompt = prompts::review_prompt(
            issue,
            pr_num,
            round,
            prev_fixed,
            review_bot_command,
            reviewer_name,
            &repo_slug,
            impasse,
        );
        let round_prompt = if let Some(failure) = pending_test_failure.take() {
            format!(
                "IMPORTANT: The previous LGTM was rejected because the project's tests failed. \
                 Fix the test failures before declaring LGTM again.\n\n\
                 Test output:\n```\n{failure}\n```\n\n{base_prompt}"
            )
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

        let resp = tokio::time::timeout(turn_timeout, agent.execute(check_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(
                    &r.output,
                    check_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: agent used \
                         disallowed tools: [{}]",
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
                if matches!(e, HarnessError::QuotaExhausted(_)) {
                    tracing::error!(
                        round,
                        error = %e,
                        "quota exhausted during review — aborting review loop"
                    );
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
        let lgtm = raw_lgtm && pending_test_failure.is_none();
        let fixed = !lgtm && !waiting;

        let current_issues = prompts::parse_issue_count(&output);

        // Jaccard loop detection: consecutive non-waiting, non-LGTM outputs that are
        // too similar indicate the reviewer is stuck repeating itself without progress.
        if !waiting && !raw_lgtm {
            if let Some(ref prev) = prev_review_output {
                let score = crate::task_executor::lifecycle::jaccard_word_similarity(prev, &output);
                if score >= jaccard_threshold {
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
            prev_review_output = None;
        }

        if !waiting {
            issue_counts.push(current_issues);
        }

        if issue_counts.len() >= 3 && !impasse {
            let tail = &issue_counts[issue_counts.len() - 3..];
            if let [Some(a), Some(b), Some(c)] = tail {
                if c >= a && c >= b {
                    impasse = true;
                    tracing::warn!(
                        task_id = %task_id,
                        round,
                        issues = %format_args!("[{a}, {b}, {c}]"),
                        "review loop impasse detected: issue count not decreasing"
                    );
                }
            }
        }

        const MAX_CONSECUTIVE_WAITS: u32 = 10;
        if waiting {
            if waiting_count >= MAX_CONSECUTIVE_WAITS {
                tracing::warn!(
                    round,
                    waiting_count,
                    "PR #{pr_num} review bot has not responded after {MAX_CONSECUTIVE_WAITS} \
                     waits; consuming a round to prevent infinite loop"
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

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: result_label.into(),
                detail: None,
            });
        })
        .await?;

        store.log_event(crate::event_replay::TaskEvent::RoundCompleted {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            round,
            result: result_label.to_string(),
        });

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

        if lgtm {
            match run_test_gate(project, pre_push_cmds, test_gate_timeout_secs, cargo_env).await {
                Ok(()) => {
                    tracing::info!("PR #{pr_num} approved at round {round}");
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Done;
                        s.turn = round.saturating_add(1);
                    })
                    .await?;
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
                        pr_url = pr_url.unwrap_or(""),
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

        prev_fixed = fixed || pending_test_failure.is_some();
        tracing::info!("PR #{pr_num} fixed at round {round}; waiting for bot re-review");
        if round < max_rounds {
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
        }
        round += 1;
    }

    let last_issue_count = issue_counts.iter().rev().find_map(|c| *c);
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
        pr_url = pr_url.unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}
