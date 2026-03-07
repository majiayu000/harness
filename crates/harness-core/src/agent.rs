use crate::types::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Core trait for all code agents (Claude Code, Codex, Anthropic API, etc.)
#[async_trait]
pub trait CodeAgent: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Vec<Capability>;
    async fn execute(&self, req: AgentRequest) -> crate::Result<AgentResponse>;
    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> crate::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    pub prompt: String,
    pub project_root: PathBuf,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub context: Vec<ContextItem>,
}

impl Default for AgentRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            project_root: PathBuf::from("."),
            allowed_tools: Vec::new(),
            model: None,
            max_budget_usd: None,
            context: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub output: String,
    pub stderr: String,
    pub items: Vec<Item>,
    pub token_usage: TokenUsage,
    pub model: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamItem {
    ItemStarted { item: Item },
    ItemCompleted { item: Item },
    TokenUsage { usage: TokenUsage },
    Error { message: String },
    Done,
}

/// Task classification for agent dispatch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClassification {
    pub complexity: TaskComplexity,
    pub language: Option<Language>,
    pub requires_write: bool,
    pub requires_network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Simple,
    Medium,
    Complex,
    Critical,
}

// === Streaming Agent Adapter (new, coexists with CodeAgent) ===

/// Events emitted by an agent adapter during a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    ItemStarted {
        item_type: String,
    },
    MessageDelta {
        text: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ApprovalRequest {
        id: String,
        command: String,
    },
    ItemCompleted,
    TurnCompleted {
        output: String,
    },
    Error {
        message: String,
    },
}

/// Decision for an approval request from the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    Reject { reason: String },
}

/// Request to start a turn via an adapter.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub prompt: String,
    pub project_root: PathBuf,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub context: Vec<ContextItem>,
    pub timeout_secs: Option<u64>,
}

/// Streaming agent adapter — coexists with legacy CodeAgent trait.
///
/// Implementations send `AgentEvent`s through the provided mpsc channel
/// during execution. The caller consumes events to drive notifications,
/// logging, and approval gates.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    fn name(&self) -> &str;

    /// Start a turn. Send events to `tx` until complete or error.
    async fn start_turn(
        &self,
        req: TurnRequest,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> crate::Result<()>;

    /// Interrupt an in-progress turn.
    async fn interrupt(&self) -> crate::Result<()>;

    /// Append instructions to an active turn (steer).
    /// Returns `Err` with `Unsupported` if the adapter doesn't support steering.
    async fn steer(&self, _text: String) -> crate::Result<()> {
        Err(crate::Error::Unsupported("steer not supported".into()))
    }

    /// Respond to an approval request from the agent.
    /// Returns `Err` with `Unsupported` if the adapter doesn't support approval.
    async fn respond_approval(
        &self,
        _id: String,
        _decision: ApprovalDecision,
    ) -> crate::Result<()> {
        Err(crate::Error::Unsupported("approval not supported".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_serde_round_trip() {
        let events = vec![
            AgentEvent::TurnStarted,
            AgentEvent::ItemStarted {
                item_type: "message".into(),
            },
            AgentEvent::MessageDelta {
                text: "hello".into(),
            },
            AgentEvent::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
            AgentEvent::ApprovalRequest {
                id: "req-1".into(),
                command: "rm -rf /tmp/test".into(),
            },
            AgentEvent::ItemCompleted,
            AgentEvent::TurnCompleted {
                output: "done".into(),
            },
            AgentEvent::Error {
                message: "oops".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, parsed);
        }
    }

    #[test]
    fn agent_event_tagged_format() {
        let event = AgentEvent::ToolCall {
            name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "bash");
    }

    #[test]
    fn approval_decision_serde_round_trip() {
        let decisions = vec![
            ApprovalDecision::Accept,
            ApprovalDecision::Reject {
                reason: "dangerous".into(),
            },
        ];

        for decision in decisions {
            let json = serde_json::to_string(&decision).unwrap();
            let parsed: ApprovalDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(decision, parsed);
        }
    }
}
