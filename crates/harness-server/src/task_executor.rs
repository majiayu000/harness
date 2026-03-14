mod helpers;
mod pr_detection;

use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use harness_core::{
    config::load_project_config, interceptor::ToolUseEvent, prompts, AgentRequest, AgentResponse,
    CodeAgent, ContextItem, Event, ExecutionPhase, HarnessError, Item, SessionId, StreamItem,
    ThreadId, TokenUsage, TurnId, TurnStatus,
};
use harness_protocol::{Notification, RpcNotification};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration, Instant};

pub(crate) use helpers::{
    collect_context_items, detect_modified_files, emit_runtime_notification, mark_turn_failed,
    persist_runtime_thread, process_stream_item, run_on_error, run_post_execute, run_post_tool_use,
    run_pre_execute, truncate_validation_error, update_status,
};
pub(crate) use pr_detection::{
    build_fix_ci_prompt, build_pr_approved_prompt, build_pr_rework_prompt,
    find_existing_pr_for_issue, parse_harness_mention_command, HarnessMentionCommand,
};
// PromptBuilder is used internally by pr_detection and re-exported for tests.
#[cfg(test)]
pub(crate) use pr_detection::PromptBuilder;

pub(crate) async fn run_turn_lifecycle(
    server: Arc<crate::server::HarnessServer>,
    thread_db: Option<crate::thread_db::ThreadDb>,
    notify_tx: Option<crate::notify::NotifySender>,
    notification_tx: tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: ThreadId,
    turn_id: TurnId,
    prompt: String,
    agent_name: String,
) {
    let Some(project_root) = server
        .thread_manager
        .get_thread(&thread_id)
        .map(|thread| thread.project_root)
    else {
        tracing::warn!(
            "run_turn_lifecycle skipped because thread {} no longer exists",
            thread_id
        );
        return;
    };

    let Some(agent) = server.agent_registry.get(&agent_name) else {
        mark_turn_failed(
            &server,
            &thread_db,
            &notify_tx,
            &notification_tx,
            &thread_id,
            &turn_id,
            format!("agent `{agent_name}` not found in registry"),
        )
        .await;
        return;
    };

    let req = AgentRequest {
        prompt,
        project_root,
        ..Default::default()
    };

    let stall_timeout = Duration::from_secs(server.config.concurrency.stall_timeout_secs);
    let (stream_tx, mut stream_rx) = mpsc::channel(128);
    let mut execution = std::pin::pin!(agent.execute_stream(req, stream_tx));
    let mut stream_closed = false;
    let mut execution_result: Option<harness_core::Result<()>> = None;
    let mut last_activity = Instant::now();

    'outer: while execution_result.is_none() || !stream_closed {
        tokio::select! {
            result = &mut execution, if execution_result.is_none() => {
                execution_result = Some(result);
            }
            incoming = stream_rx.recv(), if !stream_closed => {
                match incoming {
                    Some(item) => {
                        last_activity = Instant::now();
                        process_stream_item(
                            &server,
                            &thread_db,
                            &notify_tx,
                            &notification_tx,
                            &thread_id,
                            &turn_id,
                            item,
                        ).await;
                    }
                    None => {
                        stream_closed = true;
                    }
                }
            }
            _ = tokio::time::sleep_until(last_activity + stall_timeout) => {
                let elapsed = last_activity.elapsed();
                tracing::warn!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    elapsed_secs = elapsed.as_secs(),
                    "agent stream stall detected; no output for {}s",
                    stall_timeout.as_secs()
                );
                mark_turn_failed(
                    &server,
                    &thread_db,
                    &notify_tx,
                    &notification_tx,
                    &thread_id,
                    &turn_id,
                    format!(
                        "Agent stream stalled: no output for {}s",
                        stall_timeout.as_secs()
                    ),
                )
                .await;
                break 'outer;
            }
        }
    }

    match execution_result.unwrap_or_else(|| {
        Err(harness_core::HarnessError::AgentExecution(
            "turn execution ended without agent result".to_string(),
        ))
    }) {
        Ok(()) => match server.thread_manager.complete_turn(&thread_id, &turn_id) {
            Ok(Some(usage)) => {
                persist_runtime_thread(&thread_db, &server, &thread_id).await;
                emit_runtime_notification(
                    &notify_tx,
                    &notification_tx,
                    Notification::TurnCompleted {
                        turn_id: turn_id.clone(),
                        status: TurnStatus::Completed,
                        token_usage: usage,
                    },
                );
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("failed to complete turn after execution: {err}"),
        },
        Err(err) => {
            let error_msg = err.to_string();
            if let Err(e) = server.thread_manager.add_item(
                &thread_id,
                &turn_id,
                harness_core::Item::Error {
                    code: -1,
                    message: error_msg.clone(),
                },
            ) {
                tracing::warn!("failed to add error item to turn: {e}");
            } else {
                persist_runtime_thread(&thread_db, &server, &thread_id).await;
            }
            mark_turn_failed(
                &server,
                &thread_db,
                &notify_tx,
                &notification_tx,
                &thread_id,
                &turn_id,
                error_msg,
            )
            .await;
        }
    }
}

