use crate::{AgentRequest, AgentResponse, Decision};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

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
        Self {
            decision: Decision::Pass,
            reason: None,
            request: None,
        }
    }

    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            decision: Decision::Block,
            reason: Some(reason.into()),
            request: None,
        }
    }

    pub fn warn(reason: impl Into<String>) -> Self {
        Self {
            decision: Decision::Warn,
            reason: Some(reason.into()),
            request: None,
        }
    }
}

/// Result returned by a TurnInterceptor post_execute call.
///
/// Interceptors that perform validation (e.g. cargo fmt, tests) return
/// `fail(reason)` to signal that the agent output should be retried.
pub struct PostExecuteResult {
    /// `None` means validation passed. `Some(msg)` means validation failed.
    pub error: Option<String>,
}

impl PostExecuteResult {
    pub fn pass() -> Self {
        Self { error: None }
    }

    pub fn fail(msg: impl Into<String>) -> Self {
        Self {
            error: Some(msg.into()),
        }
    }
}

/// Describes a single tool-use event from the agent execution pipeline.
///
/// Carries the tool name and the list of files that were created or modified
/// by the tool call, enabling file-level rule scans to be triggered on the
/// exact set of affected paths.
#[derive(Debug, Clone)]
pub struct ToolUseEvent {
    /// Name of the tool that was used, e.g. `"write_file"`, `"edit_file"`.
    pub tool_name: String,
    /// Files created or modified by this tool use.
    pub affected_files: Vec<PathBuf>,
}

/// Result returned by a [`TurnInterceptor::post_tool_use`] call.
#[derive(Debug, Clone)]
pub struct PostToolUseResult {
    /// Violation feedback to inject into the agent's next turn, if any.
    pub violation_feedback: Option<String>,
}

impl PostToolUseResult {
    pub fn clean() -> Self {
        Self {
            violation_feedback: None,
        }
    }

    pub fn with_violations(feedback: impl Into<String>) -> Self {
        Self {
            violation_feedback: Some(feedback.into()),
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
    ///
    /// Return `PostExecuteResult::fail(reason)` to signal that validation
    /// failed and the turn should be retried with error context injected.
    async fn post_execute(&self, _req: &AgentRequest, _resp: &AgentResponse) -> PostExecuteResult {
        PostExecuteResult::pass()
    }

    /// Called when agent execution fails with an error.
    async fn on_error(&self, _req: &AgentRequest, _error: &str) {}

    /// Maximum number of automatic retries on post-execution validation failure.
    /// Returns None to use the executor's default (2).
    fn max_validation_retries(&self) -> Option<u32> {
        None
    }

    /// Called before a tool-use event in the agent pipeline.
    ///
    /// Return `InterceptResult::block(reason)` to prevent the tool from executing.
    /// The default implementation is a no-op pass-through.
    async fn pre_tool_use(&self, _event: &ToolUseEvent) -> InterceptResult {
        InterceptResult::pass()
    }

    /// Called after a tool-use event (e.g. file write/edit) in the agent pipeline.
    ///
    /// Return `PostToolUseResult::with_violations(msg)` to inject rule violation
    /// details into the agent's next turn prompt.
    /// The default implementation is a no-op.
    async fn post_tool_use(
        &self,
        _event: &ToolUseEvent,
        _project_root: &Path,
    ) -> PostToolUseResult {
        PostToolUseResult::clean()
    }
}
