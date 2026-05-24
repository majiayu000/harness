use crate::task_executor::helpers;
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::prompts;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Duration;

/// Stable suffix that the GitHub issue intake poller uses to recognise gate
/// failures that require operator action (no auto-retry). Reference this
/// constant from every emission site so a wording change cannot silently
/// break the poller's dispatch contract.
pub(crate) const MANUAL_RESOLUTION_REQUIRED: &str = "manual resolution required";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewEntryDecision {
    Proceed { rebase_pushed: bool },
    FailConflict { error: String, paths_csv: String },
}

pub(crate) fn review_entry_decision(
    pr_num: u64,
    review_prep: Option<&prompts::PrReviewPrepOutcome>,
) -> ReviewEntryDecision {
    match review_prep {
        Some(prompts::PrReviewPrepOutcome::RebasePushed) => ReviewEntryDecision::Proceed {
            rebase_pushed: true,
        },
        Some(prompts::PrReviewPrepOutcome::RebaseSkipped) | None => ReviewEntryDecision::Proceed {
            rebase_pushed: false,
        },
        Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths }) => {
            let paths_csv = if paths.is_empty() {
                "unknown paths".to_string()
            } else {
                paths.join(", ")
            };
            ReviewEntryDecision::FailConflict {
                error: format!(
                    "PR #{pr_num} has rebase conflicts: {paths_csv}; {MANUAL_RESOLUTION_REQUIRED}"
                ),
                paths_csv,
            }
        }
    }
}

pub(crate) fn review_prep_from_rounds(
    rounds: &[RoundResult],
) -> Option<prompts::PrReviewPrepOutcome> {
    rounds
        .iter()
        .rev()
        .find(|round| round.action == "implement")
        .and_then(|round| match round.result.as_str() {
            "rebase_pushed" => Some(prompts::PrReviewPrepOutcome::RebasePushed),
            "rebase_skipped" => Some(prompts::PrReviewPrepOutcome::RebaseSkipped),
            "rebase_conflict" => round
                .detail
                .as_deref()
                .and_then(prompts::parse_pr_review_prep_outcome)
                .or_else(|| {
                    Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() })
                }),
            _ => None,
        })
}

pub(crate) fn review_prep_from_checkpoint_phase(
    last_phase: &str,
) -> Option<prompts::PrReviewPrepOutcome> {
    match last_phase {
        "rebase_pushed" => Some(prompts::PrReviewPrepOutcome::RebasePushed),
        "rebase_skipped" => Some(prompts::PrReviewPrepOutcome::RebaseSkipped),
        "rebase_conflict" => {
            Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() })
        }
        _ => None,
    }
}

pub(crate) async fn fail_rebase_conflict(
    store: &TaskStore,
    task_id: &TaskId,
    events: &Arc<harness_observe::event_store::EventStore>,
    pr_num: u64,
    error: String,
    paths_csv: String,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.error = Some(error.clone());
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: error,
    });
    let mut event = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        "pr_rebase_conflict",
        "task_runner",
        harness_core::types::Decision::Block,
    );
    event.reason = Some(paths_csv);
    event.detail = Some(format!("task_id={} pr={pr_num}", task_id.as_str()));
    if let Err(err) = events.log(&event).await {
        tracing::warn!("failed to log pr_rebase_conflict event: {err}");
    }
    Ok(())
}

