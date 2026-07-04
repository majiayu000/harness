//! Runtime-owned eval primitives.
//!
//! This module is additive groundwork for GH-1447. Eval execution will dispatch
//! through the normal workflow runtime; this module only owns manifest parsing
//! deterministic scoring primitives, and standard-path eval dispatch helpers.

pub mod evidence;
pub mod manifest;
pub mod model;
pub mod report;
pub mod run;
pub mod scoring;

pub use evidence::{
    collect_eval_case_evidence, collect_eval_case_evidence_from_records, EvalCaseEvidence,
    EvalEvidenceStatus, EvalQualityGateEvidence, EvalSubmissionEvidence,
};
pub use manifest::{
    parse_benchmark_manifest_str, EvalBenchmarkCase, EvalBenchmarkManifest, ManifestError,
    DEFAULT_CASE_TIMEOUT_SECS,
};
pub use report::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence, EvalCaseTransition,
    EvalCaseTransitionKind, EvalReportCase, EvalReportCaseStatus, EvalReportError,
    EvalReportMetricDelta, EvalReportMetrics, EvalRunReport, EvalRunReportDiff,
};
pub use run::{
    dispatch_eval_case_workflow, enqueue_eval_case_workflow, EvalCaseDispatchOutcome,
    EvalCaseEnqueueOutcome, EvalCaseWorkflowInput, EvalCaseWorkflowPlan, EVAL_BRANCH_PREFIX,
    EVAL_PR_DRAFT_MODE,
};
pub use scoring::{score_pr_repair_eval, ScoringError};
