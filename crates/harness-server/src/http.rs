use crate::{router, server::HarnessServer};
use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use harness_protocol::{RpcRequest, RpcResponse};
use std::net::SocketAddr;
use std::sync::Arc;

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");

    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .with_state(server);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(
    State(server): State<Arc<HarnessServer>>,
    Json(req): Json<RpcRequest>,
) -> Json<RpcResponse> {
    Json(router::handle_request(&server, req).await)
}
