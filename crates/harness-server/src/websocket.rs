use crate::{http::AppState, router};
use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use harness_protocol::{codec, RpcResponse};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Returns true if the origin is a localhost origin (safe for local dev tools).
///
/// Parses the host from the origin to prevent bypass via domains like
/// `http://localhost.evil.com`.
fn is_local_origin(origin: &str) -> bool {
    if origin == "null" {
        return true;
    }
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
/// Validates the Origin header to prevent Cross-Site WebSocket Hijacking (CSWH).
/// CLI clients that omit the Origin header are always allowed.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
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
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle a single WebSocket connection.
///
/// - Incoming text frames are decoded as JSON-RPC 2.0 requests, routed through
///   the standard dispatcher, and the response is sent back as a text frame.
/// - Server-push notifications broadcast on `AppState::notification_tx` are
///   forwarded to the client as unsolicited text frames.
async fn handle_socket(ws: WebSocket, state: Arc<AppState>) {
    let (mut ws_sink, mut ws_stream) = ws.split();

    // Internal channel: both the request handler and the notification forwarder
    // write JSON strings here; the sender task drains them to the WebSocket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Task 1: drain the internal channel → WebSocket sink.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
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
                        if notif_out_tx.send(text).is_err() {
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

    // Main loop: read incoming frames, dispatch as JSON-RPC, reply.
    while let Some(result) = ws_stream.next().await {
        let text = match result {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };

        let response = match codec::decode_request(&text) {
            Ok(req) => router::handle_request(&state, req).await,
            Err(e) => Some(RpcResponse::error(
                None,
                harness_protocol::PARSE_ERROR,
                format!("parse error: {e}"),
            )),
        };

        if let Some(resp) = response {
            match codec::encode_response(&resp) {
                Ok(out) => {
                    if out_tx.send(out).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("failed to encode response: {e}"),
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
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use harness_protocol::{codec, Method, Notification, RpcNotification, RpcRequest};
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
        let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_core::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let (notification_tx, _) = broadcast::channel(notification_broadcast_capacity);

        Ok(AppState {
            core: crate::http::CoreServices {
                server,
                project_root: dir.to_path_buf(),
                tasks,
                thread_db: Some(thread_db),
                plan_db: None,
                plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            },
            engines: crate::http::EngineServices {
                skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
                rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
                gc_agent,
            },
            observability: crate::http::ObservabilityServices { events },
            concurrency: crate::http::ConcurrencyServices {
                task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
                workspace_mgr: None,
            },
            notifications: crate::http::NotificationServices {
                notification_tx,
                notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                notification_lag_log_every,
                notify_tx: None,
                initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
            interceptors: vec![],
            feishu_intake: None,
            github_intake: None,
            completion_callback: None,
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
        assert!(is_local_origin("null"));
    }

    #[test]
    fn is_local_origin_rejects_non_local() {
        assert!(!is_local_origin("http://example.com"));
        assert!(!is_local_origin("http://localhost.evil.com"));
        assert!(!is_local_origin("http://192.168.1.1"));
        assert!(!is_local_origin("http://0.0.0.0"));
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
        let resp: harness_protocol::RpcResponse = codec::decode_response(&body)?;
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
        let thread_id = harness_core::ThreadId::new();
        let turn_id = harness_core::TurnId::new();
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
                .notifications.notification_tx
                .send(RpcNotification::new(Notification::TurnStarted {
                    thread_id: harness_core::ThreadId::new(),
                    turn_id: harness_core::TurnId::new(),
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
                .notifications.notification_lagged_total
                .load(std::sync::atomic::Ordering::Relaxed),
            dropped_total
        );
        Ok(())
    }
}
