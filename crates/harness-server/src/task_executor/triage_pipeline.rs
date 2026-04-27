use super::helpers::{
    inject_skills_into_prompt, matched_skills_for_prompt, run_agent_streaming, update_status,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskPhase, TaskStatus, TaskStore,
};
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::prompts;
use harness_core::types::{Decision, Event, ExecutionPhase, SessionId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

/// Run a plan step for a complex prompt-only task when the planning gate has
/// forced `TaskPhase::Plan`.
///
/// This gives the planning gate real effect: instead of silently skipping
/// to Implement, the agent produces a plan first, which is then threaded into
/// the implementation prompt just like issue-based plans.
pub(crate) async fn run_plan_for_prompt(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &Arc<RwLock<harness_skills::store::SkillStore>>,
    events: &Arc<harness_observe::event_store::EventStore>,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity, u32)> {
    let prompt_text = req.prompt.as_deref().unwrap_or_default();
    tracing::info!(task_id = %task_id, "pipeline: starting plan phase for prompt-only task");

    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = prompts::plan_for_prompt_task(prompt_text);
    let matched_skills = matched_skills_for_prompt(skills, &plan_prompt).await;
    let skill_additions = inject_skills_into_prompt(skills, &plan_prompt).await;
    let plan_prompt = if skill_additions.is_empty() {
        plan_prompt
    } else {
        plan_prompt + &skill_additions
    };
    for (skill_id, skill_name) in matched_skills {
        let mut skill_event = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        skill_event.reason = Some(skill_name);
        skill_event.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }

    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    // Use agent.execute() directly — NOT run_agent_streaming — to avoid the
    // state side-effects that run_agent_streaming performs: PR_URL extraction,
    // turn counter mutations, and status updates.  Those side-effects would
    // corrupt the subsequent implement phase (e.g. consuming call #0 from a
    // mock/real agent that sets pr_url before the real implementation turn).
    let plan_resp = tokio::time::timeout(turn_timeout, agent.execute(plan_req))
        .await
        .map_err(|_| anyhow::anyhow!("plan phase timed out after {}s", req.turn_timeout_secs))?
        .map_err(|e| anyhow::anyhow!("plan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;
    // NOTE: intentionally no write_checkpoint() here.  Prompt-only tasks cannot
    // be recovered after a crash (http.rs startup recovery hard-fails them because
    // the original prompt is not persisted — by design, to avoid writing
    // credentials/customer data to disk).  Writing the plan blob to the checkpoint
    // table would therefore provide no resumability benefit while writing
    // prompt-derived content (which often echoes or summarises the raw prompt) to
    // the on-disk SQLite store, violating the same privacy invariant.

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete (prompt-only)");
    Ok((Some(plan_text), prompts::TriageComplexity::Medium, 1))
}

/// Run triage → plan pipeline for a fresh issue-based task.
///
/// Returns `Some(plan_text)` if the triage decided a plan is needed and the plan
/// phase completed. Returns `None` only when triage says PROCEED (trivial issue,
/// skip planning). All failures propagate as errors — no silent fallbacks.
pub(crate) async fn run_triage_plan_pipeline(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &Arc<RwLock<harness_skills::store::SkillStore>>,
    events: &Arc<harness_observe::event_store::EventStore>,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity, u32)> {
    // --- Phase 1: Triage ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting triage phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Triage;
    })
    .await?;
    update_status(store, task_id, TaskStatus::Triaging, 0).await?;

    let triage_prompt = prompts::triage_prompt(issue).to_prompt_string();
    let matched_skills = matched_skills_for_prompt(skills, &triage_prompt).await;
    let skill_additions = inject_skills_into_prompt(skills, &triage_prompt).await;
    let triage_prompt = if skill_additions.is_empty() {
        triage_prompt
    } else {
        triage_prompt + &skill_additions
    };
    for (skill_id, skill_name) in matched_skills {
        let mut skill_event = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        skill_event.reason = Some(skill_name);
        skill_event.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }
    let triage_req = AgentRequest {
        prompt: triage_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Triage),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let (triage_resp, _) = tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(agent, triage_req, task_id, store, 0),
    )
    .await
    .map_err(|_| anyhow::anyhow!("triage phase timed out after {}s", req.turn_timeout_secs))?
    .map_err(|e| anyhow::anyhow!("triage phase agent error: {e}"))?;

    let triage_text = triage_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.triage_output = Some(triage_text.clone());
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, Some(&triage_text), None, None, "triage_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write triage checkpoint: {e}");
    }

    let decision = prompts::parse_triage(&triage_resp.output).ok_or_else(|| {
        anyhow::anyhow!("triage output unparseable — agent did not produce TRIAGE=<decision>")
    })?;
    let complexity = prompts::parse_complexity(&triage_resp.output);
    tracing::info!(task_id = %task_id, ?decision, ?complexity, "triage decision");

    match decision {
        prompts::TriageDecision::Skip => {
            mutate_and_persist(store, task_id, |state| {
                state.status = crate::task_runner::TaskStatus::Done;
                state.phase = TaskPhase::Terminal;
                state.error = Some("Triage: skipped — not worth implementing".to_string());
            })
            .await?;
            anyhow::bail!("triage decided to skip issue #{issue}");
        }
        prompts::TriageDecision::NeedsClarification => {
            // Treat as ProceedWithPlan — let the planner figure out ambiguities
            // instead of failing the task outright.
            tracing::info!(task_id = %task_id, "triage: NEEDS_CLARIFICATION → treating as PROCEED_WITH_PLAN");
        }
        prompts::TriageDecision::Proceed => {
            tracing::info!(task_id = %task_id, "triage: PROCEED — skipping plan phase");
            return Ok((None, complexity, 1));
        }
        prompts::TriageDecision::ProceedWithPlan => {
            // Fall through to plan phase.
        }
    }

    // --- Phase 2: Plan ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting plan phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;
    update_status(store, task_id, TaskStatus::Planning, 0).await?;

    let plan_prompt = prompts::plan_prompt(issue, &triage_resp.output).to_prompt_string();
    let matched_plan_skills = matched_skills_for_prompt(skills, &plan_prompt).await;
    let plan_skill_additions = inject_skills_into_prompt(skills, &plan_prompt).await;
    let plan_prompt = if plan_skill_additions.is_empty() {
        plan_prompt
    } else {
        plan_prompt + &plan_skill_additions
    };
    for (skill_id, skill_name) in matched_plan_skills {
        let mut skill_event = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        skill_event.reason = Some(skill_name);
        skill_event.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }
    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let (plan_resp, _) = tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(agent, plan_req, task_id, store, 0),
    )
    .await
    .map_err(|_| anyhow::anyhow!("plan phase timed out after {}s", req.turn_timeout_secs))?
    .map_err(|e| anyhow::anyhow!("plan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, None, Some(&plan_text), None, "plan_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write plan checkpoint: {e}");
    }

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete");
    Ok((Some(plan_text), complexity, 2))
}

