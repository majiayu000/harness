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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Method, RpcRequest, RpcResponse};
    use std::path::PathBuf;

    #[test]
    fn rpc_response_success_omits_error_field() -> anyhow::Result<()> {
        let resp = RpcResponse::success(Some(serde_json::json!(1)), serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&resp)?;
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
        let back: RpcResponse = serde_json::from_str(&json)?;
        assert_eq!(back.jsonrpc, "2.0");
        assert!(back.result.is_some());
        assert!(back.error.is_none());
        Ok(())
    }

    #[test]
    fn rpc_response_error_omits_result_field() -> anyhow::Result<()> {
        let resp = RpcResponse::error(Some(serde_json::json!(2)), -32600, "Invalid Request");
        let json = serde_json::to_string(&resp)?;
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
        let back: RpcResponse = serde_json::from_str(&json)?;
        let err = back.error.ok_or_else(|| anyhow::anyhow!("error field missing"))?;
        assert_eq!(err.code, -32600);
        Ok(())
    }

    #[test]
    fn codec_response_roundtrip() -> anyhow::Result<()> {
        let resp = RpcResponse::success(Some(serde_json::json!(42)), serde_json::json!("done"));
        let encoded = encode_response(&resp)?;
        let decoded = decode_response(&encoded)?;
        assert_eq!(decoded.id, resp.id);
        assert_eq!(decoded.result, resp.result);
        Ok(())
    }

    #[test]
    fn codec_request_roundtrip() -> anyhow::Result<()> {
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart { cwd: PathBuf::from("/tmp") },
        };
        let json = serde_json::to_string(&req)?;
        let back: RpcRequest = decode_request(&json)?;
        assert_eq!(back.jsonrpc, "2.0");
        Ok(())
    }

    #[test]
    fn rpc_error_codes_are_negative() {
        assert!(crate::PARSE_ERROR < 0);
        assert!(crate::INVALID_REQUEST < 0);
        assert!(crate::METHOD_NOT_FOUND < 0);
        assert!(crate::INVALID_PARAMS < 0);
        assert!(crate::INTERNAL_ERROR < 0);
    }
}
