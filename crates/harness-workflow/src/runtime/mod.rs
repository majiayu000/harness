//! Workflow/runtime decoupling primitives.
//!
//! This module is intentionally independent from the existing task runner.
//! It models the durable contract between workflow definitions, workflow
//! instances, accepted commands, runtime jobs, and structured activity results.

pub mod bus;
mod candidate_fanout;
mod candidate_promotion;
mod candidate_selection;
mod candidate_terminal;
pub mod dispatcher;
pub mod errors;
pub mod eval;
mod job_claim;
pub mod lease_state;
mod memory_extract;
mod memory_retrieval;
pub mod model;
mod otel_trace_context;
pub mod plan_issue;
pub mod pr_feedback;
pub mod prompt_task;
pub mod quality_gate;
pub mod reducer;
pub mod remote_facts;
pub mod repo_memory;
pub mod state_registry;
pub mod status;
pub mod store;
mod store_migrations;
mod store_summary;
pub mod submission;
pub mod terminal_state;
pub mod tier_resolution;
pub mod validator;
pub mod worker;

#[cfg(test)]
mod circuit_breaker_store_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tier_resolution_dispatch_tests;

pub use bus::InMemoryWorkflowBus;
pub use candidate_fanout::{
    candidate_fanout_from_policy, candidate_fanout_from_value, CandidateFanoutRequest,
    CANDIDATE_FANOUT_BUDGET_STRATEGY, CANDIDATE_FANOUT_MAX_COUNT, CANDIDATE_FANOUT_MIN_COUNT,
};
pub use candidate_promotion::{
    candidate_promotion_plan, CandidatePromotionFailure, CandidatePromotionPlan,
    CandidatePromotionPlanError, CandidatePromotionTarget, CANDIDATE_BRANCH_ARTIFACT,
    CANDIDATE_CLEANUP_ACTIVITY, CANDIDATE_PROMOTION_ACTIVITY,
};
pub use candidate_selection::{
    select_candidate, CandidateCheckConclusion, CandidateDiffScope, CandidateEvidence,
    CandidateOutcome, CandidatePromotionRecord, CandidateRankingRecord, CandidateSelectionInput,
    CandidateSelectionRecord, CANDIDATE_SELECTION_RECORD_TYPE, CANDIDATE_SELECTION_SCHEMA,
};
pub use dispatcher::{CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeProfileSelector};
pub use errors::RuntimeJobNotFoundError;
pub use eval::{
    collect_eval_case_evidence, collect_eval_case_evidence_from_records, diff_eval_run_reports,
    dispatch_eval_case_workflow, enqueue_eval_case_workflow, eval_report_dry_run,
    eval_report_from_evidence, parse_benchmark_manifest_str, score_pr_repair_eval,
    EvalBenchmarkCase, EvalBenchmarkManifest, EvalCaseDispatchOutcome, EvalCaseEnqueueOutcome,
    EvalCaseEvidence, EvalCaseTransition, EvalCaseTransitionKind, EvalCaseWorkflowInput,
    EvalCaseWorkflowPlan, EvalEvidenceStatus, EvalQualityGateEvidence, EvalReportCase,
    EvalReportCaseStatus, EvalReportError, EvalReportMetricDelta, EvalReportMetrics, EvalRunReport,
    EvalRunReportDiff, EvalSubmissionEvidence, ManifestError, ScoringError,
    DEFAULT_CASE_TIMEOUT_SECS, EVAL_BRANCH_PREFIX, EVAL_PR_DRAFT_MODE,
};
pub use lease_state::{runtime_job_running_lease_state_at, RuntimeJobRunningLeaseState};
pub use memory_retrieval::{
    RepoMemoryRetrievalOptions, RetrievedRepoMemoryRecord, DEFAULT_REPO_MEMORY_RETRIEVAL_LIMIT,
    DEFAULT_REPO_MEMORY_TOKEN_BUDGET,
};
pub use model::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, ActivityStatus,
    RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord,
    WorkflowCommand, WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvent, WorkflowEvidence, WorkflowInstance,
    WorkflowLease, WorkflowSubject,
};
pub use otel_trace_context::WorkflowOtelTraceContext;
pub use plan_issue::{
    build_plan_issue_decision, PlanIssueDecisionInput, PlanIssueDecisionOutput,
    PlanIssueWorkflowAction, ISSUE_PLAN_ACTIVITY, ISSUE_PLAN_ARTIFACT, ISSUE_PLAN_READY_SIGNAL,
};
pub use pr_feedback::{
    build_local_review_completed_decision, build_local_review_request_decision,
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_inspect_decision,
    build_pr_feedback_sweep_decision, build_pr_hygiene_repair_decision, LocalReviewCompletedInput,
    LocalReviewDecisionInput, LocalReviewOutcome, PrDetectedDecisionInput, PrFeedbackDecisionInput,
    PrFeedbackDecisionOutput, PrFeedbackInspectDecisionInput, PrFeedbackOutcome,
    PrFeedbackSweepDecisionInput, PrFeedbackWorkflowAction, PrHygieneRepairDecisionInput,
    LOCAL_REVIEW_ACTIVITY, LOCAL_REVIEW_BLOCKED_SIGNAL, LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
    LOCAL_REVIEW_PASSED_SIGNAL, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
    PR_FEEDBACK_SNAPSHOT_ARTIFACT, PR_REPAIR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ARTIFACT,
};
pub use prompt_task::{
    build_prompt_submission_decision, PromptSubmissionDecisionInput,
    PromptSubmissionDecisionOutput, PromptTaskWorkflowAction, PROMPT_TASK_DEFINITION_ID,
    PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
pub use quality_gate::{
    build_quality_gate_run_decision, quality_gate_workflow_id, QualityGateDecisionInput,
    QualityGateDecisionOutput, QualityGateWorkflowAction, QUALITY_BLOCKED_SIGNAL,
    QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
    QUALITY_PASSED_SIGNAL,
};
pub use reducer::{
    activity_result_has_closed_issue_evidence, activity_result_value_has_closed_issue_evidence,
    reduce_runtime_job_completed, value_has_closed_issue_evidence, GITHUB_ISSUE_PR_DEFINITION_ID,
    ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL, ISSUE_STATE_ARTIFACT,
    RUNTIME_JOB_COMPLETED_EVENT, SCOPE_TOO_LARGE_SIGNAL,
};
pub use remote_facts::{
    remote_fact_command_dedupe_key, stable_remote_fact_hash, RemoteFactSnapshot,
};
pub use repo_memory::{
    RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord, REPO_MEMORY_CONFIG_ARTIFACT,
    REPO_MEMORY_DEGRADATION_ARTIFACT,
};
pub use state_registry::{
    known_workflow_definition_ids, workflow_state_definition, workflow_state_exists,
    workflow_states_for_definition, workflow_terminal_state_names_for_definition,
    WorkflowStateDefinition, WorkflowStateKey,
};
pub use status::WorkflowCommandStatus;
pub use store::{
    RuntimeHistoryPruneSummary, RuntimeUsageMetrics, RuntimeUsageRecord, RuntimeUsageUpsert,
    RuntimeUsageUpsertOutcome, WorkflowDecisionTransition, WorkflowRejectedDecisionTransition,
    WorkflowRuntimeStore, WorkflowSubmissionDecisionCommit, WorkflowSubmissionDecisionTransition,
    WorkflowSubmissionPromptPayload,
};
pub use submission::{
    build_issue_submission_decision, IssueSubmissionDecisionInput, IssueSubmissionDecisionOutput,
    IssueSubmissionWorkflowAction, SubmissionMode,
};
pub use terminal_state::{workflow_terminal_state, WorkflowTerminalState};
pub use tier_resolution::{resolve_isolation_tier, IsolationTaskMetadata, IsolationTierResolution};
pub use validator::{
    DecisionValidator, TransitionAllowlist, TransitionRule, ValidationContext,
    WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
pub use worker::{
    RuntimeJobClaimDecision, RuntimeJobClaimGuard, RuntimeJobExecutor, RuntimeWorker,
};
