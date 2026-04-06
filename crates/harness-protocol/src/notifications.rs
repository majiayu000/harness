use harness_core::types::{Item, ThreadId, ThreadStatus, TokenUsage, TurnId, TurnStatus};
use serde::{Deserialize, Serialize};

/// Server-to-client streaming notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Notification {
    #[serde(rename = "turn/started")]
    TurnStarted {
        thread_id: ThreadId,
        turn_id: TurnId,
    },
    #[serde(rename = "item/started")]
    ItemStarted { turn_id: TurnId, item: Item },
    #[serde(rename = "item/completed")]
    ItemCompleted { turn_id: TurnId, item: Item },
    #[serde(rename = "message/delta", alias = "message_delta")]
    MessageDelta { turn_id: TurnId, text: String },
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        turn_id: TurnId,
        status: TurnStatus,
        token_usage: TokenUsage,
    },
    #[serde(rename = "token_usage/updated")]
    TokenUsageUpdated {
        thread_id: ThreadId,
        usage: TokenUsage,
    },
    #[serde(rename = "thread/status_changed")]
    ThreadStatusChanged {
        thread_id: ThreadId,
        status: ThreadStatus,
    },
    #[serde(rename = "turn/approval_request")]
    ApprovalRequest {
        turn_id: TurnId,
        request_id: String,
        command: String,
    },
}

/// Notification envelope (no id field per JSON-RPC 2.0 spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    #[serde(flatten)]
    pub notification: Notification,
}

impl RpcNotification {
    pub fn new(notification: Notification) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            notification,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_delta_serializes_with_rpc_method_name() {
        let notif = RpcNotification::new(Notification::MessageDelta {
            turn_id: TurnId::from_str("turn-1"),
            text: "chunk".to_string(),
        });

        let value = serde_json::to_value(notif).expect("serialize notification");
        assert_eq!(value["method"], "message/delta");
        assert_eq!(value["params"]["turn_id"], "turn-1");
        assert_eq!(value["params"]["text"], "chunk");
    }

    #[test]
    fn message_delta_alias_deserializes_for_compatibility() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "message_delta",
            "params": {
                "turn_id": "turn-2",
                "text": "piece"
            }
        });

        let parsed: RpcNotification =
            serde_json::from_value(value).expect("deserialize alias notification");
        match parsed.notification {
            Notification::MessageDelta { turn_id, text } => {
                assert_eq!(turn_id.as_str(), "turn-2");
                assert_eq!(text, "piece");
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn delta_notification_order_is_preserved_before_turn_completed() {
        let notifications = vec![
            RpcNotification::new(Notification::MessageDelta {
                turn_id: TurnId::from_str("turn-3"),
                text: "part".to_string(),
            }),
            RpcNotification::new(Notification::TurnCompleted {
                turn_id: TurnId::from_str("turn-3"),
                status: TurnStatus::Completed,
                token_usage: TokenUsage::default(),
            }),
        ];

        let methods: Vec<String> = notifications
            .into_iter()
            .map(|notif| {
                serde_json::to_value(notif)
                    .expect("serialize")
                    .get("method")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(methods, vec!["message/delta", "turn/completed"]);
    }
}
