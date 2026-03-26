mod common;

use anyhow::Context;
use async_trait::async_trait;
use harness_agents::AgentRegistry;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, HarnessConfig, HarnessError, Item,
    StreamItem, ThreadId, TokenUsage, Turn, TurnId, TurnStatus,
};
use harness_server::{
    handlers::thread::{thread_start, turn_cancel, turn_start, turn_status},
    http::build_app_state,
    server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::time::{sleep, Duration, Instant};

#[derive(Clone)]
enum MockMode {
    CompleteAfter { delay: Duration },
    FailAfter { delay: Duration, message: String },
    BlockForever,
}

#[derive(Clone)]
struct MockAgent {
    mode: MockMode,
}

impl MockAgent {
    fn complete_after(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            mode: MockMode::CompleteAfter { delay },
        })
    }

    fn fail_after(delay: Duration, message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            mode: MockMode::FailAfter {
                delay,
                message: message.into(),
            },
        })
    }

    fn block_forever() -> Arc<Self> {
        Arc::new(Self {
            mode: MockMode::BlockForever,
        })
    }
}

#[async_trait]
impl CodeAgent for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "ok".to_string(),
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
        tx: Sender<StreamItem>,
    ) -> harness_core::Result<()> {
        match &self.mode {
            MockMode::CompleteAfter { delay } => {
                sleep(*delay).await;
                tx.send(StreamItem::ItemCompleted {
                    item: Item::AgentReasoning {
                        content: "agent done".to_string(),
                    },
                })
                .await
                .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
                tx.send(StreamItem::TokenUsage {
                    usage: TokenUsage {
                        input_tokens: 11,
                        output_tokens: 13,
                        total_tokens: 24,
                        cost_usd: 0.0,
                    },
                })
                .await
                .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
                tx.send(StreamItem::Done)
                    .await
                    .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
                Ok(())
            }
            MockMode::FailAfter { delay, message } => {
                sleep(*delay).await;
                Err(HarnessError::AgentExecution(message.clone()))
            }
            MockMode::BlockForever => loop {
                sleep(Duration::from_secs(1)).await;
            },
        }
    }
}

async fn make_state(
    root: &Path,
    agent: Arc<dyn CodeAgent>,
) -> anyhow::Result<harness_server::app_state::AppState> {
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = root.join("server-data");
    config.server.project_root = project_root.clone();
    config.agents.default_agent = "mock".to_string();

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", agent);

    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    build_app_state(server).await
}

fn result_value(resp: harness_protocol::RpcResponse) -> anyhow::Result<serde_json::Value> {
    if let Some(err) = resp.error {
        anyhow::bail!("rpc error {}: {}", err.code, err.message);
    }
    resp.result.context("missing rpc result")
}

fn parse_thread_id(value: &serde_json::Value) -> anyhow::Result<ThreadId> {
    let raw = value
        .get("thread_id")
        .and_then(|v| v.as_str())
        .context("missing thread_id")?;
    Ok(ThreadId::from_str(raw))
}

fn parse_turn_id(value: &serde_json::Value) -> anyhow::Result<TurnId> {
    let raw = value
        .get("turn_id")
        .and_then(|v| v.as_str())
        .context("missing turn_id")?;
    Ok(TurnId::from_str(raw))
}

async fn fetch_turn(
    state: &harness_server::app_state::AppState,
    turn_id: &TurnId,
) -> anyhow::Result<Turn> {
    let value =
        result_value(turn_status(state, Some(serde_json::json!(9)), turn_id.clone()).await)?;
    Ok(serde_json::from_value(value)?)
}

