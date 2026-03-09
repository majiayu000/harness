use crate::task_runner::{
    mutate_and_persist, mutate_and_persist_with, update_status, CreateTaskRequest, RoundResult,
    TaskId, TaskStatus, TaskStore,
};
use harness_core::{
    interceptor::TurnInterceptor, prompts, AgentRequest, AgentResponse, CodeAgent, ContextItem,
    Decision, Event, Item, SessionId, StreamItem, ThreadId, TurnId, TurnStatus,
};
use harness_protocol::{Notification, RpcNotification};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration};

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HarnessMentionCommand {
    Mention,
    Review,
    FixCi,
}

/// Parse the first `@harness` mention found while scanning line-by-line.
/// For each line, only the first `@harness` occurrence is considered.
pub(crate) fn parse_harness_mention_command(body: &str) -> Option<HarnessMentionCommand> {
    for line in body.lines() {
        let lowercase = line.trim().to_ascii_lowercase();
        if let Some(idx) = lowercase.find("@harness") {
            let mut command = lowercase[idx + "@harness".len()..].trim_start();
            command = command.trim_start_matches(|ch: char| {
                ch.is_whitespace() || ch == ':' || ch == ',' || ch == '-' || ch == '.'
            });

            if command.starts_with("fix ci")
                || command.starts_with("fix-ci")
                || command.starts_with("fix_ci")
            {
                return Some(HarnessMentionCommand::FixCi);
            }
            if command.starts_with("review") {
                return Some(HarnessMentionCommand::Review);
            }
            return Some(HarnessMentionCommand::Mention);
        }
    }

    None
}