pub(crate) enum ConflictGateOutcome {
    Clean,
    RebasePushed,
    Failed,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_resumed_pr_conflict_gate(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: &std::path::Path,
    pr_num: u64,
    repo_slug: &str,
    context_items: &[harness_core::types::ContextItem],
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    events: &Arc<harness_observe::event_store::EventStore>,
    cargo_env: &HashMap<String, String>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
) -> anyhow::Result<ConflictGateOutcome> {
    use crate::task_executor::conflict_resolver::{
        parse_conflict_check_output, ConflictCheckOutcome,
    };

    let gate_turn = turns_used.saturating_add(1);
    let persist_failure =
        |result: &'static str,
         reason: String,
         detail: Option<String>,
         telemetry: Option<harness_core::types::TurnTelemetry>,
         failure: Option<harness_core::types::TurnFailure>| async move {
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = gate_turn;
                s.error = Some(reason.clone());
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    result,
                    detail.clone(),
                    telemetry.clone(),
                    failure.clone(),
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Block,
                Some(reason),
                Some(format!("pr={pr_num}")),
                telemetry,
                failure,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok::<ConflictGateOutcome, anyhow::Error>(ConflictGateOutcome::Failed)
        };

    if let Some(max) = effective_max_turns {
        if *turns_used >= max {
            return persist_failure(
                "turn_budget_exhausted",
                format!(
                    "pr:{pr_num} conflict gate could not run before review; {MANUAL_RESOLUTION_REQUIRED}: turn budget exhausted after {} of {} allowed turns",
                    turns_used, max
                ),
                None,
                None,
                None,
            )
            .await;
        }
    }

    let prompt_built_at = Utc::now();
    let gate_prompt = prompts::check_resumed_pr_conflicts(pr_num, repo_slug, project);
    let gate_req = AgentRequest {
        prompt: gate_prompt,
        project_root: project.to_path_buf(),
        context: context_items.to_vec(),
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(harness_core::types::ExecutionPhase::Execution),
        env_vars: cargo_env.clone(),
        ..Default::default()
    };
    let gate_req = helpers::run_pre_execute(interceptors, gate_req).await?;
    let gate_started_at = Utc::now();
    let gate_resp = tokio::time::timeout(
        turn_timeout,
        helpers::run_agent_streaming(
            agent,
            gate_req.clone(),
            task_id,
            store,
            gate_turn,
            prompt_built_at,
            gate_started_at,
        ),
    )
    .await;
    *turns_used += 1;
    *turns_used_acc = *turns_used;

    let (response, telemetry) = match gate_resp {
        Ok(Ok(success)) => {
            let response = success.response;
            if let Some(validation_err) =
                helpers::run_post_execute(interceptors, &gate_req, &response).await
            {
                helpers::run_on_error(interceptors, &gate_req, &validation_err).await;
                return persist_failure(
                    "validation_failed",
                    format!(
                        "pr:{pr_num} conflict gate failed closed; {MANUAL_RESOLUTION_REQUIRED}: post-execution validation failed: {validation_err}"
                    ),
                    if response.output.is_empty() {
                        None
                    } else {
                        Some(response.output)
                    },
                    Some(success.telemetry),
                    None,
                )
                .await;
            }
            (response, success.telemetry)
        }
        Ok(Err(failure)) => {
            helpers::run_on_error(interceptors, &gate_req, &failure.error.to_string()).await;
            return persist_failure(
                "failed",
                format!(
                    "pr:{pr_num} conflict gate failed closed; {MANUAL_RESOLUTION_REQUIRED}: {}",
                    failure.error
                ),
                None,
                Some(failure.telemetry),
                Some(failure.failure),
            )
            .await;
        }
        Err(_) => {
            let msg = format!(
                "pr:{pr_num} conflict gate timed out; {MANUAL_RESOLUTION_REQUIRED} after {}s",
                turn_timeout.as_secs()
            );
            helpers::run_on_error(interceptors, &gate_req, &msg).await;
            let telemetry =
                helpers::telemetry_for_timeout(prompt_built_at, gate_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: Some(agent.name().to_string()),
                upstream_status: None,
                message: Some(msg.clone()),
                body_excerpt: None,
            };
            return persist_failure("timeout", msg, None, Some(telemetry), Some(failure)).await;
        }
    };

    let detail = if response.output.is_empty() {
        None
    } else {
        Some(response.output.clone())
    };
    match parse_conflict_check_output(&response.output) {
        Ok(ConflictCheckOutcome::CleanPr) => {
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    "clean",
                    detail.clone(),
                    Some(telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Complete,
                Some("resumed PR conflict gate passed cleanly".to_string()),
                Some(format!("pr={pr_num}")),
                Some(telemetry),
                None,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok(ConflictGateOutcome::Clean)
        }
        Ok(ConflictCheckOutcome::RebasePushed) => {
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    "rebase_pushed",
                    detail.clone(),
                    Some(telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Complete,
                Some("resumed PR conflict gate rebased and pushed".to_string()),
                Some(format!("pr={pr_num}")),
                Some(telemetry),
                None,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok(ConflictGateOutcome::RebasePushed)
        }
        Ok(ConflictCheckOutcome::ManualResolutionRequired) => {
            persist_failure(
                "manual_resolution_required",
                format!(
                    "pr:{pr_num} is conflicting and rebase was not pushed; {MANUAL_RESOLUTION_REQUIRED}"
                ),
                detail,
                Some(telemetry),
                None,
            )
            .await
        }
        Err(parse_err) => {
            persist_failure(
                "malformed_output",
                format!(
                    "pr:{pr_num} conflict gate failed closed; {MANUAL_RESOLUTION_REQUIRED}: malformed output ({parse_err})"
                ),
                detail,
                Some(telemetry),
                None,
            )
            .await
        }
    }
}

#[cfg(test)]
#[path = "gates_tests.rs"]
mod tests;
