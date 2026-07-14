use super::{compute_backoff_ms, implementation_failure_result};
use crate::task_executor::helpers::{
    build_task_event, detect_modified_files, run_agent_streaming_with_options, run_on_error,
    run_post_execute, run_post_tool_use, run_pre_execute, telemetry_for_timeout,
    RunAgentStreamingOptions,
};
use crate::task_runner::{mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStore};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::interceptor::{ToolUseEvent, TurnInterceptor};
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{
    ContextItem, Decision, ExecutionPhase, TurnFailure, TurnFailureKind, TurnTelemetry,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};

pub(super) struct ExecutedImplementation {
    pub(super) output: String,
    pub(super) telemetry: TurnTelemetry,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_implementation_turn(
    agent: &dyn CodeAgent,
    first_prompt: String,
    context_items: Vec<ContextItem>,
    initial_allowed_tools: Option<Vec<String>>,
    cargo_env: &HashMap<String, String>,
    req: &CreateTaskRequest,
    store: &TaskStore,
    task_id: &TaskId,
    interceptors: &[Arc<dyn TurnInterceptor>],
    events: &Arc<harness_observe::event_store::EventStore>,
    project: &Path,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
) -> anyhow::Result<ExecutedImplementation> {
    let impl_phase_start = Instant::now();

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: project.to_path_buf(),
        context: context_items.clone(),
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(ExecutionPhase::Execution),
        allowed_tools: initial_allowed_tools.clone(),
        env_vars: cargo_env.clone(),
        ..Default::default()
    };

    // Run pre_execute interceptors; Block aborts the task.
    let first_req = run_pre_execute(interceptors, initial_req).await?;

    // Execute implementation turn with post-execution validation and auto-retry.
    // Use the largest max_retries declared by any interceptor.
    // A single interceptor returning 0 should not suppress retries for others.
    let max_validation_retries: u32 = interceptors
        .iter()
        .filter_map(|i| i.max_validation_retries())
        .max()
        .unwrap_or(2);
    let mut validation_attempt = 0u32;
    let mut impl_req = first_req.clone();
    let is_auto_fix = store
        .get(task_id)
        .is_some_and(|s| s.source.as_deref() == Some("auto-fix"));
    let impl_options = RunAgentStreamingOptions {
        persist_artifacts: true,
        backfill_auto_fix_issue: is_auto_fix,
    };

