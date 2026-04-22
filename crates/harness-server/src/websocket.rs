use crate::{http::AppState, router};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use harness_protocol::{codec, methods::RpcResponse};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::broadcast::error::RecvError;

/// Returns true if the origin is a localhost origin (safe for local dev tools).
///
/// Parses the host from the origin to prevent bypass via domains like
/// `http://localhost.evil.com`.
///
/// `"null"` is intentionally NOT treated as local: browsers send `Origin: null`
/// for `file:` URLs and sandboxed iframes, which are untrusted contexts.
fn is_local_origin(origin: &str) -> bool {
    // Origin format: scheme://host or scheme://host:port
    // Extract the host by stripping scheme and optional port.
    let host = origin
        .split_once("://")
        .map(|(_, rest)| {
            rest.split(':')
                .next()
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("")
        })
        .unwrap_or("");
    host == "localhost" || host == "127.0.0.1"
}

#[derive(Debug, PartialEq, Eq)]
enum OriginValidationError {
    InvalidUtf8,
    NonLocal(String),
}

fn validate_origin_header(headers: &HeaderMap) -> Result<(), OriginValidationError> {
    let Some(origin) = headers.get("Origin") else {
        return Ok(());
    };

    let origin_str = origin
        .to_str()
        .map_err(|_| OriginValidationError::InvalidUtf8)?;

    if is_local_origin(origin_str) {
        Ok(())
    } else {
        Err(OriginValidationError::NonLocal(origin_str.to_owned()))
    }
}

