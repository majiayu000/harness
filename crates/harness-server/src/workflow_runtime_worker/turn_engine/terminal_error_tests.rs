use super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::{AdapterExecutionStrategy, AgentRegistry};
use harness_core::agent::{
    AgentAdapter, AgentEvent, AgentRequest, AgentResponse, CodeAgent, StreamItem, TurnRequest,
};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{AgentId, Capability, Item, TokenUsage, TurnStatus};
use harness_protocol::notifications::Notification;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc;

struct UnusedAgent {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl CodeAgent for UnusedAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "unused".to_string(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: "codex".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct TerminalErrorAdapter {
    calls: Arc<AtomicUsize>,
    message: String,
}

#[async_trait::async_trait]
impl AgentAdapter for TerminalErrorAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        tx.send(AgentEvent::Error {
            message: self.message.clone(),
        })
        .await
        .map_err(|error| HarnessError::AgentExecution(format!("adapter closed: {error}")))?;
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }
}

fn server_with_terminal_error_adapter(
    root: &std::path::Path,
    agent_calls: Arc<AtomicUsize>,
    adapter_calls: Arc<AtomicUsize>,
    adapter_error: String,
) -> anyhow::Result<Arc<HarnessServer>> {
    let mut config = HarnessConfig::default();
    config.server.project_root = root.to_path_buf();
    config.agents.default_agent = "codex".to_string();

    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(UnusedAgent { calls: agent_calls }));
    let adapter_error_for_factory = adapter_error.clone();
    registry
        .register_adapter_factory_with_strategy(
            "codex",
            move || {
                Arc::new(TerminalErrorAdapter {
                    calls: adapter_calls.clone(),
                    message: adapter_error_for_factory.clone(),
                })
            },
            AdapterExecutionStrategy::ExecuteTurns,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    Ok(Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        registry,
    )))
}

#[tokio::test]
async fn adapter_terminal_error_fails_turn_and_notification() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let agent_calls = Arc::new(AtomicUsize::new(0));
    let adapter_calls = Arc::new(AtomicUsize::new(0));
    let adapter_error = "adapter protocol failed".to_string();
    let server = server_with_terminal_error_adapter(
        root.path(),
        agent_calls.clone(),
        adapter_calls.clone(),
        adapter_error.clone(),
    )?;
    let thread_id = server
        .thread_manager
        .start_thread(root.path().to_path_buf());
    let turn_id = server
        .thread_manager
        .start_turn(&thread_id, "prompt".to_string(), AgentId::from_str("codex"))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(16);
    let mut notification_rx = notification_tx.subscribe();

    run_turn_lifecycle_with_options(
        server.clone(),
        None,
        notification_tx,
        thread_id.clone(),
        turn_id.clone(),
        "prompt".to_string(),
        "codex".to_string(),
        TurnLifecycleOptions::default(),
    )
    .await;

    assert_eq!(agent_calls.load(Ordering::Acquire), 0);
    assert_eq!(adapter_calls.load(Ordering::Acquire), 1);
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Failed);
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Error { message, .. } if message == &adapter_error
    )));

    let mut completion_statuses = Vec::new();
    while let Ok(notification) = notification_rx.try_recv() {
        if let Notification::TurnCompleted { status, .. } = notification.notification {
            completion_statuses.push(status);
        }
    }
    assert_eq!(completion_statuses, vec![TurnStatus::Failed]);
    Ok(())
}
