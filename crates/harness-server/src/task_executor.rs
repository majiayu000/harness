mod helpers;
mod pr_detection;

use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::{
    config::load_project_config, interceptor::ToolUseEvent, lang_detect, prompts, AgentRequest,
    AgentResponse, CapabilityProfile, CodeAgent, ContextItem, Event, ExecutionPhase, HarnessError,
    Item, SessionId, StreamItem, ThreadId, TokenUsage, TurnId, TurnStatus,
};
use harness_protocol::{Notification, RpcNotification};
use std::collections::HashMap;

/// Extract tool list from a capability profile, returning an error if the
/// profile unexpectedly returns `None` (which means Full/unrestricted).
/// A misconfigured profile causes a hard failure rather than silent degradation,
/// per U-23 (no silent capability downgrade).
fn restricted_tools(profile: CapabilityProfile) -> anyhow::Result<Vec<String>> {
    profile.tools().ok_or_else(|| {
        anyhow::anyhow!(
            "capability profile {:?} returned None from tools() — misconfiguration",
            profile
        )
    })
}
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// RAII guard that removes the per-task Cargo target directory on drop.
/// This ensures cleanup regardless of how `run_task` exits (success, error,
/// or timeout), preventing disk exhaustion from accumulated build artifacts.
struct TaskTargetDir(PathBuf);

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
use tokio::time::{sleep, Duration, Instant};

