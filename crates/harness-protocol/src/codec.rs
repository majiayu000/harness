use crate::methods::{RpcRequest, RpcResponse};
use crate::notifications::RpcNotification;
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
    use crate::methods::{Method, RpcRequest, RpcResponse};
    use std::path::PathBuf;

    #[test]
    fn rpc_response_success_omits_error_field() -> anyhow::Result<()> {
        let resp =
            RpcResponse::success(Some(serde_json::json!(1)), serde_json::json!({"ok": true}));
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
        let err = back
            .error
            .ok_or_else(|| anyhow::anyhow!("error field missing"))?;
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
            method: Method::ThreadStart {
                cwd: PathBuf::from("/tmp"),
            },
        };
        let json = serde_json::to_string(&req)?;
        let back: RpcRequest = decode_request(&json)?;
        assert_eq!(back.jsonrpc, "2.0");
        Ok(())
    }

    #[test]
    fn codec_initialized_roundtrip() -> anyhow::Result<()> {
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Method::Initialized,
        };
        let json = serde_json::to_string(&req)?;
        // method field must be "initialized"
        assert!(
            json.contains("\"method\":\"initialized\""),
            "serialized: {json}"
        );
        let back: RpcRequest = decode_request(&json)?;
        assert!(matches!(back.method, Method::Initialized));

        // Also accept an explicit empty params object for compatibility.
        let json_with_params = r#"{"jsonrpc":"2.0","id":null,"method":"initialized","params":{}}"#;
        let back_with_params: RpcRequest = decode_request(json_with_params)?;
        assert!(matches!(back_with_params.method, Method::Initialized));

        // Also accept explicit `null` params, which is handled by `RpcRequest` deserialization.
        let json_with_null_params =
            r#"{"jsonrpc":"2.0","id":null,"method":"initialized","params":null}"#;
        let back_with_null_params: RpcRequest = decode_request(json_with_null_params)?;
        assert!(matches!(back_with_null_params.method, Method::Initialized));
        Ok(())
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn rpc_error_codes_are_negative() {
        assert!(crate::methods::PARSE_ERROR < 0);
        assert!(crate::methods::INVALID_REQUEST < 0);
        assert!(crate::methods::METHOD_NOT_FOUND < 0);
        assert!(crate::methods::INVALID_PARAMS < 0);
        assert!(crate::methods::INTERNAL_ERROR < 0);
    }

    #[test]
    fn slash_style_thread_start() -> anyhow::Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"thread/start","params":{"cwd":"/tmp"}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::ThreadStart { .. }));
        Ok(())
    }

    #[test]
    fn slash_style_turn_start() -> anyhow::Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"thread_id":"t1","input":"hello"}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::TurnStart { .. }));
        Ok(())
    }

    #[test]
    fn slash_style_gc_run() -> anyhow::Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":3,"method":"gc/run","params":{}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::GcRun { .. }));
        Ok(())
    }

    #[test]
    fn skill_stale_accepts_empty_params() -> anyhow::Result<()> {
        // JSON-RPC clients that always send params: {} must not be rejected.
        let json = r#"{"jsonrpc":"2.0","id":9,"method":"skill/stale","params":{}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::SkillStale));
        let json_null = r#"{"jsonrpc":"2.0","id":10,"method":"skill/stale","params":null}"#;
        let req2: RpcRequest = decode_request(json_null)?;
        assert!(matches!(req2.method, Method::SkillStale));
        Ok(())
    }

    #[test]
    fn slash_style_exec_plan_init() -> anyhow::Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":4,"method":"exec_plan/init","params":{"spec":"do stuff","project_root":"/tmp"}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::ExecPlanInit { .. }));
        Ok(())
    }

    #[test]
    fn snake_case_still_works() -> anyhow::Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":5,"method":"thread_start","params":{"cwd":"/tmp"}}"#;
        let req: RpcRequest = decode_request(json)?;
        assert!(matches!(req.method, Method::ThreadStart { .. }));
        Ok(())
    }

    #[test]
    fn method_name_returns_slash_style() {
        let m = Method::ThreadStart {
            cwd: PathBuf::from("/tmp"),
        };
        assert_eq!(m.method_name(), "thread/start");

        let m = Method::Initialize;
        assert_eq!(m.method_name(), "initialize");

        let m = Method::GcRun { project_id: None };
        assert_eq!(m.method_name(), "gc/run");

        let m = Method::ExecPlanInit {
            spec: String::new(),
            project_root: PathBuf::from("/tmp"),
        };
        assert_eq!(m.method_name(), "exec_plan/init");
    }

    #[test]
    fn notification_serializes_slash_style() -> anyhow::Result<()> {
        use crate::notifications::{Notification, RpcNotification};
        use harness_core::{types::ThreadId, types::TurnId};

        let notif = RpcNotification::new(Notification::TurnStarted {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
        });
        let json = serde_json::to_string(&notif)?;
        assert!(json.contains("\"method\":\"turn/started\""), "got: {json}");

        let notif = RpcNotification::new(Notification::ThreadStatusChanged {
            thread_id: ThreadId::new(),
            status: harness_core::types::ThreadStatus::Active,
        });
        let json = serde_json::to_string(&notif)?;
        assert!(
            json.contains("\"method\":\"thread/status_changed\""),
            "got: {json}"
        );
        Ok(())
    }
}