/// Compute exponential backoff: `min(base_ms * 2^(attempt-1), max_ms)`.
///
/// - Attempt 1: `base_ms`
/// - Attempt 2: `base_ms * 2`
/// - Attempt 3: `base_ms * 4`
/// - …capped at `max_ms`
fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

/// Execute an agent request via [`CodeAgent::execute_stream`], broadcasting
/// each [`StreamItem`] to the per-task channel in real time, and reconstruct
/// an [`AgentResponse`] from the collected stream events.
async fn run_agent_streaming(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
) -> harness_core::Result<AgentResponse> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamItem>(128);
    let mut exec = std::pin::pin!(agent.execute_stream(req, tx));
    let mut exec_result: Option<harness_core::Result<()>> = None;
    let mut channel_closed = false;
    let mut output = String::new();
    let mut token_usage = TokenUsage::default();

    loop {
        tokio::select! {
            result = &mut exec, if exec_result.is_none() => {
                exec_result = Some(result);
            }
            item = rx.recv(), if !channel_closed => {
                match item {
                    Some(item) => {
                        store.publish_stream_item(task_id, item.clone());
                        match &item {
                            StreamItem::MessageDelta { text } => {
                                output.push_str(text);
                            }
                            StreamItem::ItemCompleted {
                                item: Item::AgentReasoning { content },
                            } => {
                                // Prefer the full content over accumulated deltas.
                                output = content.clone();
                            }
                            StreamItem::TokenUsage { usage } => {
                                token_usage = usage.clone();
                            }
                            StreamItem::Done => {
                                channel_closed = true;
                            }
                            _ => {}
                        }
                    }
                    None => {
                        channel_closed = true;
                    }
                }
            }
        }
        if exec_result.is_some() && channel_closed {
            break;
        }
    }

    match exec_result.unwrap_or_else(|| {
        Err(HarnessError::AgentExecution(
            "agent execution completed without result".into(),
        ))
    }) {
        Ok(()) => Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: Vec::new(),
            token_usage,
            model: String::new(),
            exit_code: Some(0),
        }),
        Err(e) => Err(e),
    }
}

