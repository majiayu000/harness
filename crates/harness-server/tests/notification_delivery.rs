mod common;

use harness_agents::AgentRegistry;
use harness_core::{HarnessConfig, ThreadId, ThreadStatus};
use harness_protocol::Notification;
use harness_server::{
    handlers::thread::{thread_start, turn_start},
    http::build_app_state,
    notify,
    server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::sync::Arc;

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
    state.notify_tx = Some(notify_tx);

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
    state.notify_tx = Some(notify_tx);

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