pub(crate) fn build_fix_ci_prompt(
    repository: &str,
    pr_number: u64,
    comment_body: &str,
    comment_url: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let wrapped_comment = prompts::wrap_external_data(comment_body);
    let comment_url_line = comment_url
        .map(|url| format!("- Trigger comment: {url}\n"))
        .unwrap_or_default();
    let pr_url_line = pr_url
        .map(|url| format!("- PR URL: {url}\n"))
        .unwrap_or_default();
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");

    format!(
        "CI failure repair requested for PR #{pr_number} in `{repository}`.\n\
         {comment_url_line}\
         {pr_url_line}\
         Command payload:\n\
         {wrapped_comment}\n\n\
         Required workflow:\n\
         1. Inspect failing checks for PR #{pr_number} (`gh pr checks {pr_number}`)\n\
         2. Investigate CI failure details from logs and failing tests\n\
         3. Implement a minimal fix that makes CI green\n\
         4. Run the repository's standard validation commands for the affected changes (including all failing/required CI checks)\n\
         5. Commit and push to the existing PR branch\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

/// Truncate validation error output to `max_chars` to avoid bloating agent prompts.
/// Preserves the first portion which typically contains the most actionable info.
fn truncate_validation_error(error: &str, max_chars: usize) -> String {
    if error.len() <= max_chars {
        return error.to_string();
    }
    let truncated = &error[..error.floor_char_boundary(max_chars)];
    format!("{truncated}\n\n... (output truncated, {total} chars total)", total = error.len())
}

/// Run all pre_execute interceptors in order. Returns the (possibly modified) request,
/// or an error if any interceptor returns Block.
async fn run_pre_execute(
    interceptors: &[Arc<dyn TurnInterceptor>],
    mut req: AgentRequest,
) -> anyhow::Result<AgentRequest> {
    for interceptor in interceptors {
        let result = interceptor.pre_execute(&req).await;
        if let Decision::Block = result.decision {
            let reason = result
                .reason
                .unwrap_or_else(|| interceptor.name().to_string());
            return Err(anyhow::anyhow!(
                "Blocked by interceptor '{}': {}",
                interceptor.name(),
                reason
            ));
        }
        if let Some(modified) = result.request {
            req = modified;
        }
    }
    Ok(req)
}

/// Run all post_execute interceptors in order.
/// Returns the first validation error found, or None if all pass.
async fn run_post_execute(
    interceptors: &[Arc<dyn TurnInterceptor>],
    req: &AgentRequest,
    resp: &AgentResponse,
) -> Option<String> {
    for interceptor in interceptors {
        let result = interceptor.post_execute(req, resp).await;
        if let Some(error) = result.error {
            return Some(format!("[{}] {}", interceptor.name(), error));
        }
    }
    None
}

async fn run_on_error(interceptors: &[Arc<dyn TurnInterceptor>], req: &AgentRequest, error: &str) {
    for interceptor in interceptors {
        interceptor.on_error(req, error).await;
    }
}

/// Query GitHub for an existing open PR linked to the given issue.
/// Returns `(pr_number, branch_name)` if found.
async fn find_existing_pr_for_issue(
    project: &Path,
    issue: u64,
) -> anyhow::Result<Option<(u64, String)>> {
    let output = Command::new("gh")
        .current_dir(project)
        .args([
            "pr",
            "list",
            "--search",
            &format!("#{issue}"),
            "--state",
            "open",
        ])
        .args(["--json", "number,headRefName", "--limit", "1"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run `gh pr list` for issue #{issue}: {e}"))?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "`gh pr list` for issue #{issue} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let items: Vec<GhPrListItem> = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("invalid JSON from `gh pr list`: {e}"))?;

    Ok(items
        .into_iter()
        .next()
        .map(|item| (item.number, item.head_ref_name)))
}

fn emit_runtime_notification(
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    notification: Notification,
) {
    crate::notify::emit(notify_tx, notification.clone());
    let _ = notification_tx.send(RpcNotification::new(notification));
}

async fn persist_runtime_thread(
    thread_db: &Option<crate::thread_db::ThreadDb>,
    server: &crate::server::HarnessServer,
    thread_id: &ThreadId,
) {
    if let Some(db) = thread_db {
        if let Some(thread) = server.thread_manager.get_thread(thread_id) {
            if let Err(err) = db.update(&thread).await {
                tracing::warn!("thread_db persist failed during turn execution: {err}");
            }
        }
    }
}

async fn process_stream_item(
    server: &crate::server::HarnessServer,
    thread_db: &Option<crate::thread_db::ThreadDb>,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    stream_item: StreamItem,
) {
    match stream_item {
        StreamItem::ItemStarted { item } => {
            if let Err(err) = server
                .thread_manager
                .add_item(thread_id, turn_id, item.clone())
            {
                tracing::warn!("failed to append stream item_started to turn: {err}");
            } else {
                persist_runtime_thread(thread_db, server, thread_id).await;
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ItemStarted {
                    turn_id: turn_id.clone(),
                    item,
                },
            );
        }
        StreamItem::ItemCompleted { item } => {
            if let Err(err) = server
                .thread_manager
                .add_item(thread_id, turn_id, item.clone())
            {
                tracing::warn!("failed to append stream item_completed to turn: {err}");
            } else {
                persist_runtime_thread(thread_db, server, thread_id).await;
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ItemCompleted {
                    turn_id: turn_id.clone(),
                    item,
                },
            );
        }
        StreamItem::TokenUsage { usage } => {
            match server
                .thread_manager
                .set_turn_token_usage(thread_id, turn_id, usage.clone())
            {
                Ok(true) => persist_runtime_thread(thread_db, server, thread_id).await,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!("failed to update token usage for turn: {err}");
                }
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::TokenUsageUpdated {
                    thread_id: thread_id.clone(),
                    usage,
                },
            );
        }
        StreamItem::Error { message } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                Item::Error { code: -1, message },
            ) {
                tracing::warn!("failed to append stream error item to turn: {err}");
            } else {
                persist_runtime_thread(thread_db, server, thread_id).await;
            }
        }
        StreamItem::MessageDelta { text } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::MessageDelta {
                    turn_id: turn_id.clone(),
                    text,
                },
            );
        }
        StreamItem::Done => {}
    }
}

