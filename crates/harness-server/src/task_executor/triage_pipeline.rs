/// Triage → Plan pipeline module.
///
/// Handles the initial classification phase for a new task:
/// - If a checkpoint or existing PR is found, returns `Proceed` immediately
///   with the saved plan so the orchestrator can skip triage/planning.
/// - For fresh issue-based tasks: runs triage, then optionally the plan phase.
/// - For complex prompt-only tasks (planning gate active): runs the plan phase.
use crate::task_runner::{mutate_and_persist, TaskPhase, TaskStatus};
use harness_core::agent::AgentRequest;
use harness_core::prompts;
use harness_core::types::ExecutionPhase;
use tokio::time::timeout;

use super::TaskContext;

/// Outcome returned by the triage/plan pipeline to the orchestrator.
pub(crate) enum TriageOutcome {
    /// Triage decided to skip this issue — task is already marked Done.
    Skip,
    /// Proceed with implementation.
    Proceed {
        /// Optional plan text (Some when triage emitted PROCEED_WITH_PLAN, or when
        /// a prompt-only task's complexity gate forced a plan phase).
        plan: Option<String>,
        /// Complexity classification for deriving default review round count.
        complexity: prompts::TriageComplexity,
        /// Number of agent turns consumed by this pipeline (0, 1, or 2).
        pipeline_turns: u32,
    },
}

/// Run the triage → plan pipeline.
///
/// Handles checkpoint resume, existing-PR detection, issue-based triage, and
/// prompt-only planning. Returns `TriageOutcome::Skip` when the issue should be
/// skipped; otherwise returns `TriageOutcome::Proceed` with an optional plan.
pub(crate) async fn run(
    ctx: &TaskContext<'_>,
    resumed_pr_url: Option<&str>,
    resumed_plan: Option<String>,
) -> anyhow::Result<TriageOutcome> {
    // PR already exists from a previous run — skip triage/plan entirely.
    if resumed_pr_url.is_some() {
        return Ok(TriageOutcome::Proceed {
            plan: None,
            complexity: prompts::TriageComplexity::Medium,
            pipeline_turns: 0,
        });
    }

    // Plan checkpoint found — use saved plan, skip triage/plan pipeline.
    if let Some(plan) = resumed_plan {
        tracing::info!(
            task_id = %ctx.task_id,
            "checkpoint resume: using saved plan, skipping triage/plan"
        );
        return Ok(TriageOutcome::Proceed {
            plan: Some(plan),
            complexity: prompts::TriageComplexity::Medium,
            pipeline_turns: 0,
        });
    }

    if let Some(issue) = ctx.req.issue {
        // Only triage fresh issues (no existing PR to continue).
        let has_existing_pr = super::pr_detection::find_existing_pr_for_issue(&ctx.project, issue)
            .await
            .ok()
            .flatten()
            .is_some();
        if !has_existing_pr {
            return run_triage_plan_pipeline(ctx, issue).await;
        }
        // Existing PR found — skip triage/plan.
        return Ok(TriageOutcome::Proceed {
            plan: None,
            complexity: prompts::TriageComplexity::Medium,
            pipeline_turns: 0,
        });
    }

    // Planning gate (task_runner) may have forced TaskPhase::Plan for a complex
    // prompt-only task. Check the stored phase so the gate has real effect.
    let forced_plan = ctx
        .store
        .get(ctx.task_id)
        .map(|s| s.phase == TaskPhase::Plan)
        .unwrap_or(false);
    if forced_plan && ctx.req.issue.is_none() && ctx.req.pr.is_none() {
        // Set to Implementing BEFORE the plan phase so that a crash during planning
        // leaves the task in 'implementing' status. The startup recovery code will
        // catch it and fail-close it (no pr_url → mark failed), rather than leaving
        // it stuck as a plain 'pending' that is never re-dispatched.
        super::helpers::update_status(ctx.store, ctx.task_id, TaskStatus::Implementing, 1).await?;
        return run_plan_for_prompt(ctx).await;
    }

    Ok(TriageOutcome::Proceed {
        plan: None,
        complexity: prompts::TriageComplexity::Medium,
        pipeline_turns: 0,
    })
}

