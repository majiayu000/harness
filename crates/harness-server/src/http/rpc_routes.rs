use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use harness_protocol::methods::RpcRequest;
use std::sync::Arc;

use super::rest_contract::ContractJson as Json;
use super::state::AppState;
use crate::router;

pub(crate) async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RpcRequest>,
) -> Response {
    match router::handle_request(&state, req).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