async fn mark_turn_failed(
    server: &crate::server::HarnessServer,
    thread_db: &Option<crate::thread_db::ThreadDb>,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    error_message: String,
) {
    match server
        .thread_manager
        .mark_turn_failed_with_error(thread_id, turn_id, error_message)
    {
        Ok(Some(usage)) => {
            persist_runtime_thread(thread_db, server, thread_id).await;
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::TurnCompleted {
                    turn_id: turn_id.clone(),
                    status: TurnStatus::Failed,
                    token_usage: usage,
                },
            );
        }
        Ok(None) => {}
        Err(err) => tracing::warn!("failed to move turn to failed state: {err}"),
    }
}

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

    let (stream_tx, mut stream_rx) = mpsc::channel(128);
    let mut execution = std::pin::pin!(agent.execute_stream(req, stream_tx));
    let mut stream_closed = false;
    let mut execution_result: Option<harness_core::Result<()>> = None;

    while execution_result.is_none() || !stream_closed {
        tokio::select! {
            result = &mut execution, if execution_result.is_none() => {
                execution_result = Some(result);
            }
            incoming = stream_rx.recv(), if !stream_closed => {
                match incoming {
                    Some(item) => {
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
            mark_turn_failed(
                &server,
                &thread_db,
                &notify_tx,
                &notification_tx,
                &thread_id,
                &turn_id,
                err.to_string(),
            )
            .await;
        }
    }
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    review_config: &harness_core::AgentReviewConfig,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Arc<Vec<Arc<dyn TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
) -> anyhow::Result<()> {
    update_status(store, task_id, TaskStatus::Implementing, 1).await;

    let first_prompt = if let Some(issue) = req.issue {
        match find_existing_pr_for_issue(&project, issue).await {
            Ok(Some((pr_num, branch))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                prompts::continue_existing_pr(issue, pr_num, &branch)
            }
            Ok(None) => prompts::implement_from_issue(issue),
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue)
            }
        }
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(pr, &review_config.review_bot_command)
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default())
    };

    let mut context_items: Vec<ContextItem> = {
        let guard = skills.read().await;
        guard
            .list()
            .iter()
            .map(|s| ContextItem::Skill {
                id: s.id.to_string(),
                content: s.content.clone(),
            })
            .collect()
    };

    // Load cascading AGENTS.md files and inject as context
    let agents_md = harness_core::agents_md::load_agents_md(&project);
    if !agents_md.is_empty() {
        context_items.push(ContextItem::AgentsMd { content: agents_md });
    }

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: project.clone(),
        context: context_items.clone(),
        ..Default::default()
    };

    // Run pre_execute interceptors; Block aborts the task.
    let first_req = run_pre_execute(&interceptors, initial_req).await?;

    // Execute implementation turn with post-execution validation and auto-retry.
    let max_validation_retries: u32 = interceptors
        .iter()
        .filter_map(|i| i.max_validation_retries())
        .min()
        .unwrap_or(2);
    let mut validation_attempt = 0u32;
    let mut impl_req = first_req.clone();

    let resp = loop {
        let raw = tokio::time::timeout(turn_timeout, agent.execute(impl_req.clone())).await;
        match raw {
            Ok(Ok(r)) => {
                if let Some(err) = run_post_execute(&interceptors, &impl_req, &r).await {
                    if validation_attempt < max_validation_retries {
                        validation_attempt += 1;
                        tracing::warn!(
                            attempt = validation_attempt,
                            max = max_validation_retries,
                            error = %err,
                            "post-execution validation failed; retrying with error context"
                        );
                        // Truncate error to avoid bloating the prompt with huge
                        // compiler output. Keep only the summary of which commands
                        // failed and the first portion of each error.
                        let truncated = truncate_validation_error(&err, 1500);
                        impl_req = AgentRequest {
                            prompt: format!(
                                "{}\n\n\
                                 Post-execution validation failed (attempt {validation_attempt}/{max_validation_retries}).\n\
                                 Fix these errors, then commit and push:\n\n\
                                 {truncated}",
                                first_req.prompt
                            ),
                            ..first_req.clone()
                        };
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "Validation failed after {validation_attempt} retries: {err}"
                    ));
                }
                break r;
            }
            Ok(Err(e)) => {
                run_on_error(&interceptors, &impl_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!("Turn 1 timed out after {}s", req.turn_timeout_secs);
                run_on_error(&interceptors, &impl_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }
    };

    let pr_url = prompts::parse_pr_url(&resp.output);
    let pr_number = pr_url
        .as_ref()
        .and_then(|u| prompts::extract_pr_number(u))
        .or(req.pr);

    mutate_and_persist(store, task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_url.is_some() || req.pr.is_some() {
                "pr_created".into()
            } else {
                "implemented".into()
            },
        });
    })
    .await;

    let pr_num = match pr_number {
        Some(n) => n,
        None => {
            update_status(store, task_id, TaskStatus::Done, 1).await;
            return Ok(());
        }
    };

    // Agent review phase: independent reviewer evaluates PR diff before GitHub review
    if review_config.enabled {
        if let Some(reviewer) = reviewer {
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
            tracing::info!("agent review enabled but no reviewer available, skipping");
        }
    }

    // Review loop: Turn 2..N
    let last_review_round = req.max_rounds.saturating_add(1);
    let mut prev_fixed = false;
    let mut round = 2u32;
    let max_waiting_retries = 3u32;

    while round <= last_review_round {
        update_status(store, task_id, TaskStatus::Waiting, round).await;
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let review_req = AgentRequest {
            prompt: prompts::review_prompt(
                req.issue,
                pr_num,
                round,
                prev_fixed,
                &review_config.review_bot_command,
            ),
            project_root: project.clone(),
            context: context_items.clone(),
            ..Default::default()
        };

        let review_req = run_pre_execute(&interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, agent.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(&interceptors, &review_req, &r).await {
                    tracing::warn!(
                        round,
                        error = %val_err,
                        "post-execute validation failed in review round; continuing"
                    );
                }
                r
            }
            Ok(Err(e)) => {
                run_on_error(&interceptors, &review_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!("Turn {round} timed out after {}s", req.turn_timeout_secs);
                run_on_error(&interceptors, &review_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        if prompts::is_waiting(&resp.output) {
            let waiting_count = mutate_and_persist_with(store, task_id, |s| {
                s.rounds.push(RoundResult {
                    turn: round,
                    action: "review".into(),
                    result: "waiting".into(),
                });
                s.rounds
                    .iter()
                    .filter(|r| r.result == "waiting" && r.turn == round)
                    .count() as u32
            })
            .await
            .unwrap_or(0);

            if waiting_count >= max_waiting_retries {
                prev_fixed = true;
                round += 1;
            }
            continue;
        }

        let lgtm = prompts::is_lgtm(&resp.output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
            });
        })
        .await;

        // Log pr_review event for observability and GC signal detection.
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                Decision::Complete
            } else {
                Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if lgtm {
            format!("round {round}: lgtm")
        } else {
            format!("round {round}: fixed")
        });
        if let Err(e) = events.log(&ev) {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            update_status(store, task_id, TaskStatus::Done, round).await;
            return Ok(());
        }

        prev_fixed = true;
        round += 1;
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
    interceptors: &[Arc<dyn TurnInterceptor>],
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

        let approved = prompts::is_approved(&resp.output);
        let issues = prompts::extract_review_issues(&resp.output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0, // agent review rounds use turn 0
                action: "agent_review".into(),
                result: if approved {
                    "approved".into()
                } else {
                    format!("{} issues", issues.len())
                },
            });
        })
        .await;

        // Log agent_review event
        let mut ev = harness_core::Event::new(
            harness_core::SessionId::new(),
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
        if let Err(e) = events.log(&ev) {
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
    fn gh_pr_list_item_parses_from_json() {
        let json = r#"[{"number":50,"headRefName":"fix/issue-29"}]"#;
        let items: Vec<GhPrListItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 50);
        assert_eq!(items[0].head_ref_name, "fix/issue-29");
    }

    #[test]
    fn gh_pr_list_empty_array_parses() {
        let json = r#"[]"#;
        let items: Vec<GhPrListItem> = serde_json::from_str(json).unwrap();
        assert!(items.is_empty());
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
}