/// Run a plan step for a complex prompt-only task when the planning gate has
/// forced `TaskPhase::Plan`.
async fn run_plan_for_prompt(ctx: &TaskContext<'_>) -> anyhow::Result<TriageOutcome> {
    let prompt_text = ctx.req.prompt.as_deref().unwrap_or_default();
    tracing::info!(
        task_id = %ctx.task_id,
        "pipeline: starting plan phase for prompt-only task"
    );

    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = prompts::plan_for_task_prompt(prompt_text);

    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: ctx.project.clone(),
        env_vars: ctx.cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    // Use agent.execute() directly — NOT run_agent_streaming — to avoid the
    // state side-effects that run_agent_streaming performs: PR_URL extraction,
    // turn counter mutations, and status updates. Those side-effects would
    // corrupt the subsequent implement phase.
    let plan_resp = timeout(ctx.turn_timeout, ctx.agent.execute(plan_req))
        .await
        .map_err(|_| anyhow::anyhow!("plan phase timed out after {}s", ctx.req.turn_timeout_secs))?
        .map_err(|e| anyhow::anyhow!("plan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;

    tracing::info!(
        task_id = %ctx.task_id,
        plan_len = plan_text.len(),
        "plan phase complete (prompt-only)"
    );
    Ok(TriageOutcome::Proceed {
        plan: Some(plan_text),
        complexity: prompts::TriageComplexity::Medium,
        pipeline_turns: 1,
    })
}

/// Run triage → plan pipeline for a fresh issue-based task.
///
/// Returns `TriageOutcome::Skip` if triage decides to skip the issue.
/// Returns `TriageOutcome::Proceed` with an optional plan and complexity.
/// All failures propagate as errors — no silent fallbacks.
async fn run_triage_plan_pipeline(
    ctx: &TaskContext<'_>,
    issue: u64,
) -> anyhow::Result<TriageOutcome> {
    // --- Phase 1: Triage ---
    tracing::info!(task_id = %ctx.task_id, issue, "pipeline: starting triage phase");
    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.phase = TaskPhase::Triage;
    })
    .await?;

    let triage_prompt = prompts::triage_prompt(issue).to_prompt_string();
    let triage_req = AgentRequest {
        prompt: triage_prompt,
        project_root: ctx.project.clone(),
        env_vars: ctx.cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let (triage_resp, _) = timeout(
        ctx.turn_timeout,
        super::run_agent_streaming(ctx.agent, triage_req, ctx.task_id, ctx.store, 0),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "triage phase timed out after {}s",
            ctx.req.turn_timeout_secs
        )
    })?
    .map_err(|e| anyhow::anyhow!("triage phase agent error: {e}"))?;

    let triage_text = triage_resp.output.clone();
    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.triage_output = Some(triage_text.clone());
    })
    .await?;
    if let Err(e) = ctx
        .store
        .write_checkpoint(ctx.task_id, Some(&triage_text), None, None, "triage_done")
        .await
    {
        tracing::warn!(task_id = %ctx.task_id, "failed to write triage checkpoint: {e}");
    }

    let decision = prompts::parse_triage(&triage_resp.output).ok_or_else(|| {
        anyhow::anyhow!("triage output unparseable — agent did not produce TRIAGE=<decision>")
    })?;
    let complexity = prompts::parse_complexity(&triage_resp.output);
    tracing::info!(task_id = %ctx.task_id, ?decision, ?complexity, "triage decision");

    match decision {
        prompts::TriageDecision::Skip => {
            mutate_and_persist(ctx.store, ctx.task_id, |state| {
                state.status = TaskStatus::Done;
                state.phase = TaskPhase::Terminal;
                state.error = Some("Triage: skipped — not worth implementing".to_string());
            })
            .await?;
            return Ok(TriageOutcome::Skip);
        }
        prompts::TriageDecision::NeedsClarification => {
            // Treat as ProceedWithPlan — let the planner figure out ambiguities
            // instead of failing the task outright.
            tracing::info!(
                task_id = %ctx.task_id,
                "triage: NEEDS_CLARIFICATION → treating as PROCEED_WITH_PLAN"
            );
        }
        prompts::TriageDecision::Proceed => {
            tracing::info!(task_id = %ctx.task_id, "triage: PROCEED — skipping plan phase");
            return Ok(TriageOutcome::Proceed {
                plan: None,
                complexity,
                pipeline_turns: 1,
            });
        }
        prompts::TriageDecision::ProceedWithPlan => {
            // Fall through to plan phase.
        }
    }

    // --- Phase 2: Plan ---
    tracing::info!(task_id = %ctx.task_id, issue, "pipeline: starting plan phase");
    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = prompts::plan_prompt(issue, &triage_resp.output).to_prompt_string();
    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: ctx.project.clone(),
        env_vars: ctx.cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let (plan_resp, _) = timeout(
        ctx.turn_timeout,
        super::run_agent_streaming(ctx.agent, plan_req, ctx.task_id, ctx.store, 0),
    )
    .await
    .map_err(|_| anyhow::anyhow!("plan phase timed out after {}s", ctx.req.turn_timeout_secs))?
    .map_err(|e| anyhow::anyhow!("plan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(ctx.store, ctx.task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;
    if let Err(e) = ctx
        .store
        .write_checkpoint(ctx.task_id, None, Some(&plan_text), None, "plan_done")
        .await
    {
        tracing::warn!(task_id = %ctx.task_id, "failed to write plan checkpoint: {e}");
    }

    tracing::info!(
        task_id = %ctx.task_id,
        plan_len = plan_text.len(),
        "plan phase complete"
    );
    Ok(TriageOutcome::Proceed {
        plan: Some(plan_text),
        complexity,
        pipeline_turns: 2,
    })
}
