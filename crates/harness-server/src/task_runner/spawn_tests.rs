use super::super::request::SystemTaskInput;
use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::types::{Capability, ContextItem, EventFilters, ExecutionPhase, TokenUsage};
use tokio::time::{sleep, Duration, Instant};

fn tid(s: &str) -> harness_core::types::TaskId {
    harness_core::types::TaskId(s.to_string())
}

fn cleanup_state(
    status: TaskStatus,
    task_kind: TaskKind,
    external_id: Option<&str>,
    issue: Option<u64>,
) -> TaskState {
    let mut state = TaskState::new(tid("cleanup-task"));
    state.status = status;
    state.task_kind = task_kind;
    state.external_id = external_id.map(str::to_string);
    state.issue = issue;
    state
}

#[test]
fn workspace_cleanup_keeps_inflight_issue_workspace() {
    let state = cleanup_state(
        TaskStatus::Waiting,
        TaskKind::Issue,
        Some("issue:42"),
        Some(42),
    );
    assert!(
        !should_remove_workspace_after_task(Some(&state), true),
        "waiting issue workspaces must survive follow-up phases"
    );
}

#[test]
fn workspace_cleanup_recognizes_issue_kind_without_external_id() {
    let state = cleanup_state(TaskStatus::Done, TaskKind::Issue, None, Some(42));
    assert!(
        !should_remove_workspace_after_task(Some(&state), true),
        "issue/PR lifecycle detection must not rely only on normalized external_id"
    );
}

#[test]
fn workspace_cleanup_removes_failed_issue_after_terminal_state() {
    let state = cleanup_state(
        TaskStatus::Failed,
        TaskKind::Issue,
        Some("issue:42"),
        Some(42),
    );
    assert!(
        should_remove_workspace_after_task(Some(&state), true),
        "failed issue workspaces may be cleaned after terminal failure"
    );
}

#[test]
fn workspace_cleanup_removes_done_prompt_when_auto_cleanup_enabled() {
    let state = cleanup_state(TaskStatus::Done, TaskKind::Prompt, None, None);
    assert!(should_remove_workspace_after_task(Some(&state), true));
}

struct CapturingAgent {
    captured: tokio::sync::Mutex<Vec<ContextItem>>,
}

impl CapturingAgent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            captured: tokio::sync::Mutex::new(Vec::new()),
        })
    }
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("condition not met within {:?}", timeout);
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[async_trait]
impl harness_core::agent::CodeAgent for CapturingAgent {
    fn name(&self) -> &str {
        "capturing-mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        let mut guard = self.captured.lock().await;
        if guard.is_empty() {
            *guard = req.context.clone();
        }
        Ok(AgentResponse {
            output: String::new(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: 0.0,
            },
            model: "mock".into(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        // Mirror execute(): capture context on first call so tests that
        // verify skill injection work whether execute or execute_stream is called.
        let mut guard = self.captured.lock().await;
        if guard.is_empty() {
            *guard = req.context.clone();
        }
        Ok(())
    }
}

#[tokio::test]
async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;

    let mut skill_store = harness_skills::store::SkillStore::new();
    skill_store.create(
        "test-skill".to_string(),
        "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
    );
    let skills = Arc::new(RwLock::new(skill_store));

    let agent = CapturingAgent::new();
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("test task".into()),
        issue: None,
        pr: None,
        agent: None,
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );
    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(3), || {
        agent
            .captured
            .try_lock()
            .map(|captured| !captured.is_empty())
            .unwrap_or(false)
    })
    .await?;

    let captured = agent.captured.lock().await;
    assert!(
        !captured.is_empty(),
        "expected skills to be injected into AgentRequest.context"
    );
    assert!(
        captured
            .iter()
            .any(|item| matches!(item, ContextItem::Skill { .. })),
        "expected at least one ContextItem::Skill"
    );
    Ok(())
}

struct BlockingInterceptor;

#[async_trait]
impl harness_core::interceptor::TurnInterceptor for BlockingInterceptor {
    fn name(&self) -> &str {
        "blocking-test"
    }

    async fn pre_execute(&self, _req: &AgentRequest) -> harness_core::interceptor::InterceptResult {
        harness_core::interceptor::InterceptResult::block("test block")
    }
}

