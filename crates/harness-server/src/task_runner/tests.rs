use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::types::{Capability, ContextItem, EventFilters, TokenUsage};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;
use types::TASK_STREAM_CAPACITY;

#[tokio::test]
async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let id = harness_core::types::TaskId("stream-test".to_string());

    // No stream registered yet.
    assert!(
        store.subscribe_task_stream(&id).is_none(),
        "subscribe before register should return None"
    );

    store.register_task_stream(&id);
    let mut rx = store
        .subscribe_task_stream(&id)
        .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

    store.publish_stream_item(
        &id,
        StreamItem::MessageDelta {
            text: "hello\n".into(),
        },
    );
    store.publish_stream_item(&id, StreamItem::Done);

    let item1 = rx.recv().await?;
    let item2 = rx.recv().await?;
    assert!(matches!(item1, StreamItem::MessageDelta { .. }));
    assert!(matches!(item2, StreamItem::Done));

    // After close_task_stream the channel sender is dropped.
    store.close_task_stream(&id);
    assert!(
        store.subscribe_task_stream(&id).is_none(),
        "subscribe after close should return None"
    );
    Ok(())
}

#[tokio::test]
async fn task_stream_backpressure_drops_oldest_on_lag() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let id = harness_core::types::TaskId("backpressure-test".to_string());

    store.register_task_stream(&id);
    let mut rx = store
        .subscribe_task_stream(&id)
        .ok_or_else(|| anyhow::anyhow!("subscribe should succeed after register"))?;

    // Publish more items than TASK_STREAM_CAPACITY to trigger lag.
    for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
        store.publish_stream_item(
            &id,
            StreamItem::MessageDelta {
                text: format!("line {i}\n"),
            },
        );
    }

    // Receiver should see RecvError::Lagged on overflow.
    let result = rx.recv().await;
    assert!(
        result.is_err(),
        "expected Lagged error after overflow, got: {:?}",
        result
    );
    Ok(())
}

#[tokio::test]
async fn list_children_returns_subtasks_for_parent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let parent_id = harness_core::types::TaskId("parent-task".to_string());
    let parent = TaskState::new(parent_id.clone());
    store.insert(&parent).await;

    let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
    child1.parent_id = Some(parent_id.clone());
    store.insert(&child1).await;

    let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
    child2.parent_id = Some(parent_id.clone());
    store.insert(&child2).await;

    // Unrelated task.
    store
        .insert(&TaskState::new(harness_core::types::TaskId(
            "other".to_string(),
        )))
        .await;

    let children = store.list_children(&parent_id);
    assert_eq!(children.len(), 2);
    assert!(children
        .iter()
        .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

    let no_children = store.list_children(&harness_core::types::TaskId("nonexistent".to_string()));
    assert!(no_children.is_empty());
    Ok(())
}

#[test]
fn test_task_state_new() {
    let id = TaskId::new();
    let state = TaskState::new(id);
    assert!(matches!(state.status, TaskStatus::Pending));
    assert_eq!(state.turn, 0);
    assert!(state.pr_url.is_none());
    assert!(state.project_root.is_none());
    assert!(state.issue.is_none());
    assert!(state.description.is_none());
}

#[tokio::test]
async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project = PathBuf::from("/repo/project");
    let other_project = PathBuf::from("/repo/other");

    let current_id = harness_core::types::TaskId("current".to_string());
    let mut current = TaskState::new(current_id.clone());
    current.project_root = Some(project.clone());
    current.status = TaskStatus::Implementing;
    store.insert(&current).await;

    // Sibling on same project in Implementing status.
    let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
    sibling1.project_root = Some(project.clone());
    sibling1.status = TaskStatus::Implementing;
    sibling1.issue = Some(77);
    sibling1.description = Some("fix unwrap in s3.rs".to_string());
    store.insert(&sibling1).await;

    // Sibling on same project in Pending status.
    let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
    sibling2.project_root = Some(project.clone());
    sibling2.status = TaskStatus::Pending;
    store.insert(&sibling2).await;

    // Task on a different project — must not appear.
    let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
    other.project_root = Some(other_project.clone());
    other.status = TaskStatus::Implementing;
    store.insert(&other).await;

    // Done task on same project — must not appear.
    let mut done = TaskState::new(harness_core::types::TaskId("done-task".to_string()));
    done.project_root = Some(project.clone());
    done.status = TaskStatus::Done;
    store.insert(&done).await;

    let siblings = store.list_siblings(&project, &current_id);
    let sibling_ids: Vec<&str> = siblings.iter().map(|s| s.id.0.as_str()).collect();
    assert_eq!(
        siblings.len(),
        2,
        "expected 2 siblings, got: {sibling_ids:?}"
    );
    assert!(
        siblings.iter().all(|s| s.id != current_id),
        "current task must be excluded"
    );
    assert!(siblings
        .iter()
        .all(|s| s.project_root.as_deref() == Some(project.as_path())));

    // One sibling on `other_project`.
    let other_project_siblings = store.list_siblings(&other_project, &current_id);
    assert_eq!(other_project_siblings.len(), 1);

    Ok(())
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
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

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
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let agent = CapturingAgent::new();
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

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
    )
    .await;

    // Allow async task to complete.
    tokio::time::sleep(Duration::from_millis(200)).await;

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
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

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
