use crate::task_runner::{TaskId, TaskKind, TaskStatus, TaskStore};
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::error::HarnessError;
use harness_core::interceptor::{ToolUseEvent, TurnInterceptor};
use harness_core::types::{
    ContextItem, Decision, Event, EventMetadata, SessionId, SkillId, ThreadId, TurnFailure, TurnId,
    TurnStatus, TurnTelemetry,
};
use harness_observe::event_store::EventStore;
use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) mod streaming;
pub(crate) use streaming::{
    run_agent_streaming, run_agent_streaming_with_options, RunAgentStreamingOptions,
};

/// Truncate validation error output to `max_chars` to avoid bloating agent prompts.
/// Preserves the first portion which typically contains the most actionable info.
pub(crate) fn truncate_validation_error(error: &str, max_chars: usize) -> String {
    if error.len() <= max_chars {
        return error.to_string();
    }
    // Find the last valid char boundary at or before max_chars.
    let mut boundary = max_chars;
    while boundary > 0 && !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let truncated = &error[..boundary];
    format!(
        "{truncated}\n\n... (output truncated, {total} chars total)",
        total = error.len()
    )
}

#[derive(Debug)]
pub(crate) struct TurnExecutionSuccess {
    pub response: AgentResponse,
    pub telemetry: TurnTelemetry,
}

#[derive(Debug)]
pub(crate) struct TurnExecutionFailure {
    pub error: HarnessError,
    pub telemetry: TurnTelemetry,
    pub failure: TurnFailure,
}

pub(crate) fn is_prompt_only_task(store: &TaskStore, task_id: &TaskId) -> bool {
    store
        .get(task_id)
        .is_some_and(|state| matches!(state.task_kind, TaskKind::Prompt))
}

pub(crate) fn telemetry_for_timeout(
    prompt_built_at: DateTime<Utc>,
    agent_started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    retry_count: Option<u32>,
) -> TurnTelemetry {
    let completed_latency_ms = completed_at
        .signed_duration_since(agent_started_at)
        .num_milliseconds()
        .max(0) as u64;
    TurnTelemetry {
        prompt_built_at: Some(prompt_built_at),
        agent_started_at: Some(agent_started_at),
        first_output_at: None,
        completed_at: Some(completed_at),
        first_token_latency_ms: None,
        completed_latency_ms: Some(completed_latency_ms),
        retry_count,
        exit_code: None,
    }
}

pub(crate) fn build_task_event(
    task_id: &TaskId,
    turn: u32,
    phase: &str,
    hook: &str,
    decision: Decision,
    reason: Option<String>,
    detail: Option<String>,
    telemetry: Option<TurnTelemetry>,
    failure: Option<TurnFailure>,
    content: Option<String>,
) -> Event {
    let mut event = Event::new(SessionId::new(), hook, "task_runner", decision);
    event.reason = reason;
    event.detail = detail;
    event.content = content;
    event.metadata = Some(EventMetadata {
        task_id: Some(task_id.clone()),
        turn: Some(turn),
        phase: Some(phase.to_string()),
        telemetry,
        failure,
    });
    event
}