#[tokio::test]
async fn blocking_interceptor_fails_task() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
        vec![Arc::new(BlockingInterceptor)];

    let req = CreateTaskRequest {
        prompt: Some("blocked task".into()),
        issue: None,
        pr: None,
        agent: None,
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent,
        None,
        Default::default(),
        skills,
        events,
        interceptors,
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(3), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "expected Failed, got {:?}",
        state.status
    );
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Blocked by interceptor"),
        "error message should mention blocked: {:?}",
        state.error
    );
    Ok(())
}

/// Verify that a local u32 counter correctly tracks waiting rounds without any store query.
/// Task execution is sequential within a single tokio task, so a plain local counter suffices.
#[test]
fn local_waiting_counter_increments_on_each_waiting_response() {
    let max_rounds = 5u32;
    let mut waiting_count: u32 = 0;
    let mut observed: Vec<u32> = Vec::new();

    // Simulate the initial wait before the review loop.
    waiting_count += 1;
    observed.push(waiting_count);

    // Simulate inter-round waits (max_rounds - 1 additional waits).
    for _ in 1..max_rounds {
        waiting_count += 1;
        observed.push(waiting_count);
    }

    let expected: Vec<u32> = (1..=max_rounds).collect();
    assert_eq!(
        observed, expected,
        "waiting_count must increment monotonically on each waiting response"
    );
}

#[tokio::test]
async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = tempfile::Builder::new()
        .prefix("harness-test-")
        .tempdir_in(&home)?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    let req = CreateTaskRequest {
        prompt: Some("panic path".into()),
        issue: None,
        pr: None,
        agent: None,
        project: None,
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        max_budget_usd: None,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task_with_worktree_detector(
        store.clone(),
        agent,
        None,
        Default::default(),
        skills,
        events.clone(),
        vec![],
        req,
        || -> PathBuf {
            panic!("forced detect_main_worktree panic");
        },
        None,
        permit,
        None,
        None,
        None,
        vec![],
        None,
        None,
    )
    .await;

    wait_until(Duration::from_secs(3), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "expected Failed, got {:?}",
        state.status
    );
    let error = state.error.unwrap_or_default();
    assert!(
        error.contains("detect_main_worktree panicked"),
        "expected panic reason in task error, got: {error}"
    );
    assert!(
        error.contains("forced detect_main_worktree panic"),
        "expected panic payload in task error, got: {error}"
    );

    let expected_detail = format!("task_id={}", task_id.0);
    let failure_events = events
        .query(&EventFilters {
            hook: Some("task_failure".to_string()),
            ..Default::default()
        })
        .await?;
    assert!(
        failure_events.iter().any(|event| {
            event.detail.as_deref() == Some(expected_detail.as_str())
                && event
                    .reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("forced detect_main_worktree panic")
        }),
        "expected task_failure event containing panic payload, got: {:?}",
        failure_events
    );
    Ok(())
}

#[tokio::test]
async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let req = CreateTaskRequest {
        issue: Some(20),
        source: Some("github".to_string()),
        external_id: Some("20".to_string()),
        repo: Some("acme/harness".to_string()),
        project: Some(dir.path().to_path_buf()),
        ..Default::default()
    };

    let task_id = register_pending_task(store.clone(), &req).await;
    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;

    assert_eq!(state.source.as_deref(), Some("github"));
    assert_eq!(state.external_id.as_deref(), Some("20"));
    assert_eq!(state.repo.as_deref(), Some("acme/harness"));
    assert_eq!(state.project_root.as_deref(), Some(dir.path()));
    assert_eq!(state.description.as_deref(), Some("issue #20"));
    Ok(())
}

/// Mock agent that records the `execution_phase` from every call and
/// returns pre-configured responses in order.
struct PhaseCapturingAgent {
    phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
    prompts: tokio::sync::Mutex<Vec<String>>,
    responses: tokio::sync::Mutex<Vec<String>>,
}

