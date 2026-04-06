use crate::task_runner::{TaskId, TaskStatus, TaskStore};
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::interceptor::{ToolUseEvent, TurnInterceptor};
use harness_core::types::{ContextItem, Decision, SkillId, ThreadId, TurnId, TurnStatus};
use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

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

/// Detect files added or modified in `project_root` via `git status --porcelain`.
///
/// Deleted entries are excluded. Returns an empty list when git is unavailable
/// or `project_root` is not inside a git repository.
pub(crate) async fn detect_modified_files(project_root: &Path) -> Vec<std::path::PathBuf> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(project_root)
        .output()
        .await;
    match output {
        Ok(out) => {
            if !out.status.success() {
                tracing::debug!(
                    project_root = %project_root.display(),
                    status = ?out.status.code(),
                    "detect_modified_files: git status failed"
                );
                return Vec::new();
            }
            parse_porcelain_z_paths(&out.stdout)
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                project_root = %project_root.display(),
                "detect_modified_files: git status unavailable"
            );
            Vec::new()
        }
    }
}

fn parse_porcelain_z_paths(stdout: &[u8]) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let mut records = stdout.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 3 {
            continue;
        }

        let x = record[0] as char;
        let y = record[1] as char;
        let is_rename_or_copy = matches!(x, 'R' | 'C');

        if x == 'D' || y == 'D' {
            if is_rename_or_copy {
                let _ = records.next();
            }
            continue;
        }

        let mut path_bytes = &record[3..];
        if is_rename_or_copy {
            if let Some(new_path) = records.next() {
                path_bytes = new_path;
            } else {
                continue;
            }
        }

        if path_bytes.is_empty() {
            continue;
        }
        paths.push(std::path::PathBuf::from(
            String::from_utf8_lossy(path_bytes).into_owned(),
        ));
    }
    paths
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
        StreamItem::ApprovalRequest { id, command } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                harness_core::types::Item::ApprovalRequest {
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
///
/// Lock discipline: holds read lock briefly, drops it, then acquires write lock
/// only if there are matched skills to record usage for. Never holds both locks
/// simultaneously.
pub(crate) async fn inject_skills_into_prompt(
    skills: &RwLock<harness_skills::store::SkillStore>,
    prompt: &str,
) -> String {
    // Early return when the store is empty — avoids acquiring locks unnecessarily.
    {
        let guard = skills.read().await;
        if guard.list().is_empty() {
            return String::new();
        }
    }

    // Read phase: collect all needed data while holding read lock minimally.
    let (matched_data, all_skills) = {
        let guard = skills.read().await;
        let matched: Vec<(harness_core::types::SkillId, String, String)> = guard
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

    // Write phase: record usage for matched skills (brief write lock).
    if !matched_data.is_empty() {
        let mut guard = skills.write().await;
        for (id, _, _) in &matched_data {
            guard.record_use(id);
        }
    }

    // Build prompt additions.
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

#[cfg(test)]
#[path = "helpers_tests.rs"]
mod tests;
