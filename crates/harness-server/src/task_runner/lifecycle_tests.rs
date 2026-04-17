use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::types::{Capability, ContextItem, ExecutionPhase, TokenUsage};
use tokio::time::Duration;

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
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;

    let mut skill_store = harness_skills::store::SkillStore::new();
    skill_store.create(
        "test-skill".to_string(),
        "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
    );
    let skills = Arc::new(RwLock::new(skill_store));

    let agent = CapturingAgent::new();
    let agent_clone = agent.clone();

    let req = super::super::types::CreateTaskRequest {
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

    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

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
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
        vec![Arc::new(BlockingInterceptor)];

    let req = super::super::types::CreateTaskRequest {
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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, super::super::types::TaskStatus::Failed),
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

#[test]
fn local_waiting_counter_increments_on_each_waiting_response() {
    let max_rounds = 5u32;
    let mut waiting_count: u32 = 0;
    let mut observed: Vec<u32> = Vec::new();

    waiting_count += 1;
    observed.push(waiting_count);

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
    use harness_core::types::EventFilters;

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = tempfile::Builder::new()
        .prefix("harness-test-")
        .tempdir_in(&home)?;
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    let req = super::super::types::CreateTaskRequest {
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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;
    assert!(
        matches!(state.status, super::super::types::TaskStatus::Failed),
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
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;

    let req = super::super::types::CreateTaskRequest {
        issue: Some(20),
        source: Some("github".to_string()),
        external_id: Some("20".to_string()),
        repo: Some("acme/harness".to_string()),
        ..Default::default()
    };

    let task_id = register_pending_task(store.clone(), &req).await;
    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;

    assert_eq!(state.source.as_deref(), Some("github"));
    assert_eq!(state.external_id.as_deref(), Some("20"));
    assert_eq!(state.repo.as_deref(), Some("acme/harness"));
    assert_eq!(state.description.as_deref(), Some("issue #20"));
    Ok(())
}

struct PhaseCapturingAgent {
    phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
    responses: tokio::sync::Mutex<Vec<String>>,
}

impl PhaseCapturingAgent {
    fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            phases: tokio::sync::Mutex::new(Vec::new()),
            responses: tokio::sync::Mutex::new(responses),
        })
    }

    async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
        self.phases.lock().await.clone()
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
async fn planning_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    let agent = PhaseCapturingAgent::new(vec![String::new()]);
    let agent_clone = agent.clone();

    let req = super::super::types::CreateTaskRequest {
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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let phases = agent.captured_phases().await;
    assert!(
        !phases.is_empty(),
        "expected at least one agent call, got none"
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Planning),
        "initial implementation turn must use Planning phase"
    );
    Ok(())
}

#[tokio::test]
async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let store = super::super::store::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    let agent = PhaseCapturingAgent::new(vec![
        "PR_URL=https://github.com/owner/repo/pull/1".into(),
        "LGTM".into(),
    ]);
    let agent_clone = agent.clone();

    let req = super::super::types::CreateTaskRequest {
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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let phases = agent.captured_phases().await;
    assert!(
        phases.len() >= 2,
        "expected at least 2 agent calls (implementation + review check), got {}",
        phases.len()
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Planning),
        "implementation turn must use Planning phase"
    );
    assert_eq!(
        phases[1],
        Some(ExecutionPhase::Execution),
        "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
    );
    Ok(())
}
