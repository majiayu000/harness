use async_trait::async_trait;
use axum::{extract::State, http::StatusCode, Json};
use dashmap::DashMap;
use serde::Deserialize;
use std::sync::Arc;

use super::{IncomingIssue, IntakeSource, TaskCompletionResult};
use crate::http::AppState;
use crate::task_runner::{TaskId, TaskStatus};

/// Feishu (飞书) Bot intake — webhook-driven task creation from chat messages.
pub struct FeishuIntake {
    pub(crate) config: harness_core::FeishuIntakeConfig,
    http: reqwest::Client,
    /// message_id → task_id, for deduplication and on_task_complete lookup.
    dispatched: DashMap<String, TaskId>,
    /// message_id → chat_id, populated by the webhook handler before mark_dispatched.
    chat_ids: DashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    tenant_access_token: String,
}

impl FeishuIntake {
    pub fn new(config: harness_core::FeishuIntakeConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            dispatched: DashMap::new(),
            chat_ids: DashMap::new(),
        }
    }

    /// Store the chat_id for a message so mark_dispatched can reply.
    /// Must be called before mark_dispatched for the same message_id.
    pub fn store_chat_id(&self, message_id: &str, chat_id: &str) {
        self.chat_ids
            .insert(message_id.to_string(), chat_id.to_string());
    }

    fn app_id(&self) -> Option<String> {
        self.config
            .app_id
            .clone()
            .or_else(|| std::env::var("FEISHU_APP_ID").ok())
    }

    fn app_secret(&self) -> Option<String> {
        self.config
            .app_secret
            .clone()
            .or_else(|| std::env::var("FEISHU_APP_SECRET").ok())
    }

    async fn get_tenant_access_token(&self) -> anyhow::Result<String> {
        let app_id = self
            .app_id()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_id not configured"))?;
        let app_secret = self
            .app_secret()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_secret not configured"))?;

        let resp = self
            .http
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&serde_json::json!({
                "app_id": app_id,
                "app_secret": app_secret,
            }))
            .send()
            .await?;

        let body: TokenResponse = resp.json().await?;
        Ok(body.tenant_access_token)
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let content = serde_json::to_string(&serde_json::json!({ "text": text }))?;

        let resp = self
            .http
            .post("https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id")
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Feishu send_message failed: {status}: {body}");
        }
        Ok(())
    }
}

#[async_trait]
impl IntakeSource for FeishuIntake {
    fn name(&self) -> &str {
        "feishu"
    }

    /// Feishu intake is webhook-driven; poll returns empty.
    async fn poll(&self) -> anyhow::Result<Vec<IncomingIssue>> {
        Ok(vec![])
    }

    async fn mark_dispatched(&self, external_id: &str, task_id: &TaskId) -> anyhow::Result<()> {
        self.dispatched
            .insert(external_id.to_string(), task_id.clone());

        if let Some(chat_id) = self.chat_ids.get(external_id) {
            let text = format!("Harness task `{}` created. Working on it...", task_id.0);
            if let Err(e) = self.send_message(&chat_id, &text).await {
                tracing::warn!(
                    message_id = %external_id,
                    "feishu: failed to send task-created reply: {e}"
                );
            }
        }

        Ok(())
    }

    async fn on_task_complete(
        &self,
        external_id: &str,
        result: &TaskCompletionResult,
    ) -> anyhow::Result<()> {
        let text = match result.status {
            TaskStatus::Done => match &result.pr_url {
                Some(pr_url) => format!("Task complete. PR: {pr_url}\n\n{}", result.summary),
                None => format!("Task complete.\n\n{}", result.summary),
            },
            TaskStatus::Failed => format!(
                "Task failed: {}",
                result.error.as_deref().unwrap_or("unknown error")
            ),
            _ => return Ok(()),
        };

        if let Some(chat_id) = self.chat_ids.get(external_id) {
            if let Err(e) = self.send_message(&chat_id, &text).await {
                tracing::warn!(
                    message_id = %external_id,
                    "feishu: failed to send task-complete reply: {e}"
                );
            }
        }

        if matches!(result.status, TaskStatus::Done | TaskStatus::Failed) {
            self.dispatched.remove(external_id);
            self.chat_ids.remove(external_id);
        }

        Ok(())
    }
}

/// POST /webhook/feishu
///
/// Handles Feishu event subscription webhooks:
/// - Returns `{"challenge": ...}` for verification handshake.
/// - For `im.message.receive_v1` events containing the trigger keyword,
///   creates a Harness task and replies in the originating chat.
/// Verify the token field in a Feishu webhook payload against the configured verification_token.
/// Returns true if no verification_token is configured (open mode) or if the token matches.
fn verify_feishu_token(
    config: &harness_core::FeishuIntakeConfig,
    payload: &serde_json::Value,
) -> bool {
    let Some(expected) = &config.verification_token else {
        return true;
    };
    // Challenge payloads carry "token" at root; event payloads carry "header.token".
    let token = payload["token"]
        .as_str()
        .or_else(|| payload["header"]["token"].as_str())
        .unwrap_or("");
    token == expected.as_str()
}

