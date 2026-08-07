//! Tests for the runtime turn lifecycle, split out of `turn_lifecycle.rs`
//! to keep that file under the repository file-size ceiling.

use super::turn_lifecycle::{
    bridge_agent_event, run_turn_lifecycle_with_options, TurnLifecycleOptions,
};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::{AdapterExecutionStrategy, AgentRegistry};
use harness_core::agent::{
    AgentAdapter, AgentEvent, AgentRequest, AgentResponse, CodeAgent, StreamItem, TurnRequest,
};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{AgentId, Capability, Item, TokenUsage, TurnId, TurnStatus};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc;

struct CountingAgent {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl CodeAgent for CountingAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "ok".to_string(),
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
        tx: mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        tx.send(StreamItem::ItemCompleted {
            item: Item::AgentReasoning {
                content: "agent stream done".to_string(),
            },
        })
        .await
        .map_err(|error| HarnessError::AgentExecution(format!("stream closed: {error}")))?;
        tx.send(StreamItem::Done)
            .await
            .map_err(|error| HarnessError::AgentExecution(format!("stream closed: {error}")))?;
        Ok(())
    }
}

struct CountingAdapter {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentAdapter for CountingAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        tx.send(AgentEvent::TurnCompleted {
            output: "adapter done".to_string(),
        })
        .await
        .map_err(|error| HarnessError::AgentExecution(format!("adapter closed: {error}")))?;
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }
}

struct InterruptTrackingAdapter {
    interrupt_calls: Arc<AtomicUsize>,
    /// start_turn blocks until interrupt fires, so the lease-lost branch
    /// of the turn loop deterministically wins the select race.
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl AgentAdapter for InterruptTrackingAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        self.release.notified().await;
        tx.send(AgentEvent::TurnCompleted {
            output: "adapter done after interrupt".to_string(),
        })
        .await
        .map_err(|error| HarnessError::AgentExecution(format!("adapter closed: {error}")))?;
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        self.interrupt_calls.fetch_add(1, Ordering::AcqRel);
        self.release.notify_waiters();
        Ok(())
    }
}

