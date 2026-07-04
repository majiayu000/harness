use super::run_agent_streaming;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::types::{Capability, TokenUsage, TurnFailureKind};

struct SilentAgent;
struct DelayedChannelCloseAgent;

#[async_trait]
impl CodeAgent for SilentAgent {
    fn name(&self) -> &str {
        "silent-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: String::new(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        let _tx = tx;
        std::future::pending().await
    }
}

#[async_trait]
impl CodeAgent for DelayedChannelCloseAgent {
    fn name(&self) -> &str {
        "delayed-channel-close-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: String::new(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        let delayed_tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            if delayed_tx.send(StreamItem::Done).await.is_err() {
                tracing::debug!("stream receiver closed before delayed Done");
            }
        });
        Ok(())
    }
}

fn make_req() -> AgentRequest {
    AgentRequest {
        prompt: "test prompt".to_string(),
        project_root: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    }
}

#[tokio::test]
async fn run_agent_streaming_fails_silent_stream_with_stall_reason() -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = crate::task_runner::TaskId::new();
    let mut req = crate::task_runner::CreateTaskRequest::default();
    req.stall_timeout_secs = 1;
    req.turn_timeout_secs = 2;
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.request_settings = Some(crate::task_runner::PersistedRequestSettings::from_req(&req));
    store.insert(&task).await;

    let failure = run_agent_streaming(
        &SilentAgent,
        make_req(),
        &task_id,
        &store,
        1,
        chrono::Utc::now(),
        chrono::Utc::now(),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("silent stream should fail"))?;

    assert_eq!(failure.failure.kind, TurnFailureKind::Timeout);
    assert_eq!(
        failure.failure.message.as_deref(),
        Some("Agent stream stalled: no output for 1s")
    );
    Ok(())
}

#[tokio::test]
async fn run_agent_streaming_preserves_completed_result_while_channel_closes() -> anyhow::Result<()>
{
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = crate::task_runner::TaskId::new();
    let mut req = crate::task_runner::CreateTaskRequest::default();
    req.stall_timeout_secs = 1;
    req.turn_timeout_secs = 2;
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.request_settings = Some(crate::task_runner::PersistedRequestSettings::from_req(&req));
    store.insert(&task).await;

    let success = run_agent_streaming(
        &DelayedChannelCloseAgent,
        make_req(),
        &task_id,
        &store,
        1,
        chrono::Utc::now(),
        chrono::Utc::now(),
    )
    .await
    .map_err(|failure| anyhow::anyhow!("completed agent result failed: {}", failure.error))?;

    assert_eq!(success.response.exit_code, Some(0));
    Ok(())
}
