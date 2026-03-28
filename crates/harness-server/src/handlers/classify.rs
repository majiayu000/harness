use harness_protocol::{methods::RpcResponse, methods::INTERNAL_ERROR};

pub async fn task_classify(
    id: Option<serde_json::Value>,
    prompt: String,
    issue: Option<u64>,
    pr: Option<u64>,
) -> RpcResponse {
    let classification = crate::complexity_router::classify(&prompt, issue, pr);
    match serde_json::to_value(&classification) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
