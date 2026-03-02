use crate::{RpcRequest, RpcResponse, RpcNotification};
use serde_json;

/// Encode a response to a JSON line (for stdio transport).
pub fn encode_response(resp: &RpcResponse) -> Result<String, serde_json::Error> {
    serde_json::to_string(resp)
}

/// Encode a notification to a JSON line.
pub fn encode_notification(notif: &RpcNotification) -> Result<String, serde_json::Error> {
    serde_json::to_string(notif)
}

/// Decode a request from a JSON line.
pub fn decode_request(line: &str) -> Result<RpcRequest, serde_json::Error> {
    serde_json::from_str(line)
}

/// Decode a response from a JSON line.
pub fn decode_response(line: &str) -> Result<RpcResponse, serde_json::Error> {
    serde_json::from_str(line)
}