pub(crate) use helpers::{
    collect_context_items, detect_modified_files, emit_runtime_notification,
    inject_skills_into_prompt, mark_turn_failed, persist_runtime_thread, process_stream_item,
    run_on_error, run_post_execute, run_post_tool_use, run_pre_execute, truncate_validation_error,
    update_status,
};
pub(crate) use pr_detection::{
    build_fix_ci_prompt, build_pr_approved_prompt, build_pr_rework_prompt, detect_repo_slug,
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
        let msg = format!("agent `{agent_name}` not found in registry");
        if let Err(e) = server.thread_manager.add_item(
            &thread_id,
            &turn_id,
            harness_core::Item::Error {
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

/// Persist a completed stream item as a task artifact when it carries content
/// worth retaining across context loss (shell commands, file edits, tool calls).
async fn persist_artifact(
    store: &TaskStore,
    task_id: &TaskId,
    turn: u32,
    item: &harness_core::Item,
) {
    let (artifact_type, content) = match item {
        harness_core::Item::ShellCommand {
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
        harness_core::Item::FileEdit {
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
        harness_core::Item::ToolCall {
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
async fn run_agent_streaming(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
    turn: u32,
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

fn prepend_constitution(prompt: String, enabled: bool) -> String {
    const CONSTITUTION: &str = include_str!("../../../config/constitution.md");
    if enabled {
        format!("{CONSTITUTION}\n\n{prompt}")
    } else {
        prompt
    }
}

/// Run triage → plan pipeline for a fresh issue-based task.
///
/// Returns `Some(plan_text)` if the triage decided a plan is needed and the plan
/// phase completed. Returns `None` only when triage says PROCEED (trivial issue,
/// skip planning). All failures propagate as errors — no silent fallbacks.
async fn run_triage_plan_pipeline(
    agent: &dyn CodeAgent,
    store: &TaskStore,
    task_id: &TaskId,
    issue: u64,
    cargo_env: &HashMap<String, String>,
    project: &Path,
    req: &CreateTaskRequest,
) -> anyhow::Result<Option<String>> {
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

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);
    let triage_resp = tokio::time::timeout(
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

    let decision = prompts::parse_triage(&triage_resp.output).ok_or_else(|| {
        anyhow::anyhow!("triage output unparseable — agent did not produce TRIAGE=<decision>")
    })?;
    tracing::info!(task_id = %task_id, ?decision, "triage decision");

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
            return Ok(None);
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

    tracing::info!(task_id = %task_id, plan_len = plan_text.len(), "plan phase complete");
    Ok(Some(plan_text))
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    server_config: &harness_core::HarnessConfig,
) -> anyhow::Result<()> {
    let task_start = Instant::now();

    if !project.exists() {
        anyhow::bail!("project_root does not exist: {}", project.display());
    }

    // Set CARGO_TARGET_DIR to a per-task temp path so parallel agents running
    // cargo check/test simultaneously do not contend on the same build directory.
    // A per-project path caused `.cargo-lock` contention and build failures when
    // two tasks targeted the same project concurrently (issue #488).
    let task_target = std::env::temp_dir()
        .join("harness-cargo-targets")
        .join(task_id.as_str());
    let cargo_env: HashMap<String, String> = [(
        "CARGO_TARGET_DIR".to_string(),
        task_target.display().to_string(),
    )]
    .into();
    // Guard ensures the directory is removed when run_task exits, regardless of
    // the exit path (success, validation failure, timeout, or review exhaustion).
    let _task_target_guard = TaskTargetDir(task_target);

    let project_config = load_project_config(&project);
    let resolved = harness_core::config::resolve_config(server_config, &project_config);
    let review_config = &resolved.review;
    let git = Some(&project_config.git);
    let repo_slug = detect_repo_slug(&project)
        .await
        .unwrap_or_else(|| "{owner}/{repo}".to_string());

    // --- Pipeline: Triage → Plan → Implement ---
    // For issue-based tasks without an existing PR, run triage first.
    // Triage decides whether to skip planning or go through a plan phase.
    let plan_output = if let Some(issue) = req.issue {
        // Only triage fresh issues (no existing PR to continue).
        let has_existing_pr = find_existing_pr_for_issue(&project, issue)
            .await
            .ok()
            .flatten()
            .is_some();
        if !has_existing_pr {
            run_triage_plan_pipeline(agent, store, task_id, issue, &cargo_env, &project, req)
                .await?
        } else {
            None
        }
    } else {
        None
    };

    // Resume normal flow — update status to Implementing for the main turn.
    update_status(store, task_id, TaskStatus::Implementing, 1).await?;
    let impl_phase_start = Instant::now();

    let first_prompt = if let Some(issue) = req.issue {
        let base = match find_existing_pr_for_issue(&project, issue).await {
            Ok(Some((pr_num, branch, pr_url))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                let slug = prompts::repo_slug_from_pr_url(Some(&pr_url));
                prompts::continue_existing_pr(issue, pr_num, &branch, &slug)
            }
            Ok(None) => {
                prompts::implement_from_issue(issue, git, plan_output.as_deref()).to_prompt_string()
            }
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue, git, plan_output.as_deref()).to_prompt_string()
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
        prompts::check_existing_pr(pr, &review_config.review_bot_command, &repo_slug)
    } else if req.source.as_deref() == Some("periodic_review") {
        // Review tasks use their prompt as-is — no "create PR" wrapper.
        req.prompt.clone().unwrap_or_default()
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default(), git)
    };

    // Inject language-detected validation instructions into the prompt when no
    // explicit validation config is set. This delegates validation to the agent
    // instead of running external commands that may not be installed.
    let first_prompt = if project_config.validation.pre_commit.is_empty()
        && project_config.validation.pre_push.is_empty()
    {
        let lang = lang_detect::detect_language(&project);
        let instructions = lang_detect::validation_prompt_instructions(lang, &project);
        if instructions.is_empty() {
            first_prompt
        } else {
            format!("{first_prompt}\n\n{instructions}")
        }
    } else {
        first_prompt
    };

    // Inject sibling-awareness context when other agents are working on the same project.
    // This prevents parallel agents from over-scoping their changes into each other's files.
    //
    // `project` may be a per-task worktree path when workspace isolation is active, but the
    // sibling cache stores the canonical source repo path (set at spawn time before the worktree
    // is created). Look up the canonical root from the task's own cache entry so that
    // list_siblings() path comparison works correctly in the isolated-worktree case.
    let first_prompt = {
        let canonical_project = store
            .get(task_id)
            .and_then(|s| s.project_root)
            .unwrap_or_else(|| project.clone());
        let siblings = store.list_siblings(&canonical_project, task_id);
        if siblings.is_empty() {
            first_prompt
        } else {
            let sibling_tasks: Vec<prompts::SiblingTask> = siblings
                .into_iter()
                .filter_map(|s| {
                    s.description.and_then(|description| {
                        if description.is_empty() {
                            None
                        } else {
                            Some(prompts::SiblingTask {
                                issue: s.issue,
                                description,
                            })
                        }
                    })
                })
                .collect();
            let ctx = prompts::sibling_task_context(&sibling_tasks);
            format!("{first_prompt}\n\n{ctx}")
        }
    };

    // Prepend the Golden Principles constitution when enabled.
    let first_prompt =
        prepend_constitution(first_prompt, server_config.server.constitution_enabled);

    // Inject skill content directly into the prompt text.
    // Since harness uses single-turn `claude -p`, context items are not visible
    // to the agent — we must embed skill content in the prompt string itself.
    // Also records usage for any matched skills via record_use().
    let skill_additions = inject_skills_into_prompt(&skills, &first_prompt).await;
    let first_prompt = if skill_additions.is_empty() {
        first_prompt
    } else {
        first_prompt + &skill_additions
    };

    let context_items = collect_context_items(&skills, &project, &first_prompt).await;

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);

    // Periodic review tasks need Bash to run guard check commands but should
    // not have unrestricted write access — use Standard profile. All other
    // tasks (implementation) keep Full (no restriction, Vec::new()).
    //
    // NOTE: --allowedTools is NOT passed as a CLI flag (see claude.rs).
    // Tool restriction is enforced via prompt_note injection below.
    let (initial_allowed_tools, capability_prompt_note) =
        if req.source.as_deref() == Some("periodic_review") {
            (
                restricted_tools(CapabilityProfile::Standard)?,
                CapabilityProfile::Standard.prompt_note(),
            )
        } else {
            (Vec::new(), None)
        };

    // Prepend capability restriction note so the agent knows which tools are
    // permitted. This is the primary enforcement path now that --allowedTools
    // is not passed to the CLI.
    let first_prompt = if let Some(note) = capability_prompt_note {
        format!("{note}\n\n{first_prompt}")
    } else {
        first_prompt
    };

    tracing::info!(
        task_id = %task_id,
        prompt_len = first_prompt.len(),
        prompt_empty = first_prompt.is_empty(),
        source = ?req.source,
        "run_task: prompt constructed"
    );
    if first_prompt.is_empty() {
        tracing::error!(task_id = %task_id, "run_task: prompt is empty — agent will fail");
    }

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: project.clone(),
        context: context_items.clone(),
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(ExecutionPhase::Planning),
        allowed_tools: initial_allowed_tools,
        env_vars: cargo_env.clone(),
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
            run_agent_streaming(agent, impl_req.clone(), task_id, store, 1),
        )
        .await;
        match raw {
            Ok(Ok(r)) => {
                // Post-execution tool isolation check. Since --allowedTools is not
                // passed to the CLI, we scan the output for disallowed tool calls.
                // Violations feed into the retry loop so the agent gets a chance to
                // self-correct (fail-closed, not fail-open).
                let tool_violations = validate_tool_usage(&r.output, &impl_req.allowed_tools);
                let violation_err: Option<String> = if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                        tool_violations.join(", "),
                        impl_req.allowed_tools.join(", ")
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
                    let modified = detect_modified_files(&project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                            session_id: None,
                        };
                        run_post_tool_use(&interceptors, &hook_event, &project).await
                    }
                };
                let post_err = run_post_execute(&interceptors, &impl_req, &r).await;
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

    let AgentResponse {
        output,
        stderr,
        token_usage: impl_token_usage,
        ..
    } = resp;

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

    // Review-only tasks produce a report, not a PR.
    // Persist the output and return immediately — no PR parsing or review loop.
    let is_review_task = store.get(task_id).is_some_and(|s| {
        matches!(
            s.source.as_deref(),
            Some("periodic_review") | Some("sprint_planner")
        )
    });

    if is_review_task {
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 1;
            s.rounds.push(RoundResult {
                turn: 1,
                action: "review".into(),
                result: "completed".into(),
                detail: if output.is_empty() {
                    None
                } else {
                    Some(output.clone())
                },
            });
        })
        .await?;
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 1,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
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
            detail: if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
        });
    })
    .await?;

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
        .await?;
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 2,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
    };

    // Agent review loop (if enabled and reviewer available)
    if review_config.enabled {
        if let Some(reviewer) = reviewer {
            tracing::info!(pr_url = %pr_url.as_deref().unwrap_or(""), "starting agent review");
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
                pr_url.as_deref().unwrap_or(""),
                &events,
                &cargo_env,
            )
            .await?;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }

    // Wait for external review bot.
    // Use a local counter instead of querying the store to derive waiting_count —
    // task execution is sequential within a single tokio task, so a plain u32 suffices.
    let mut waiting_count: u32 = 0;
    waiting_count += 1;
    update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;

    let wait_secs = req.wait_secs;
    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    let review_phase_start = Instant::now();

    // Review loop
    for round in 1..=req.max_rounds {
        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let check_req = AgentRequest {
            prompt: {
                let slug = prompts::repo_slug_from_pr_url(pr_url.as_deref());
                let base =
                    prompts::check_existing_pr(pr_num, &review_config.review_bot_command, &slug);
                // Inject capability note — primary enforcement now that --allowedTools
                // is not passed to the CLI (issue #483).
                if let Some(note) = CapabilityProfile::ReadOnly.prompt_note() {
                    format!("{note}\n\n{base}")
                } else {
                    base
                }
            },
            project_root: project.clone(),
            context: context_items.clone(),
            execution_phase: Some(ExecutionPhase::Validation),
            allowed_tools: restricted_tools(CapabilityProfile::ReadOnly)?,
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let check_req = run_pre_execute(&interceptors, check_req).await?;

        let resp = tokio::time::timeout(turn_timeout, agent.execute(check_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(&r.output, &check_req.allowed_tools);
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        round,
                        ?tool_violations,
                        "review check: agent used tools outside allowed list"
                    );
                    run_on_error(&interceptors, &check_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
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
        let waiting = prompts::is_waiting(&output);

        // WAITING means review bot hasn't posted yet (e.g., quota exhausted).
        // Don't consume a round — just sleep and retry without incrementing.
        if waiting {
            tracing::info!(
                round,
                "PR #{pr_num} review bot has not responded yet; sleeping without consuming round"
            );
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
            continue;
        }

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
                detail: None,
            });
        })
        .await?;

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
            .await?;
            tracing::info!(
                task_id = %task_id,
                phase = "reviewing",
                elapsed_secs = review_phase_start.elapsed().as_secs(),
                "phase_completed"
            );
            tracing::info!(
                task_id = %task_id,
                status = "done",
                turns = round.saturating_add(1),
                pr_url = pr_url.as_deref().unwrap_or(""),
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(());
        }

        tracing::info!("PR #{pr_num} not yet approved at round {round}; waiting");
        if round < req.max_rounds {
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
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
    .await?;
    tracing::info!(
        task_id = %task_id,
        phase = "reviewing",
        elapsed_secs = review_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    tracing::info!(
        task_id = %task_id,
        status = "failed",
        turns = req.max_rounds.saturating_add(1),
        pr_url = pr_url.as_deref().unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
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
    pr_url: &str,
    events: &harness_observe::EventStore,
    cargo_env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let max_rounds = review_config.max_rounds;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let review_req = AgentRequest {
            prompt: {
                let base = prompts::agent_review_prompt(pr_url, agent_round);
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
            allowed_tools: vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
                "Bash".to_string(),
            ],
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, reviewer.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(&r.output, &review_req.allowed_tools);
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
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
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

        if agent_round == max_rounds {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
            );
            break;
        }

        // Implementor fixes the issues
        let fix_req = AgentRequest {
            prompt: prompts::agent_review_fix_prompt(pr_url, &issues, agent_round),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
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

    #[test]
    fn review_check_turn_uses_readonly_profile() {
        let tools = restricted_tools(CapabilityProfile::ReadOnly).unwrap();
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Grep".to_string()));
        assert!(tools.contains(&"Glob".to_string()));
        assert!(!tools.contains(&"Write".to_string()));
        assert!(!tools.contains(&"Edit".to_string()));
        assert!(!tools.contains(&"Bash".to_string()));
    }

    #[test]
    fn periodic_review_turn_uses_standard_profile_with_bash() {
        let tools = restricted_tools(CapabilityProfile::Standard).unwrap();
        assert!(tools.contains(&"Bash".to_string()));
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Write".to_string()));
        assert!(tools.contains(&"Edit".to_string()));
        // Standard does not include Grep/Glob — it's distinct from ReadOnly.
        assert!(!tools.contains(&"Grep".to_string()));
    }

    #[test]
    fn implementation_turn_uses_full_profile_no_restriction() {
        // Full profile returns None — no tool restriction is applied to the agent.
        assert!(CapabilityProfile::Full.tools().is_none());
    }

    #[test]
    fn constitution_present_when_enabled() {
        let result = prepend_constitution("Do the task.".to_string(), true);
        assert!(result.contains("GP-01"));
        assert!(result.contains("GP-02"));
        assert!(result.contains("GP-03"));
        assert!(result.contains("GP-04"));
        assert!(result.contains("GP-05"));
        assert!(result.ends_with("Do the task."));
    }

    #[test]
    fn constitution_absent_when_disabled() {
        let result = prepend_constitution("Do the task.".to_string(), false);
        assert_eq!(result, "Do the task.");
        assert!(!result.contains("GP-01"));
    }
}