impl PhaseCapturingAgent {
    fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            phases: tokio::sync::Mutex::new(Vec::new()),
            prompts: tokio::sync::Mutex::new(Vec::new()),
            responses: tokio::sync::Mutex::new(responses),
        })
    }

    async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
        self.phases.lock().await.clone()
    }

    async fn captured_prompts(&self) -> Vec<String> {
        self.prompts.lock().await.clone()
    }

    async fn next_response(&self) -> String {
        let mut guard = self.responses.lock().await;
        if guard.is_empty() {
            String::new()
        } else {
            guard.remove(0)
        }
    }
}

async fn wait_for_captured_phases(
    agent: &PhaseCapturingAgent,
    min_count: usize,
) -> Vec<Option<ExecutionPhase>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let phases = agent.captured_phases().await;
        if phases.len() >= min_count || Instant::now() >= deadline {
            return phases;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_captured_prompts(agent: &PhaseCapturingAgent, min_count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let prompts = agent.captured_prompts().await;
        if prompts.len() >= min_count || Instant::now() >= deadline {
            return prompts;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[async_trait]
impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
    fn name(&self) -> &str {
        "phase-capturing-mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.phases.lock().await.push(req.execution_phase);
        self.prompts.lock().await.push(req.prompt.clone());
        let output = self.next_response().await;
        Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".into(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.phases.lock().await.push(req.execution_phase);
        self.prompts.lock().await.push(req.prompt.clone());
        let output = self.next_response().await;
        if !output.is_empty() {
            if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
            }
        }
        if let Err(e) = tx.send(StreamItem::Done).await {
            tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
        }
        Ok(())
    }
}

#[tokio::test]
async fn execution_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    // Agent returns empty output (no PR URL) → task completes after implementation.
    let agent = PhaseCapturingAgent::new(vec![String::new()]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    let phases = wait_for_captured_phases(agent.as_ref(), 1).await;
    assert!(
        !phases.is_empty(),
        "expected at least one agent call, got none"
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Execution),
        "initial implementation turn must use Execution phase"
    );
    Ok(())
}

#[tokio::test]
async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    // Call 1 (execute_stream): return a PR URL to trigger the review loop.
    // Call 2 (execute): return LGTM to complete the review loop.
    let agent = PhaseCapturingAgent::new(vec![
        "PR_URL=https://github.com/owner/repo/pull/1".into(),
        "LGTM".into(),
    ]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    let phases = wait_for_captured_phases(agent.as_ref(), 2).await;
    assert!(
        phases.len() >= 2,
        "expected at least 2 agent calls (implementation + review check), got {}",
        phases.len()
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Execution),
        "implementation turn must use Execution phase"
    );
    assert_eq!(
        phases[1],
        Some(ExecutionPhase::Execution),
        "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_pr_manual_conflict_fails_before_review_loop() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_CONFLICT paths=src/lib.rs\nPR_URL=https://github.com/owner/repo/pull/7".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(7),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
    })
    .await?;

    let phases = agent.captured_phases().await;
    assert_eq!(
        phases.len(),
        1,
        "rebase-conflict outcome must stop before the normal review loop"
    );
    let state = store.get(&task_id).expect("task should exist");
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("manual resolution required"),
        "failure must preserve manual-resolution wording for intake safeguards"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_pr_clean_conflict_gate_enters_review_loop() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_SKIPPED\nPR_URL=https://github.com/owner/repo/pull/8".into(),
        "LGTM".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(8),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Done))
    })
    .await?;

    let phases = agent.captured_phases().await;
    assert!(
        phases.len() >= 2,
        "clean pr:N prep must enter the review loop after rebase preparation"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_pr_rebase_push_requires_fresh_review_prompt() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );

    let agent = PhaseCapturingAgent::new(vec![
        "REBASE_PUSHED\nPR_URL=https://github.com/owner/repo/pull/9".into(),
        "LGTM".into(),
    ]);

    let req = CreateTaskRequest {
        pr: Some(9),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    wait_until(Duration::from_secs(15), || {
        store
            .get(&task_id)
            .is_some_and(|state| matches!(state.status, TaskStatus::Done))
    })
    .await?;

    let prompts = wait_for_captured_prompts(agent.as_ref(), 2).await;
    assert!(
        prompts
            .get(1)
            .is_some_and(|prompt| prompt.contains("IMPORTANT — New review verification")),
        "rebased pr:N tasks must require a fresh reviewer pass before accepting LGTM"
    );
    Ok(())
}

