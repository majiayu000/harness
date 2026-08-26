use super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentAdapter, AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{AgentId, Capability, Item, TokenUsage, TurnStatus};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct SilentLifecycleAgent;

struct PendingDrainAdapter {
    terminate_calls: Arc<AtomicUsize>,
    terminate_error: bool,
}

#[async_trait]
impl AgentAdapter for PendingDrainAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<harness_core::agent::AgentEvent>,
    ) -> harness_core::error::Result<()> {
        std::future::pending().await
    }

    async fn terminate_and_drain(&self) -> harness_core::error::Result<()> {
        self.terminate_calls.fetch_add(1, Ordering::AcqRel);
        if self.terminate_error {
            return Err(HarnessError::AgentExecution(
                "forced termination failed".to_string(),
            ));
        }
        Ok(())
    }
}

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

#[tokio::test]
async fn lifecycle_wall_clock_timeout_wins_when_stall_cannot_be_shorter() -> anyhow::Result<()> {
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
        notification_tx,
        thread_id.clone(),
        turn_id.clone(),
        "prompt".to_string(),
        "codex".to_string(),
        TurnLifecycleOptions {
            timeout_secs: Some(1),
            stall_timeout_secs: Some(600),
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
        Item::Error { message, .. } if message.contains("Agent turn timed out after 1s")
    )));
    assert!(!turn.items.iter().any(|item| matches!(
        item,
        Item::Error { message, .. } if message.contains("Agent stream stalled")
    )));
    Ok(())
}

#[tokio::test]
async fn lifecycle_drains_pending_adapter_after_stall_and_wall_clock_timeout() -> anyhow::Result<()>
{
    for (timeout_secs, stall_timeout_secs) in [(2, 1), (1, 600)] {
        let root = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        config.server.project_root = root.path().to_path_buf();
        config.agents.default_agent = "codex".to_string();
        let terminate_calls = Arc::new(AtomicUsize::new(0));
        let terminate_calls_for_factory = terminate_calls.clone();
        let mut registry = AgentRegistry::new("codex");
        registry.register("codex", Arc::new(SilentLifecycleAgent));
        registry
            .register_turn_backend_factory("codex", move || {
                Arc::new(PendingDrainAdapter {
                    terminate_calls: terminate_calls_for_factory.clone(),
                    terminate_error: false,
                })
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?;
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
            server,
            None,
            notification_tx,
            thread_id,
            turn_id,
            "prompt".to_string(),
            "codex".to_string(),
            TurnLifecycleOptions {
                timeout_secs: Some(timeout_secs),
                stall_timeout_secs: Some(stall_timeout_secs),
                ..TurnLifecycleOptions::default()
            },
        )
        .await;

        assert_eq!(terminate_calls.load(Ordering::Acquire), 1);
    }
    Ok(())
}

#[tokio::test]
async fn lifecycle_retains_running_turn_when_agent_cannot_be_drained() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    config.agents.default_agent = "codex".to_string();
    let terminate_calls = Arc::new(AtomicUsize::new(0));
    let terminate_calls_for_factory = terminate_calls.clone();
    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(SilentLifecycleAgent));
    registry
        .register_turn_backend_factory("codex", move || {
            Arc::new(PendingDrainAdapter {
                terminate_calls: terminate_calls_for_factory.clone(),
                terminate_error: true,
            })
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
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
    let termination_not_drained = Arc::new(AtomicBool::new(false));

    run_turn_lifecycle_with_options(
        server.clone(),
        None,
        notification_tx,
        thread_id.clone(),
        turn_id.clone(),
        "prompt".to_string(),
        "codex".to_string(),
        TurnLifecycleOptions {
            timeout_secs: Some(1),
            stall_timeout_secs: Some(600),
            termination_not_drained: Some(termination_not_drained.clone()),
            ..TurnLifecycleOptions::default()
        },
    )
    .await;

    assert_eq!(terminate_calls.load(Ordering::Acquire), 1);
    assert!(termination_not_drained.load(Ordering::Acquire));
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Running);
    Ok(())
}
