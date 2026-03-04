use crate::{AgentRequest, AgentResponse, Decision};
use async_trait::async_trait;

/// Result returned by a TurnInterceptor pre_execute call.
pub struct InterceptResult {
    /// Pass: allow execution, Block: abort with reason.
    pub decision: Decision,
    /// Human-readable explanation for the decision.
    pub reason: Option<String>,
    /// Optionally replace the agent request with a modified version.
    pub request: Option<AgentRequest>,
}

impl InterceptResult {
    pub fn pass() -> Self {
        Self { decision: Decision::Pass, reason: None, request: None }
    }

    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            decision: Decision::Block,
            reason: Some(reason.into()),
            request: None,
        }
    }
}

/// Hook interface for intercepting agent task execution.
///
/// Interceptors run before and after each agent.execute() call.
/// Multiple interceptors are composed in registration order.
#[async_trait]
pub trait TurnInterceptor: Send + Sync {
    fn name(&self) -> &str;

    /// Called before agent execution. Return Block to abort the task.
    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult;

    /// Called after successful agent execution.
    async fn post_execute(&self, _req: &AgentRequest, _resp: &AgentResponse) {}

    /// Called when agent execution fails with an error.
    async fn on_error(&self, _req: &AgentRequest, _error: &str) {}
}
