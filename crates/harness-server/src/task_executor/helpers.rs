use crate::task_runner::{TaskId, TaskStatus, TaskStore};
use harness_core::{
    interceptor::{ToolUseEvent, TurnInterceptor},
    AgentRequest, AgentResponse, ContextItem, Decision, StreamItem, ThreadId, TurnId, TurnStatus,
};
use harness_protocol::{Notification, RpcNotification};
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
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .await;
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.len() < 4 {
                    return None;
                }
                if line[..2].contains('D') {
                    return None;
                }
                Some(std::path::PathBuf::from(line[3..].trim()))
            })
            .collect(),
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
                harness_core::Item::Error { code: -1, message },
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
            token_usage: harness_core::TokenUsage::default(),
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
    skills: &RwLock<harness_skills::SkillStore>,
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
    skills: &RwLock<harness_skills::SkillStore>,
    prompt: &str,
) -> String {
    // Read phase: collect all needed data while holding read lock minimally.
    let (matched_data, all_skills) = {
        let guard = skills.read().await;
        let matched: Vec<(harness_core::SkillId, String, String)> = guard
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
    format!("{listing}{section}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::{
        interceptor::{
            InterceptResult, PostExecuteResult, PostToolUseResult, ToolUseEvent, TurnInterceptor,
        },
        AgentRequest, AgentResponse, Decision, TokenUsage,
    };
    use std::sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    };

    // ── Mock helpers ─────────────────────────────────────────────────────────

    fn make_req() -> AgentRequest {
        AgentRequest {
            prompt: "test prompt".to_string(),
            project_root: std::path::PathBuf::from("/tmp"),
            ..Default::default()
        }
    }

    fn make_resp() -> AgentResponse {
        AgentResponse {
            output: "done".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        }
    }

    // ── Mock interceptors ─────────────────────────────────────────────────────

    struct PassInterceptor;

    #[async_trait]
    impl TurnInterceptor for PassInterceptor {
        fn name(&self) -> &str {
            "pass"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::pass()
        }
    }

    struct BlockInterceptor {
        reason: String,
    }

    impl BlockInterceptor {
        fn new(reason: impl Into<String>) -> Self {
            Self {
                reason: reason.into(),
            }
        }
    }

    #[async_trait]
    impl TurnInterceptor for BlockInterceptor {
        fn name(&self) -> &str {
            "block"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::block(self.reason.clone())
        }
    }

    struct WarnInterceptor;

    #[async_trait]
    impl TurnInterceptor for WarnInterceptor {
        fn name(&self) -> &str {
            "warn"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::warn("non-fatal warning")
        }
    }

    struct ModifyingInterceptor;

    #[async_trait]
    impl TurnInterceptor for ModifyingInterceptor {
        fn name(&self) -> &str {
            "modifying"
        }
        async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult {
            let mut modified = req.clone();
            modified.prompt = format!("MODIFIED: {}", req.prompt);
            InterceptResult {
                decision: Decision::Pass,
                reason: None,
                request: Some(modified),
            }
        }
    }

    struct FailingPostInterceptor;

    #[async_trait]
    impl TurnInterceptor for FailingPostInterceptor {
        fn name(&self) -> &str {
            "failing_post"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::pass()
        }
        async fn post_execute(
            &self,
            _req: &AgentRequest,
            _resp: &AgentResponse,
        ) -> PostExecuteResult {
            PostExecuteResult::fail("validation failed")
        }
    }

    struct CountingErrorInterceptor {
        count: Arc<AtomicU32>,
    }

    #[async_trait]
    impl TurnInterceptor for CountingErrorInterceptor {
        fn name(&self) -> &str {
            "counting_error"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::pass()
        }
        async fn on_error(&self, _req: &AgentRequest, _error: &str) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ViolatingToolInterceptor;

    #[async_trait]
    impl TurnInterceptor for ViolatingToolInterceptor {
        fn name(&self) -> &str {
            "violating_tool"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::pass()
        }
        async fn post_tool_use(
            &self,
            _event: &ToolUseEvent,
            _root: &std::path::Path,
        ) -> PostToolUseResult {
            PostToolUseResult::with_violations("found a violation")
        }
    }

    /// Interceptor that records whether `pre_execute` was invoked.
    struct CallTrackedInterceptor {
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TurnInterceptor for CallTrackedInterceptor {
        fn name(&self) -> &str {
            "call_tracked"
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            self.called.store(true, Ordering::SeqCst);
            InterceptResult::pass()
        }
    }

    /// Failing post interceptor with a configurable name and error message.
    struct NamedFailingPostInterceptor {
        name_str: &'static str,
        error_msg: &'static str,
    }

    #[async_trait]
    impl TurnInterceptor for NamedFailingPostInterceptor {
        fn name(&self) -> &str {
            self.name_str
        }
        async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
            InterceptResult::pass()
        }
        async fn post_execute(
            &self,
            _req: &AgentRequest,
            _resp: &AgentResponse,
        ) -> PostExecuteResult {
            PostExecuteResult::fail(self.error_msg)
        }
    }

    fn wrap<T: TurnInterceptor + 'static>(t: T) -> Arc<dyn TurnInterceptor> {
        Arc::new(t)
    }

    // ── run_pre_execute ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_pre_execute_passes_with_pass_interceptor() {
        let interceptors = vec![wrap(PassInterceptor)];
        let result = run_pre_execute(&interceptors, make_req()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_pre_execute_fails_with_blocking_interceptor() {
        let interceptors = vec![wrap(BlockInterceptor::new("not allowed"))];
        let result = run_pre_execute(&interceptors, make_req()).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Blocked by interceptor"));
        assert!(msg.contains("not allowed"));
    }

    #[tokio::test]
    async fn run_pre_execute_warn_does_not_block() {
        let interceptors = vec![wrap(WarnInterceptor)];
        let result = run_pre_execute(&interceptors, make_req()).await;
        assert!(result.is_ok(), "warn should not block execution");
    }

    #[tokio::test]
    async fn run_pre_execute_returns_modified_request() {
        let interceptors = vec![wrap(ModifyingInterceptor)];
        let req = make_req();
        let result = run_pre_execute(&interceptors, req).await.unwrap();
        assert!(
            result.prompt.starts_with("MODIFIED:"),
            "interceptor should have modified the prompt"
        );
    }

    #[tokio::test]
    async fn run_pre_execute_empty_interceptors_returns_original() {
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![];
        let req = make_req();
        let result = run_pre_execute(&interceptors, req.clone()).await.unwrap();
        assert_eq!(result.prompt, req.prompt);
    }

    #[tokio::test]
    async fn run_pre_execute_stops_chain_at_first_block() {
        // Prove the second interceptor is never invoked when the first blocks.
        let second_called = Arc::new(AtomicBool::new(false));
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![
            Arc::new(BlockInterceptor::new("early block")),
            Arc::new(CallTrackedInterceptor {
                called: second_called.clone(),
            }),
        ];
        let result = run_pre_execute(&interceptors, make_req()).await;
        assert!(result.is_err(), "should fail due to block");
        assert!(
            !second_called.load(Ordering::SeqCst),
            "interceptor after block must not be called"
        );
    }

    // ── run_post_execute ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_post_execute_returns_none_when_all_pass() {
        let interceptors = vec![wrap(PassInterceptor)];
        let result = run_post_execute(&interceptors, &make_req(), &make_resp()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_post_execute_returns_error_when_interceptor_fails() {
        let interceptors = vec![wrap(FailingPostInterceptor)];
        let result = run_post_execute(&interceptors, &make_req(), &make_resp()).await;
        assert!(result.is_some());
        let err = result.unwrap();
        assert!(
            err.contains("failing_post"),
            "error should name the interceptor"
        );
        assert!(err.contains("validation failed"));
    }

    #[tokio::test]
    async fn run_post_execute_returns_first_failure_only() {
        // Two interceptors with distinct names and messages — only the first must appear.
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![
            Arc::new(NamedFailingPostInterceptor {
                name_str: "first_fail",
                error_msg: "first error",
            }),
            Arc::new(NamedFailingPostInterceptor {
                name_str: "second_fail",
                error_msg: "second error",
            }),
        ];
        let result = run_post_execute(&interceptors, &make_req(), &make_resp()).await;
        let error = result.expect("should have an error");
        assert!(
            error.contains("first_fail"),
            "should name the first interceptor"
        );
        assert!(
            error.contains("first error"),
            "should contain the first error message"
        );
        assert!(
            !error.contains("second_fail"),
            "second interceptor must not run"
        );
        assert!(
            !error.contains("second error"),
            "second interceptor must not run"
        );
    }

    #[tokio::test]
    async fn run_post_execute_empty_interceptors_returns_none() {
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![];
        let result = run_post_execute(&interceptors, &make_req(), &make_resp()).await;
        assert!(result.is_none());
    }

    // ── run_on_error ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_on_error_calls_all_interceptors() {
        let count = Arc::new(AtomicU32::new(0));
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![
            Arc::new(CountingErrorInterceptor {
                count: count.clone(),
            }),
            Arc::new(CountingErrorInterceptor {
                count: count.clone(),
            }),
        ];
        run_on_error(&interceptors, &make_req(), "some error").await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "both interceptors should have been called"
        );
    }

    #[tokio::test]
    async fn run_on_error_empty_interceptors_is_noop() {
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![];
        // Should not panic.
        run_on_error(&interceptors, &make_req(), "error").await;
    }

    // ── run_post_tool_use ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_post_tool_use_returns_none_when_no_violations() {
        let interceptors = vec![wrap(PassInterceptor)];
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![],
        };
        let result = run_post_tool_use(&interceptors, &event, std::path::Path::new("/tmp")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_post_tool_use_returns_violation_feedback() {
        let interceptors = vec![wrap(ViolatingToolInterceptor)];
        let event = ToolUseEvent {
            tool_name: "write_file".to_string(),
            affected_files: vec![std::path::PathBuf::from("foo.rs")],
        };
        let result = run_post_tool_use(&interceptors, &event, std::path::Path::new("/tmp")).await;
        assert!(result.is_some());
        let feedback = result.unwrap();
        assert!(
            feedback.contains("violating_tool"),
            "feedback should name the interceptor"
        );
        assert!(feedback.contains("found a violation"));
    }

    #[tokio::test]
    async fn run_post_tool_use_empty_interceptors_returns_none() {
        let interceptors: Vec<Arc<dyn TurnInterceptor>> = vec![];
        let event = ToolUseEvent {
            tool_name: "read_file".to_string(),
            affected_files: vec![],
        };
        let result = run_post_tool_use(&interceptors, &event, std::path::Path::new("/tmp")).await;
        assert!(result.is_none());
    }

    // ── inject_skills_into_prompt ─────────────────────────────────────────────

    fn make_skill_store_with_review() -> harness_skills::SkillStore {
        let mut store = harness_skills::SkillStore::new();
        store.create(
            "review".to_string(),
            "# Review\n<!-- trigger-patterns: code review -->\nReview code carefully.".to_string(),
        );
        store
    }

    #[tokio::test]
    async fn inject_skills_includes_all_skills_listing() {
        let skills = RwLock::new(make_skill_store_with_review());
        let result = inject_skills_into_prompt(&skills, "unrelated prompt").await;
        assert!(
            result.contains("## Available Skills"),
            "listing must appear even when no skill matches"
        );
        assert!(
            result.contains("review"),
            "listing must include skill names"
        );
    }

    #[tokio::test]
    async fn inject_skills_adds_relevant_section_when_prompt_matches() {
        let skills = RwLock::new(make_skill_store_with_review());
        let result = inject_skills_into_prompt(&skills, "please do a code review").await;
        assert!(
            result.contains("## Relevant Skills"),
            "matched skills section must appear"
        );
        assert!(
            result.contains("### review"),
            "matched skill name must appear in section header"
        );
        assert!(
            result.contains("Review code carefully."),
            "matched skill content must appear"
        );
    }

    #[tokio::test]
    async fn inject_skills_omits_relevant_section_when_no_match() {
        let skills = RwLock::new(make_skill_store_with_review());
        let result = inject_skills_into_prompt(&skills, "implement feature X").await;
        assert!(
            !result.contains("## Relevant Skills"),
            "relevant section must not appear when no skill matches"
        );
        assert!(
            result.contains("## Available Skills"),
            "available skills listing must still appear"
        );
    }

    #[tokio::test]
    async fn inject_skills_records_usage_for_matched_skill() {
        let skills = RwLock::new(make_skill_store_with_review());
        inject_skills_into_prompt(&skills, "please do a code review").await;
        let guard = skills.read().await;
        assert_eq!(
            guard.list()[0].usage_count,
            1,
            "usage must be recorded for matched skill"
        );
    }

    #[tokio::test]
    async fn inject_skills_does_not_record_usage_for_unmatched_skill() {
        let skills = RwLock::new(make_skill_store_with_review());
        inject_skills_into_prompt(&skills, "implement feature X").await;
        let guard = skills.read().await;
        assert_eq!(
            guard.list()[0].usage_count,
            0,
            "usage must not be recorded when skill did not match"
        );
    }

    #[tokio::test]
    async fn inject_skills_empty_store_returns_empty_string() {
        let skills = RwLock::new(harness_skills::SkillStore::new());
        let result = inject_skills_into_prompt(&skills, "any prompt").await;
        assert!(
            result.is_empty(),
            "empty skill store must produce empty string"
        );
    }

    // ── truncate_validation_error ─────────────────────────────────────────────

    #[test]
    fn truncate_short_error_passes_through() {
        assert_eq!(truncate_validation_error("short", 100), "short");
    }

    #[test]
    fn truncate_long_error_includes_summary() {
        let input = "x".repeat(200);
        let result = truncate_validation_error(&input, 50);
        assert!(result.starts_with(&"x".repeat(50)));
        assert!(result.contains("(output truncated, 200 chars total)"));
    }
}