    let resp = loop {
        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used,
                    max
                ));
            }
        }
        let prompt_built_at = Utc::now();
        let agent_started_at = Utc::now();
        let raw = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming_with_options(
                agent,
                impl_req.clone(),
                task_id,
                store,
                1,
                prompt_built_at,
                agent_started_at,
                impl_options,
            ),
        )
        .await;
        *turns_used += 1;
        *turns_used_acc = *turns_used;
        match raw {
            Ok(Ok(success)) => {
                let mut impl_telemetry = success.telemetry.clone();
                impl_telemetry.retry_count = Some(validation_attempt);
                let r = success.response;
                // Post-execution tool isolation check (defense-in-depth alongside
                // --allowedTools CLI enforcement). Violations feed into the retry loop
                // so the agent gets a chance to self-correct (fail-closed, not fail-open).
                let impl_tools = impl_req.allowed_tools.as_deref().unwrap_or(&[]);
                let tool_violations = validate_tool_usage(&r.output, impl_tools);
                let violation_err: Option<String> = if !tool_violations.is_empty() {
                    let msg = format!(
                    "[VALIDATION ERROR] Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                    tool_violations.join(", "),
                    impl_tools.join(", ")
                );
                    tracing::warn!(
                        ?tool_violations,
                        "implementation turn: agent used tools outside allowed list"
                    );
                    Some(msg)
                } else {
                    None
                };
                // PreToolUse / PostToolUse hook injection point:
                // detect files written during this turn and fire post_tool_use hooks.
                let hook_err = {
                    let modified = detect_modified_files(project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                            session_id: None,
                        };
                        run_post_tool_use(interceptors, &hook_event, project).await
                    }
                };
                let post_err = run_post_execute(interceptors, &impl_req, &r).await;
                let combined_err = violation_err.or(hook_err).or(post_err);
                if let Some(err) = combined_err {
                    if validation_attempt < max_validation_retries {
                        validation_attempt += 1;
                        let backoff_ms = compute_backoff_ms(
                            req.retry_base_backoff_ms,
                            req.retry_max_backoff_ms,
                            validation_attempt,
                        );
                        tracing::warn!(
                            attempt = validation_attempt,
                            max = max_validation_retries,
                            backoff_ms,
                            error = %err,
                            "post-execution validation failed; backing off before retry"
                        );
                        let truncated =
                            crate::task_executor::helpers::truncate_validation_error(&err, 2000);
                        impl_req.prompt = prompts::validation_retry_prompt(
                            &first_req.prompt,
                            validation_attempt,
                            max_validation_retries,
                            &truncated,
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    } else {
                        tracing::error!(
                            max = max_validation_retries,
                            error = %err,
                            "post-execution validation failed after max retries; aborting task"
                        );
                        run_on_error(interceptors, &impl_req, &err).await;
                        let failure = TurnFailure {
                            kind: TurnFailureKind::Protocol,
                            provider: Some(agent.name().to_string()),
                            upstream_status: None,
                            message: Some(err.clone()),
                            body_excerpt: None,
                        };
                        mutate_and_persist(store, task_id, |s| {
                            s.rounds.push(RoundResult::new(
                                1,
                                "implement",
                                implementation_failure_result(&failure),
                                Some(r.output.clone()),
                                Some(impl_telemetry.clone()),
                                Some(failure.clone()),
                            ));
                        })
                        .await?;
                        let event = build_task_event(
                            task_id,
                            1,
                            "implement",
                            "task_implement",
                            Decision::Block,
                            Some("implementation validation failed".to_string()),
                            None,
                            Some(impl_telemetry.clone()),
                            Some(failure),
                            Some(r.output.clone()),
                        );
                        if let Err(log_err) = events.log(&event).await {
                            tracing::warn!("failed to log task_implement event: {log_err}");
                        }
                        return Err(anyhow::anyhow!(
                            "Post-execution validation failed after {} attempts: {}",
                            max_validation_retries,
                            err
                        ));
                    }
                }
                break (r, impl_telemetry);
            }
            Ok(Err(failure)) => {
                let mut telemetry = failure.telemetry.clone();
                telemetry.retry_count = Some(validation_attempt);
                run_on_error(interceptors, &impl_req, &failure.error.to_string()).await;
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        1,
                        "implement",
                        implementation_failure_result(&failure.failure),
                        None,
                        Some(telemetry.clone()),
                        Some(failure.failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    1,
                    "implement",
                    "task_implement",
                    Decision::Block,
                    Some("implementation failed".to_string()),
                    None,
                    Some(telemetry),
                    Some(failure.failure.clone()),
                    None,
                );
                if let Err(log_err) = events.log(&event).await {
                    tracing::warn!("failed to log task_implement event: {log_err}");
                }
                return Err(failure.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "Implementation turn timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &impl_req, &msg).await;
                let telemetry = telemetry_for_timeout(
                    prompt_built_at,
                    agent_started_at,
                    Utc::now(),
                    Some(validation_attempt),
                );
                let failure = TurnFailure {
                    kind: TurnFailureKind::Timeout,
                    provider: Some(agent.name().to_string()),
                    upstream_status: None,
                    message: Some(msg.clone()),
                    body_excerpt: None,
                };
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        1,
                        "implement",
                        "timeout",
                        None,
                        Some(telemetry.clone()),
                        Some(failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    1,
                    "implement",
                    "task_implement",
                    Decision::Block,
                    Some("implementation timed out".to_string()),
                    None,
                    Some(telemetry),
                    Some(failure),
                    None,
                );
                if let Err(log_err) = events.log(&event).await {
                    tracing::warn!("failed to log task_implement event: {log_err}");
                }
                return Err(anyhow::anyhow!("{msg}"));
            }
        }
    };

    let (
        AgentResponse {
            output,
            stderr,
            token_usage: impl_token_usage,
            ..
        },
        impl_telemetry,
    ) = resp;

    tracing::info!(
        task_id = %task_id,
        phase = "implementing",
        elapsed_secs = impl_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    {
        let preview: String = output.chars().take(200).collect();
        tracing::info!(
            task_id = %task_id,
            output_chars = output.len(),
            preview = %preview,
            input_tokens = impl_token_usage.input_tokens,
            output_tokens = impl_token_usage.output_tokens,
            "agent_output_summary"
        );
    }

    if !stderr.is_empty() {
        tracing::warn!(stderr = %stderr, "agent stderr during implementation");
    }

    // Append stderr to output so that sentinel parsers (parse_pr_url, etc.) see
    // the full combined text. For streaming adapters stderr is always empty here;
    // for non-streaming adapters (execute path) it carries real process output.
    let output = if !stderr.is_empty() {
        format!("{output}\n--- stderr ---\n{stderr}")
    } else {
        output
    };

    Ok(ExecutedImplementation {
        output,
        telemetry: impl_telemetry,
    })
}