fn server_with_codex_counts(
    root: &std::path::Path,
    agent_calls: Arc<AtomicUsize>,
    adapter_calls: Arc<AtomicUsize>,
) -> anyhow::Result<Arc<HarnessServer>> {
    let mut config = HarnessConfig::default();
    config.server.project_root = root.to_path_buf();
    config.agents.default_agent = "codex".to_string();

    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(CountingAgent { calls: agent_calls }));
    let adapter_calls_for_factory = adapter_calls.clone();
    registry
        .register_adapter_factory_with_strategy(
            "codex",
            move || {
                Arc::new(CountingAdapter {
                    calls: adapter_calls_for_factory.clone(),
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

fn start_test_turn(server: &HarnessServer, root: &std::path::Path) -> anyhow::Result<TurnId> {
    let thread_id = server.thread_manager.start_thread(root.to_path_buf());
    server
        .thread_manager
        .start_turn(&thread_id, "prompt".to_string(), AgentId::from_str("codex"))
        .map_err(|error| anyhow::anyhow!("{error}"))
}

async fn run_test_turn(
    server: Arc<HarnessServer>,
    root: &std::path::Path,
    turn_id: TurnId,
    options: TurnLifecycleOptions,
) -> anyhow::Result<()> {
    let thread_id = server
        .thread_manager
        .find_thread_for_turn(&turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(16);
    run_turn_lifecycle_with_options(
        server,
        None,
        notification_tx,
        thread_id,
        turn_id,
        "prompt".to_string(),
        "codex".to_string(),
        options,
    )
    .await;
    anyhow::ensure!(root.exists(), "test root should still exist");
    Ok(())
}

#[tokio::test]
async fn lifecycle_uses_registered_turn_adapter_by_default() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let agent_calls = Arc::new(AtomicUsize::new(0));
    let adapter_calls = Arc::new(AtomicUsize::new(0));
    let server = server_with_codex_counts(root.path(), agent_calls.clone(), adapter_calls.clone())?;
    let turn_id = start_test_turn(&server, root.path())?;

    run_test_turn(
        server.clone(),
        root.path(),
        turn_id.clone(),
        TurnLifecycleOptions::default(),
    )
    .await?;

    assert_eq!(agent_calls.load(Ordering::Acquire), 0);
    assert_eq!(adapter_calls.load(Ordering::Acquire), 1);
    let thread_id = server
        .thread_manager
        .find_thread_for_turn(&turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Completed);
    Ok(())
}

#[tokio::test]
async fn lifecycle_force_code_agent_bypasses_turn_adapter() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let agent_calls = Arc::new(AtomicUsize::new(0));
    let adapter_calls = Arc::new(AtomicUsize::new(0));
    let server = server_with_codex_counts(root.path(), agent_calls.clone(), adapter_calls.clone())?;
    let turn_id = start_test_turn(&server, root.path())?;

    run_test_turn(
        server.clone(),
        root.path(),
        turn_id.clone(),
        TurnLifecycleOptions {
            force_code_agent: true,
            ..TurnLifecycleOptions::default()
        },
    )
    .await?;

    assert_eq!(agent_calls.load(Ordering::Acquire), 1);
    assert_eq!(adapter_calls.load(Ordering::Acquire), 0);
    let thread_id = server
        .thread_manager
        .find_thread_for_turn(&turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Completed);
    Ok(())
}

#[tokio::test]
async fn prefired_lease_lost_still_interrupts_turn() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let agent_calls = Arc::new(AtomicUsize::new(0));
    let interrupt_calls = Arc::new(AtomicUsize::new(0));
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    config.agents.default_agent = "codex".to_string();
    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", Arc::new(CountingAgent { calls: agent_calls }));
    let interrupt_calls_for_factory = interrupt_calls.clone();
    let release_for_factory = Arc::new(tokio::sync::Notify::new());
    registry
        .register_adapter_factory_with_strategy(
            "codex",
            move || {
                Arc::new(InterruptTrackingAdapter {
                    interrupt_calls: interrupt_calls_for_factory.clone(),
                    release: release_for_factory.clone(),
                })
            },
            AdapterExecutionStrategy::ExecuteTurns,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let turn_id = start_test_turn(&server, root.path())?;

    // Fire the lease-lost signal BEFORE the turn loop starts polling: the
    // watch channel must not lose it (GH-1877).
    let (lease_lost, receiver) = tokio::sync::watch::channel(false);
    let _ = lease_lost.send(true);
    run_test_turn(
        server.clone(),
        root.path(),
        turn_id.clone(),
        TurnLifecycleOptions {
            lease_lost: Some(receiver),
            ..TurnLifecycleOptions::default()
        },
    )
    .await?;

    assert_eq!(
        interrupt_calls.load(Ordering::Acquire),
        1,
        "pre-fired lease-lost must interrupt the agent"
    );
    let thread_id = server
        .thread_manager
        .find_thread_for_turn(&turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
    assert_eq!(turn.status, TurnStatus::Failed);
    Ok(())
}

#[test]
fn bridge_preserves_warning_and_token_usage_events() {
    let mut output_buf = String::new();
    let mut warning_completion = false;
    let mut usage_completion = false;

    let warning = bridge_agent_event(
        AgentEvent::Warning {
            message: "careful".into(),
        },
        &mut output_buf,
        &mut warning_completion,
    );
    let usage = bridge_agent_event(
        AgentEvent::TokenUsage {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cost_usd: 0.0,
            },
        },
        &mut output_buf,
        &mut usage_completion,
    );

    assert_eq!(
        warning,
        Some(StreamItem::Warning {
            message: "careful".into()
        })
    );
    assert_eq!(
        usage,
        Some(StreamItem::TokenUsage {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cost_usd: 0.0,
            }
        })
    );
}

#[test]
fn bridge_uses_buffered_output_when_turn_completed_payload_is_empty() {
    let mut output_buf = String::new();
    let mut emitted_agent_completion = false;
    let _ = bridge_agent_event(
        AgentEvent::MessageDelta {
            text: "hello".into(),
        },
        &mut output_buf,
        &mut emitted_agent_completion,
    );
    let completed = bridge_agent_event(
        AgentEvent::TurnCompleted {
            output: String::new(),
        },
        &mut output_buf,
        &mut emitted_agent_completion,
    );

    assert_eq!(
        completed,
        Some(StreamItem::ItemCompleted {
            item: Item::AgentReasoning {
                content: "hello".into()
            }
        })
    );
    assert!(output_buf.is_empty());
}

#[test]
fn bridge_suppresses_duplicate_turn_completed_after_agent_message_completion() {
    let mut output_buf = String::new();
    let mut emitted_agent_completion = false;
    let item_completed = bridge_agent_event(
        AgentEvent::ItemCompletedPayload {
            item: Item::AgentReasoning {
                content: "done".into(),
            },
        },
        &mut output_buf,
        &mut emitted_agent_completion,
    );
    let turn_completed = bridge_agent_event(
        AgentEvent::TurnCompleted {
            output: "done".into(),
        },
        &mut output_buf,
        &mut emitted_agent_completion,
    );

    assert_eq!(
        item_completed,
        Some(StreamItem::ItemCompleted {
            item: Item::AgentReasoning {
                content: "done".into()
            }
        })
    );
    assert!(emitted_agent_completion);
    assert_eq!(turn_completed, None);
    assert!(output_buf.is_empty());
}