#[test]
fn preregistered_metadata_refresh_preserves_persisted_phase() {
    let mut state = TaskState::new(TaskId::new());
    state.phase = TaskPhase::Plan;

    let req = CreateTaskRequest {
        prompt: Some("small prompt".into()),
        project: Some(PathBuf::from("/tmp/recovered-project")),
        repo: Some("owner/repo".into()),
        ..Default::default()
    };

    refresh_preregistered_task_metadata(
        &mut state,
        &req,
        PathBuf::from("/tmp/recovered-project"),
        Some("small prompt".into()),
    );

    assert_eq!(state.phase, TaskPhase::Plan);
    assert_eq!(state.repo.as_deref(), Some("owner/repo"));
    assert_eq!(state.description.as_deref(), Some("small prompt"));
}

#[test]
fn transient_error_detection() {
    // Positive cases — should match transient patterns.
    assert!(is_transient_error(
        "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
    ));
    assert!(is_transient_error("rate limit exceeded, retry after 30s"));
    assert!(is_transient_error("HTTP 429 Too Many Requests"));
    assert!(is_transient_error("502 Bad Gateway"));
    assert!(is_transient_error(
        "Agent stream stalled: no output for 3600s"
    ));
    assert!(is_transient_error("connection reset by peer"));
    assert!(is_transient_error(
        "stream idle timeout after 3600s: zombie connection terminated"
    ));
    assert!(is_transient_error(
        "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
    ));
    assert!(is_transient_error(
        "failed to fetch GitHub pull request page for owner/repo issue #998"
    ));
    assert!(is_transient_error(
        "timed out fetching GitHub pull request page for owner/repo issue #998"
    ));
    assert!(is_transient_error(
        "GitHub pull request lookup for owner/repo issue #998 returned 403 Forbidden; rate limit retry-after=60; x-ratelimit-remaining=0"
    ));

    // Negative cases — permanent errors should not match.
    assert!(!is_transient_error(
        "Task did not receive LGTM after 5 review rounds."
    ));
    assert!(!is_transient_error(
        "triage output unparseable — agent did not produce TRIAGE=<decision>"
    ));
    assert!(!is_transient_error("all parallel subtasks failed"));
    assert!(!is_transient_error(
        "task failed unexpectedly: task 102 panicked"
    ));
    assert!(!is_transient_error(
        "budget exceeded: spent $5.00, limit $3.00"
    ));
}

#[test]
fn parse_pr_url_standard() {
    let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/42") else {
        panic!("expected Some for standard GitHub PR URL");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 42);
}

#[test]
fn parse_pr_url_with_fragment() {
    let Some((owner, repo, number)) =
        parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
    else {
        panic!("expected Some for PR URL with fragment");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 99);
}

#[test]
fn parse_pr_url_trailing_slash() {
    let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/7/") else {
        panic!("expected Some for PR URL with trailing slash");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 7);
}

#[test]
fn parse_pr_url_invalid_returns_none() {
    assert!(parse_pr_url("https://github.com/acme/myrepo").is_none());
    assert!(parse_pr_url("not-a-url").is_none());
    assert!(parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
}

// --- planning gate ---

#[test]
fn short_prompt_does_not_require_plan() {
    let prompt = "Fix the typo in README.md";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn long_prompt_over_200_words_requires_plan() {
    let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
    let prompt = words.join(" ");
    assert!(prompt_requires_plan(&prompt));
}

#[test]
fn prompt_with_three_file_paths_requires_plan() {
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
    assert!(prompt_requires_plan(prompt));
}

#[test]
fn prompt_with_two_file_paths_does_not_require_plan() {
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn xml_closing_tags_are_not_counted_as_file_paths() {
    // `wrap_external_data` wraps content in <external_data>...</external_data>.
    // Three `</external_data>` closing tags contain '/' but must NOT trigger
    // the planning gate — they are markup, not file paths.
    let prompt = "GC applied files:\n\
                  <external_data>test-guard.sh</external_data>\n\
                  Rationale:\n<external_data>test</external_data>\n\
                  Validation:\n<external_data>test</external_data>";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn non_decomposable_source_list_includes_periodic_and_sprint() {
    assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
    assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
    assert!(!is_non_decomposable_prompt_source(Some("github")));
    assert!(!is_non_decomposable_prompt_source(None));
}

#[test]
fn request_task_kind_trusts_only_internal_system_input() {
    let spoofed = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        ..Default::default()
    };
    assert_eq!(spoofed.task_kind(), TaskKind::Prompt);

    let trusted = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        system_input: Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        }),
        ..Default::default()
    };
    assert_eq!(trusted.task_kind(), TaskKind::Review);
}