pub async fn feishu_webhook(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(feishu) = state.feishu_intake.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Feishu intake not configured"})),
        );
    };

    // 1. Verify token before processing any payload.
    if !verify_feishu_token(&feishu.config, &payload) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid verification token"})),
        );
    }

    // 2. Handle Feishu verification challenge.
    if let Some(challenge) = payload.get("challenge") {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "challenge": challenge })),
        );
    }

    // 3. Only handle im.message.receive_v1 events.
    let event_type = payload["header"]["event_type"].as_str();
    if event_type != Some("im.message.receive_v1") {
        return (StatusCode::OK, Json(serde_json::json!({"ok": true})));
    }

    // 4. Extract message fields.
    let message = &payload["event"]["message"];
    let message_id = match message["message_id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing message_id"})),
            )
        }
    };
    let chat_id = match message["chat_id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing chat_id"})),
            )
        }
    };

    // 5. Parse message content text.
    let text_owned: String = message["content"]
        .as_str()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["text"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let text = text_owned.as_str();

    // 6. Check trigger keyword.
    if !text.contains(feishu.config.trigger_keyword.as_str()) {
        return (StatusCode::OK, Json(serde_json::json!({"ok": true})));
    }

    // 7. Extract description (text after keyword).
    let description = text
        .split_once(feishu.config.trigger_keyword.as_str())
        .map(|x| x.1)
        .unwrap_or("")
        .trim()
        .to_string();

    if description.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({"ok": true})));
    }

    // 8. Build IncomingIssue.
    let short_id: String = message_id.chars().take(8).collect();
    let issue = IncomingIssue {
        source: "feishu".to_string(),
        external_id: message_id.clone(),
        identifier: format!("feishu-{short_id}"),
        title: description
            .lines()
            .next()
            .unwrap_or("Feishu task")
            .to_string(),
        description: Some(description.clone()),
        repo: feishu.config.default_repo.clone(),
        url: None,
        priority: None,
        labels: vec!["feishu".to_string()],
        created_at: None,
    };

    // 9. Store chat_id before dispatching so mark_dispatched can reply.
    feishu.store_chat_id(&message_id, &chat_id);

    // 10. Dispatch task.
    let prompt = super::build_prompt_from_issue(&issue);
    let req = crate::task_runner::CreateTaskRequest {
        prompt: Some(prompt),
        project: Some(state.project_root.clone()),
        ..Default::default()
    };

    let task_id = match crate::http::task_routes::enqueue_task(&state, req).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("feishu: failed to enqueue task: {e:?}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to enqueue task"})),
            );
        }
    };

    // 11. Reply in chat asynchronously (don't block the webhook response).
    let feishu_arc = feishu.clone();
    let message_id_reply = message_id.clone();
    let task_id_reply = task_id.clone();
    tokio::spawn(async move {
        if let Err(e) = feishu_arc
            .mark_dispatched(&message_id_reply, &task_id_reply)
            .await
        {
            tracing::warn!("feishu: mark_dispatched failed: {e}");
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_feishu_config() -> harness_core::FeishuIntakeConfig {
        harness_core::FeishuIntakeConfig {
            enabled: true,
            app_id: None,
            app_secret: None,
            verification_token: None,
            trigger_keyword: "harness".to_string(),
            default_repo: None,
        }
    }

    #[test]
    fn verify_token_passes_when_no_token_configured() {
        let config = make_feishu_config();
        let payload = serde_json::json!({"challenge": "test"});
        assert!(verify_feishu_token(&config, &payload));
    }

    #[test]
    fn verify_token_passes_with_matching_root_token() {
        let mut config = make_feishu_config();
        config.verification_token = Some("secret-123".to_string());
        let payload = serde_json::json!({"challenge": "test", "token": "secret-123"});
        assert!(verify_feishu_token(&config, &payload));
    }

    #[test]
    fn verify_token_passes_with_matching_header_token() {
        let mut config = make_feishu_config();
        config.verification_token = Some("secret-123".to_string());
        let payload = serde_json::json!({
            "header": { "token": "secret-123", "event_type": "im.message.receive_v1" }
        });
        assert!(verify_feishu_token(&config, &payload));
    }

    #[test]
    fn verify_token_rejects_wrong_token() {
        let mut config = make_feishu_config();
        config.verification_token = Some("secret-123".to_string());
        let payload = serde_json::json!({"challenge": "test", "token": "wrong"});
        assert!(!verify_feishu_token(&config, &payload));
    }

    #[test]
    fn verify_token_rejects_missing_token() {
        let mut config = make_feishu_config();
        config.verification_token = Some("secret-123".to_string());
        let payload = serde_json::json!({"challenge": "test"});
        assert!(!verify_feishu_token(&config, &payload));
    }

    #[test]
    fn feishu_intake_name_is_feishu() {
        let intake = FeishuIntake::new(make_feishu_config());
        assert_eq!(intake.name(), "feishu");
    }

    #[tokio::test]
    async fn poll_returns_empty() {
        let intake = FeishuIntake::new(make_feishu_config());
        let issues = intake.poll().await.unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn store_chat_id_stores_mapping() {
        let intake = FeishuIntake::new(make_feishu_config());
        intake.store_chat_id("msg-001", "chat-abc");
        assert_eq!(
            intake.chat_ids.get("msg-001").as_deref().cloned(),
            Some("chat-abc".to_string())
        );
    }

    /// Build a minimal im.message.receive_v1 payload.
    fn make_message_payload(text: &str, message_id: &str, chat_id: &str) -> serde_json::Value {
        let content = serde_json::to_string(&serde_json::json!({ "text": text })).unwrap();
        serde_json::json!({
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "message": {
                    "message_id": message_id,
                    "chat_id": chat_id,
                    "message_type": "text",
                    "content": content
                }
            }
        })
    }

    #[test]
    fn verification_challenge_payload_detected() {
        let payload = serde_json::json!({ "challenge": "test-token-123" });
        assert!(payload.get("challenge").is_some());
        assert_eq!(payload["challenge"].as_str().unwrap(), "test-token-123");
    }

    #[test]
    fn message_payload_parsed_correctly() {
        let payload = make_message_payload("harness fix login bug", "msg-001", "oc_chat");
        let event_type = payload["header"]["event_type"].as_str();
        assert_eq!(event_type, Some("im.message.receive_v1"));

        let message = &payload["event"]["message"];
        assert_eq!(message["message_id"].as_str(), Some("msg-001"));
        assert_eq!(message["chat_id"].as_str(), Some("oc_chat"));

        let content: serde_json::Value =
            serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        let text = content["text"].as_str().unwrap();
        assert_eq!(text, "harness fix login bug");
    }

    #[test]
    fn trigger_keyword_detection() {
        let config = make_feishu_config();
        let keyword = &config.trigger_keyword;

        assert!("harness fix login bug".contains(keyword.as_str()));
        assert!("please harness fix this".contains(keyword.as_str()));
        assert!(!"unrelated message".contains(keyword.as_str()));
    }

    #[test]
    fn description_extraction_after_keyword() {
        let text = "harness fix the login bug\nExtra context here";
        let keyword = "harness";
        let description = text
            .split_once(keyword)
            .map(|x| x.1)
            .unwrap_or("")
            .trim()
            .to_string();
        assert_eq!(description, "fix the login bug\nExtra context here");
    }

    #[test]
    fn description_extraction_empty_after_keyword() {
        let text = "harness";
        let keyword = "harness";
        let description = text
            .split_once(keyword)
            .map(|x| x.1)
            .unwrap_or("")
            .trim()
            .to_string();
        assert!(description.is_empty());
    }

    #[test]
    fn non_message_event_type_is_ignored() {
        let payload = serde_json::json!({
            "header": { "event_type": "im.chat.member.bot.added_v1" }
        });
        let event_type = payload["header"]["event_type"].as_str();
        assert_ne!(event_type, Some("im.message.receive_v1"));
    }

    #[test]
    fn incoming_issue_built_from_description() {
        let description = "fix login bug\nUsers cannot log in after password reset.";
        let message_id = "om_abc12345xyz";
        let short_id: String = message_id.chars().take(8).collect();
        let issue = IncomingIssue {
            source: "feishu".to_string(),
            external_id: message_id.to_string(),
            identifier: format!("feishu-{short_id}"),
            title: description
                .lines()
                .next()
                .unwrap_or("Feishu task")
                .to_string(),
            description: Some(description.to_string()),
            repo: None,
            url: None,
            priority: None,
            labels: vec!["feishu".to_string()],
            created_at: None,
        };

        assert_eq!(issue.source, "feishu");
        assert_eq!(issue.external_id, message_id);
        assert_eq!(issue.identifier, "feishu-om_abc12");
        assert_eq!(issue.title, "fix login bug");
        assert_eq!(
            issue.description.as_deref(),
            Some("fix login bug\nUsers cannot log in after password reset.")
        );
        assert_eq!(issue.labels, vec!["feishu"]);
    }
}
