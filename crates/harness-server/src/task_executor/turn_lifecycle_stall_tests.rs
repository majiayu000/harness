use super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{AgentId, Capability, Item, TokenUsage, TurnStatus};
use std::sync::Arc;

struct SilentLifecycleAgent;

#[async_trait]
impl CodeAgent for SilentLifecycleAgent {
    fn name(&self) -> &str {
        "codex"
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
        std::future::pending::<Result<(), HarnessError>>().await
    }
}

#[tokio::test]
async fn lifecycle_fails_silent_stream_with_stall_reason() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    config.agents.default_agent = "codex".to_string();

    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(SilentLifecycleAgent));
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let thread_id = server
        .thread_manager
        .start_thread(root.path().to_path_buf());
    let turn_id = server.thread_manager.start_turn(
        &thread_id,
        "prompt".to_string(),
        AgentId::from_str("codex"),
    )?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(16);

    run_turn_lifecycle_with_options(
        server.clone(),
        None,
        None,
        notification_tx,
        thread_id.clone(),
        turn_id.clone(),
        "prompt".to_string(),
        "codex".to_string(),
        TurnLifecycleOptions {
            timeout_secs: Some(2),
            stall_timeout_secs: Some(1),
            force_code_agent: true,
            ..TurnLifecycleOptions::default()
        },
    )
    .await;

    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Failed);
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Error { message, .. } if message.contains("Agent stream stalled: no output for 1s")
    )));
    Ok(())
}