/// Run all pre_execute interceptors in order. Returns the (possibly modified) request,
/// or an error if any interceptor returns Block.
pub(crate) async fn run_pre_execute(
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
pub(crate) async fn run_post_execute(
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

pub(crate) async fn run_on_error(
    interceptors: &[Arc<dyn TurnInterceptor>],
    req: &AgentRequest,
    error: &str,
) {
    for interceptor in interceptors {
        interceptor.on_error(req, error).await;
    }
}

/// Call `post_tool_use` on all interceptors for the given tool-use event.
///
/// This is the hook injection point for file-write events in the agent pipeline.
/// Returns the first non-empty violation feedback found, or `None` when all
/// interceptors pass cleanly.
pub(crate) async fn run_post_tool_use(
    interceptors: &[Arc<dyn TurnInterceptor>],
    event: &ToolUseEvent,
    project_root: &Path,
) -> Option<String> {
    for interceptor in interceptors {
        let result = interceptor.post_tool_use(event, project_root).await;
        if let Some(feedback) = result.violation_feedback {
            return Some(format!("[{}] {}", interceptor.name(), feedback));
        }
    }
    None
}

/// Detect files added or modified in `project_root`.
///
/// Host-side git inspection is disabled by project policy, so this returns an
/// empty list. Agents remain responsible for reporting modified files in their
/// final output and validation prompts.
pub(crate) async fn detect_modified_files(project_root: &Path) -> Vec<std::path::PathBuf> {
    tracing::debug!(
        project_root = %project_root.display(),
        "detect_modified_files: host-side git inspection disabled"
    );
    Vec::new()
}

pub(crate) fn emit_runtime_notification(
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    notification: Notification,
) {
    crate::notify::emit(notify_tx, notification.clone());
    let _ = notification_tx.send(RpcNotification::new(notification));
}

pub(crate) async fn persist_runtime_thread(
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

pub(crate) async fn process_stream_item(
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
            if let Err(err) =
                server
                    .thread_manager
                    .set_turn_token_usage(thread_id, turn_id, usage.clone())
            {
                tracing::warn!("failed to update turn token usage: {err}");
            } else {
                persist_runtime_thread(thread_db, server, thread_id).await;
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
                harness_core::types::Item::Error { code: -1, message },
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
        StreamItem::ToolOutputDelta { item_id, text } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ToolOutputDelta {
                    turn_id: turn_id.clone(),
                    item_id,
                    text,
                },
            );
        }
        StreamItem::ApprovalRequest { id, command } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                harness_core::types::Item::ApprovalRequest {
                    id: Some(id.clone()),
                    action: command.clone(),
                    approved: None,
                },
            ) {
                tracing::warn!("failed to append approval request item to turn: {err}");
            } else {
                persist_runtime_thread(thread_db, server, thread_id).await;
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ApprovalRequest {
                    turn_id: turn_id.clone(),
                    request_id: id,
                    command,
                },
            );
        }
        StreamItem::Warning { message } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::Warning {
                    turn_id: turn_id.clone(),
                    message,
                },
            );
        }
        _ => {}
    }
}

pub(crate) async fn mark_turn_failed(
    server: &crate::server::HarnessServer,
    thread_db: &Option<crate::thread_db::ThreadDb>,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    error: String,
) {
    if let Err(err) = server.thread_manager.fail_turn(thread_id, turn_id) {
        tracing::warn!("failed to mark turn as failed: {err}");
    } else {
        persist_runtime_thread(thread_db, server, thread_id).await;
    }
    emit_runtime_notification(
        notify_tx,
        notification_tx,
        Notification::TurnCompleted {
            turn_id: turn_id.clone(),
            status: TurnStatus::Failed,
            token_usage: harness_core::types::TokenUsage::default(),
        },
    );
    tracing::error!("turn failed: {error}");
}

pub(crate) async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    round: u32,
) -> anyhow::Result<()> {
    crate::task_runner::update_status(store, task_id, status, round).await
}

/// Build the context item list for an agent request: skills matching the prompt
/// trigger patterns plus any cascading AGENTS.md content found under
/// `project_root`.
pub(crate) async fn collect_context_items(
    skills: &RwLock<harness_skills::store::SkillStore>,
    project_root: &Path,
    prompt: &str,
) -> Vec<ContextItem> {
    let mut items: Vec<ContextItem> = {
        let guard = skills.read().await;
        guard
            .match_prompt(prompt)
            .into_iter()
            .map(|s| ContextItem::Skill {
                id: s.id.to_string(),
                content: s.content.clone(),
            })
            .collect()
    };
    let agents_md = harness_core::agents_md::load_agents_md(project_root);
    if !agents_md.is_empty() {
        items.push(ContextItem::AgentsMd { content: agents_md });
    }
    items
}

/// Inject project instruction files directly into the prompt text.
///
/// Harness currently invokes CLI agents in single-turn mode (`claude -p` /
/// `codex ...`), and `AgentRequest.context` is retained for observability rather
/// than automatically forwarded to the CLI. Embed AGENTS.md / CLAUDE.md content
/// in the prompt so agents actually see project-specific rules.
pub(crate) fn inject_project_context_into_prompt(project_root: &Path, prompt: String) -> String {
    let project_context = harness_core::agents_md::load_agents_md(project_root);
    if project_context.trim().is_empty() {
        return prompt;
    }

    format!(
        "{prompt}\n\n## Project Instructions\n\
         The following trusted project instructions were loaded from AGENTS.md / CLAUDE.md files. \
         Follow them for this task.\n\n{project_context}"
    )
}

