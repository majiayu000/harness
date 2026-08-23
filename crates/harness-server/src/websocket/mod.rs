use crate::http::AppState;
use axum::{
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use subtle::ConstantTimeEq;

mod connection;
mod origin;

use connection::handle_socket;
use origin::{validate_origin_header, OriginValidationError};

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
    let auth_mode = match crate::http::auth::resolve_api_auth_mode(&state.core.server.config.server)
    {
        Ok(mode) => mode,
        Err(error) => {
            tracing::error!("WebSocket auth misconfigured after startup: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Some(expected) = auth_mode.expected_token() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket_test_support::{
        bind_ws_test_listener, connect_ws_test_client, make_test_state, make_test_state_with_config,
    };
    use futures::SinkExt;
    use harness_core::config::HarnessConfig;
    use harness_protocol::{
        codec, methods::Method, methods::RpcRequest, notifications::Notification,
        notifications::RpcNotification,
    };
    use tokio::sync::broadcast::error::RecvError;

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

    /// GH-1984 follow-up coverage: requests pipelined over the real socket get
    /// exactly one response each, in arrival order, through the serialized
    /// dispatch worker.
    #[tokio::test]
    async fn websocket_pipelined_requests_answered_in_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        config.server.ws_heartbeat_interval_secs = 1;
        let mut state = make_test_state_with_config(dir.path(), config).await?;
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

        let url = format!("ws://127.0.0.1:{port}/ws", port = addr.port());
        let mut ws = match connect_ws_test_client(&url).await? {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Pipeline several `initialize` requests without waiting. Only the
        // first can succeed (the initialized flag flips after the handshake);
        // the rest are rejected — but every request must yield exactly one
        // response, in arrival order.
        const COUNT: u64 = 8;
        for id in 0..COUNT {
            let req = RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(id)),
                method: Method::Initialize,
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&req)?.into(),
            ))
            .await?;
        }

        use futures::StreamExt;
        for expected_id in 0..COUNT {
            let msg = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next())
                .await?
                .ok_or_else(|| anyhow::anyhow!("connection closed mid-pipeline"))??;
            let body = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                other => anyhow::bail!("unexpected frame: {other:?}"),
            };
            let resp = codec::decode_response(&body)?;
            assert_eq!(resp.id, Some(serde_json::json!(expected_id)));
        }

        Ok(())
    }

    /// A client that stops reading must not grow server memory without
    /// limit: once the bounded outbound backlog fills, the server closes
    /// the connection instead of buffering indefinitely (issue #1996).
    #[tokio::test]
    async fn websocket_closes_connection_when_outbound_backlog_overflows() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        // Keep the heartbeat out of the picture: this test must pass or fail
        // on backlog overflow alone, not stale-pong detection.
        config.server.ws_heartbeat_interval_secs = 600;
        let state = Arc::new(make_test_state_with_config(dir.path(), config).await?);

        let listener = match bind_ws_test_listener().await? {
            Some(listener) => listener,
            None => return Ok(()),
        };
        let addr = listener.local_addr()?;

        let flood_state = state.clone();
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

        // Complete an initialize round-trip so the handler loop is live and
        // its notification forwarder is subscribed before the flood begins.
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

        // Sustained flood from a separate task while the client refuses to
        // read. A one-shot burst would merely trip the notification ring's
        // lag accounting (capacity 256): the burst finishes before the
        // forwarder drains anything, so almost nothing reaches the outbound
        // queue. Pacing the flood keeps steady pressure on so the forwarder
        // keeps pushing frames past the auto-tuned loopback socket buffers
        // (~MBs) into the outbound queue until `try_send` hits the cap.
        const FLOOD: usize = 600_000;
        let flood_task = tokio::spawn(async move {
            for i in 0..FLOOD {
                let _ = flood_state
                    .notifications
                    .notification_tx
                    .send(RpcNotification::new(Notification::TurnStarted {
                        thread_id: harness_core::types::ThreadId::new(),
                        turn_id: harness_core::types::TurnId::new(),
                    }));
                if i % 2_000 == 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                }
            }
        });
        tokio::time::timeout(tokio::time::Duration::from_secs(60), flood_task)
            .await
            .expect("flood task did not finish")?;

        // Give the notifier time to hit the cap and signal the close before
        // the client starts draining.
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // The stream must terminate early: either a Close frame, an abrupt
        // reset, or EOF — anything but delivery of every flooded message.
        use futures::StreamExt;
        let mut delivered = 0usize;
        loop {
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.next()).await {
                Err(_) => {
                    anyhow::bail!("connection stayed open; slow-client backlog grew without bound");
                }
                Ok(None) => break,
                Ok(Some(Err(_))) => break, // abrupt close without handshake is fine
                Ok(Some(Ok(msg))) => match msg {
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    tokio_tungstenite::tungstenite::Message::Text(_) => delivered += 1,
                    _ => {}
                },
            }
        }
        assert!(
            delivered < FLOOD,
            "client received all {FLOOD} notifications; server buffered instead of closing"
        );
        Ok(())
    }
}
