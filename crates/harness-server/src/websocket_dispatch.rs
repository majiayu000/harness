//! Serialized per-connection JSON-RPC dispatch worker for WebSocket sessions.
//!
//! Request handlers run here — on their own task — so a slow handler (a long
//! agent call, a blocked store query) cannot stall the connection loop. The
//! connection loop must service Pong frames and heartbeat ticks promptly or a
//! healthy connection is misjudged as stale and killed (GH-1984).

use crate::{http::AppState, router};
use axum::extract::ws::Message;
use harness_protocol::{codec, methods::RpcResponse};
use std::sync::Arc;
use std::time::Duration;

/// Maximum number of JSON-RPC requests queued for dispatch per connection.
/// When the backlog is full the server rejects the request with an error
/// instead of growing memory without bound.
pub(crate) const WS_REQUEST_BACKLOG: usize = 16;

/// How long connection teardown waits for the dispatch worker to finish its
/// in-flight request (and drain the queued backlog) before cancelling it, and
/// for the sender task to flush pending outbound frames before dropping the
/// sink. Bounds teardown without cancelling handlers mid-turn.
pub(crate) const WS_DISPATCH_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Handle queued request texts one at a time (bounded outbound queue)., writing each encoded response to
/// `out_tx`. Processing strictly in arrival order preserves the FIFO response
/// ordering the previous inline dispatch provided. Exits when the queue closes
/// or the outgoing channel drops.
pub(crate) async fn dispatch_requests(
    mut req_rx: tokio::sync::mpsc::Receiver<String>,
    out_tx: tokio::sync::mpsc::Sender<Message>,
    backlog_close_tx: tokio::sync::watch::Sender<bool>,
    state: Arc<AppState>,
) {
    while let Some(text) = req_rx.recv().await {
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
                        tracing::warn!("WebSocket outbound backlog full; closing slow connection");
                        let _ = backlog_close_tx.send(true);
                        break;
                    }
                    Err(_) => break,
                },
                Err(e) => tracing::warn!("failed to encode response: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket_test_support::make_test_state;
    use harness_protocol::{codec, methods::Method, methods::RpcRequest, methods::RpcResponse};

    async fn fresh_state_for_initialize(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
        // The router rejects `initialize` when the initialized flag is already
        // set; reset it so each test can run a clean handshake.
        let mut state = make_test_state(dir).await?;
        state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Ok(Arc::new(state))
    }

    #[tokio::test]
    async fn dispatch_requests_answers_pipelined_requests_in_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = fresh_state_for_initialize(dir.path()).await?;

        let (req_tx, req_rx) = tokio::sync::mpsc::channel::<String>(WS_REQUEST_BACKLOG);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(64);
        let (backlog_close_tx, mut backlog_close_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(dispatch_requests(req_rx, out_tx, backlog_close_tx, state));
        backlog_close_rx.borrow_and_update();

        // Pipeline several requests without waiting; responses must come back
        // in arrival order.
        const COUNT: u64 = 5;
        for id in 0..COUNT {
            let req = RpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(id)),
                method: Method::Initialize,
            };
            req_tx.send(serde_json::to_string(&req)?).await?;
        }

        for expected_id in 0..COUNT {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
                .await?
                .ok_or_else(|| anyhow::anyhow!("dispatch worker dropped output"))?;
            let text = match msg {
                Message::Text(t) => t.to_string(),
                other => anyhow::bail!("unexpected message: {other:?}"),
            };
            let resp: RpcResponse = codec::decode_response(&text)?;
            assert!(resp.error.is_none(), "initialize error: {:?}", resp.error);
            assert_eq!(resp.id, Some(serde_json::json!(expected_id)));
        }

        Ok(())
    }

    #[tokio::test]
    async fn dispatch_requests_reports_parse_error_without_dropping_queue() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = fresh_state_for_initialize(dir.path()).await?;

        let (req_tx, req_rx) = tokio::sync::mpsc::channel::<String>(WS_REQUEST_BACKLOG);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(64);
        let (backlog_close_tx, mut backlog_close_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(dispatch_requests(req_rx, out_tx, backlog_close_tx, state));
        backlog_close_rx.borrow_and_update();

        req_tx.send("not json".to_string()).await?;
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(7)),
            method: Method::Initialize,
        };
        req_tx.send(serde_json::to_string(&req)?).await?;

        // First response is the parse-error reply…
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("no parse-error response"))?;
        let text = match first {
            Message::Text(t) => t.to_string(),
            other => anyhow::bail!("unexpected message: {other:?}"),
        };
        let resp: RpcResponse = codec::decode_response(&text)?;
        assert_eq!(
            resp.error.map(|e| e.code),
            Some(harness_protocol::methods::PARSE_ERROR)
        );

        // …and the worker still answers the next valid request.
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("worker stopped after parse error"))?;
        let text = match second {
            Message::Text(t) => t.to_string(),
            other => anyhow::bail!("unexpected message: {other:?}"),
        };
        let resp: RpcResponse = codec::decode_response(&text)?;
        assert!(resp.error.is_none());
        assert_eq!(resp.id, Some(serde_json::json!(7)));

        Ok(())
    }
}
