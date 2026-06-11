use super::review_loop::run_review_loop;
use crate::task_runner::{CreateTaskRequest, TaskId, TaskState, TaskStatus, TaskStore};
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