/// Axum handler that upgrades the HTTP connection to WebSocket.
///
/// Two-layer access control:
/// 1. Origin check (CSWH prevention): when an Origin header is present it must
///    identify a localhost origin.  This blocks Cross-Site WebSocket Hijacking
///    from remote websites.
/// 2. Bearer token auth: when an API token is configured, **every** client must
///    present a valid `Authorization: Bearer <token>` header, regardless of
///    whether an Origin header is present.  Checking Origin alone is insufficient
///    because non-browser clients can forge `Origin: http://localhost` while
///    omitting the secret token.  Browsers that need to connect to this endpoint
///    should obtain and forward the token via an alternative mechanism (e.g. a
///    pre-flight REST call that returns a short-lived credential).
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Layer 1: CSWH prevention via Origin check.
    if let Err(err) = validate_origin_header(&headers) {
        match err {
            OriginValidationError::InvalidUtf8 => {
                tracing::warn!("WebSocket connection rejected: Origin header is not valid UTF-8");
            }
            OriginValidationError::NonLocal(origin) => {
                tracing::warn!(
                    "WebSocket connection rejected: non-local Origin {:?}",
                    origin
                );
            }
        }
        return StatusCode::FORBIDDEN.into_response();
    }

    // Layer 2: Bearer token auth for all clients when a token is configured.
    // Origin headers can be forged by non-browser tools, so they do not exempt
    // a client from token authentication.
    if let Some(expected) = crate::http::auth::resolve_api_token(&state.core.server.config.server) {
        let authorized = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|tok| tok.as_bytes().ct_eq(expected.as_bytes()).into())
            .unwrap_or(false);
        if !authorized {
            tracing::warn!("WebSocket connection rejected: missing or invalid Bearer token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle a single WebSocket connection.
///
/// - Incoming text frames are decoded as JSON-RPC 2.0 requests, routed through
///   the standard dispatcher, and the response is sent back as a text frame.
/// - Server-push notifications broadcast on `AppState::notification_tx` are
///   forwarded to the client as unsolicited text frames.
/// - A Ping frame is sent every `ws_heartbeat_interval_secs` seconds. If the
///   client does not respond with a Pong before the next Ping, the connection
///   is treated as stale and closed.
/// - When the server signals graceful shutdown via `ws_shutdown_tx`, a Close
///   frame is sent and the handler exits.
async fn handle_socket(ws: WebSocket, state: Arc<AppState>) {
    let (mut ws_sink, mut ws_stream) = ws.split();

    // Internal channel: both the request handler and the notification forwarder
    // write messages here; the sender task drains them to the WebSocket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Task 1: drain the internal channel → WebSocket sink.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Task 2: subscribe to the notification broadcast and forward to client.
    let notif_out_tx = out_tx.clone();
    let mut notif_rx = state.notifications.notification_tx.subscribe();
    let notif_state = state.clone();
    let notif_task = tokio::spawn(async move {
        loop {
            match notif_rx.recv().await {
                Ok(notif) => match codec::encode_notification(&notif) {
                    Ok(text) => {
                        if notif_out_tx.send(Message::Text(text.into())).is_err() {
                            break;
                        }
                    }
                    Err(e) => tracing::warn!("failed to encode notification: {e}"),
                },
                Err(RecvError::Lagged(skipped)) => {
                    notif_state.observe_notification_lag(skipped as u64);
                    continue;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    // Heartbeat: send a Ping every heartbeat_interval. If no Pong arrives before
    // the next tick, treat the connection as stale and close it.
    let heartbeat_interval_secs = state
        .core
        .server
        .config
        .server
        .ws_heartbeat_interval_secs
        .max(1);
    let heartbeat_interval = tokio::time::Duration::from_secs(heartbeat_interval_secs);
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.tick().await; // consume the first immediate tick
    let mut pong_pending = false;

    // Subscribe to the graceful-shutdown signal.
    let mut ws_shutdown_rx = state.notifications.ws_shutdown_tx.subscribe();

    // Main loop: read incoming frames, dispatch as JSON-RPC, reply.
    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                let result = match msg {
                    Some(r) => r,
                    None => break,
                };
                let text = match result {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Pong(_)) => {
                        pong_pending = false;
                        continue;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };

                let response = match codec::decode_request(&text) {
                    Ok(req) => router::handle_request(&state, req).await,
                    Err(e) => Some(RpcResponse::error(
                        None,
                        harness_protocol::methods::PARSE_ERROR,
                        format!("parse error: {e}"),
                    )),
                };

                if let Some(resp) = response {
                    match codec::encode_response(&resp) {
                        Ok(out) => {
                            if out_tx.send(Message::Text(out.into())).is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("failed to encode response: {e}"),
                    }
                }
            }

            _ = heartbeat.tick() => {
                if pong_pending {
                    tracing::debug!("WebSocket heartbeat timeout: closing stale connection");
                    break;
                }
                pong_pending = true;
                if out_tx.send(Message::Ping(Bytes::new())).is_err() {
                    break;
                }
            }

            _ = ws_shutdown_rx.recv() => {
                out_tx.send(Message::Close(None)).ok();
                break;
            }
        }
    }

    notif_task.abort();
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use axum::http::HeaderValue;
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;
    use harness_protocol::{
        codec, methods::Method, methods::RpcRequest, notifications::Notification,
        notifications::RpcNotification,
    };
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tokio::sync::RwLock;
    type TestWebSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        make_test_state_with_config(dir, HarnessConfig::default()).await
    }

    async fn make_test_state_with_config(
        dir: &std::path::Path,
        config: HarnessConfig,
    ) -> anyhow::Result<AppState> {
        let notification_broadcast_capacity = config.server.notification_broadcast_capacity.max(1);
        let notification_lag_log_every = config.server.notification_lag_log_every;
        let server = Arc::new(HarnessServer::new(
            config,
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
        let (notification_tx, _) = broadcast::channel(notification_broadcast_capacity);
        let (ws_shutdown_tx, _) = broadcast::channel(1);

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
                q_values: None,
                maintenance_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
                review_task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
                workspace_mgr: None,
            },
            #[cfg(test)]
            _db_state_guard: Some(crate::test_helpers::acquire_db_state_guard().await?),
            runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
            runtime_project_cache: Arc::new(
                crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
            ),
            runtime_state_persist_lock: tokio::sync::Mutex::new(()),
            runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
            notifications: crate::http::NotificationServices {
                notification_tx,
                notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                notification_lag_log_every,
                notify_tx: None,
                initializing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ws_shutdown_tx,
            },
            interceptors: vec![],
            degraded_subsystems: vec![],
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

    async fn bind_ws_test_listener() -> anyhow::Result<Option<tokio::net::TcpListener>> {
        match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => Ok(Some(listener)),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!("skipping websocket test due to sandbox network restriction: {err}");
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn connect_ws_test_client(url: &str) -> anyhow::Result<Option<TestWebSocket>> {
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => Ok(Some(ws)),
            Err(tokio_tungstenite::tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                tracing::warn!("skipping websocket test due to sandbox network restriction: {err}");
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    #[test]
    fn is_local_origin_accepts_localhost_variants() {
        assert!(is_local_origin("http://localhost"));
        assert!(is_local_origin("http://localhost:3000"));
        assert!(is_local_origin("https://localhost:9800"));
        assert!(is_local_origin("http://127.0.0.1"));
        assert!(is_local_origin("http://127.0.0.1:8080"));
    }

    #[test]
    fn is_local_origin_rejects_non_local() {
        assert!(!is_local_origin("http://example.com"));
        assert!(!is_local_origin("http://localhost.evil.com"));
        assert!(!is_local_origin("http://192.168.1.1"));
        assert!(!is_local_origin("http://0.0.0.0"));
        // "null" is sent by browsers for file: URLs and sandboxed iframes —
        // these are untrusted contexts and must NOT be treated as local.
        assert!(!is_local_origin("null"));
    }

    #[test]
    fn validate_origin_header_allows_missing_origin() {
        let headers = HeaderMap::new();
        assert!(validate_origin_header(&headers).is_ok());
    }

    #[test]
    fn validate_origin_header_allows_local_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static("http://localhost:9800"));
        assert!(validate_origin_header(&headers).is_ok());
    }

    #[test]
    fn validate_origin_header_rejects_remote_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static("http://evil.com"));
        assert!(matches!(
            validate_origin_header(&headers),
            Err(OriginValidationError::NonLocal(_))
        ));
    }

    #[test]
    fn validate_origin_header_rejects_non_utf8_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Origin",
            HeaderValue::from_bytes(b"http://localhost\xff").expect("valid raw header value"),
        );

        assert!(matches!(
            validate_origin_header(&headers),
            Err(OriginValidationError::InvalidUtf8)
        ));
    }

    /// Integration test: spin up the HTTP server on a random port and connect
    /// via WebSocket.  Sends an `initialize` request and checks the response.
    #[tokio::test]
    async fn websocket_initialize_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;
        state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = Arc::new(state);

        let listener = match bind_ws_test_listener().await? {
            Some(listener) => listener,
            None => return Ok(()),
        };
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Connect with tokio-tungstenite.
        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let mut ws = match connect_ws_test_client(&url).await? {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Send `initialize`.
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::Initialize,
        };
        let text = serde_json::to_string(&req)?;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
            .await?;

        // Receive response.
        use futures::StreamExt;
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("no message"))??;
        let body = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            other => anyhow::bail!("unexpected message: {other:?}"),
        };
        let resp: harness_protocol::methods::RpcResponse = codec::decode_response(&body)?;
        assert!(resp.error.is_none(), "initialize error: {:?}", resp.error);
        let result = resp.result.ok_or_else(|| anyhow::anyhow!("no result"))?;
        assert!(result["capabilities"].is_object());

        Ok(())
    }

    /// Verify that notifications broadcast on `notification_tx` reach a
    /// connected WebSocket client.
    #[tokio::test]
    async fn websocket_receives_server_push_notification() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = Arc::new(make_test_state(dir.path()).await?);
        let notif_tx = state.notifications.notification_tx.clone();

        let listener = match bind_ws_test_listener().await? {
            Some(listener) => listener,
            None => return Ok(()),
        };
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let mut ws = match connect_ws_test_client(&url).await? {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Ensure the server-side handler is fully running (broadcast subscriber
        // registered) before sending the notification. We do this by completing
        // an initialize round-trip: once we receive the response, the handler
        // loop is live and ready to forward notifications.
        let init_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(0)),
            method: Method::Initialize,
        };
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&init_req)?.into(),
        ))
        .await?;
        {
            use futures::StreamExt;
            ws.next()
                .await
                .ok_or_else(|| anyhow::anyhow!("no init response"))??;
        }

        // Broadcast a notification.
        let thread_id = harness_core::types::ThreadId::new();
        let turn_id = harness_core::types::TurnId::new();
        notif_tx
            .send(RpcNotification::new(Notification::TurnStarted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
            }))
            .ok();

        // Client should receive it.
        use futures::StreamExt;
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("no message"))??;

        let body = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            other => anyhow::bail!("unexpected message: {other:?}"),
        };

        let notif: RpcNotification = serde_json::from_str(&body)?;
        assert_eq!(notif.jsonrpc, "2.0");
        match notif.notification {
            Notification::TurnStarted {
                thread_id: tid,
                turn_id: tuid,
            } => {
                assert_eq!(tid, thread_id);
                assert_eq!(tuid, turn_id);
            }
            other => anyhow::bail!("unexpected notification variant: {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn websocket_tracks_lagged_notifications_under_load() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        config.server.notification_broadcast_capacity = 4;
        config.server.notification_lag_log_every = 1;
        let state = make_test_state_with_config(dir.path(), config).await?;
        let mut rx = state.notifications.notification_tx.subscribe();

        for _ in 0..512 {
            state
                .notifications
                .notification_tx
                .send(RpcNotification::new(Notification::TurnStarted {
                    thread_id: harness_core::types::ThreadId::new(),
                    turn_id: harness_core::types::TurnId::new(),
                }))
                .ok();
        }

        let skipped = loop {
            match tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(_)) => continue,
                Ok(Err(RecvError::Lagged(skipped))) => break skipped,
                Ok(Err(other)) => anyhow::bail!("unexpected recv error: {other:?}"),
                Err(_) => anyhow::bail!("timed out waiting for lagged receiver signal"),
            }
        };

        let dropped_total = state.observe_notification_lag(skipped as u64);
        assert!(dropped_total >= skipped as u64);
        assert_eq!(
            state
                .notifications
                .notification_lagged_total
                .load(std::sync::atomic::Ordering::Relaxed),
            dropped_total
        );
        Ok(())
    }

    /// Verify that the server sends Ping frames at the configured heartbeat interval
    /// and that the connection stays alive when Pong frames are received.
    #[tokio::test]
    async fn websocket_heartbeat_ping_sent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        // Use a 1-second heartbeat so the test completes quickly.
        config.server.ws_heartbeat_interval_secs = 1;
        let state = Arc::new(make_test_state_with_config(dir.path(), config).await?);

        let listener = match bind_ws_test_listener().await? {
            Some(listener) => listener,
            None => return Ok(()),
        };
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let mut ws = match connect_ws_test_client(&url).await? {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Wait up to 3 seconds for a Ping frame from the server.
        // tokio-tungstenite delivers Ping frames to the application before auto-replying.
        use futures::StreamExt;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for heartbeat Ping");
            }
            let msg = match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => anyhow::bail!("ws error: {e}"),
                Ok(None) | Err(_) => anyhow::bail!("connection closed before Ping arrived"),
            };
            match msg {
                tokio_tungstenite::tungstenite::Message::Ping(_) => break,
                _ => continue, // skip other frames (e.g. notifications)
            }
        }

        Ok(())
    }

    /// Verify that broadcasting on `ws_shutdown_tx` causes the server to close
    /// the WebSocket connection gracefully.
    #[tokio::test]
    async fn websocket_graceful_shutdown_closes_connection() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = Arc::new(make_test_state(dir.path()).await?);
        let ws_shutdown_tx = state.notifications.ws_shutdown_tx.clone();

        let listener = match bind_ws_test_listener().await? {
            Some(listener) => listener,
            None => return Ok(()),
        };
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let mut ws = match connect_ws_test_client(&url).await? {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Complete an initialize round-trip to ensure the handler loop is live.
        let init_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(0)),
            method: Method::Initialize,
        };
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&init_req)?.into(),
        ))
        .await?;
        {
            use futures::StreamExt;
            ws.next()
                .await
                .ok_or_else(|| anyhow::anyhow!("no init response"))??;
        }

        // Signal graceful shutdown.
        ws_shutdown_tx.send(()).ok();

        // The client should receive a Close frame or see the connection drop.
        use futures::StreamExt;
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next()).await?;
        match result {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {}
            Some(Ok(other)) => anyhow::bail!("expected Close, got: {other:?}"),
            Some(Err(_)) => {} // connection reset is also acceptable
        }

        Ok(())
    }
}
