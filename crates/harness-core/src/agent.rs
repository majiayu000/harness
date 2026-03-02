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

