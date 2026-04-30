//! Workflow/runtime decoupling primitives.
//!
//! This module is intentionally independent from the existing task runner.
//! It models the durable contract between workflow definitions, workflow
//! instances, accepted commands, runtime jobs, and structured activity results.

pub mod bus;
pub mod model;
pub mod plan_issue;
pub mod pr_feedback;
pub mod store;
pub mod validator;

#[cfg(test)]
mod tests;

pub use bus::InMemoryWorkflowBus;
pub use model::{
    ActivityArtifact, ActivityResult, ActivitySignal, ActivityStatus, RuntimeEvent, RuntimeJob,
    RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord, WorkflowDefinition,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowLease, WorkflowSubject,
};
pub use plan_issue::{
    build_plan_issue_decision, PlanIssueDecisionInput, PlanIssueDecisionOutput,
    PlanIssueWorkflowAction,
};
pub use pr_feedback::{
    build_pr_detected_decision, build_pr_feedback_decision, PrDetectedDecisionInput,
    PrFeedbackDecisionInput, PrFeedbackDecisionOutput, PrFeedbackOutcome, PrFeedbackWorkflowAction,
};
pub use store::WorkflowRuntimeStore;
pub use validator::{
    DecisionValidator, TransitionAllowlist, TransitionRule, ValidationContext,
    WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
