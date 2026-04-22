use super::helpers::{
    build_task_event, run_agent_streaming, run_agent_streaming_with_options, telemetry_for_timeout,
    RunAgentStreamingOptions,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskPhase, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::prompts;
use harness_core::types::{Decision, ExecutionPhase, TurnFailure, TurnTelemetry};
use harness_observe::event_store::EventStore;
use std::collections::HashMap;
use std::path::Path;
use tokio::time::Duration;

async fn record_phase_observability(
    store: &TaskStore,
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    action: &str,
    result: &str,
    detail: Option<String>,
    telemetry: Option<TurnTelemetry>,
    failure: Option<TurnFailure>,
    decision: Decision,
    reason: Option<String>,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |state| {
        state.rounds.push(RoundResult::new(
            turn,
            action,
            result,
            detail.clone(),
            telemetry.clone(),
            failure.clone(),
        ));
    })
    .await?;

    let event = build_task_event(
        task_id,
        turn,
        action,
        &format!("task_{action}"),
        decision,
        reason,
        None,
        telemetry,
        failure,
        detail,
    );
    if let Err(error) = events.log(&event).await {
        tracing::warn!(task_id = %task_id, action, "failed to log {action} event: {error}");
    }

    Ok(())
}

fn redact_prompt_plan_failure(failure: TurnFailure) -> TurnFailure {
    TurnFailure {
        message: None,
        body_excerpt: None,
        ..failure
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
    events: &EventStore,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity, u32)> {
    let prompt_text = req.prompt.as_deref().unwrap_or_default();
    tracing::info!(task_id = %task_id, "pipeline: starting plan phase for prompt-only task");

    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let plan_prompt = prompts::plan_for_prompt_task(prompt_text);
    let prompt_built_at = Utc::now();
    let agent_started_at = Utc::now();

    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    let plan_resp = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming_with_options(
            agent,
            plan_req,
            task_id,
            store,
            0,
            prompt_built_at,
            agent_started_at,
            RunAgentStreamingOptions {
                persist_artifacts: false,
            },
        ),
    )
    .await
    {
        Ok(Ok(success)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "completed",
                None,
                Some(success.telemetry.clone()),
                None,
                Decision::Complete,
                Some("prompt task plan completed".to_string()),
            )
            .await?;
            success.response
        }
        Ok(Err(failure)) => {
            let persisted_failure = redact_prompt_plan_failure(failure.failure.clone());
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "failed",
                None,
                Some(failure.telemetry),
                Some(persisted_failure),
                Decision::Block,
                Some("prompt task plan failed".to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!("plan phase agent error: {}", failure.error));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(prompt_built_at, agent_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "plan phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some("prompt task plan timed out".to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "plan phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

    let plan_text = plan_resp.output.clone();
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Implement;
    })
    .await?;
    // NOTE: prompt-only task plans stay in-memory only. The returned `plan_text`
    // feeds the immediate implement phase, but we intentionally do not persist
    // it in task state, round detail, or checkpoints because it can echo raw
    // user prompt content, including secrets.

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
    events: &EventStore,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
) -> anyhow::Result<(Option<String>, prompts::TriageComplexity, u32)> {
    // --- Phase 1: Triage ---
    tracing::info!(task_id = %task_id, issue, "pipeline: starting triage phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Triage;
    })
    .await?;

    let triage_prompt = prompts::triage_prompt(issue).to_prompt_string();
    let triage_prompt_built_at = Utc::now();
    let triage_started_at = Utc::now();
    let triage_req = AgentRequest {
        prompt: triage_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Triage),
        ..Default::default()
    };

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let triage_resp = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(
            agent,
            triage_req,
            task_id,
            store,
            0,
            triage_prompt_built_at,
            triage_started_at,
        ),
    )
    .await
    {
        Ok(Ok(success)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "completed",
                Some(success.response.output.clone()),
                Some(success.telemetry.clone()),
                None,
                Decision::Complete,
                Some(format!("issue #{issue} triage completed")),
            )
            .await?;
            success.response
        }
        Ok(Err(failure)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "failed",
                None,
                Some(failure.telemetry),
                Some(failure.failure),
                Decision::Block,
                Some(format!("issue #{issue} triage failed")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "triage phase agent error: {}",
                failure.error
            ));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(triage_prompt_built_at, triage_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "triage phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "triage",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some(format!("issue #{issue} triage timed out")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "triage phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

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
                state.status = TaskStatus::Done;
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

    let plan_prompt = prompts::plan_prompt(issue, &triage_resp.output).to_prompt_string();
    let plan_prompt_built_at = Utc::now();
    let plan_started_at = Utc::now();
    let plan_req = AgentRequest {
        prompt: plan_prompt,
        project_root: project.to_path_buf(),
        env_vars: cargo_env.clone(),
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    let plan_resp = match tokio::time::timeout(
        turn_timeout,
        run_agent_streaming(
            agent,
            plan_req,
            task_id,
            store,
            0,
            plan_prompt_built_at,
            plan_started_at,
        ),
    )
    .await
    {
        Ok(Ok(success)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "completed",
                Some(success.response.output.clone()),
                Some(success.telemetry.clone()),
                None,
                Decision::Complete,
                Some(format!("issue #{issue} plan completed")),
            )
            .await?;
            success.response
        }
        Ok(Err(failure)) => {
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "failed",
                None,
                Some(failure.telemetry),
                Some(failure.failure),
                Decision::Block,
                Some(format!("issue #{issue} plan failed")),
            )
            .await?;
            return Err(anyhow::anyhow!("plan phase agent error: {}", failure.error));
        }
        Err(_) => {
            let telemetry =
                telemetry_for_timeout(plan_prompt_built_at, plan_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!(
                    "plan phase timed out after {}s",
                    req.turn_timeout_secs
                )),
                body_excerpt: None,
            };
            record_phase_observability(
                store,
                events,
                task_id,
                0,
                "plan",
                "timeout",
                None,
                Some(telemetry),
                Some(failure),
                Decision::Block,
                Some(format!("issue #{issue} plan timed out")),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "plan phase timed out after {}s",
                req.turn_timeout_secs
            ));
        }
    };

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
) -> anyhow::Result<String> {
    tracing::info!(task_id = %task_id, issue, "pipeline: starting replan phase");
    mutate_and_persist(store, task_id, |state| {
        state.phase = TaskPhase::Plan;
    })
    .await?;

    let prompt = prompts::replan_prompt(issue, prior_plan, plan_issue).to_prompt_string();
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
        state.rounds.push(RoundResult::new(
            state.turn,
            "replan",
            "plan_ready",
            Some(plan_text.clone()),
            None,
            None,
        ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::agent::{AgentResponse, StreamItem};
    use harness_core::error::HarnessError;
    use harness_core::types::{Capability, EventFilters, TokenUsage};

    struct PromptPlanningAgent;

    struct FailingPromptPlanningAgent;

    #[async_trait]
    impl CodeAgent for PromptPlanningAgent {
        fn name(&self) -> &str {
            "prompt-planner"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Ok(AgentResponse {
                output: "step 1\nstep 2".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            tx.send(StreamItem::ItemCompleted {
                item: harness_core::types::Item::AgentReasoning {
                    content: "step 1\nstep 2".to_string(),
                },
            })
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
            tx.send(StreamItem::Done)
                .await
                .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
            Ok(())
        }
    }

    #[async_trait]
    impl CodeAgent for FailingPromptPlanningAgent {
        fn name(&self) -> &str {
            "prompt-planner"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Err(HarnessError::AgentExecution(
                "planner echoed secret prompt".to_string(),
            ))
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            Err(HarnessError::AgentExecution(
                "planner exited with exit status: 1: stdout_tail=[secret prompt]".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn run_plan_for_prompt_keeps_prompt_only_plan_text_out_of_rounds_and_events() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db"))
            .await
            .expect("task store");
        let events = EventStore::new(dir.path()).await.expect("event store");
        let task_id = crate::task_runner::TaskId::new();
        let mut task = crate::task_runner::TaskState::new(task_id.clone());
        task.description = Some("prompt task".to_string());
        store.insert(&task).await;

        let req = CreateTaskRequest {
            prompt: Some("secret prompt".to_string()),
            ..Default::default()
        };

        let (plan, complexity, turns) = run_plan_for_prompt(
            &PromptPlanningAgent,
            &store,
            &task_id,
            &events,
            &HashMap::new(),
            dir.path(),
            &req,
        )
        .await
        .expect("prompt plan should succeed");

        assert_eq!(plan.as_deref(), Some("step 1\nstep 2"));
        assert_eq!(complexity, prompts::TriageComplexity::Medium);
        assert_eq!(turns, 1);

        let task = store.get(&task_id).expect("task state");
        assert_eq!(task.rounds.len(), 1);
        assert_eq!(task.rounds[0].action, "plan");
        assert_eq!(task.rounds[0].detail, None);

        let events = events
            .query(&EventFilters {
                hook: Some("task_plan".to_string()),
                ..Default::default()
            })
            .await
            .expect("query events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, None);
        assert_eq!(
            events[0].reason.as_deref(),
            Some("prompt task plan completed")
        );
    }

    #[tokio::test]
    async fn run_plan_for_prompt_redacts_failure_metadata_before_persisting() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db"))
            .await
            .expect("task store");
        let events = EventStore::new(dir.path()).await.expect("event store");
        let task_id = crate::task_runner::TaskId::new();
        let mut task = crate::task_runner::TaskState::new(task_id.clone());
        task.description = Some("prompt task".to_string());
        store.insert(&task).await;

        let req = CreateTaskRequest {
            prompt: Some("secret prompt".to_string()),
            ..Default::default()
        };

        let err = run_plan_for_prompt(
            &FailingPromptPlanningAgent,
            &store,
            &task_id,
            &events,
            &HashMap::new(),
            dir.path(),
            &req,
        )
        .await
        .expect_err("prompt plan should fail");
        assert!(
            err.to_string().contains("plan phase agent error"),
            "unexpected error: {err}"
        );

        let task = store.get(&task_id).expect("task state");
        assert_eq!(task.rounds.len(), 1);
        let failure = task.rounds[0].failure.as_ref().expect("round failure");
        assert!(
            failure.message.is_none(),
            "failure message should be redacted"
        );
        assert!(
            failure.body_excerpt.is_none(),
            "failure body excerpt should be redacted"
        );

        let events = events
            .query(&EventFilters {
                hook: Some("task_plan".to_string()),
                ..Default::default()
            })
            .await
            .expect("query events");
        assert_eq!(events.len(), 1);
        let failure = events[0]
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.failure.as_ref())
            .expect("event failure metadata");
        assert!(
            failure.message.is_none(),
            "event failure message should be redacted"
        );
        assert!(
            failure.body_excerpt.is_none(),
            "event failure body excerpt should be redacted"
        );
    }

    #[test]
    fn redact_prompt_plan_failure_clears_message_and_excerpt() {
        let failure = TurnFailure {
            kind: harness_core::types::TurnFailureKind::Unknown,
            provider: Some("codex".to_string()),
            upstream_status: Some(1),
            message: Some("secret prompt".to_string()),
            body_excerpt: Some("still secret".to_string()),
        };

        let redacted = redact_prompt_plan_failure(failure);
        assert_eq!(redacted.provider.as_deref(), Some("codex"));
        assert_eq!(redacted.upstream_status, Some(1));
        assert!(redacted.message.is_none());
        assert!(redacted.body_excerpt.is_none());
    }
}