async fn wait_for_status(
    state: &harness_server::app_state::AppState,
    turn_id: &TurnId,
    expected: TurnStatus,
    timeout: Duration,
) -> anyhow::Result<Turn> {
    let deadline = Instant::now() + timeout;
    loop {
        let turn = fetch_turn(state, turn_id).await?;
        if turn.status == expected {
            return Ok(turn);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timeout waiting for status {:?}; current status {:?}",
                expected,
                turn.status
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn start_thread_and_turn(
    state: &harness_server::app_state::AppState,
    cwd: PathBuf,
    input: &str,
) -> anyhow::Result<(ThreadId, TurnId)> {
    let thread_value = result_value(thread_start(state, Some(serde_json::json!(1)), cwd).await)?;
    let thread_id = parse_thread_id(&thread_value)?;

    let turn_value = result_value(
        turn_start(
            state,
            Some(serde_json::json!(2)),
            thread_id.clone(),
            input.to_string(),
        )
        .await,
    )?;
    let turn_id = parse_turn_id(&turn_value)?;
    Ok((thread_id, turn_id))
}

#[tokio::test]
async fn running_to_completed_updates_items_and_usage() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-turn-lifecycle-")?;
    let state = make_state(
        sandbox.path(),
        MockAgent::complete_after(Duration::from_millis(120)),
    )
    .await?;

    let (_, turn_id) =
        start_thread_and_turn(&state, sandbox.path().join("project"), "hello").await?;

    let running = fetch_turn(&state, &turn_id).await?;
    assert_eq!(running.status, TurnStatus::Running);
    assert!(
        !running.items.is_empty(),
        "running turn should expose user input item"
    );

    let completed = wait_for_status(
        &state,
        &turn_id,
        TurnStatus::Completed,
        Duration::from_secs(3),
    )
    .await?;
    assert!(
        completed
            .items
            .iter()
            .any(|item| matches!(item, Item::AgentReasoning { .. })),
        "completed turn should include agent output item"
    );
    assert_eq!(completed.token_usage.total_tokens, 24);
    Ok(())
}

#[tokio::test]
async fn running_to_failed_is_persisted() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-turn-lifecycle-")?;
    let state = make_state(
        sandbox.path(),
        MockAgent::fail_after(Duration::from_millis(120), "mock failure"),
    )
    .await?;

    let (_, turn_id) =
        start_thread_and_turn(&state, sandbox.path().join("project"), "fail please").await?;

    let running = fetch_turn(&state, &turn_id).await?;
    assert_eq!(running.status, TurnStatus::Running);

    let failed =
        wait_for_status(&state, &turn_id, TurnStatus::Failed, Duration::from_secs(3)).await?;
    assert!(
        failed
            .items
            .iter()
            .any(|item| matches!(item, Item::Error { .. })),
        "failed turn should include an error item"
    );
    Ok(())
}

#[tokio::test]
async fn running_to_cancelled_stops_turn() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-turn-lifecycle-")?;
    let state = make_state(sandbox.path(), MockAgent::block_forever()).await?;

    let (_, turn_id) =
        start_thread_and_turn(&state, sandbox.path().join("project"), "cancel me").await?;

    let running = fetch_turn(&state, &turn_id).await?;
    assert_eq!(running.status, TurnStatus::Running);

    let _ = result_value(turn_cancel(&state, Some(serde_json::json!(3)), turn_id.clone()).await)?;

    let cancelled = wait_for_status(
        &state,
        &turn_id,
        TurnStatus::Cancelled,
        Duration::from_secs(2),
    )
    .await?;
    assert_eq!(cancelled.status, TurnStatus::Cancelled);
    Ok(())
}

/// When the configured default agent is not registered in the registry,
/// `run_turn_lifecycle` immediately marks the turn as Failed and records
/// an Error item containing the missing agent name.
#[tokio::test]
async fn turn_fails_when_agent_not_registered() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-turn-lifecycle-")?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = sandbox.path().join("server-data");
    config.server.project_root = project_root.clone();
    // "ghost" is the configured default agent but is never registered.
    config.agents.default_agent = "ghost".to_string();

    let registry = AgentRegistry::new("ghost");
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let state = build_app_state(server).await?;

    let (_, turn_id) =
        start_thread_and_turn(&state, project_root, "do work with ghost agent").await?;

    let failed =
        wait_for_status(&state, &turn_id, TurnStatus::Failed, Duration::from_secs(3)).await?;
    // The agent-not-found path must append an Item::Error naming the missing agent.
    assert!(
        failed.items.iter().any(|item| matches!(
            item,
            Item::Error { message, .. } if message.contains("ghost")
        )),
        "failed turn should contain an Item::Error mentioning the missing agent name"
    );
    Ok(())
}

/// When the agent blocks indefinitely without emitting any stream items,
/// the stall detection in `run_turn_lifecycle` fires after `stall_timeout_secs`
/// and transitions the turn to Failed.
#[tokio::test]
async fn turn_fails_on_stall_timeout() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-turn-lifecycle-")?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = sandbox.path().join("server-data");
    config.server.project_root = project_root.clone();
    config.agents.default_agent = "mock".to_string();
    // Use a very short stall timeout so the test completes quickly.
    config.concurrency.stall_timeout_secs = 1;

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", MockAgent::block_forever());

    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let state = build_app_state(server).await?;

    let (_, turn_id) = start_thread_and_turn(&state, project_root, "slow task that stalls").await?;

    let running = fetch_turn(&state, &turn_id).await?;
    assert_eq!(running.status, TurnStatus::Running);

    // Allow 5s for the 1s stall to fire and persist the Failed state.
    let failed =
        wait_for_status(&state, &turn_id, TurnStatus::Failed, Duration::from_secs(5)).await?;
    // The stall-detection path must append an Item::Error with the stall-specific reason,
    // not merely a generic fallback message.
    assert!(
        failed.items.iter().any(|item| matches!(
            item,
            Item::Error { message, .. } if message.contains("stalled") || message.contains("no output for")
        )),
        "stall-timed-out turn should include an Item::Error from stall detection"
    );
    Ok(())
}