/// Resolve the reviewer agent using the effective (post-project-override) review config.
///
/// Called after `resolve_config` so that project-level `[review] enabled = true`
/// can activate review even when the server default is disabled.
pub(crate) fn resolve_reviewer(
    registry: &harness_agents::AgentRegistry,
    config: &harness_core::AgentReviewConfig,
    implementor_name: &str,
) -> Option<std::sync::Arc<dyn CodeAgent>> {
    if !config.enabled {
        return None;
    }

    // Explicit reviewer
    if !config.reviewer_agent.is_empty() {
        if config.reviewer_agent == implementor_name {
            tracing::warn!(
                "agents.review.reviewer_agent == implementor '{}', skipping agent review",
                implementor_name
            );
            return None;
        }
        if let Some(agent) = registry.get(&config.reviewer_agent) {
            return Some(agent);
        }
        tracing::warn!(
            "agents.review.reviewer_agent '{}' not registered, skipping agent review",
            config.reviewer_agent
        );
        return None;
    }

    // Auto-select: first registered agent that isn't the implementor
    for name in registry.list() {
        if name != implementor_name {
            if let Some(agent) = registry.get(name) {
                return Some(agent);
            }
        }
    }

    None
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    registry: &harness_agents::AgentRegistry,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    server_config: &harness_core::HarnessConfig,
) -> anyhow::Result<()> {
    update_status(store, task_id, TaskStatus::Implementing, 1).await;

    let project_config = load_project_config(&project);
    let resolved = harness_core::config::resolve_config(server_config, &project_config);
    let review_config = &resolved.review;
    let git = Some(&project_config.git);

    // Fix 1: resolve reviewer after project config is loaded, so project-level
    // `[review] enabled = true` can override a globally-disabled review.
    let reviewer_arc = resolve_reviewer(registry, review_config, agent.name());
    let reviewer = reviewer_arc.as_deref();

    // Fix 2: store the effective bot command in TaskState so the completion
    // callback uses the project-specific command, not the global one.
    mutate_and_persist(store, task_id, |s| {
        s.review_bot_command = Some(review_config.review_bot_command.clone());
    })
    .await;

    let first_prompt = if let Some(issue) = req.issue {
        let base = match find_existing_pr_for_issue(&project, issue).await {
            Ok(Some((pr_num, branch))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                prompts::continue_existing_pr(issue, pr_num, &branch)
            }
            Ok(None) => prompts::implement_from_issue(issue, git),
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue, git)
            }
        };
        // If the caller also supplied a description alongside the issue number, include it
        // as additional context. Without this, batch tasks that set both `description` and
        // `issue` would silently discard the description.
        if let Some(hint) = req.prompt.as_deref().filter(|s| !s.is_empty()) {
            format!(
                "{base}\n\nAdditional context from caller:\n{}",
                prompts::wrap_external_data(hint)
            )
        } else {
            base
        }
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(pr, &review_config.review_bot_command)
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default(), git)
    };

    let context_items = collect_context_items(&skills, &project, &first_prompt).await;

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: project.clone(),
        context: context_items.clone(),
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(ExecutionPhase::Planning),
        ..Default::default()
    };

    // Run pre_execute interceptors; Block aborts the task.
    let first_req = run_pre_execute(&interceptors, initial_req).await?;

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

    let resp = loop {
        let raw = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(agent, impl_req.clone(), task_id, store),
        )
        .await;
        match raw {
            Ok(Ok(r)) => {
                // PreToolUse / PostToolUse hook injection point:
                // detect files written during this turn and fire post_tool_use hooks.
                let hook_err = {
                    let modified = detect_modified_files(&project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                        };
                        run_post_tool_use(&interceptors, &hook_event, &project).await
                    }
                };
                let post_err = run_post_execute(&interceptors, &impl_req, &r).await;
                let combined_err = hook_err.or(post_err);
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
                        let truncated = truncate_validation_error(&err, 2000);
                        impl_req.prompt = format!(
                            "{}\n\nPost-execution validation failed (attempt {}/{}):\n{}",
                            first_req.prompt, validation_attempt, max_validation_retries, truncated
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    } else {
                        tracing::error!(
                            max = max_validation_retries,
                            error = %err,
                            "post-execution validation failed after max retries; aborting task"
                        );
                        run_on_error(&interceptors, &impl_req, &err).await;
                        return Err(anyhow::anyhow!(
                            "Post-execution validation failed after {} attempts: {}",
                            max_validation_retries,
                            err
                        ));
                    }
                }
                break r;
            }
            Ok(Err(e)) => {
                run_on_error(&interceptors, &impl_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Implementation turn timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(&interceptors, &impl_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }
    };

    let AgentResponse { output, stderr, .. } = resp;

    if !stderr.is_empty() {
        tracing::warn!(stderr = %stderr, "agent stderr during implementation");
    }

    let pr_url = prompts::parse_pr_url(&output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);

    mutate_and_persist(store, task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_num.is_some() {
                "pr_created".into()
            } else {
                "no_pr".into()
            },
            detail: None,
        });
    })
    .await;

    // Log implementation event
    let mut ev = Event::new(
        SessionId::new(),
        "task_implement",
        "task_runner",
        harness_core::Decision::Complete,
    );
    ev.detail = pr_num.map(|n| format!("pr={n}"));
    if let Err(e) = events.log(&ev).await {
        tracing::warn!("failed to log task_implement event: {e}");
    }

    let Some(pr_num) = pr_num else {
        tracing::warn!("no PR number found in agent output; skipping review");
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await;
        return Ok(());
    };

    // Agent review loop (if enabled and reviewer available)
    if review_config.enabled {
        if let Some(reviewer) = reviewer {
            tracing::info!("starting agent review for PR #{pr_num}");
            run_agent_review(
                store,
                task_id,
                agent,
                reviewer,
                review_config,
                &context_items,
                &project,
                &interceptors,
                turn_timeout,
                pr_num,
                &events,
            )
            .await?;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }

    // Wait for external review bot
    update_status(store, task_id, TaskStatus::Waiting, 1).await;

    let wait_secs = req.wait_secs;
    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    // Review loop
    for round in 1..=req.max_rounds {
        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let check_req = AgentRequest {
            prompt: prompts::check_existing_pr(pr_num, &review_config.review_bot_command),
            project_root: project.clone(),
            context: context_items.clone(),
            execution_phase: Some(ExecutionPhase::Validation),
            ..Default::default()
        };
        let check_req = run_pre_execute(&interceptors, check_req).await?;

        let resp = tokio::time::timeout(turn_timeout, agent.execute(check_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(&interceptors, &check_req, &r).await {
                    tracing::warn!(
                        round,
                        error = %val_err,
                        "post-execute validation failed in review check; continuing"
                    );
                }
                r
            }
            Ok(Err(e)) => {
                run_on_error(&interceptors, &check_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(&interceptors, &check_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let AgentResponse { output, stderr, .. } = resp;

        if !stderr.is_empty() {
            tracing::warn!(round, stderr = %stderr, "agent stderr during review check");
        }

        let lgtm = prompts::is_lgtm(&output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
                detail: None,
            });
        })
        .await;

        // Log pr_review event for observability and GC signal detection.
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if lgtm {
            format!("round {round}: lgtm")
        } else {
            format!("round {round}: fixed")
        });
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            tracing::info!("PR #{pr_num} approved at round {round}");
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = round.saturating_add(1);
            })
            .await;
            return Ok(());
        }

        tracing::info!("PR #{pr_num} not yet approved at round {round}; waiting");
        if round < req.max_rounds {
            sleep(Duration::from_secs(wait_secs)).await;
        }
    }

    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = req.max_rounds.saturating_add(1);
        s.error = Some(format!(
            "Task did not receive LGTM after {} review rounds.",
            req.max_rounds
        ));
    })
    .await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_review(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: &dyn CodeAgent,
    review_config: &harness_core::AgentReviewConfig,
    context_items: &[ContextItem],
    project: &Path,
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    turn_timeout: Duration,
    pr_num: u64,
    events: &harness_observe::EventStore,
) -> anyhow::Result<()> {
    let max_rounds = review_config.max_rounds;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await;

        // Reviewer evaluates the PR diff
        let review_req = AgentRequest {
            prompt: prompts::agent_review_prompt(pr_num, agent_round),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
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
        .await;

        // Log agent_review event
        let mut ev = Event::new(
            SessionId::new(),
            "agent_review",
            "task_runner",
            if approved {
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
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

        if approved || issues.is_empty() {
            tracing::info!("agent review approved at round {agent_round}");
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
            context: context_items.to_vec(),
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
        .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_three_consecutive_retries() {
        let base_ms: u64 = 10_000;
        let max_ms: u64 = 300_000;

        // Attempt 1: base_ms * 2^0 = 10_000 ms (10 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 1), 10_000);
        // Attempt 2: base_ms * 2^1 = 20_000 ms (20 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 2), 20_000);
        // Attempt 3: base_ms * 2^2 = 40_000 ms (40 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 3), 40_000);
    }

    #[test]
    fn exponential_backoff_capped_at_max() {
        let base_ms: u64 = 10_000;
        let max_ms: u64 = 300_000;

        // Attempt 6: 10_000 * 2^5 = 320_000 — should be capped at 300_000
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 6), 300_000);
    }

    #[test]
    fn parse_harness_review_command() {
        let cmd = parse_harness_mention_command("@harness review");
        assert_eq!(cmd, Some(HarnessMentionCommand::Review));
    }

    #[test]
    fn parse_harness_fix_ci_command_case_insensitive() {
        let cmd = parse_harness_mention_command("please @Harness FIX CI");
        assert_eq!(cmd, Some(HarnessMentionCommand::FixCi));
    }

    #[test]
    fn parse_harness_plain_mention_command() {
        let cmd = parse_harness_mention_command("hello @harness can you help?");
        assert_eq!(cmd, Some(HarnessMentionCommand::Mention));
    }

    #[test]
    fn parse_harness_command_returns_none_without_mention() {
        let cmd = parse_harness_mention_command("no command here");
        assert_eq!(cmd, None);
    }

    #[test]
    fn parse_harness_first_mention_per_line_is_used() {
        let cmd = parse_harness_mention_command("@harness review then @harness fix ci");
        assert_eq!(cmd, Some(HarnessMentionCommand::Review));
    }

    #[test]
    fn prompt_builder_no_sections_adds_trailing_newline() {
        let result = PromptBuilder::new("Title line.").build();
        assert_eq!(result, "Title line.\n");
    }

    #[test]
    fn prompt_builder_optional_url_absent_is_skipped() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", None)
            .build();
        assert_eq!(result, "Title.\n");
    }

    #[test]
    fn prompt_builder_optional_url_present_appears_in_output() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", Some("https://example.com"))
            .build();
        assert!(result.contains("- Link: "));
        assert!(result.contains("https://example.com"));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn prompt_builder_add_section_wraps_external_data() {
        let result = PromptBuilder::new("Title.")
            .add_section("Payload", "content here")
            .build();
        assert!(result.contains("Payload:\n"));
        assert!(result.contains("<external_data>"));
        assert!(result.contains("content here"));
    }

    #[test]
    fn prompt_builder_multiple_urls_all_appear() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("First", Some("url1"))
            .add_optional_url("Second", None)
            .add_optional_url("Third", Some("url3"))
            .build();
        assert!(result.contains("- First: "));
        assert!(result.contains("url1"));
        assert!(!result.contains("Second"));
        assert!(result.contains("- Third: "));
        assert!(result.contains("url3"));
    }

    #[test]
    fn build_fix_ci_prompt_contains_context() {
        let prompt = build_fix_ci_prompt(
            "majiayu000/harness",
            42,
            "@harness fix CI",
            Some("https://github.com/majiayu000/harness/issues/42#issuecomment-1"),
            Some("https://github.com/majiayu000/harness/pull/42"),
        );

        assert!(prompt.contains("CI failure repair requested for PR #42"));
        assert!(prompt.contains("majiayu000/harness"));
        assert!(prompt.contains("<external_data>"));
        assert!(prompt.contains("PR_URL=https://github.com/majiayu000/harness/pull/42"));
    }

    #[test]
    fn truncate_short_string_passes_through() {
        let input = "short error";
        let result = truncate_validation_error(input, 100);
        assert_eq!(result, "short error");
    }

    #[test]
    fn truncate_at_max_chars_boundary() {
        let input = "a".repeat(200);
        let result = truncate_validation_error(&input, 50);
        assert!(result.starts_with(&"a".repeat(50)));
        assert!(result.contains("(output truncated, 200 chars total)"));
    }

    #[test]
    fn truncate_preserves_utf8_boundary() {
        // "é" is 2 bytes; build a string where max_chars lands mid-character.
        let input = "ééééé"; // 10 bytes, 5 chars
        let result = truncate_validation_error(input, 3); // byte 3 is mid-char
                                                          // Should back up to byte 2 (1 full "é").
        assert!(result.starts_with("é"));
        assert!(result.contains("(output truncated,"));
    }
}
