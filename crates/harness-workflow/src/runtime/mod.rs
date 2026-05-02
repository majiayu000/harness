//! Workflow/runtime decoupling primitives.
//!
//! This module is intentionally independent from the existing task runner.
//! It models the durable contract between workflow definitions, workflow
//! instances, accepted commands, runtime jobs, and structured activity results.

pub mod bus;
pub mod dispatcher;
pub mod model;
pub mod plan_issue;
pub mod pr_feedback;
pub mod quality_gate;
pub mod reducer;
pub mod repo_backlog;
pub mod store;
pub mod submission;
pub mod validator;
pub mod worker;

#[cfg(test)]
mod tests;

pub use bus::InMemoryWorkflowBus;
pub use dispatcher::{CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeProfileSelector};
pub use model::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, ActivityStatus,
    RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord,
    WorkflowCommand, WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvent, WorkflowEvidence, WorkflowInstance,
    WorkflowLease, WorkflowSubject,
};
pub use plan_issue::{
    build_plan_issue_decision, PlanIssueDecisionInput, PlanIssueDecisionOutput,
    PlanIssueWorkflowAction,
};
pub use pr_feedback::{
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_sweep_decision,
    PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackDecisionOutput, PrFeedbackOutcome,
    PrFeedbackSweepDecisionInput, PrFeedbackWorkflowAction,
};
pub use quality_gate::{
    build_quality_gate_run_decision, quality_gate_workflow_id, QualityGateDecisionInput,
    QualityGateDecisionOutput, QualityGateWorkflowAction, QUALITY_BLOCKED_SIGNAL,
    QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
    QUALITY_PASSED_SIGNAL,
};
pub use reducer::{
    reduce_runtime_job_completed, GITHUB_ISSUE_PR_DEFINITION_ID, RUNTIME_JOB_COMPLETED_EVENT,
};
pub use repo_backlog::{
    build_merged_pr_decision, build_open_issue_without_workflow_decision,
    build_stale_active_workflow_decision, repo_backlog_workflow_id, MergedPrDecisionInput,
    OpenIssueDecisionInput, RepoBacklogDecisionOutput, RepoBacklogWorkflowAction,
    StaleWorkflowDecisionInput, REPO_BACKLOG_DEFINITION_ID,
};
pub use store::WorkflowRuntimeStore;
pub use submission::{
    build_issue_submission_decision, IssueSubmissionDecisionInput, IssueSubmissionDecisionOutput,
    IssueSubmissionWorkflowAction,
};
pub use validator::{
    DecisionValidator, TransitionAllowlist, TransitionRule, ValidationContext,
    WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
pub use worker::{RuntimeJobExecutor, RuntimeWorker};
