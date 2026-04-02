use crate::http::AppState;
use crate::router;
use harness_protocol::{codec, methods::RpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

async fn process_line(state: &AppState, line: &str) -> anyhow::Result<Option<String>> {
    let response = match codec::decode_request(line) {
        Ok(req) => router::handle_request(state, req).await,
        Err(e) => Some(RpcResponse::error(
            None,
            harness_protocol::methods::PARSE_ERROR,
            format!("parse error: {e}"),
        )),
    };

    match response {
        Some(r) => Ok(Some(codec::encode_response(&r)?)),
        None => Ok(None),
    }
}

/// Serve JSON-RPC over stdio (one JSON object per line).
///
/// Both JSON-RPC responses and server-push notifications are written to the
/// same stdout stream through a single writer task, preventing interleaving.
pub async fn serve(mut state: AppState) -> anyhow::Result<()> {
    // Unified output channel: all strings written to stdout go through here.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(128);

    // Notification sub-channel: handlers call `notify::emit` which sends here.
    let (notify_tx, mut notify_rx) = crate::notify::channel(64);
    state.notifications.notify_tx = Some(notify_tx);

    // Notification encoder: RpcNotification -> JSON line -> out_tx.
    let out_tx_notif = out_tx.clone();
    tokio::spawn(async move {
        while let Some(notif) = notify_rx.recv().await {
            match codec::encode_notification(&notif) {
                Ok(line) => {
                    if out_tx_notif.send(line).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::error!("notification encode error: {e}"),
            }
        }
    });

    // Stdout writer: single task owns stdout to prevent concurrent writes.
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    tracing::info!("harness: stdio server started");

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let mut shutdown = std::pin::pin!(shutdown_signal());

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line? {
                    Some(line) if !line.trim().is_empty() => {
                        if let Some(out) = process_line(&state, &line).await? {
                            if out_tx.send(out).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            _ = &mut shutdown => {
                tracing::info!("stdio server received shutdown signal");
                break;
            }
        }
    }

    state.observability.events.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("stdio: received Ctrl+C"),
        _ = terminate => tracing::info!("stdio: received SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;
    use harness_protocol::{methods::Method, methods::RpcRequest};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(
            &harness_core::config::dirs::default_db_path(dir, "tasks"),
        )
        .await?;
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir).await?);
        let signal_detector = harness_gc::signal_detector::SignalDetector::new(
            server.config.gc.signal_thresholds.clone().into(),
            harness_core::types::ProjectId::new(),
        );
        let draft_store = harness_gc::draft_store::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::gc_agent::GcAgent::new(
            server.config.gc.clone(),
            signal_detector,
            draft_store,
            dir.to_path_buf(),
        ));
        let thread_db = crate::thread_db::ThreadDb::open(
            &harness_core::config::dirs::default_db_path(dir, "threads"),
        )
        .await?;
        let _project_svc_tmp = crate::project_registry::ProjectRegistry::open(
            &harness_core::config::dirs::default_db_path(dir, "projects"),
        )
        .await?;
        let project_svc = crate::services::project::DefaultProjectService::new(
            _project_svc_tmp,
            dir.to_path_buf(),
        );
        let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
        let execution_svc = crate::services::execution::DefaultExecutionService::new(
            tasks.clone(),
            server.agent_registry.clone(),
            Arc::new(server.config.clone()),
            Default::default(),
            events.clone(),
            vec![],
            None,
            Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            None,
            None,
            vec![],
        );

        Ok(AppState {
            core: crate::http::CoreServices {
                server,
                project_root: dir.to_path_buf(),
                home_dir: std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| dir.to_path_buf()),
                tasks,
                thread_db: Some(thread_db),
                plan_db: None,
                plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
                project_registry: None,
                runtime_state_store: None,
            },
            engines: crate::http::EngineServices {
                skills: Arc::new(RwLock::new(harness_skills::store::SkillStore::new())),
                rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
                gc_agent,
            },
            observability: crate::http::ObservabilityServices {
                events,
                signal_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::SignalRateLimiter::new(100),
                ),
                password_reset_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::PasswordResetRateLimiter::new(5),
                ),
                review_store: None,
            },
            concurrency: crate::http::ConcurrencyServices {
                task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
                workspace_mgr: None,
            },
            runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
            runtime_project_cache: Arc::new(
                crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
            ),
            notifications: crate::http::NotificationServices {
                notification_tx: tokio::sync::broadcast::channel(32).0,
                notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                notification_lag_log_every: 1,
                notify_tx: None,
                initializing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
            },
            interceptors: vec![],
            intake: crate::http::IntakeServices {
                feishu_intake: None,
                github_pollers: vec![],
                completion_callback: None,
            },
            project_svc,
            task_svc,
            execution_svc,
        })
    }

    #[tokio::test]
    async fn stdio_processes_initialize_then_initialized() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;
        state.notifications.initialized =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let init_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::Initialize,
        })?;
        let init_out = process_line(&state, &init_line)
            .await?
            .expect("initialize should return a response");
        let init_resp: harness_protocol::methods::RpcResponse = codec::decode_response(&init_out)?;
        assert!(
            init_resp.error.is_none(),
            "initialize failed: {:?}",
            init_resp.error
        );
        let init_result = init_resp
            .result
            .ok_or_else(|| anyhow::anyhow!("initialize response missing result"))?;
        assert!(
            init_result["capabilities"].is_object(),
            "initialize should return capabilities"
        );
        assert!(
            init_result["capabilities"]["notifications"]
                .as_bool()
                .unwrap_or(false),
            "capabilities should advertise notifications support"
        );

        // Initialized with id=None is a JSON-RPC notification — no response expected.
        let initialized_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Method::Initialized,
        })?;
        let initialized_out = process_line(&state, &initialized_line).await?;
        assert!(
            initialized_out.is_none(),
            "initialized notification (id=None) should produce no response"
        );
        Ok(())
    }

    #[tokio::test]
    async fn thread_start_emits_notification() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;

        let (notify_tx, mut notify_rx) = crate::notify::channel(8);
        state.notifications.notify_tx = Some(notify_tx);

        let proj_dir = crate::test_helpers::tempdir_in_home("harness-test-")?;

        let line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart {
                cwd: proj_dir.path().to_path_buf(),
            },
        })?;

        let out = process_line(&state, &line)
            .await?
            .expect("should return a response");
        let resp: harness_protocol::methods::RpcResponse = codec::decode_response(&out)?;
        assert!(
            resp.error.is_none(),
            "thread_start failed: {:?}",
            resp.error
        );

        let notif = notify_rx
            .try_recv()
            .expect("thread_start should emit a notification");
        let json = serde_json::to_string(&notif)?;
        assert!(
            json.contains("thread/status_changed"),
            "expected thread/status_changed notification, got: {json}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn turn_start_emits_notification() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;

        let (notify_tx, mut notify_rx) = crate::notify::channel(8);
        state.notifications.notify_tx = Some(notify_tx);

        let proj_dir = crate::test_helpers::tempdir_in_home("harness-test-")?;

        // First create a thread.
        let thread_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart {
                cwd: proj_dir.path().to_path_buf(),
            },
        })?;
        let thread_out = process_line(&state, &thread_line)
            .await?
            .expect("thread_start should return a response");
        let thread_resp: harness_protocol::methods::RpcResponse =
            codec::decode_response(&thread_out)?;
        let thread_id_str = thread_resp.result.unwrap()["thread_id"]
            .as_str()
            .unwrap()
            .to_string();
        let thread_id = harness_core::types::ThreadId::from_str(&thread_id_str);

        // Drain the thread_start notification.
        let _ = notify_rx.try_recv();

        // Start a turn.
        let turn_line = serde_json::to_string(&RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(2)),
            method: Method::TurnStart {
                thread_id,
                input: "hello".to_string(),
            },
        })?;
        let turn_out = process_line(&state, &turn_line)
            .await?
            .expect("turn_start should return a response");
        let turn_resp: harness_protocol::methods::RpcResponse = codec::decode_response(&turn_out)?;
        assert!(
            turn_resp.error.is_none(),
            "turn_start failed: {:?}",
            turn_resp.error
        );

        let notif = notify_rx
            .try_recv()
            .expect("turn_start should emit a notification");
        let json = serde_json::to_string(&notif)?;
        assert!(
            json.contains("turn/started"),
            "expected turn/started notification, got: {json}"
        );
        Ok(())
    }
}