/// Return all skills whose trigger patterns match `prompt`, including their IDs
/// and names for observability/event logging.
pub(crate) async fn matched_skills_for_prompt(
    skills: &RwLock<harness_skills::store::SkillStore>,
    prompt: &str,
) -> Vec<(SkillId, String)> {
    let guard = skills.read().await;
    guard
        .match_prompt(prompt)
        .into_iter()
        .map(|s| (s.id.clone(), s.name.clone()))
        .collect()
}

/// Collect matched skills and all skills from the store, record usage for
/// matches, and return both.
///
/// Lock discipline: holds read lock briefly to collect data, drops it, then
/// acquires write lock only if there are matches. Never holds both locks
/// simultaneously.
async fn collect_and_record_skill_matches(
    skills: &RwLock<harness_skills::store::SkillStore>,
    prompt: &str,
) -> (Vec<(SkillId, String, String)>, Vec<(String, String)>) {
    {
        let guard = skills.read().await;
        if guard.list().is_empty() {
            return (vec![], vec![]);
        }
    }
    let (matched, all) = {
        let guard = skills.read().await;
        let matched: Vec<(SkillId, String, String)> = guard
            .match_prompt(prompt)
            .into_iter()
            .map(|s| (s.id.clone(), s.name.clone(), s.content.clone()))
            .collect();
        let all: Vec<(String, String)> = guard
            .list()
            .iter()
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect();
        (matched, all)
    };
    if !matched.is_empty() {
        let mut guard = skills.write().await;
        for (id, _, _) in &matched {
            guard.record_use(id);
        }
    }
    (matched, all)
}

/// Augment a prompt with matching skills: match once, record usage, log `skill_used` events,
/// and return the augmented prompt string. Replaces the repeated
/// `matched_skills_for_prompt` + `inject_skills_into_prompt` + event-log loop pattern.
pub(crate) async fn augment_prompt_with_skills(
    skills: &RwLock<harness_skills::store::SkillStore>,
    events: &EventStore,
    task_id: &TaskId,
    prompt: String,
) -> String {
    let (matched, all_skills) = collect_and_record_skill_matches(skills, &prompt).await;

    for (skill_id, skill_name, _) in &matched {
        let mut ev = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        ev.reason = Some(skill_name.clone());
        ev.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&ev).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }

    let listing = harness_core::prompts::build_available_skills_listing(
        all_skills.iter().map(|(n, d)| (n.as_str(), d.as_str())),
    );
    let section = harness_core::prompts::build_matched_skills_section(
        matched.iter().map(|(_, n, c)| (n.as_str(), c.as_str())),
    );
    let mut additions = listing;
    additions.push_str(&section);

    if additions.is_empty() {
        prompt
    } else {
        prompt + &additions
    }
}

/// Match skills against the prompt, record usage for each match, and return a
/// string to append directly to the agent prompt.
///
/// Since harness uses single-turn `claude -p`, context items are not visible to
/// agents — this function injects skill content into the prompt text itself.
///
/// Two sections are appended:
/// - **Available Skills**: a brief listing of every skill (name + description).
/// - **Relevant Skills**: full content of skills whose trigger patterns matched.
///
/// Returns an empty string when the store is empty (no sections added).
/// The "Relevant Skills" section is omitted when no skills match.
pub(crate) async fn inject_skills_into_prompt(
    skills: &RwLock<harness_skills::store::SkillStore>,
    prompt: &str,
) -> String {
    let (matched_data, all_skills) = collect_and_record_skill_matches(skills, prompt).await;

    let listing = harness_core::prompts::build_available_skills_listing(
        all_skills.iter().map(|(n, d)| (n.as_str(), d.as_str())),
    );
    let section = harness_core::prompts::build_matched_skills_section(
        matched_data
            .iter()
            .map(|(_, n, c)| (n.as_str(), c.as_str())),
    );
    let mut result = listing;
    result.push_str(&section);
    result
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

#[cfg(test)]
#[path = "helpers_tests.rs"]
mod tests;
