//! Shared Feishu request builders (GH-1582 B-014).
//!
//! Single source for the tenant-token and send-message request shapes used
//! by both the intake reply path (`intake/feishu.rs`, via `reqwest`) and the
//! alerting Feishu adapter (via the mockable `AlertTransport`).

pub const TENANT_TOKEN_URL: &str =
    "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";
pub const SEND_MESSAGE_URL: &str =
    "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id";

/// Body for the tenant access token request.
pub fn tenant_token_request(app_id: &str, app_secret: &str) -> serde_json::Value {
    serde_json::json!({
        "app_id": app_id,
        "app_secret": app_secret,
    })
}

/// Extract the tenant access token from the token endpoint response.
pub fn parse_tenant_token(response: &serde_json::Value) -> anyhow::Result<String> {
    response["tenant_access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Feishu token response missing tenant_access_token"))
}

/// Body for a plain-text chat message to `receive_id` (chat id).
pub fn send_text_request(receive_id: &str, text: &str) -> anyhow::Result<serde_json::Value> {
    let content = serde_json::to_string(&serde_json::json!({ "text": text }))?;
    Ok(serde_json::json!({
        "receive_id": receive_id,
        "msg_type": "text",
        "content": content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_request_shape() {
        let body = tenant_token_request("app-1", "secret-1");
        assert_eq!(body["app_id"], "app-1");
        assert_eq!(body["app_secret"], "secret-1");
    }

    #[test]
    fn parse_tenant_token_extracts_token() {
        let response = serde_json::json!({ "tenant_access_token": "tok-1", "code": 0 });
        assert_eq!(parse_tenant_token(&response).unwrap(), "tok-1");
    }

    #[test]
    fn parse_tenant_token_errors_when_missing() {
        let response = serde_json::json!({ "code": 99991663 });
        assert!(parse_tenant_token(&response).is_err());
    }

    #[test]
    fn send_text_request_nests_content_as_string() {
        let body = send_text_request("oc_chat", "hello").unwrap();
        assert_eq!(body["receive_id"], "oc_chat");
        assert_eq!(body["msg_type"], "text");
        let content: serde_json::Value =
            serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], "hello");
    }
}
