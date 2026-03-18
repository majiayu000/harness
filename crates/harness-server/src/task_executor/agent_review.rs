use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStatus, TaskStore};
use harness_core::{
    prompts, AgentRequest, AgentResponse, CodeAgent, ContextItem, Decision, Event, ExecutionPhase,
    SessionId,
};
use std::path::Path;
use std::sync::Arc;
use tokio::time::Duration;

use super::helpers::{run_on_error, run_post_execute, run_pre_execute, update_status};

/// Context bundle for the agent-review cycle.
#[derive(Clone, Copy)]
pub(super) struct AgentReviewContext<'a> {
    pub store: &'a TaskStore,
    pub task_id: &'a TaskId,
    pub agent: &'a dyn CodeAgent,
    pub reviewer: &'a dyn CodeAgent,
    pub review_config: &'a harness_core::AgentReviewConfig,
    pub context_items: &'a [ContextItem],
    pub project: &'a Path,
    pub interceptors: &'a [Arc<dyn harness_core::interceptor::TurnInterceptor>],
    pub turn_timeout: Duration,
    pub pr_num: u64,
    pub events: &'a harness_observe::EventStore,
}

/// Run the agent-review cycle: reviewer evaluates the PR, implementor fixes
/// issues, up to `review_config.max_rounds` iterations.
pub(super) async fn run_agent_review(ctx: &AgentReviewContext<'_>) -> anyhow::Result<()> {
    let AgentReviewContext {
        store,
        task_id,
        agent,
        reviewer,
        review_config,
        context_items,
        project,
        interceptors,
        turn_timeout,
        pr_num,
        events,
    } = *ctx;
    let max_rounds = review_config.max_rounds;
    let context_vec = context_items.to_vec();
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff
        let review_req = AgentRequest {
            prompt: prompts::agent_review_prompt(pr_num, agent_round),
            project_root: project.to_path_buf(),
            context: context_vec.clone(),
            execution_phase: Some(ExecutionPhase::Validation),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, reviewer.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(interceptors, &review_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review; continuing"
                    );
                }
                r
            }
            Ok(Err(e)) => {
                run_on_error(interceptors, &review_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &review_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let AgentResponse { output, stderr, .. } = resp;

        if !stderr.is_empty() {
            tracing::warn!(agent_round, stderr = %stderr, "agent reviewer stderr");
        }

        let approved = prompts::is_approved(&output);
        let issues = prompts::extract_review_issues(&output);
        let review_detail = output;

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0, // agent review rounds use turn 0
                action: "agent_review".into(),
                result: if approved {
                    "approved".into()
                } else {
                    format!("{} issues", issues.len())
                },
                detail: Some(review_detail),
            });
        })
        .await?;

        // Log agent_review event
        let mut ev = Event::new(
            SessionId::new(),
            "agent_review",
            "task_runner",
            if approved {
                Decision::Complete
            } else {
                Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if approved {
            format!("round {agent_round}: approved")
        } else {
            format!("round {agent_round}: {} issues", issues.len())
        });
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log agent_review event: {e}");
        }

        if approved {
            tracing::info!("agent review approved at round {agent_round}");
            break;
        }

        // Malformed reviewer output: neither APPROVED nor any ISSUE: lines.
        // Sending an empty fix prompt would produce arbitrary or no-op commits,
        // so treat this as a reviewer protocol failure and abort the review loop.
        if issues.is_empty() {
            tracing::warn!(
                agent_round,
                "agent reviewer output contained neither APPROVED nor ISSUE: lines; \
                 treating as protocol failure and skipping fix round"
            );
            break;
        }

        if agent_round == max_rounds {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
            );
            break;
        }

        // Implementor fixes the issues
        let fix_req = AgentRequest {
            prompt: prompts::agent_review_fix_prompt(pr_num, &issues, agent_round),
            project_root: project.to_path_buf(),
            context: context_vec.clone(),
            execution_phase: Some(ExecutionPhase::Execution),
            ..Default::default()
        };
        let fix_req = run_pre_execute(interceptors, fix_req).await?;

        let fix_resp = tokio::time::timeout(turn_timeout, agent.execute(fix_req.clone())).await;
        match fix_resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(interceptors, &fix_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review fix; continuing"
                    );
                }
            }
            Ok(Err(e)) => {
                run_on_error(interceptors, &fix_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review fix round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &fix_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0,
                action: "agent_review_fix".into(),
                result: "fixed".into(),
                detail: None,
            });
        })
        .await?;
    }

    Ok(())
}
