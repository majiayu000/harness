use crate::task_executor::{helpers, implement_pipeline, restricted_tools};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, TaskId, TaskKind, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::agents::CapabilityProfile;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use helpers::update_status;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_non_implementation_task(
    store: &TaskStore,
    task_id: &TaskId,
    task_kind: TaskKind,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: &std::path::Path,
    server_config: &harness_core::config::HarnessConfig,
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    events: &Arc<harness_observe::event_store::EventStore>,
    skills: &Arc<RwLock<harness_skills::store::SkillStore>>,
    cargo_env: &HashMap<String, String>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
) -> anyhow::Result<()> {
    let Some(system_input) = req.system_input.as_ref() else {
        anyhow::bail!(
            "{} task is missing restart-safe input metadata",
            task_kind.as_ref()
        );
    };

    update_status(store, task_id, task_kind.execution_status(), 1).await?;

    let mut prompt = implement_pipeline::prepend_constitution(
        system_input.prompt().to_string(),
        server_config.server.constitution_enabled,
    );
    let skill_match_prompt = prompt.clone();
    let skill_additions = helpers::inject_skills_into_prompt(skills, &skill_match_prompt).await;
    prompt = helpers::inject_project_context_into_prompt(project, prompt);
    if !skill_additions.is_empty() {
        prompt.push_str(&skill_additions);
    }
    let context_items = helpers::collect_context_items(skills, project, &skill_match_prompt).await;
    let allowed_tools = Some(restricted_tools(CapabilityProfile::Standard)?);
    if let Some(note) = CapabilityProfile::Standard.prompt_note() {
        prompt = format!("{note}\n\n{prompt}");
    }

    let prompt_built_at = Utc::now();
    let initial_req = AgentRequest {
        prompt,
        project_root: project.to_path_buf(),
        context: context_items,
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(harness_core::types::ExecutionPhase::Planning),
        allowed_tools: allowed_tools.clone(),
        env_vars: cargo_env.clone(),
        ..Default::default()
    };
    let first_req = helpers::run_pre_execute(interceptors, initial_req).await?;
    let max_validation_retries: u32 = interceptors
        .iter()
        .filter_map(|i| i.max_validation_retries())
        .max()
        .unwrap_or(2);
    let mut validation_attempt = 0u32;
    let mut turn_req = first_req.clone();

    let resp = loop {
        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                anyhow::bail!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used,
                    max
                );
            }
        }
        let agent_started_at = Utc::now();
        let raw = tokio::time::timeout(
            turn_timeout,
            helpers::run_agent_streaming(
                agent,
                turn_req.clone(),
                task_id,
                store,
                1,
                prompt_built_at,
                agent_started_at,
            ),
        )
        .await;
        *turns_used += 1;
        *turns_used_acc = *turns_used;
        match raw {
            Ok(Ok(success)) => {
                let response = &success.response;
                let turn_tools = turn_req.allowed_tools.as_deref().unwrap_or(&[]);
                let tool_violations = validate_tool_usage(&response.output, turn_tools);
                let violation_err: Option<String> = if tool_violations.is_empty() {
                    None
                } else {
                    Some(format!(
                        "[VALIDATION ERROR] Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                        tool_violations.join(", "),
                        turn_tools.join(", ")
                    ))
                };
                let hook_err = {
                    let modified = helpers::detect_modified_files(project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = harness_core::interceptor::ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                            session_id: None,
                        };
                        helpers::run_post_tool_use(interceptors, &hook_event, project).await
                    }
                };
                let post_err = helpers::run_post_execute(interceptors, &turn_req, response).await;
                if let Some(err) = violation_err.or(hook_err).or(post_err) {
                    if validation_attempt < max_validation_retries {
                        validation_attempt += 1;
                        let backoff_ms = implement_pipeline::compute_backoff_ms(
                            req.retry_base_backoff_ms,
                            req.retry_max_backoff_ms,
                            validation_attempt,
                        );
                        let truncated = helpers::truncate_validation_error(&err, 2000);
                        turn_req.prompt = prompts::validation_retry_prompt(
                            &first_req.prompt,
                            validation_attempt,
                            max_validation_retries,
                            &truncated,
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    helpers::run_on_error(interceptors, &turn_req, &err).await;
                    anyhow::bail!(
                        "Post-execution validation failed after {} attempts: {}",
                        max_validation_retries,
                        err
                    );
                }
                break success;
            }
            Ok(Err(err)) => {
                helpers::run_on_error(interceptors, &turn_req, &err.error.to_string()).await;
                return Err(err.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "{} timed out after {}s",
                    task_kind.as_ref(),
                    turn_timeout.as_secs()
                );
                helpers::run_on_error(interceptors, &turn_req, &msg).await;
                anyhow::bail!(msg);
            }
        }
    };

    if implement_pipeline::contains_worktree_collision_sentinel(&resp.response.output) {
        let collision_telemetry = resp.telemetry.clone();
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 1;
            s.error = Some(
                "WorktreeCollision: agent observed worktree managed by another harness session"
                    .into(),
            );
            s.rounds.push(crate::task_runner::RoundResult::new(
                1,
                task_kind.as_ref().to_string(),
                "worktree_collision",
                if resp.response.output.is_empty() {
                    None
                } else {
                    Some(resp.response.output.clone())
                },
                Some(collision_telemetry),
                None,
            ));
        })
        .await?;
        tracing::info!(
            task_id = %task_id,
            task_kind = task_kind.as_ref(),
            status = "failed",
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
    }

    let done_telemetry = resp.telemetry.clone();
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Done;
        s.turn = 1;
        s.rounds.push(crate::task_runner::RoundResult::new(
            1,
            task_kind.as_ref().to_string(),
            "completed",
            if resp.response.output.is_empty() {
                None
            } else {
                Some(resp.response.output.clone())
            },
            Some(done_telemetry),
            None,
        ));
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Completed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
    });
    let event_name = format!("task_{}", task_kind.as_ref());
    let mut ev = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        &event_name,
        "task_runner",
        harness_core::types::Decision::Complete,
    );
    ev.detail = Some(format!("task_id={}", task_id.as_str()));
    if let Err(err) = events.log(&ev).await {
        tracing::warn!("failed to log {} event: {err}", task_kind.as_ref());
    }
    tracing::info!(
        task_id = %task_id,
        task_kind = task_kind.as_ref(),
        status = "done",
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}
