use super::model::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, ActivityStatus,
    RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
    WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowSubject,
};
use super::store::RuntimeJobEnqueueOutcome;
use super::validator::{DecisionValidator, ValidationContext, WorkflowDecisionRejectionKind};
use super::{
    build_issue_submission_decision, build_plan_issue_decision, build_pr_detected_decision,
    build_pr_feedback_decision, build_pr_feedback_sweep_decision, build_prompt_submission_decision,
    build_quality_gate_run_decision, reduce_runtime_job_completed, CandidateFanoutRequest,
    CommandDispatchOutcome, InMemoryWorkflowBus, IssueSubmissionDecisionInput,
    IssueSubmissionWorkflowAction, PlanIssueDecisionInput, PlanIssueWorkflowAction,
    PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackOutcome,
    PrFeedbackSweepDecisionInput, PrFeedbackWorkflowAction, PromptSubmissionDecisionInput,
    QualityGateDecisionInput, QualityGateWorkflowAction, RuntimeCommandDispatcher,
    RuntimeJobExecutor, RuntimeProfileSelector, RuntimeWorker, SubmissionMode,
    WorkflowCommandStatus, WorkflowDecisionTransition, WorkflowRuntimeStore,
    GITHUB_ISSUE_PR_DEFINITION_ID, LOCAL_REVIEW_ACTIVITY, PROMPT_TASK_DEFINITION_ID,
    PROMPT_TASK_IMPLEMENT_ACTIVITY, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL, SCOPE_TOO_LARGE_SIGNAL,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

include!("tests/common.rs");
include!("tests/decision_builders.rs");
include!("tests/completion_reducer_core.rs");
include!("tests/completion_reducer_quality.rs");
include!("tests/runtime_failure_classification.rs");
include!("tests/decision_validator.rs");
include!("tests/command_bus.rs");
include!("tests/worker_lifecycle.rs");
include!("tests/quality_gate_worker.rs");
include!("tests/worker_limits.rs");
include!("tests/durable_store.rs");
include!("tests/command_dispatcher.rs");
include!("tests/command_store.rs");

mod issue_planning;
mod local_review;
mod otel_spans;
mod p1_followups;
mod pr_repair_evidence;
mod replay_determinism;
mod retry;
mod runtime_store;
mod runtime_usage;
