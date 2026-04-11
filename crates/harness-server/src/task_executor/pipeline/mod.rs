pub(crate) mod run;

pub(crate) use run::run_task;

use crate::task_executor::{helpers, lifecycle, pr_detection};
use crate::task_runner::{mutate_and_persist, CreateTaskRequest, TaskId, TaskStatus, TaskStore};
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::agents::CapabilityProfile;
use harness_core::types::ExecutionPhase;
use harness_core::{config::project::load_project_config, lang_detect, prompts};
use helpers::update_status;
use pr_detection::find_existing_pr_for_issue;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

/// Extract tool list from a capability profile, returning an error if the
/// profile unexpectedly returns `None` (which means Full/unrestricted).
/// A misconfigured profile causes a hard failure rather than silent degradation,
/// per U-23 (no silent capability downgrade).
pub(crate) fn restricted_tools(profile: CapabilityProfile) -> anyhow::Result<Vec<String>> {
    profile.tools().ok_or_else(|| {
        anyhow::anyhow!(
            "capability profile {:?} returned None from tools() — misconfiguration",
            profile
        )
    })
}

/// RAII guard that removes the per-task Cargo target directory on drop.
/// This ensures cleanup regardless of how `run_task` exits (success, error,
/// or timeout), preventing disk exhaustion from accumulated build artifacts.
pub(crate) struct TaskTargetDir(pub(crate) PathBuf);

impl Drop for TaskTargetDir {
    fn drop(&mut self) {
        if self.0.exists() {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                tracing::warn!(
                    path = %self.0.display(),
                    "failed to remove per-task cargo target dir: {e}"
                );
            }
        }
    }
}

/// Run the project's test commands as a hard gate before accepting LGTM.
///
/// When `custom_cmds` is non-empty (from `validation.pre_push` in project config),
/// those commands are run in order instead of language-detected defaults.
/// When `custom_cmds` is empty, falls back to language detection.
///
/// Returns `Ok(())` when all commands pass or when no test command is detectable
/// (soft degradation — unknown project type skips rather than hard-fails).
///
/// Returns `Err(output)` containing stdout/stderr of the first failing command.
pub(crate) async fn run_test_gate(
    project_root: &Path,
    custom_cmds: &[String],
    timeout_secs: u64,
    extra_env: &HashMap<String, String>,
) -> Result<(), String> {
    // Prefer explicitly configured pre_push commands; fall back to language detection.
    let cmds: Vec<String> = if !custom_cmds.is_empty() {
        // Issue 1 fix: validate every custom command against the safety allowlist
        // before executing. Malicious repos could supply shell-injection payloads
        // via `.harness/config.toml` validation.pre_push.
        for cmd in custom_cmds {
            if let Err(e) = crate::post_validator::validate_command_safety(cmd) {
                return Err(format!("test gate: command rejected by safety check: {e}"));
            }
        }
        custom_cmds.to_vec()
    } else {
        match lang_detect::primary_test_command(project_root) {
            Some(cmd) => vec![cmd],
            None => {
                tracing::info!(
                    project = %project_root.display(),
                    "test gate: no test command detected for project, skipping"
                );
                return Ok(());
            }
        }
    };

    for cmd in &cmds {
        tracing::info!(cmd = %cmd, "test gate: running tests before accepting LGTM");

        let child = match TokioCommand::new("sh")
            .args(["-c", cmd])
            .current_dir(project_root)
            // Issue 3 fix: inherit the per-task CARGO_TARGET_DIR so parallel
            // Rust tasks do not contend on the same build directory (issue #488).
            .envs(extra_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("test gate: failed to spawn `{cmd}`: {e}")),
        };

        match timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => {
                tracing::info!(cmd = %cmd, "test gate: tests passed");
            }
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let code = out.status.code().unwrap_or(-1);
                return Err(format!(
                    "Test gate failed (exit {code})\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ));
            }
            Ok(Err(e)) => return Err(format!("test gate: `{cmd}` failed to wait: {e}")),
            Err(_) => {
                return Err(format!(
                    "Test gate timed out after {timeout_secs}s (command: `{cmd}`)"
                ))
            }
        }
    }

    Ok(())
}

