use crate::{http::AppState, router};
use axum::{
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use harness_protocol::{codec, RpcResponse};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Returns true if the origin is a localhost origin (safe for local dev tools).
fn is_local_origin(origin: &str) -> bool {
    origin == "null"
        || origin.starts_with("http://localhost")
        || origin.starts_with("https://localhost")
        || origin.starts_with("http://127.0.0.1")
        || origin.starts_with("https://127.0.0.1")
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
    if let Some(origin) = headers.get("Origin") {
        let origin_str = origin.to_str().unwrap_or("");
        if !is_local_origin(origin_str) {
            tracing::warn!("WebSocket connection rejected: non-local Origin {:?}", origin_str);
            return StatusCode::FORBIDDEN.into_response();
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
    let mut notif_rx = state.notification_tx.subscribe();
    let notif_task = tokio::spawn(async move {
        loop {
            match notif_rx.recv().await {
                Ok(notif) => {
                    match codec::encode_notification(&notif) {
                        Ok(text) => {
                            if notif_out_tx.send(text).is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("failed to encode notification: {e}"),
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // Main loop: read incoming frames, dispatch as JSON-RPC, reply.
    while let Some(result) = ws_stream.next().await {
        let text = match result {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };

        let response = match codec::decode_request(&text) {
            Ok(req) => router::handle_request(&state, req).await,
            Err(e) => RpcResponse::error(
                None,
                harness_protocol::PARSE_ERROR,
                format!("parse error: {e}"),
            ),
        };

        match codec::encode_response(&response) {
            Ok(out) => {
                if out_tx.send(out).is_err() {
                    break;
                }
            }
            Err(e) => tracing::warn!("failed to encode response: {e}"),
        }
    }

    notif_task.abort();
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use harness_protocol::{codec, Method, Notification, RpcNotification, RpcRequest};
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tokio::sync::RwLock;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
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
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let (notification_tx, _) = broadcast::channel(16);

        Ok(AppState {
            server,
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            interceptors: vec![],
            notification_tx,
        })
    }

    /// Integration test: spin up the HTTP server on a random port and connect
    /// via WebSocket.  Sends an `initialize` request and checks the response.
    #[tokio::test]
    async fn websocket_initialize_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = Arc::new(make_test_state(dir.path()).await?);

        // Bind to a random port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Connect with tokio-tungstenite.
        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

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
        let msg = ws.next().await.ok_or_else(|| anyhow::anyhow!("no message"))??;
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
        let notif_tx = state.notification_tx.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

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
            ws.next().await.ok_or_else(|| anyhow::anyhow!("no init response"))??;
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
        let msg = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            ws.next(),
        )
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
}
