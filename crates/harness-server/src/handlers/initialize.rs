use harness_protocol::methods::RpcResponse;

/// Handle the `initialized` notification sent after `initialize`.
///
/// JSON-RPC notifications do not receive a response. The transport returns
/// `204 No Content` when the request has no id.
pub async fn initialized() {}

pub async fn initialize(id: Option<serde_json::Value>) -> RpcResponse {
    RpcResponse::success(
        id,
        serde_json::json!({
            "name": "harness",
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": {
                "gc": true,
                "skills": true,
                "rules": true,
                "exec_plan": true,
                "observe": true,
                "notifications": true,
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_does_not_advertise_removed_thread_capability() {
        let response = initialize(Some(serde_json::json!(1))).await;
        let result = response.result.expect("initialize should return a result");
        let capabilities = result["capabilities"]
            .as_object()
            .expect("capabilities should be an object");

        assert!(!capabilities.contains_key("threads"));
        assert_eq!(
            capabilities.get("notifications"),
            Some(&serde_json::json!(true))
        );
    }
}
