use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse};
use harness_core::interceptor::{
    InterceptResult, PostExecuteResult, PostToolUseResult, ToolUseEvent, TurnInterceptor,
};
use harness_core::types::{Decision, TokenUsage};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use tokio::sync::RwLock;

// ── Mock helpers ──────────────────────────────────────────────────────────────

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

// ── Mock interceptors ─────────────────────────────────────────────────────────

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
    async fn post_execute(&self, _req: &AgentRequest, _resp: &AgentResponse) -> PostExecuteResult {
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
    async fn post_execute(&self, _req: &AgentRequest, _resp: &AgentResponse) -> PostExecuteResult {
        PostExecuteResult::fail(self.error_msg)
    }
}

fn wrap<T: TurnInterceptor + 'static>(t: T) -> Arc<dyn TurnInterceptor> {
    Arc::new(t)
}

fn task_with_prompt_settings(
    description: &str,
    settings: crate::task_runner::PersistedRequestSettings,
) -> crate::task_runner::TaskState {
    crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        status: crate::task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: Some(description.to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
    }
}

// ── run_pre_execute ───────────────────────────────────────────────────────────

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

#[test]
fn legacy_manual_prompt_only_tasks_still_skip_prompt_persistence() {
    let task = task_with_prompt_settings(
        "prompt task",
        crate::task_runner::PersistedRequestSettings::default(),
    );
    assert!(
        should_skip_prompt_persistence(Some(&task)),
        "legacy manual prompt-only tasks must not persist prompt text"
    );
}

#[test]
fn restart_bundle_keeps_system_prompt_tasks_persistable_without_origin_flag() {
    let dir = tempfile::tempdir().unwrap();
    let task = task_with_prompt_settings(
        "prompt task",
        crate::task_runner::PersistedRequestSettings {
            system_prompt_restart_bundle: Some(
                crate::task_runner::SystemPromptRestartBundle::periodic_review(
                    crate::task_runner::PeriodicReviewPromptInputs {
                        project_root: dir.path().display().to_string(),
                        since_arg: "2026-04-20T00:00:00Z".to_string(),
                        review_type: "mixed".to_string(),
                        guard_scan_output: None,
                    },
                ),
            ),
            ..Default::default()
        },
    );
    assert!(
        !should_skip_prompt_persistence(Some(&task)),
        "restart bundles must keep system-generated prompt tasks eligible for prompt persistence"
    );
}

// ── run_post_execute ──────────────────────────────────────────────────────────

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

// ── run_on_error ──────────────────────────────────────────────────────────────

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
    run_on_error(&interceptors, &make_req(), "error").await;
}

// ── run_post_tool_use ─────────────────────────────────────────────────────────

#[tokio::test]
async fn run_post_tool_use_returns_none_when_no_violations() {
    let interceptors = vec![wrap(PassInterceptor)];
    let event = ToolUseEvent {
        tool_name: "write_file".to_string(),
        affected_files: vec![],
        session_id: None,
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
        session_id: None,
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
        session_id: None,
    };
    let result = run_post_tool_use(&interceptors, &event, std::path::Path::new("/tmp")).await;
    assert!(result.is_none());
}

// ── inject_skills_into_prompt ─────────────────────────────────────────────────

fn make_skill_store_with_review() -> harness_skills::store::SkillStore {
    let mut store = harness_skills::store::SkillStore::new();
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
async fn matched_skills_returns_id_and_name_for_matches() {
    let skills = RwLock::new(make_skill_store_with_review());
    let matched = matched_skills_for_prompt(&skills, "please do a code review").await;
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].1, "review");
    assert!(
        !matched[0].0.as_str().is_empty(),
        "matched skill id should not be empty"
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
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    let result = inject_skills_into_prompt(&skills, "any prompt").await;
    assert!(
        result.is_empty(),
        "empty skill store must produce empty string"
    );
}

// ── truncate_validation_error ─────────────────────────────────────────────────

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

#[test]
fn parse_porcelain_z_paths_uses_rename_destination() {
    let raw = b"R  old.txt\0new.txt\0";
    let paths = parse_porcelain_z_paths(raw);
    assert_eq!(paths, vec![std::path::PathBuf::from("new.txt")]);
}

#[test]
fn parse_porcelain_z_paths_ignores_deletions() {
    let raw = b"D  gone.txt\0";
    let paths = parse_porcelain_z_paths(raw);
    assert!(paths.is_empty());
}

#[test]
fn parse_porcelain_z_paths_keeps_modified_and_untracked() {
    let raw = b" M src/lib.rs\0?? new_file.rs\0";
    let paths = parse_porcelain_z_paths(raw);
    assert_eq!(
        paths,
        vec![
            std::path::PathBuf::from("src/lib.rs"),
            std::path::PathBuf::from("new_file.rs")
        ]
    );
}

// ── process_stream_item: ApprovalRequest ─────────────────────────────────────

#[tokio::test]
async fn process_stream_item_approval_request_appends_item_and_emits_notification() {
    use crate::server::HarnessServer;
    use crate::thread_manager::ThreadManager;
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::StreamItem;
    use harness_core::config::HarnessConfig;
    use harness_core::types::AgentId;
    use harness_protocol::notifications::RpcNotification;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new(""),
    ));
    let thread_id = server.thread_manager.start_thread(PathBuf::from("/tmp"));
    let turn_id = server
        .thread_manager
        .start_turn(&thread_id, "task".to_string(), AgentId::new())
        .unwrap();

    let (notification_tx, mut notification_rx) = broadcast::channel::<RpcNotification>(16);

    process_stream_item(
        &server,
        &None,
        &None,
        &notification_tx,
        &thread_id,
        &turn_id,
        StreamItem::ApprovalRequest {
            id: "req-42".to_string(),
            command: "rm -rf /tmp/test".to_string(),
        },
    )
    .await;

    // Check item was appended.
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .expect("turn must exist");
    let has_approval = turn.items.iter().any(|item| {
        matches!(
            item,
            harness_core::types::Item::ApprovalRequest {
                action,
                approved: None,
                ..
            } if action == "rm -rf /tmp/test"
        )
    });
    assert!(
        has_approval,
        "ApprovalRequest item must be appended to turn"
    );

    // Check notification was emitted.
    let notif = notification_rx
        .try_recv()
        .expect("notification must be emitted");
    match notif.notification {
        harness_protocol::notifications::Notification::ApprovalRequest {
            turn_id: notif_turn_id,
            request_id,
            command,
        } => {
            assert_eq!(notif_turn_id, turn_id);
            assert_eq!(request_id, "req-42");
            assert_eq!(command, "rm -rf /tmp/test");
        }
        other => panic!("expected ApprovalRequest notification, got {other:?}"),
    }
}
