use harness_protocol::methods::{RpcResponse, INTERNAL_ERROR, INVALID_PARAMS, VALIDATION_ERROR};

pub(crate) fn invalid_params(
    id: Option<serde_json::Value>,
    message: impl Into<String>,
) -> RpcResponse {
    RpcResponse::error(id, INVALID_PARAMS, message)
}

pub(crate) fn validation(id: Option<serde_json::Value>, message: impl Into<String>) -> RpcResponse {
    RpcResponse::error(id, VALIDATION_ERROR, message)
}

pub(crate) fn internal(id: Option<serde_json::Value>, message: impl Into<String>) -> RpcResponse {
    RpcResponse::error(id, INTERNAL_ERROR, message)
}
