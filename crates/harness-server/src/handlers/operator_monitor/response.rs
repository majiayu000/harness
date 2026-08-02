use super::{build_operator_monitor, OperatorMonitorPayload};
use crate::http::{rest_contract::LegacyJson as Json, AppState};
use axum::{extract::State, http::StatusCode};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OperatorMonitorResponse {
    Payload(Box<OperatorMonitorPayload>),
    Error { error: String },
}

pub(crate) async fn operator_monitor(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<OperatorMonitorResponse>) {
    match build_operator_monitor(&state).await {
        Ok(payload) => (
            StatusCode::OK,
            Json(OperatorMonitorResponse::Payload(Box::new(payload))),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OperatorMonitorResponse::Error {
                error: error.to_string(),
            }),
        ),
    }
}
