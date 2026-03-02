use crate::server::HarnessServer;
use axum::{
    routing::post,
    Json, Router,
};
use harness_protocol::{RpcRequest, RpcResponse};
use std::net::SocketAddr;
use std::sync::Arc;

#[allow(dead_code)]
struct AppState {
    server: Arc<HarnessServer>,
}

pub async fn serve(_server: &HarnessServer, addr: SocketAddr) -> anyhow::Result<()> {
    // We need to create a new server reference for the router
    // In production, the server would be Arc-wrapped from the start
    tracing::info!("harness: HTTP server listening on {addr}");

    let app = Router::new()
        .route("/rpc", post(handle_rpc));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(Json(req): Json<RpcRequest>) -> Json<RpcResponse> {
    // Placeholder: in production, this would use shared state
    Json(RpcResponse::error(
        req.id,
        harness_protocol::INTERNAL_ERROR,
        "HTTP transport not fully wired yet",
    ))
}
