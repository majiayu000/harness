//! Per-connection WebSocket pump: bounded outbound queue, notification
//! forwarding, heartbeat, and graceful shutdown.

use crate::{http::AppState, router};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use harness_protocol::{codec, methods::RpcResponse};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Outbound per-connection queue capacity. A client that falls this far
/// behind is closed instead of being buffered indefinitely (issue #1996),
/// mirroring the stale-heartbeat rule: one stalled peer can neither grow
/// server memory without limit nor defeat broadcast-lag accounting.
const WS_OUTBOUND_BACKLOG: usize = 256;

/// Handle a single WebSocket connection.
///
/// - Incoming text frames are decoded as JSON-RPC 2.0 requests, routed through
///   the standard dispatcher, and the response is sent back as a text frame.
/// - Server-push notifications broadcast on `AppState::notification_tx` are
///   forwarded to the client as unsolicited text frames.
/// - A Ping frame is sent every `ws_heartbeat_interval_secs` seconds. If the
///   client does not respond with a Pong before the next Ping, the connection
///   is treated as stale and closed.
/// - When the outbound queue fills because the client stopped reading, the
///   connection is closed instead of buffering without limit.
/// - When the server signals graceful shutdown via `ws_shutdown_tx`, a Close
///   frame is sent and the handler exits.
pub(super) async fn handle_socket(ws: WebSocket, state: Arc<AppState>) {
    let (mut ws_sink, mut ws_stream) = ws.split();

    // Internal bounded channel: both the request handler and the notification
    // forwarder write messages here; the sender task drains them to the
    // WebSocket. Producers use `try_send` — a full backlog closes the
    // connection via `backlog_close_tx` rather than buffering forever.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(WS_OUTBOUND_BACKLOG);
    let (backlog_close_tx, mut backlog_close_rx) = tokio::sync::watch::channel(false);

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
                    Ok(text) => match notif_out_tx.try_send(Message::Text(text.into())) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                "WebSocket outbound backlog full; closing slow connection"
                            );
                            let _ = backlog_close_tx.send(true);
                            break;
                        }
                        Err(_) => break,
                    },
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
                        Ok(out) => match out_tx.try_send(Message::Text(out.into())) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                tracing::warn!(
                                    "WebSocket outbound backlog full; closing slow connection"
                                );
                                break;
                            }
                            Err(_) => break,
                        },
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
                if out_tx.try_send(Message::Ping(Bytes::new())).is_err() {
                    break;
                }
            }

            _ = ws_shutdown_rx.recv() => {
                let _ = out_tx.try_send(Message::Close(None));
                break;
            }

            changed = backlog_close_rx.changed() => {
                // `true` means the notifier hit the backlog cap and asked for
                // this connection to close. A dropped sender means the
                // notifier task exited, which also ends service here.
                if changed.is_err() || *backlog_close_rx.borrow_and_update() {
                    break;
                }
            }
        }
    }

    notif_task.abort();
    send_task.abort();
}