/// Run a repair-plan step after an implementation attempt emitted `PLAN_ISSUE=...`.
///
/// Returns the corrected plan text and advances the task phase back to
/// `Implement`. The caller decides whether to retry implementation or fail.
pub(crate) async fn run_replan_for_issue(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    prior_plan: Option<&str>,
    plan_issue: &str,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
    skills: &Arc<RwLock<harness_skills::store::SkillStore>>,
    events: &Arc<harness_observe::event_store::EventStore>,
) -> anyhow::Result<String> {
    tracing::info!(task_id = %task_id, issue, "pipeline: starting replan phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let prompt = prompts::replan_prompt(issue, prior_plan, plan_issue).to_prompt_string();
    let matched_skills = matched_skills_for_prompt(skills, &prompt).await;
    let skill_additions = inject_skills_into_prompt(skills, &prompt).await;
    let prompt = if skill_additions.is_empty() {
        prompt
    } else {
        prompt + &skill_additions
    };
    for (skill_id, skill_name) in matched_skills {
        let mut skill_event = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        skill_event.reason = Some(skill_name);
        skill_event.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }
    let plan_req = AgentRequest {
        prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let plan_resp = tokio::time::timeout(turn_timeout, agent.execute(plan_req))
        .await
        .map_err(|_| anyhow::anyhow!("replan phase timed out after {}s", req.turn_timeout_secs))?
        .map_err(|e| anyhow::anyhow!("replan phase agent error: {e}"))?;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
        state.rounds.push(RoundResult {
            turn: state.turn,
            action: "replan".into(),
            result: "plan_ready".into(),
            detail: Some(plan_text.clone()),
            first_token_latency_ms: None,
        });
    })
    .await?;
    if let Err(e) = store
        .write_checkpoint(task_id, None, Some(&plan_text), None, "replan_done")
        .await
    {
        tracing::warn!(task_id = %task_id, "failed to write replan checkpoint: {e}");
    }

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "replan phase complete");
    Ok(plan_text)
}

/// Fire a single agent turn to rebase a conflicting PR onto `origin/main`.
///
/// Returns `true` if the agent reported `REBASE_OK` (i.e. a new commit was
/// force-pushed), `false` in every other case.  Errors are not propagated —
/// a rebase failure is not fatal; the review loop will handle the PR.
pub(crate) async fn run_rebase_turn(
    agent: &dyn CodeAgent,
    pr_num: u64,
    project: &std::path::Path,
    repo: &str,
    turn_timeout: Duration,
    cargo_env: &HashMap<String, String>,
) -> bool {
    // Fetch the branch name from GitHub so the prompt has an exact ref.
    let branch = {
        let out = tokio::process::Command::new("gh")
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "headRefName",
                "--jq",
                ".headRefName",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_matches('"')
                .to_string(),
            _ => {
                tracing::warn!(
                    pr = pr_num,
                    "run_rebase_turn: could not fetch branch name; skipping"
                );
                return false;
            }
        }
    };

    // Reject branch names that contain shell metacharacters.  The branch
    // name is embedded inside single-quoted shell commands in the agent
    // prompt; a single-quote in the name would break out of the quoting.
    if !branch
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '@' | '~' | '+' | ':'))
    {
        tracing::warn!(
            pr = pr_num,
            branch = %branch,
            "run_rebase_turn: branch name contains unsafe characters; skipping"
        );
        return false;
    }

    let prompt = harness_core::prompts::rebase_conflicting_pr(pr_num, &branch, repo, project);
    let req = AgentRequest {
        prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(harness_core::types::ExecutionPhase::Rebase),
        ..Default::default()
    };

    match tokio::time::timeout(turn_timeout, agent.execute(req)).await {
        Ok(Ok(resp)) => {
            let last = resp.output.lines().next_back().unwrap_or("").trim();
            if last.contains("REBASE_OK") {
                tracing::info!(pr = pr_num, "rebase turn: REBASE_OK");
                true
            } else {
                tracing::warn!(
                    pr = pr_num,
                    last_line = last,
                    "rebase turn: REBASE_FAILED or unexpected output"
                );
                false
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(pr = pr_num, error = %e, "rebase turn: agent error");
            false
        }
        Err(_) => {
            tracing::warn!(pr = pr_num, "rebase turn: timed out");
            false
        }
    }
}
