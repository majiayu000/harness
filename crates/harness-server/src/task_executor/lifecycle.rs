use crate::task_executor::helpers::{
    emit_runtime_notification, mark_turn_failed, persist_runtime_thread, process_stream_item,
    run_on_error, run_post_execute, run_pre_execute, update_status,
};
use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStatus, TaskStore};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{
    ContextItem, Event, ExecutionPhase, Item, SessionId, ThreadId, TokenUsage, TurnId, TurnStatus,
};
use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

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
        let msg = format!("agent `{agent_name}` not found in registry");
        if let Err(e) = server.thread_manager.add_item(
            &thread_id,
            &turn_id,
            harness_core::types::Item::Error {
                code: -1,
                message: msg.clone(),
            },
        ) {
            tracing::warn!("failed to add agent-not-found error item: {e}");
        }
        mark_turn_failed(
            &server,
            &thread_db,
            &notify_tx,
            &notification_tx,
            &thread_id,
            &turn_id,
            msg,
        )
        .await;
        return;
    };

    // RAII guard: ensures the adapter is deregistered when the turn scope exits,
    // even if the task is cancelled before reaching the end of this function.
    struct AdapterGuard {
        server: Arc<crate::server::HarnessServer>,
        turn_id: TurnId,
    }
    impl Drop for AdapterGuard {
        fn drop(&mut self) {
            self.server
                .thread_manager
                .deregister_active_adapter(&self.turn_id);
        }
    }

    // Register adapter for this turn if one is configured for this agent name.
    // Enables turn/steer and turn/respond_approval to reach the live process.
    // Gracefully no-ops when no adapter is registered (adapter_registry is empty by default).
    let _adapter_guard = server.adapter_registry.get(&agent_name).map(|adapter_arc| {
        server
            .thread_manager
            .register_active_adapter(&turn_id, adapter_arc.clone());
        AdapterGuard {
            server: server.clone(),
            turn_id: turn_id.clone(),
        }
    });

    let req = AgentRequest {
        prompt,
        project_root,
        ..Default::default()
    };

    let stall_timeout = Duration::from_secs(server.config.concurrency.stall_timeout_secs);
    let (stream_tx, mut stream_rx) = mpsc::channel(128);
    let mut execution = std::pin::pin!(agent.execute_stream(req, stream_tx));
    let mut stream_closed = false;
    let mut execution_result: Option<harness_core::error::Result<()>> = None;
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
                // Store the stall reason as the execution result so the Err branch
                // below appends a stall-specific Item::Error before marking failed.
                execution_result = Some(Err(HarnessError::AgentExecution(format!(
                    "Agent stream stalled: no output for {}s",
                    stall_timeout.as_secs()
                ))));
                break 'outer;
            }
        }
    }

    match execution_result.unwrap_or_else(|| {
        Err(harness_core::error::HarnessError::AgentExecution(
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
                harness_core::types::Item::Error {
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
pub(crate) fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImplementationOutcome {
    PlanIssue(String),
    ParsedPr {
        pr_url: Option<String>,
        pr_num: Option<u64>,
    },
}

pub(crate) fn parse_implementation_outcome(output: &str) -> ImplementationOutcome {
    if let Some(desc) = prompts::parse_plan_issue(output) {
        return ImplementationOutcome::PlanIssue(desc);
    }
    let pr_url = prompts::parse_pr_url(output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);
    ImplementationOutcome::ParsedPr { pr_url, pr_num }
}

/// Persist a completed stream item as a task artifact when it carries content
/// worth retaining across context loss (shell commands, file edits, tool calls).
pub(crate) async fn persist_artifact(
    store: &TaskStore,
    task_id: &TaskId,
    turn: u32,
    item: &harness_core::types::Item,
) {
    let (artifact_type, content) = match item {
        harness_core::types::Item::ShellCommand {
            command,
            exit_code,
            stdout,
            stderr,
        } => {
            let c = serde_json::json!({
                "command": command,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            })
            .to_string();
            ("shell_command", c)
        }
        harness_core::types::Item::FileEdit {
            path,
            before,
            after,
        } => {
            let c = serde_json::json!({
                "path": path,
                "before": before,
                "after": after,
            })
            .to_string();
            ("file_edit", c)
        }
        harness_core::types::Item::ToolCall {
            name,
            input,
            output,
        } => {
            let c = serde_json::json!({
                "name": name,
                "input": input,
                "output": output,
            })
            .to_string();
            ("tool_call", c)
        }
        _ => return,
    };
    store
        .insert_artifact(task_id, turn, artifact_type, &content)
        .await;
}

/// Execute an agent request via [`CodeAgent::execute_stream`], broadcasting
/// each [`StreamItem`] to the per-task channel in real time, and reconstruct
/// an [`AgentResponse`] from the collected stream events.
///
/// `turn` is stored with each captured artifact so callers can distinguish
/// implementation turn (1) from later review or retry turns.
pub(crate) async fn run_agent_streaming(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
    turn: u32,
) -> harness_core::error::Result<AgentResponse> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamItem>(128);
    let mut exec = std::pin::pin!(agent.execute_stream(req, tx));
    let mut exec_result: Option<harness_core::error::Result<()>> = None;
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
                            StreamItem::ItemCompleted { item: completed_item } => {
                                persist_artifact(store, task_id, turn, completed_item).await;
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

/// Normalize a set of review issues into a canonical ordered form.
/// Issues are sorted by reference before collecting so that insertion order does not affect
/// equality comparisons, and strings are not unnecessarily cloned during the sort.
pub(crate) fn normalize_issues(issues: &[String]) -> Vec<String> {
    let mut sorted: Vec<_> = issues.iter().collect();
    sorted.sort();
    sorted.into_iter().cloned().collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_review(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: &dyn CodeAgent,
    review_config: &harness_core::config::agents::AgentReviewConfig,
    context_items: &[ContextItem],
    project: &Path,
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    turn_timeout: Duration,
    pr_url: &str,
    project_type: &str,
    events: &harness_observe::event_store::EventStore,
    cargo_env: &HashMap<String, String>,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
) -> anyhow::Result<(bool, bool)> {
    let max_rounds = review_config.max_rounds;
    // (normalized_issues, consecutive_count): tracks how many consecutive rounds produced identical issues.
    let mut impasse_tracker: Option<(Vec<String>, u32)> = None;
    // Whether the last action in this loop pushed a new commit (fix or intervention).
    let mut pushed_commit = false;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let review_req = AgentRequest {
            prompt: {
                let base = prompts::agent_review_prompt(pr_url, agent_round, project_type);
                // Inject capability note — primary enforcement now that --allowedTools
                // is not passed to the CLI (issue #483).
                let note = "Tool restriction: you are operating in review mode. \
                     Only Read, Grep, Glob, and Bash are permitted. \
                     Use Bash ONLY for read-only commands like `gh pr diff`. \
                     Do NOT call Write, Edit, or any other tool.";
                format!("{note}\n\n{base}")
            },
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Validation),
            allowed_tools: Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
                "Bash".to_string(),
            ]),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review round {agent_round}: used {} of {} allowed turns",
                    turns_used, max
                ));
            }
        }
        let resp = tokio::time::timeout(turn_timeout, reviewer.execute(review_req.clone())).await;
        *turns_used += 1;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(
                    &r.output,
                    review_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in agent review round {agent_round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        agent_round,
                        ?tool_violations,
                        "agent review: agent used tools outside allowed list"
                    );
                    run_on_error(interceptors, &review_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
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
                if matches!(e, HarnessError::QuotaExhausted(_)) {
                    tracing::error!(agent_round, error = %e, "quota exhausted during agent review — aborting");
                    run_on_error(interceptors, &review_req, &e.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok((false, false));
                }
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
                harness_core::types::Decision::Complete
            } else {
                harness_core::types::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_url}"));
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

        // Detect impasse: track how many consecutive rounds produced identical issues.
        // Must happen before the max_rounds break so thresholds are reachable at the last round.
        // Compare normalized issue lists directly to avoid false positives from hash collisions.
        let normalized = normalize_issues(&issues);
        let consecutive_count = match &impasse_tracker {
            Some((prev, c)) if *prev == normalized => c + 1,
            _ => 1,
        };
        impasse_tracker = Some((normalized, consecutive_count));

        // 5 consecutive rounds with identical issues → mark task as Failed immediately.
        if consecutive_count >= 5 {
            tracing::warn!(
                agent_round,
                consecutive_count,
                "agent review impasse: same issues repeated 5 times — marking task as failed"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!(
                    "Impasse detected: identical issues repeated {consecutive_count} consecutive rounds."
                ));
            })
            .await?;
            return Ok((false, false));
        }

        // 3+ consecutive rounds with identical issues → use the intervention prompt.
        let is_impasse = consecutive_count >= 3;
        if is_impasse {
            tracing::warn!(
                agent_round,
                consecutive_count,
                "agent review impasse detected — same issues repeated, using intervention prompt"
            );
        }

        // Skip the fix round only when rounds are exhausted and there is no impasse.
        // When impasse is detected at the last round, we still apply the intervention prompt
        // to give the agent one final attempt to break the cycle before GitHub review.
        if agent_round == max_rounds && !is_impasse {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
            );
            break;
        }

        // Implementor fixes the issues
        let fix_req = AgentRequest {
            prompt: {
                let prompt_fn = if is_impasse {
                    prompts::agent_review_intervention_prompt
                } else {
                    prompts::agent_review_fix_prompt
                };
                prompt_fn(pr_url, &issues, agent_round, project_type)
            },
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let fix_req = run_pre_execute(interceptors, fix_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review fix round {agent_round}: used {} of {} allowed turns",
                    turns_used, max
                ));
            }
        }
        let fix_resp = tokio::time::timeout(turn_timeout, agent.execute(fix_req.clone())).await;
        *turns_used += 1;
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
        pushed_commit = true;
    }

    Ok((true, pushed_commit))
}

/// Compute word-level Jaccard similarity between two strings.
/// Used to detect stuck review loops where the reviewer repeats itself.
pub(crate) fn jaccard_word_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: HashSet<&str> = a
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let tokens_b: HashSet<&str> = b
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();

    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }

    let intersection = tokens_a.intersection(&tokens_b).count();
    let union = tokens_a.union(&tokens_b).count();
    intersection as f64 / union as f64
}
