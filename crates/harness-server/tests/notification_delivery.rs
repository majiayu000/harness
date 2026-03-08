use harness_agents::AgentRegistry;
use harness_core::{HarnessConfig, ThreadId};
use harness_server::{
    handlers::thread::{thread_start, turn_start},
    http::build_app_state,
    notify,
    server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::path::PathBuf;
use std::sync::Arc;

fn tempdir_in_home(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("resolve cwd"));
    if let Ok(dir) = tempfile::Builder::new().prefix(prefix).tempdir_in(&home) {
        return Ok(dir);
    }
    let fallback = std::env::current_dir()?.join(".harness-test-home");
    std::fs::create_dir_all(&fallback)?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&fallback)
        .map_err(Into::into)
}

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
    let sandbox = tempdir_in_home("harness-notif-")?;
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
    let json = serde_json::to_string(&notif)?;
    assert!(
        json.contains("thread/status_changed"),
        "expected thread/status_changed notification, got: {json}"
    );
    assert!(
        json.contains(&thread_id_str),
        "notification should contain the thread_id, got: {json}"
    );
    Ok(())
}

#[tokio::test]
async fn turn_start_delivers_turn_started_notification() -> anyhow::Result<()> {
    let sandbox = tempdir_in_home("harness-notif-")?;
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
    let json = serde_json::to_string(&notif)?;
    assert!(
        json.contains("turn/started"),
        "expected turn/started notification, got: {json}"
    );
    assert!(
        json.contains(&thread_id_str),
        "notification should contain the thread_id, got: {json}"
    );
    assert!(
        json.contains(&turn_id_str),
        "notification should contain the turn_id, got: {json}"
    );
    Ok(())
}
