//! Per-connection WebSocket pump: serialized dispatch worker, bounded outbound
//! queue, notification forwarding, heartbeat, and graceful shutdown.

use crate::{http::AppState, websocket_dispatch::WS_REQUEST_BACKLOG};
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
/// - Incoming text frames are forwarded to the per-connection dispatch worker
///   ([`crate::websocket_dispatch::dispatch_requests`]), which routes them
///   through the standard dispatcher and sends each response back as a text
///   frame. The connection loop itself never awaits a request handler, so
///   heartbeats stay live during long calls (GH-1984).
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
    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Task 2: subscribe to the notification broadcast and forward to client.
    let notif_out_tx = out_tx.clone();
    let notif_close_tx = backlog_close_tx.clone();
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
                            let _ = notif_close_tx.send(true);
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

    // Task 3: serialized JSON-RPC dispatch worker (GH-1984). Request handlers
    // run on their own task so a slow handler cannot stall the connection
    // loop, which must service Pong frames and heartbeat ticks promptly or a
    // healthy connection gets misjudged as stale and killed.
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<String>(WS_REQUEST_BACKLOG);
    let dispatch_close_tx = backlog_close_tx.clone();
    let mut dispatch_task = tokio::spawn(crate::websocket_dispatch::dispatch_requests(
        req_rx,
        out_tx.clone(),
        dispatch_close_tx,
        state.clone(),
    ));

    // Subscribe to the graceful-shutdown signal.
    let mut ws_shutdown_rx = state.notifications.ws_shutdown_tx.subscribe();

    // Main loop: forward requests to the dispatch worker; service heartbeat
    // and shutdown locally. Never awaits a request handler (GH-1984).
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

                // Fail-closed backpressure on the request queue: reject instead
                // of growing the queue without bound (GH-1984). Echo the request
                // id when the frame parses so the client can correlate the
                // rejection; unparseable frames keep id null.
                match req_tx.try_send(text.to_string()) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("WebSocket request backlog full; rejecting request");
                        let id = codec::decode_request(&text).ok().and_then(|req| req.id);
                        let resp = RpcResponse::error(
                            id,
                            harness_protocol::methods::INTERNAL_ERROR,
                            "request backlog full".to_string(),
                        );
                        if let Ok(out) = codec::encode_response(&resp) {
                            match out_tx.try_send(Message::Text(out.into())) {
                                Ok(()) => {}
                                Err(_) => break,
                            }
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
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

    // Graceful teardown (GH-1985 review): close the request queue and give
    // the dispatch worker a bounded window to finish its in-flight handler
    // and drain queued requests instead of aborting mid-turn. Then stop
    // notification forwarding, drop the loop-side outbound handle, and give
    // the sender task a bounded window to flush pending frames to the socket.
    drop(req_tx);
    if tokio::time::timeout(
        crate::websocket_dispatch::WS_DISPATCH_DRAIN_GRACE,
        &mut dispatch_task,
    )
    .await
    .is_err()
    {
        tracing::warn!("WebSocket dispatch worker did not drain in time; cancelling");
        dispatch_task.abort();
    }
    notif_task.abort();
    drop(out_tx);
    if tokio::time::timeout(
        crate::websocket_dispatch::WS_DISPATCH_DRAIN_GRACE,
        &mut send_task,
    )
    .await
    .is_err()
    {
        send_task.abort();
    }
}