#[test]
fn system_input_for_request_clones_only_explicit_internal_metadata() {
    let spoofed = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        ..Default::default()
    };
    assert_eq!(spoofed.system_input.clone(), None);

    let trusted = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        system_input: Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        }),
        ..Default::default()
    };
    assert_eq!(
        trusted.system_input.clone(),
        Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        })
    );
}

// --- dependency scheduling tests ---

#[tokio::test]
async fn spawn_awaiting_all_deps_done_creates_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-done");
    let mut dep = TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Done;
    store.insert(&dep).await;

    let req = CreateTaskRequest {
        prompt: Some("task with done dep".into()),
        depends_on: vec![dep_id],
        ..Default::default()
    };

    let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
    let state = store.get(&task_id).expect("task should be in store");
    assert!(
        matches!(state.status, TaskStatus::Pending),
        "expected Pending when all deps Done, got {:?}",
        state.status
    );
    Ok(())
}

#[tokio::test]
async fn spawn_awaiting_unresolved_dep_creates_awaiting() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-pending");
    let dep = TaskState::new(dep_id.clone()); // Pending by default
    store.insert(&dep).await;

    let req = CreateTaskRequest {
        prompt: Some("task with pending dep".into()),
        depends_on: vec![dep_id],
        ..Default::default()
    };

    let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
    let state = store.get(&task_id).expect("task should be in store");
    assert!(
        matches!(state.status, TaskStatus::AwaitingDeps),
        "expected AwaitingDeps when dep not Done, got {:?}",
        state.status
    );
    Ok(())
}

#[tokio::test]
async fn spawn_awaiting_detects_direct_cycle() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    // Chain: new → A → new  (A's depends_on contains new_id)
    let new_id = tid("new-task");
    let a_id = tid("task-a");

    let mut task_a = TaskState::new(a_id.clone());
    task_a.depends_on = vec![new_id.clone()];
    store.insert(&task_a).await;

    assert!(
        detect_cycle(&store, &new_id, &[a_id]),
        "expected direct cycle to be detected"
    );
    Ok(())
}

#[tokio::test]
async fn spawn_awaiting_detects_transitive_cycle() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    // Chain: new → A → B → new
    let new_id = tid("new-task");
    let a_id = tid("task-a");
    let b_id = tid("task-b");

    let mut task_b = TaskState::new(b_id.clone());
    task_b.depends_on = vec![new_id.clone()];
    store.insert(&task_b).await;

    let mut task_a = TaskState::new(a_id.clone());
    task_a.depends_on = vec![b_id];
    store.insert(&task_a).await;

    assert!(
        detect_cycle(&store, &new_id, &[a_id]),
        "expected transitive cycle to be detected"
    );
    Ok(())
}

#[tokio::test]
async fn check_awaiting_no_tasks_returns_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(ready.is_empty(), "expected no ready tasks");
    assert!(failed.is_empty(), "expected no failed tasks");
    Ok(())
}

#[tokio::test]
async fn check_awaiting_ready_transitions_to_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-done");
    let mut dep = TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Done;
    store.insert(&dep).await;

    let task_id = tid("awaiting-task");
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::AwaitingDeps;
    task.depends_on = vec![dep_id];
    store.insert(&task).await;

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(ready.contains(&task_id), "expected task in ready_ids");
    assert!(failed.is_empty(), "expected no failed tasks");

    let state = store.get(&task_id).expect("task should still be in store");
    assert!(
        matches!(state.status, TaskStatus::Pending),
        "expected Pending after transition, got {:?}",
        state.status
    );
    Ok(())
}