pub(crate) fn prepend_constitution(prompt: String, enabled: bool) -> String {
    const CONSTITUTION: &str = include_str!("../../../../../config/constitution.md");
    if enabled {
        format!("{CONSTITUTION}\n\n{prompt}")
    } else {
        prompt
    }
}

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
    turns_used_acc: &mut u32,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity)> {
    use crate::task_runner::TaskPhase;

    let prompt_text = req.prompt.as_deref().unwrap_or_default();
    tracing::info!(task_id = %task_id, "pipeline: starting plan phase for prompt-only task");

    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = format!(
        "Before implementing, produce a concise implementation plan for the following task.\n\
         List the files to change, the approach, and any non-obvious design decisions.\n\
         Do NOT write code yet — planning only.\n\n\
         Task:\n{prompt_text}"
    );

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
    // Persist the turn immediately so transient-retry restarts count this turn.
    *turns_used_acc += 1;

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.plan_output = Some(plan_text.clone());
        state.phase = TaskPhase::Implement;
    })
    .await?;

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete (prompt-only)");
    Ok((Some(plan_text), prompts::TriageComplexity::Medium))
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
    turns_used_acc: &mut u32,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity)> {
    use crate::task_runner::TaskPhase;

    // --- Phase 1: Triage ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting triage phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Triage;
    })
    .await?;

    let triage_prompt = prompts::triage_prompt(issue).to_prompt_string();
    let triage_req = AgentRequest {
        prompt: triage_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let triage_resp = tokio::time::timeout(
        turn_timeout,
        lifecycle::run_agent_streaming(agent, triage_req, task_id, store, 0),
    )
    .await
    .map_err(|_| anyhow::anyhow!("triage phase timed out after {}s", req.turn_timeout_secs))?
    .map_err(|e| anyhow::anyhow!("triage phase agent error: {e}"))?;
    // Persist triage turn immediately so transient-retry restarts count it.
    *turns_used_acc += 1;

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
            mutate_and_persist(store, task_id, |state| {
                state.status = crate::task_runner::TaskStatus::Failed;
                state.phase = TaskPhase::Terminal;
                state.error = Some("Triage: needs clarification before implementation".to_string());
            })
            .await?;
            anyhow::bail!("triage requires clarification on issue #{issue}");
        }
        prompts::TriageDecision::Proceed => {
            tracing::info!(task_id = %task_id, "triage: PROCEED — skipping plan phase");
            return Ok((None, complexity));
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

    let plan_prompt = prompts::plan_prompt(issue, &triage_resp.output).to_prompt_string();
    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let plan_resp = tokio::time::timeout(
        turn_timeout,
        lifecycle::run_agent_streaming(agent, plan_req, task_id, store, 0),
    )
    .await
    .map_err(|_| anyhow::anyhow!("plan phase timed out after {}s", req.turn_timeout_secs))?
    .map_err(|e| anyhow::anyhow!("plan phase agent error: {e}"))?;
    // Persist plan turn immediately so transient-retry restarts count it.
    *turns_used_acc += 1;

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
    Ok((Some(plan_text), complexity))
}

/// Load project config and resolve settings needed by run_task.
pub(crate) async fn load_task_config(
    store: &TaskStore,
    task_id: &TaskId,
    project: &Path,
    server_config: &harness_core::config::HarnessConfig,
) -> anyhow::Result<(
    harness_core::config::project::ProjectConfig,
    harness_core::config::resolve::ResolvedConfig,
    Option<String>, // resumed_pr_url
    Option<String>, // resumed_plan
    String,         // repo_slug
)> {
    use anyhow::Context;

    let project_config = load_project_config(project).with_context(|| {
        format!(
            "failed to load project config for task {} at {}",
            task_id.as_str(),
            project.display()
        )
    })?;
    let resolved = harness_core::config::resolve::resolve_config(server_config, &project_config);
    let repo_slug = pr_detection::detect_repo_slug(project)
        .await
        .unwrap_or_else(|| "{owner}/{repo}".to_string());

    let checkpoint = store.load_checkpoint(task_id).await?;
    let resumed_pr_url: Option<String> = store
        .get(task_id)
        .and_then(|t| t.pr_url)
        .or_else(|| checkpoint.as_ref().and_then(|c| c.pr_url.clone()));
    let resumed_plan: Option<String> = checkpoint.and_then(|c| c.plan_output);

    Ok((
        project_config,
        resolved,
        resumed_pr_url,
        resumed_plan,
        repo_slug,
    ))
}

/// Determine triage/plan output: run the triage pipeline or use checkpoint resume.
///
/// `turns_used_acc` is incremented after each completed agent call so that
/// transient-retry restarts start from the correct accumulated turn count,
/// even if the function ultimately returns `Err`.
pub(crate) async fn resolve_plan(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: &Path,
    cargo_env: &HashMap<String, String>,
    resumed_pr_url: Option<&str>,
    resumed_plan: Option<String>,
    turns_used_acc: &mut u32,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity)> {
    if resumed_pr_url.is_some() {
        return Ok((None, prompts::TriageComplexity::Medium));
    }
    if let Some(plan) = resumed_plan {
        tracing::info!(task_id = %task_id, "checkpoint resume: using saved plan, skipping triage/plan");
        return Ok((Some(plan), prompts::TriageComplexity::Medium));
    }
    if let Some(issue) = req.issue {
        let has_existing_pr = find_existing_pr_for_issue(project, issue)
            .await
            .ok()
            .flatten()
            .is_some();
        if !has_existing_pr {
            return run_triage_plan_pipeline(
                agent,
                store,
                task_id,
                issue,
                cargo_env,
                project,
                req,
                turns_used_acc,
            )
            .await;
        }
        return Ok((None, prompts::TriageComplexity::Medium));
    }
    // Planning gate (task_runner) may have forced TaskPhase::Plan for a
    // complex prompt-only task.
    let forced_plan = store
        .get(task_id)
        .map(|s| s.phase == crate::task_runner::TaskPhase::Plan)
        .unwrap_or(false);
    if forced_plan && req.issue.is_none() && req.pr.is_none() {
        update_status(store, task_id, TaskStatus::Implementing, 1).await?;
        return run_plan_for_prompt(
            agent,
            store,
            task_id,
            cargo_env,
            project,
            req,
            turns_used_acc,
        )
        .await;
    }
    Ok((None, prompts::TriageComplexity::Medium))
}
