use super::review_loop::run_review_loop;
use crate::event_replay::TaskEvent;
use crate::task_runner::{
    CreateTaskRequest, TaskId, TaskPhase, TaskState, TaskStatus, TaskStore, TaskTerminalFailure,
};
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::types::{Capability, TokenUsage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

struct StaticReviewAgent {
    output: &'static str,
    requests: Mutex<u32>,
}

impl StaticReviewAgent {
    fn new(output: &'static str) -> Self {
        Self {
            output,
            requests: Mutex::new(0),
        }
    }

    async fn request_count(&self) -> u32 {
        *self.requests.lock().await
    }
}

#[async_trait::async_trait]
impl CodeAgent for StaticReviewAgent {
    fn name(&self) -> &str {
        "static-review-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        *self.requests.lock().await += 1;
        Ok(AgentResponse {
            output: self.output.to_string(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: "test".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        *self.requests.lock().await += 1;
        let _ = tx
            .send(StreamItem::MessageDelta {
                text: self.output.to_string(),
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

#[tokio::test]
async fn review_loop_signal_fetch_failure_fails_before_prompt_when_wait_budget_exceeded(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent = StaticReviewAgent::new("LGTM");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now() - Duration::from_secs(2),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("review wait budget exceeded"),
        "unexpected error: {:?}",
        final_state.error
    );
    assert_eq!(agent.request_count().await, 0);
    Ok(())
}

#[tokio::test]
async fn review_loop_waiting_output_fails_when_wait_budget_exceeded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent = StaticReviewAgent::new("Review bot has not replied yet.\nWAITING");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now() - Duration::from_secs(2),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("review wait budget exceeded"),
        "unexpected error: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn review_loop_round_budget_exhausted_marks_terminal_once() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent =
        StaticReviewAgent::new("Found 4 issues.\n1. Missing test\n2. Race\n3. Leak\n4. Panic");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 60,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now(),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert_eq!(final_state.phase, TaskPhase::Terminal);
    assert_eq!(final_state.turn, 2);
    let failure: TaskTerminalFailure = serde_json::from_str(
        final_state
            .error
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal failure should include a reason"))?,
    )?;
    assert_eq!(failure.reason, "round_budget_exhausted");
    assert_eq!(failure.rounds_used, 1);
    assert_eq!(failure.last_status, TaskStatus::Reviewing);
    assert_eq!(failure.waiting_on.as_deref(), Some("local_review_gate"));
    assert_eq!(agent.request_count().await, 1);

    let event_log = std::fs::read_to_string(dir.path().join("task-events.jsonl"))?;
    let failed_events = event_log
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskEvent>(line).ok())
        .filter(|event| {
            matches!(
                event,
                TaskEvent::Failed { reason, .. }
                    if reason.contains(r#""reason":"round_budget_exhausted""#)
            )
        })
        .count();
    assert_eq!(failed_events, 1);
    Ok(())
}