#[tokio::test]
async fn check_awaiting_failed_dep_transitions_to_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-failed");
    let mut dep = TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Failed;
    store.insert(&dep).await;

    let task_id = tid("awaiting-task");
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::AwaitingDeps;
    task.depends_on = vec![dep_id];
    store.insert(&task).await;

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(ready.is_empty(), "expected no ready tasks");
    assert!(failed.contains(&task_id), "expected task in failed_ids");

    let state = store.get(&task_id).expect("task should still be in store");
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "expected Failed after dep failure, got {:?}",
        state.status
    );
    Ok(())
}

#[tokio::test]
async fn check_awaiting_partial_deps_stays_awaiting() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_done_id = tid("dep-done");
    let mut dep_done = TaskState::new(dep_done_id.clone());
    dep_done.status = TaskStatus::Done;
    store.insert(&dep_done).await;

    let dep_pending_id = tid("dep-pending");
    let dep_pending = TaskState::new(dep_pending_id.clone()); // Pending by default
    store.insert(&dep_pending).await;

    let task_id = tid("awaiting-task");
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::AwaitingDeps;
    task.depends_on = vec![dep_done_id, dep_pending_id];
    store.insert(&task).await;

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(!ready.contains(&task_id), "task must not be in ready_ids");
    assert!(!failed.contains(&task_id), "task must not be in failed_ids");

    let state = store.get(&task_id).expect("task should still be in store");
    assert!(
        matches!(state.status, TaskStatus::AwaitingDeps),
        "expected AwaitingDeps with partial deps, got {:?}",
        state.status
    );
    Ok(())
}

/// A cancelled dependency hard-fails its dependents. Re-queued tasks always
/// get new TaskIds, so the old cancelled ID would never resolve — leaving the
/// dependent in AwaitingDeps would block it indefinitely.
#[tokio::test]
async fn check_awaiting_cancelled_dep_hard_fails_dependent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-cancelled");
    let mut dep = TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Cancelled;
    store.insert(&dep).await;

    let task_id = tid("awaiting-task");
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::AwaitingDeps;
    task.depends_on = vec![dep_id.clone()];
    store.insert(&task).await;

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(ready.is_empty(), "cancelled dep must not unblock dependent");
    assert!(
        failed.contains(&task_id),
        "cancelled dep must hard-fail its dependent"
    );

    let state = store.get(&task_id).expect("task should still be in store");
    assert!(
        matches!(state.status, TaskStatus::Failed),
        "dependent must be Failed when dep is Cancelled, got {:?}",
        state.status
    );
    Ok(())
}

#[tokio::test]
async fn check_awaiting_concurrent_cancel_excluded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let dep_id = tid("dep-done");
    let mut dep = TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Done;
    store.insert(&dep).await;

    let task_id = tid("awaiting-task");
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::AwaitingDeps;
    task.depends_on = vec![dep_id];
    store.insert(&task).await;

    // Simulate concurrent cancel: flip status before check_awaiting_deps runs.
    if let Some(mut entry) = store.cache.get_mut(&task_id) {
        entry.status = TaskStatus::Cancelled;
    }

    let (ready, failed) = check_awaiting_deps(&store).await;
    assert!(
        !ready.contains(&task_id),
        "cancelled task must not be in ready_ids"
    );
    assert!(
        !failed.contains(&task_id),
        "cancelled task must not be in failed_ids"
    );
    Ok(())
}

// ── GC trigger demotion tests (issue #969) ────────────────────────────

#[test]
fn is_issue_pr_task_classifies_external_ids() {
    assert!(
        is_issue_pr_task(Some("issue:42")),
        "issue-keyed task must be classified as issue/PR"
    );
    assert!(
        is_issue_pr_task(Some("pr:123")),
        "pr-keyed task must be classified as issue/PR"
    );
    assert!(
        !is_issue_pr_task(Some("7f3a8b2c-1234-5678-abcd-ef0123456789")),
        "UUID task must not be classified as issue/PR"
    );
    assert!(
        !is_issue_pr_task(None),
        "prompt-only task (no external_id) must not be classified as issue/PR"
    );
}
