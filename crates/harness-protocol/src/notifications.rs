use harness_core::{Item, ThreadId, ThreadStatus, TokenUsage, TurnId, TurnStatus};
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
    ItemStarted {
        turn_id: TurnId,
        item: Item,
    },
    #[serde(rename = "item/completed")]
    ItemCompleted {
        turn_id: TurnId,
        item: Item,
    },
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
