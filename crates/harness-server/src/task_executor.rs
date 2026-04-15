pub(crate) mod agent_review;
pub(crate) mod helpers;
pub(crate) mod implement_pipeline;
pub(crate) mod pr_detection;
pub(crate) mod review_loop;
pub(crate) mod triage_pipeline;

use crate::task_runner::{mutate_and_persist, CreateTaskRequest, TaskId, TaskStatus, TaskStore};
use anyhow::Context;
use harness_core::agent::{
    AgentEvent, AgentRequest, AgentResponse, CodeAgent, StreamItem, TurnRequest,
};
use harness_core::config::agents::{AgentReviewConfig, CapabilityProfile};
use harness_core::error::HarnessError;
use harness_core::types::{Item, ThreadId, TokenUsage, TurnId, TurnStatus};
use harness_core::{
    config::project::{load_project_config, ProjectConfig},
    lang_detect, prompts,
};
use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use std::collections::{HashMap, HashSet};

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
#[cfg(test)]
use helpers::truncate_validation_error;
use helpers::{
    emit_runtime_notification, mark_turn_failed, persist_runtime_thread, process_stream_item,
    update_status,
};
use pr_detection::detect_repo_slug;
#[cfg(test)]
use pr_detection::{
    build_fix_ci_prompt, parse_harness_mention_command, HarnessMentionCommand, PromptBuilder,
};
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
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration, Instant};

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
async fn run_test_gate(
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

    // Get the adapter for this agent, if any.
    let adapter_opt = server.agent_registry.get_adapter(&agent_name);

    // Register as live adapter (RAII guard for cleanup on turn exit).
    // When an adapter is available, execution goes through adapter.start_turn()
    // (see below), so the adapter's stdin is initialized before any
    // steer/respond_approval call arrives.
    let _adapter_guard = adapter_opt.as_ref().map(|adapter_arc| {
        server
            .thread_manager
            .register_active_adapter(&turn_id, adapter_arc.clone());
        AdapterGuard {
            server: server.clone(),
            turn_id: turn_id.clone(),
        }
    });

    let stall_timeout = Duration::from_secs(server.config.concurrency.stall_timeout_secs);
    let (stream_tx, mut stream_rx) = mpsc::channel(128);

    // Only Codex uses the adapter for turn execution (initializes stdin so that
    // subsequent steer/respond_approval calls can find a live process).
    // Other adapters (e.g., ClaudeAdapter) exist for interrupt/steer only and must
    // not override the configured CodeAgent execution path — ClaudeCodeAgent carries
    // config-level settings (model, sandbox) that ClaudeAdapter does not replicate.
    let execution_adapter = adapter_opt
        .as_ref()
        .filter(|a| a.name() == "codex")
        .cloned();
    let mut execution: std::pin::Pin<
        Box<dyn std::future::Future<Output = harness_core::error::Result<()>> + Send>,
    > = if let Some(adapter_arc) = execution_adapter {
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
        // Move stream_tx into the bridge task so dropping it closes stream_rx.
        let bridge_tx = stream_tx;
        tokio::spawn(async move {
            let mut output_buf = String::new();
            while let Some(event) = event_rx.recv().await {
                let maybe_item: Option<StreamItem> = match event {
                    AgentEvent::MessageDelta { ref text } => {
                        output_buf.push_str(text);
                        Some(StreamItem::MessageDelta { text: text.clone() })
                    }
                    AgentEvent::ApprovalRequest { id, command } => {
                        Some(StreamItem::ApprovalRequest { id, command })
                    }
                    AgentEvent::Error { message } => Some(StreamItem::Error { message }),
                    AgentEvent::TurnCompleted { output } => {
                        let content = if output.is_empty() {
                            std::mem::take(&mut output_buf)
                        } else {
                            output
                        };
                        Some(StreamItem::ItemCompleted {
                            item: harness_core::types::Item::AgentReasoning { content },
                        })
                    }
                    _ => None,
                };
                if let Some(item) = maybe_item {
                    if bridge_tx.send(item).await.is_err() {
                        return;
                    }
                }
            }
            // event_rx closed → adapter done; dropping bridge_tx closes stream_rx.
        });
        let turn_req = TurnRequest {
            prompt,
            project_root,
            model: None,
            allowed_tools: vec![],
            context: vec![],
            timeout_secs: None,
        };
        Box::pin(async move { adapter_arc.start_turn(turn_req, event_tx).await })
    } else {
        let req = AgentRequest {
            prompt,
            project_root,
            ..Default::default()
        };
        Box::pin(agent.execute_stream(req, stream_tx))
    };
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
            Err(err) => {
                let error_msg = err.to_string();
                tracing::error!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    "failed to complete turn after execution: {error_msg}"
                );
                if let Err(e) = server.thread_manager.add_item(
                    &thread_id,
                    &turn_id,
                    harness_core::types::Item::Error {
                        code: -1,
                        message: format!("Failed to complete turn: {error_msg}"),
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
fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImplementationOutcome {
    PlanIssue(String),
    ParsedPr {
        pr_url: Option<String>,
        pr_num: Option<u64>,
        created_issue_num: Option<u64>,
    },
}

fn parse_implementation_outcome(output: &str) -> ImplementationOutcome {
    if let Some(desc) = prompts::parse_plan_issue(output) {
        return ImplementationOutcome::PlanIssue(desc);
    }
    let pr_url = prompts::parse_pr_url(output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);
    let created_issue_num = prompts::parse_created_issue_number(output);
    ImplementationOutcome::ParsedPr {
        pr_url,
        pr_num,
        created_issue_num,
    }
}

/// Persist a completed stream item as a task artifact when it carries content
/// worth retaining across context loss (shell commands, file edits, tool calls).
async fn persist_artifact(
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
async fn run_agent_streaming(
    agent: &dyn CodeAgent,
    req: AgentRequest,
    task_id: &TaskId,
    store: &TaskStore,
    turn: u32,
) -> harness_core::error::Result<(AgentResponse, Option<u64>)> {
    let turn_start = Instant::now();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamItem>(128);
    let mut exec = std::pin::pin!(agent.execute_stream(req, tx));
    let mut exec_result: Option<harness_core::error::Result<()>> = None;
    let mut channel_closed = false;
    let mut output = String::new();
    let mut token_usage = TokenUsage::default();
    let mut first_token_latency_ms: Option<u64> = None;
    // Tracks the last CREATED_ISSUE= number we successfully wrote to the DB so
    // we can (a) skip redundant writes and (b) detect agent self-corrections
    // that replace an earlier sentinel with a later one ("last sentinel wins").
    // Using Option<u64> rather than a boolean means each distinct value causes
    // exactly one DB write, and a second CREATED_ISSUE=20 after CREATED_ISSUE=10
    // correctly overwrites the stored external_id.
    let mut last_backfilled_issue: Option<u64> = None;
    // Pre-check whether this task is auto-fix to avoid a cache lookup on every
    // MessageDelta.  The source field is immutable after creation.
    let is_auto_fix_task = store
        .get(task_id)
        .is_some_and(|s| s.source.as_deref() == Some("auto-fix"));

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
                                if first_token_latency_ms.is_none() {
                                    first_token_latency_ms =
                                        Some(turn_start.elapsed().as_millis() as u64);
                                }
                                output.push_str(text);

                                // Early backfill: scan the full accumulated output on
                                // every delta.  Scanning the full buffer (rather than
                                // only the newly appended window) is required for two
                                // correctness properties:
                                //
                                // 1. Chunked streaming: if the sentinel was split across
                                //    chunk boundaries (e.g. "CREATED_ISSUE=" in one
                                //    delta, "42\n" in the next), a window-only scan
                                //    would miss the number.  The full-buffer scan sees
                                //    the complete sentinel once both chunks have arrived.
                                //
                                // 2. "Last sentinel wins": parse_created_issue_number
                                //    returns the LAST CREATED_ISSUE= in the output, so
                                //    a later self-correction (CREATED_ISSUE=20 after
                                //    CREATED_ISSUE=10) is written to the DB.
                                //    last_backfilled_issue prevents redundant writes
                                //    when the parsed number has not changed.
                                if is_auto_fix_task {
                                    // Only scan up to the last newline so that a
                                    // sentinel split across chunk boundaries (e.g.
                                    // "CREATED_ISSUE=4" then "2\n" in the next delta)
                                    // does not produce a spurious partial value.
                                    let complete_len =
                                        output.rfind('\n').map(|i| i + 1).unwrap_or(0);
                                    if let Some(issue_num) =
                                        prompts::parse_created_issue_number(
                                            &output[..complete_len],
                                        )
                                    {
                                        if Some(issue_num) != last_backfilled_issue {
                                            let eid = format!("issue:{issue_num}");
                                            match store
                                                .overwrite_external_id_auto_fix(task_id, &eid)
                                                .await
                                            {
                                                Ok(()) => tracing::info!(
                                                    task_id = %task_id,
                                                    external_id = %eid,
                                                    "streaming: backfilled external_id for auto-fix task"
                                                ),
                                                Err(e) => tracing::warn!(
                                                    task_id = %task_id,
                                                    "streaming: failed to backfill external_id: {e}"
                                                ),
                                            }
                                            last_backfilled_issue = Some(issue_num);
                                        }
                                    }
                                }
                            }
                            StreamItem::ItemCompleted {
                                item: Item::AgentReasoning { content },
                            } => {
                                // Non-streaming adapters (e.g. AnthropicApiAgent) call
                                // execute() to completion before emitting this item, so
                                // elapsed time here is full-request latency, not TTFB.
                                // Recording it as first_token_latency_ms would silently
                                // mix whole-request durations with real streaming TTFB
                                // values and inflate p50.  Only MessageDelta (true
                                // streaming events) sets first_token_latency_ms.
                                // Prefer the full content over accumulated deltas.
                                output = content.clone();
                                // For non-streaming adapters the full output arrives
                                // here; apply the same sentinel scan so backfill
                                // happens before the post-execution path runs (closes
                                // the webhook race for these adapters too).
                                if is_auto_fix_task {
                                    if let Some(issue_num) =
                                        prompts::parse_created_issue_number(&output)
                                    {
                                        if Some(issue_num) != last_backfilled_issue {
                                            let eid = format!("issue:{issue_num}");
                                            match store
                                                .overwrite_external_id_auto_fix(task_id, &eid)
                                                .await
                                            {
                                                Ok(()) => tracing::info!(
                                                    task_id = %task_id,
                                                    external_id = %eid,
                                                    "streaming: backfilled external_id for auto-fix task"
                                                ),
                                                Err(e) => tracing::warn!(
                                                    task_id = %task_id,
                                                    "streaming: failed to backfill external_id: {e}"
                                                ),
                                            }
                                            last_backfilled_issue = Some(issue_num);
                                        }
                                    }
                                }
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
        Ok(()) => Ok((
            AgentResponse {
                output,
                stderr: String::new(),
                items: Vec::new(),
                token_usage,
                model: String::new(),
                exit_code: Some(0),
            },
            first_token_latency_ms,
        )),
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

/// Shared task execution context passed to all pipeline phase modules.
///
/// Built once in `run_task` and passed by reference to each phase, eliminating
/// the need to thread individual parameters through every pipeline call.
pub(crate) struct TaskContext<'a> {
    pub store: &'a TaskStore,
    pub task_id: &'a TaskId,
    pub agent: &'a dyn CodeAgent,
    pub reviewer: Option<&'a dyn CodeAgent>,
    pub skills: std::sync::Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    pub events: std::sync::Arc<harness_observe::event_store::EventStore>,
    pub interceptors:
        std::sync::Arc<Vec<std::sync::Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    pub req: &'a CreateTaskRequest,
    pub project: std::path::PathBuf,
    pub server_config: &'a harness_core::config::HarnessConfig,
    pub cargo_env: std::collections::HashMap<String, String>,
    pub project_config: ProjectConfig,
    pub review_config: AgentReviewConfig,
    pub repo_slug: String,
    pub turn_timeout: tokio::time::Duration,
    pub jaccard_threshold: f64,
    pub task_start: tokio::time::Instant,
    pub resolved_review_wait_secs: Option<u64>,
    pub resolved_review_max_rounds: Option<u32>,
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    server_config: &harness_core::config::HarnessConfig,
    // Accumulated turn count from previous transient-retry attempts.
    // Ensures the max_turns budget is global across the full task lifecycle,
    // not reset on each retry (fix for budget-reset-on-retry bug).
    turns_used_acc: &mut u32,
) -> anyhow::Result<()> {
    let task_start = Instant::now();

    if !project.exists() {
        anyhow::bail!("project_root does not exist: {}", project.display());
    }

    // Set CARGO_TARGET_DIR to a per-task temp path so parallel agents running
    // cargo check/test simultaneously do not contend on the same build directory.
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

    let project_config = load_project_config(&project).with_context(|| {
        format!(
            "failed to load project config for task {} at {}",
            task_id.as_str(),
            project.display()
        )
    })?;
    let resolved = harness_core::config::resolve::resolve_config(server_config, &project_config);
    let repo_slug = detect_repo_slug(&project)
        .await
        .unwrap_or_else(|| "{owner}/{repo}".to_string());

    // --- Checkpoint-based resume detection ---
    // Load checkpoint and task state to determine if we can skip phases.
    // This is the duplicate-PR prevention gate: if the task already has a PR,
    // we skip triage/plan/implement and jump directly to review.
    //
    // Check task store for an existing pr_url first — this survives checkpoint
    // read failures (e.g. transient SQLite contention) and lets us safely
    // resume review even when the checkpoint row is temporarily unreadable.
    let task_pr_url: Option<String> = store.get(task_id).and_then(|t| t.pr_url);
    let checkpoint = match store.load_checkpoint(task_id).await {
        Ok(cp) => cp,
        Err(e) => {
            if task_pr_url.is_some() {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "checkpoint load failed but task already has pr_url; resuming review without checkpoint"
                );
                None
            } else {
                return Err(e).with_context(|| {
                    format!(
                        "failed to load checkpoint for task {}; aborting to prevent duplicate PR",
                        task_id
                    )
                });
            }
        }
    };
    let resumed_pr_url: Option<String> =
        task_pr_url.or_else(|| checkpoint.as_ref().and_then(|c| c.pr_url.clone()));
    let resumed_plan: Option<String> = checkpoint.and_then(|c| c.plan_output);

    // --- Build shared TaskContext ---
    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);
    let jaccard_threshold = server_config.concurrency.loop_jaccard_threshold;
    let ctx = TaskContext {
        store,
        task_id,
        agent,
        reviewer,
        skills,
        events,
        interceptors,
        req,
        project: project.clone(),
        server_config,
        cargo_env,
        project_config: project_config.clone(),
        review_config: resolved.review.clone(),
        repo_slug,
        turn_timeout,
        jaccard_threshold,
        task_start,
        resolved_review_wait_secs: resolved.review_wait_secs,
        resolved_review_max_rounds: resolved.review_max_rounds,
    };

    // --- Triage → Plan pipeline ---
    let triage_outcome =
        triage_pipeline::run(&ctx, resumed_pr_url.as_deref(), resumed_plan).await?;
    let (plan_output, triage_complexity, pipeline_turns) = match triage_outcome {
        triage_pipeline::TriageOutcome::Skip => return Ok(()),
        triage_pipeline::TriageOutcome::Proceed {
            plan,
            complexity,
            pipeline_turns,
        } => (plan, complexity, pipeline_turns),
    };

    // Derive dynamic parameters from triage complexity.
    // Triage provides a DEFAULT only — caller's explicit max_rounds always wins.
    // Low complexity no longer skips agent review to preserve the review gate.
    let (triage_default_rounds, skip_agent_review) = match triage_complexity {
        prompts::TriageComplexity::Low => (2u32, false),
        prompts::TriageComplexity::Medium => (8u32, false),
        prompts::TriageComplexity::High => (8u32, false),
    };
    let effective_max_rounds = req.max_rounds.unwrap_or(triage_default_rounds);
    let effective_max_turns: Option<u32> = req.max_turns.or(server_config.concurrency.max_turns);
    let mut turns_used: u32 = *turns_used_acc + pipeline_turns;
    *turns_used_acc = turns_used;
    tracing::info!(
        task_id = %task_id,
        ?triage_complexity,
        effective_max_rounds,
        skip_agent_review,
        ?effective_max_turns,
        "triage complexity applied"
    );

    // --- Implementation phase ---
    update_status(store, task_id, TaskStatus::Implementing, 1).await?;
    let impl_outcome = implement_pipeline::run(
        &ctx,
        plan_output,
        resumed_pr_url,
        effective_max_turns,
        &mut turns_used,
    )
    .await?;
    *turns_used_acc = turns_used;

    let (pr_url, pr_num, context_items, mut agent_pushed_commit) = match impl_outcome {
        implement_pipeline::ImplementOutcome::Handled => return Ok(()),
        implement_pipeline::ImplementOutcome::PrReady {
            pr_url,
            pr_num,
            context_items,
            agent_pushed_commit,
        } => (pr_url, pr_num, context_items, agent_pushed_commit),
    };

    // --- Agent review phase (if enabled) ---
    if ctx.review_config.enabled && !skip_agent_review {
        let outcome = agent_review::run(
            &ctx,
            pr_url.as_deref().unwrap_or(""),
            &context_items,
            effective_max_turns,
            &mut turns_used,
        )
        .await?;
        *turns_used_acc = turns_used;
        match outcome {
            agent_review::AgentReviewOutcome::Handled => return Ok(()),
            agent_review::AgentReviewOutcome::ReviewComplete { pushed_commit } => {
                if pushed_commit {
                    agent_pushed_commit = true;
                }
            }
        }
    }

    // Skip external review bot wait when auto-trigger is disabled — there is
    // no bot to wait for, so the loop would always exhaust all rounds and fail.
    if !ctx.review_config.review_bot_auto_trigger {
        tracing::info!("review_bot_auto_trigger disabled; skipping external review wait");
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
        store.log_event(crate::event_replay::TaskEvent::Completed {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 2,
            pr_url = pr_url.as_deref().unwrap_or(""),
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
    }

    // --- External review bot wait loop ---
    review_loop::run(
        &ctx,
        pr_url,
        pr_num,
        context_items,
        effective_max_rounds,
        effective_max_turns,
        agent_pushed_commit,
        &mut turns_used,
    )
    .await?;
    *turns_used_acc = turns_used;

    Ok(())
}

/// Compute Jaccard word-similarity between two strings.
///
/// Tokenizes each string into a set of non-empty words (split on non-alphanumeric chars),
/// then returns |intersection| / |union|.
/// Both empty → 1.0; one empty → 0.0.
fn jaccard_word_similarity(a: &str, b: &str) -> f64 {
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

/// Normalize a set of review issues into a canonical ordered form.
/// Issues are sorted by reference before collecting so that insertion order does not affect
/// equality comparisons, and strings are not unnecessarily cloned during the sort.
fn normalize_issues(issues: &[String]) -> Vec<String> {
    let mut sorted: Vec<_> = issues.iter().collect();
    sorted.sort();
    sorted.into_iter().cloned().collect()
}

// run_agent_review removed — replaced by agent_review::run (issue #772)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_review_source_uses_standard_allowed_tools() {
        // Verifies that periodic_review tasks get a non-empty allowed_tools list,
        // which causes claude.rs to pass --allowedTools (hard enforcement) instead
        // of --dangerously-skip-permissions.
        let tools = restricted_tools(CapabilityProfile::Standard).unwrap_or_default();
        assert_eq!(
            tools,
            CapabilityProfile::Standard.tools().unwrap_or_default()
        );
        assert!(!tools.is_empty());
    }

    #[test]
    fn standard_implementation_turn_uses_full_profile() {
        // Non-periodic_review tasks use None → Full profile →
        // --dangerously-skip-permissions in claude.rs.
        let implementation_allowed_tools: Option<Vec<String>> = None;
        assert!(implementation_allowed_tools.is_none());
    }

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
    fn parse_implementation_outcome_prefers_plan_issue() {
        let output = "PLAN_ISSUE=Plan missed rollback path\nPR_URL=https://github.com/o/r/pull/123";
        let parsed = parse_implementation_outcome(output);
        assert_eq!(
            parsed,
            ImplementationOutcome::PlanIssue("Plan missed rollback path".to_string())
        );
    }

    #[test]
    fn parse_implementation_outcome_extracts_pr_when_no_plan_issue() {
        let output = "Done.\nPR_URL=https://github.com/majiayu000/harness/pull/42";
        let parsed = parse_implementation_outcome(output);
        assert_eq!(
            parsed,
            ImplementationOutcome::ParsedPr {
                pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
                pr_num: Some(42),
                created_issue_num: None,
            }
        );
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

    #[test]
    fn normalize_issues_is_order_invariant() {
        let ordered = vec!["issue A".to_string(), "issue B".to_string()];
        let reversed = vec!["issue B".to_string(), "issue A".to_string()];
        assert_eq!(normalize_issues(&ordered), normalize_issues(&reversed));
    }

    fn step_tracker(
        tracker: &mut Option<(Vec<String>, u32)>,
        issues: &[String],
    ) -> (u32, bool, bool) {
        let normalized = normalize_issues(issues);
        let count = match tracker.as_ref() {
            Some((prev, c)) if *prev == normalized => c + 1,
            _ => 1,
        };
        *tracker = Some((normalized, count));
        let intervention = count >= 3;
        let fatal = count >= 5;
        (count, intervention, fatal)
    }

    #[test]
    fn impasse_no_intervention_for_first_two_rounds() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        let (c1, i1, f1) = step_tracker(&mut tracker, &issues);
        let (c2, i2, f2) = step_tracker(&mut tracker, &issues);
        assert_eq!(c1, 1);
        assert!(!i1 && !f1, "no action on first occurrence");
        assert_eq!(c2, 2);
        assert!(!i2 && !f2, "no action on second occurrence");
    }

    #[test]
    fn impasse_intervention_at_third_consecutive_round() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        step_tracker(&mut tracker, &issues); // round 1
        step_tracker(&mut tracker, &issues); // round 2
        let (c3, i3, f3) = step_tracker(&mut tracker, &issues); // round 3
        assert_eq!(c3, 3);
        assert!(i3, "intervention at 3rd consecutive round");
        assert!(!f3, "not yet fatal at round 3");
    }

    #[test]
    fn impasse_fatal_at_fifth_consecutive_round() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        for _ in 0..4 {
            step_tracker(&mut tracker, &issues);
        }
        let (c5, i5, f5) = step_tracker(&mut tracker, &issues);
        assert_eq!(c5, 5);
        assert!(i5, "intervention still active at round 5");
        assert!(f5, "fatal at 5th consecutive round");
    }

    #[test]
    fn impasse_counter_resets_when_issues_change() {
        let issues = vec!["null pointer".to_string()];
        let other = vec!["different bug".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        step_tracker(&mut tracker, &issues); // count 1
        step_tracker(&mut tracker, &issues); // count 2
        step_tracker(&mut tracker, &issues); // count 3 — would trigger intervention
        let (c_reset, i_reset, _) = step_tracker(&mut tracker, &other); // different issues
        assert_eq!(c_reset, 1, "counter resets on different issues");
        assert!(!i_reset, "no intervention after reset");
    }

    // --- jaccard_word_similarity unit tests ---

    #[test]
    fn jaccard_identical_strings() {
        assert_eq!(jaccard_word_similarity("hello world", "hello world"), 1.0);
    }

    #[test]
    fn jaccard_disjoint_strings() {
        assert_eq!(jaccard_word_similarity("foo bar", "baz qux"), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {"a", "b"} ∩ {"b", "c"} = {"b"}, union = {"a","b","c"} → 1/3
        let score = jaccard_word_similarity("a b", "b c");
        let expected = 1.0_f64 / 3.0_f64;
        assert!(
            (score - expected).abs() < 1e-10,
            "expected ~{expected}, got {score}"
        );
    }

    #[test]
    fn jaccard_one_empty() {
        assert_eq!(jaccard_word_similarity("", "hello world"), 0.0);
        assert_eq!(jaccard_word_similarity("hello world", ""), 0.0);
    }

    #[test]
    fn jaccard_both_empty() {
        assert_eq!(jaccard_word_similarity("", ""), 1.0);
    }
}
