mod common;

use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{Capability, Item, ThreadId, ThreadStatus, TokenUsage, TurnStatus};
use harness_protocol::notifications::Notification;
use harness_server::{
    handlers::thread::{thread_start, turn_start},
    http::build_app_state,
    notify,
    server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::time::{timeout, Duration};

// ---------------------------------------------------------------------------
// MockAgent — minimal in-process agent used by turn lifecycle tests
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum MockMode {
    Complete,
    Fail,
}

#[derive(Clone)]
struct MockAgent {
    mode: MockMode,
}

impl MockAgent {
    fn complete() -> Arc<Self> {
        Arc::new(Self {
            mode: MockMode::Complete,
        })
    }

    fn fail() -> Arc<Self> {
        Arc::new(Self {
            mode: MockMode::Fail,
        })
    }
}

#[async_trait]
impl CodeAgent for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
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
    ) -> harness_core::error::Result<()> {
        match self.mode {
            MockMode::Complete => {
                tx.send(StreamItem::ItemCompleted {
                    item: Item::AgentReasoning {
                        content: "done".to_string(),
                    },
                })
                .await
                .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
                tx.send(StreamItem::TokenUsage {
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 5,
                        total_tokens: 10,
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
            MockMode::Fail => Err(HarnessError::AgentExecution("mock failure".to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// State builders
// ---------------------------------------------------------------------------

async fn make_state(root: &std::path::Path) -> anyhow::Result<harness_server::http::AppState> {
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = root.join("server-data");
    config.server.project_root = project_root;

    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    build_app_state(server).await
}

async fn make_state_with_mock(
    root: &std::path::Path,
    agent: Arc<MockAgent>,
) -> anyhow::Result<harness_server::http::AppState> {
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = root.join("server-data");
    config.server.project_root = project_root;
    config.agents.default_agent = "mock".to_string();

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", agent);

    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    build_app_state(server).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain the notify channel until the predicate matches, or timeout elapses.
async fn wait_for_notification(
    rx: &mut notify::NotifyReceiver,
    predicate: impl Fn(&Notification) -> bool,
    timeout_ms: u64,
) -> anyhow::Result<Notification> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timeout: expected notification not received");
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(rpc)) => {
                if predicate(&rpc.notification) {
                    return Ok(rpc.notification);
                }
            }
            Ok(None) => anyhow::bail!("notification channel closed unexpectedly"),
            Err(_) => anyhow::bail!("timeout: expected notification not received"),
        }
    }
}

fn parse_str_field(value: &serde_json::Value, field: &str) -> anyhow::Result<String> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing field: {field}"))
}

#[tokio::test]
async fn thread_start_delivers_status_changed_notification() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-notif-")?;
    let mut state = make_state(sandbox.path()).await?;

    let (notify_tx, mut notify_rx) = notify::channel(8);
    state.notifications.notify_tx = Some(notify_tx);

    let cwd = sandbox.path().join("project");
    let resp = thread_start(&state, Some(serde_json::json!(1)), cwd).await;
    assert!(
        resp.error.is_none(),
        "thread_start failed: {:?}",
        resp.error
    );
    let thread_id_str = parse_str_field(resp.result.as_ref().unwrap(), "thread_id")?;

    let notif = notify_rx
        .try_recv()
        .expect("thread_start should emit a notification");
    if let Notification::ThreadStatusChanged { thread_id, status } = notif.notification {
        assert_eq!(thread_id.as_str(), thread_id_str);
        assert_eq!(status, ThreadStatus::Idle);
    } else {
        panic!(
            "expected thread/status_changed notification, got: {:?}",
            notif.notification
        );
    }
    Ok(())
}

#[tokio::test]
async fn turn_start_delivers_turn_started_notification() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-notif-")?;
    let mut state = make_state(sandbox.path()).await?;

    let (notify_tx, mut notify_rx) = notify::channel(8);
    state.notifications.notify_tx = Some(notify_tx);

    let cwd = sandbox.path().join("project");

    // Create a thread first.
    let thread_resp = thread_start(&state, Some(serde_json::json!(1)), cwd).await;
    assert!(
        thread_resp.error.is_none(),
        "thread_start failed: {:?}",
        thread_resp.error
    );
    let thread_id_str = parse_str_field(thread_resp.result.as_ref().unwrap(), "thread_id")?;
    let thread_id = ThreadId::from_str(&thread_id_str);

    // Drain the thread_start notification.
    let _ = notify_rx.try_recv();

    // Start a turn on the thread.
    let turn_resp = turn_start(
        &state,
        Some(serde_json::json!(2)),
        thread_id.clone(),
        "hello".to_string(),
    )
    .await;
    assert!(
        turn_resp.error.is_none(),
        "turn_start failed: {:?}",
        turn_resp.error
    );
    let turn_id_str = parse_str_field(turn_resp.result.as_ref().unwrap(), "turn_id")?;

    let notif = notify_rx
        .try_recv()
        .expect("turn_start should emit a turn/started notification");
    if let Notification::TurnStarted { thread_id, turn_id } = notif.notification {
        assert_eq!(thread_id.as_str(), thread_id_str);
        assert_eq!(turn_id.as_str(), turn_id_str);
    } else {
        panic!(
            "expected turn/started notification, got: {:?}",
            notif.notification
        );
    }
    Ok(())
}

#[tokio::test]
async fn turn_completed_delivers_turn_completed_notification() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("harness-notif-")?;
    let mut state = make_state_with_mock(sandbox.path(), MockAgent::complete()).await?;

    let (notify_tx, mut notify_rx) = notify::channel(32);
    state.notifications.notify_tx = Some(notify_tx);

    let cwd = sandbox.path().join("project");

    let thread_resp = thread_start(&state, Some(serde_json::json!(1)), cwd).await;
    assert!(
        thread_resp.error.is_none(),
        "thread_start failed: {:?}",
        thread_resp.error
    );
    let thread_id_str = parse_str_field(thread_resp.result.as_ref().unwrap(), "thread_id")?;
    let thread_id = ThreadId::from_str(&thread_id_str);

    let turn_resp = turn_start(
        &state,
        Some(serde_json::json!(2)),
        thread_id,
        "hello".to_string(),
    )
    .await;
    assert!(
        turn_resp.error.is_none(),
        "turn_start failed: {:?}",
        turn_resp.error
    );
    let turn_id_str = parse_str_field(turn_resp.result.as_ref().unwrap(), "turn_id")?;

    let notif = wait_for_notification(
        &mut notify_rx,
        |n| matches!(n, Notification::TurnCompleted { .. }),
        3_000,
    )
    .await?;

    if let Notification::TurnCompleted {
        turn_id,
        status,
        token_usage,
    } = notif
    {
        assert_eq!(turn_id.as_str(), turn_id_str);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(token_usage.total_tokens, 10);
    } else {
        panic!("expected turn/completed notification, got: {:?}", notif);
    }
    Ok(())
}

