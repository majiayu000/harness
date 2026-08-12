//! Runtime-owned eval primitives.
//!
//! This module is additive groundwork for GH-1447. Eval execution will dispatch
//! through the normal workflow runtime; this module only owns manifest parsing
//! deterministic scoring primitives, and standard-path eval dispatch helpers.

pub mod attestation;
mod data;
pub mod evidence;
pub mod historical_replay;
pub mod manifest;
pub mod model;
pub mod report;
pub mod run;
#[cfg(test)]
#[path = "run_concurrency_tests.rs"]
mod run_concurrency_tests;
pub mod scoring;
mod transition_outcome;

pub use attestation::{
    classify_eval_run_attestation, eval_run_attestation_payload_digest,
    verify_eval_run_attestation, EvalAttestationDecision, EvalAttestationSummary,
    EvalAttestationTrust, EvalAttestationVerificationError, EvalRunAttestation,
    EvalRunAttestationClaims, EvalRunAttestationExpected, KeylessOidcProvider,
    KeylessOidcVerification, VerifiedEvalRunAttestation, EVAL_RUN_ATTESTATION_SCHEMA_VERSION,
};
pub use evidence::{
    collect_eval_case_evidence, collect_eval_case_evidence_from_records, EvalCaseEvidence,
    EvalEvidenceStatus, EvalIsolationEvidence, EvalQualityGateEvidence, EvalSubmissionEvidence,
};
pub use historical_replay::{
    historical_replay_command_digest, parse_historical_replay_cohort_str,
    validate_historical_replay_cohort, HistoricalReplayCase, HistoricalReplayCohort,
    HistoricalReplayCohortVerdict, HistoricalReplayCommandEvidence, HistoricalReplayCommandRun,
    HistoricalReplayComparison, HistoricalReplayError, HistoricalReplayIssueSnapshot,
    HistoricalReplayPullRequestSnapshot, HistoricalReplayVerification,
    HISTORICAL_REPLAY_COHORT_SCHEMA,
};
pub use manifest::{
    parse_benchmark_manifest_str, EvalBenchmarkCase, EvalBenchmarkManifest, EvalCaseRisk,
    EvalCaseVerdict, EvalCommitResolution, EvalIsolationLifecycle, EvalIsolationProfile,
    ManifestError, DEFAULT_CASE_TIMEOUT_SECS, DEFAULT_EVAL_ISOLATION_BACKEND,
    DEFAULT_EVAL_ISOLATION_IMAGE, DEFAULT_EVAL_ISOLATION_RUNTIME_PROFILE,
    DEFAULT_EVAL_ISOLATION_SANDBOX,
};
pub use report::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence,
    EvalCaseInfrastructureStatus, EvalCaseTransition, EvalCaseTransitionCounts,
    EvalCaseTransitionKind, EvalReportCase, EvalReportCaseStatus, EvalReportError,
    EvalReportFailedGate, EvalReportMetricDelta, EvalReportMetrics, EvalRunReport,
    EvalRunReportDiff,
};
pub use run::{
    cleanup_cancelled_eval_run, dispatch_eval_case_workflow, enqueue_eval_case_workflow,
    eval_isolated_runtime_profile, EvalCaseDispatchOutcome, EvalCaseEnqueueOutcome,
    EvalCaseWorkflowInput, EvalCaseWorkflowPlan, EvalRunCleanupInput, EvalRunCleanupSummary,
    EVAL_BRANCH_PREFIX, EVAL_PR_DRAFT_MODE,
};
pub use scoring::{score_pr_repair_eval, ScoringError};