#[tokio::test]
async fn turn_failed_delivers_turn_completed_notification_with_failed_status() -> anyhow::Result<()>
{
    let sandbox = common::tempdir_in_home("harness-notif-")?;
    let mut state = make_state_with_mock(sandbox.path(), MockAgent::fail()).await?;

    let (notify_tx, mut notify_rx) = notify::channel(32);
    state.notifications.notify_tx = Some(notify_tx);

    let cwd = sandbox.path().join("project");

    let thread_resp = thread_start(&state, Some(serde_json::json!(1)), cwd).await;
    assert!(
        thread_resp.error.is_none(),
        "thread_start failed: {:?}",
        thread_resp.error
    );
    let thread_id_str = parse_str_field(thread_resp.result.as_ref().unwrap(), "thread_id")?;
    let thread_id = ThreadId::from_str(&thread_id_str);

    let turn_resp = turn_start(
        &state,
        Some(serde_json::json!(2)),
        thread_id,
        "fail please".to_string(),
    )
    .await;
    assert!(
        turn_resp.error.is_none(),
        "turn_start failed: {:?}",
        turn_resp.error
    );
    let turn_id_str = parse_str_field(turn_resp.result.as_ref().unwrap(), "turn_id")?;

    let notif = wait_for_notification(
        &mut notify_rx,
        |n| matches!(n, Notification::TurnCompleted { .. }),
        3_000,
    )
    .await?;

    if let Notification::TurnCompleted {
        turn_id, status, ..
    } = notif
    {
        assert_eq!(turn_id.as_str(), turn_id_str);
        assert_eq!(status, TurnStatus::Failed);
    } else {
        panic!("expected turn/completed notification, got: {:?}", notif);
    }
    Ok(())
}
