use super::attestation::{EvalAttestationDecision, EvalAttestationTrust};
use super::evidence::{EvalCaseEvidence, EvalEvidenceStatus};
use super::manifest::EvalBenchmarkManifest;
use super::model::{EvalGrade, GateStatus, HardGateName, QualitySnapshot};
use super::verification_evidence::EvalValidationCommandEvidence;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::{error::Error, fmt};

mod outcome;
use outcome::inferred_run_outcome;
pub use outcome::{eval_report_effective_outcome, EvalReportCaseOutcome, EvalRunOutcome};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalRunReport {
    pub run_id: String,
    pub suite: String,
    pub k: u32,
    pub metrics: EvalReportMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<EvalRunOutcome>,
    pub cases: Vec<EvalReportCase>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalReportMetrics {
    pub total_cases: u64,
    pub scored_cases: u64,
    pub passed_cases: u64,
    pub failed_cases: u64,
    #[serde(default)]
    pub skipped_cases: u64,
    pub pending_cases: u64,
    pub infra_failed_cases: u64,
    pub pass_at_1: f64,
    pub pass_to_k: f64,
    pub total_tokens: u64,
    pub avg_tokens_per_scored_case: Option<f64>,
    pub total_cost_usd_micros: u64,
    pub avg_cost_usd_micros_per_scored_case: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalReportCase {
    pub case_id: String,
    pub repo: String,
    pub issue: u64,
    pub base_commit: String,
    #[serde(default)]
    pub source_commit: String,
    pub verify_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_evidence: Vec<EvalValidationCommandEvidence>,
    pub status: EvalReportCaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<EvalReportCaseOutcome>,
    pub passed: bool,
    #[serde(default)]
    pub attestation_trust: EvalAttestationTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_decision: Option<EvalAttestationDecision>,
    #[serde(default)]
    pub explicit_evidence: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_grade: Option<EvalGrade>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_hard_gates: Vec<EvalReportFailedGate>,
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub terminal_state: Option<String>,
    #[serde(default)]
    pub infrastructure_status: EvalCaseInfrastructureStatus,
    pub total_tokens: u64,
    pub cost_usd_micros: u64,
    pub missing_evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalReportFailedGate {
    pub name: HardGateName,
    pub grade_cap: Option<EvalGrade>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalReportCaseStatus {
    Pending,
    Passed,
    Failed,
    Skipped,
    InfraFailed,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseInfrastructureStatus {
    #[default]
    Unknown,
    Healthy,
    MissingEvidence,
    InfraFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalRunReportDiff {
    pub baseline_run_id: String,
    pub candidate_run_id: String,
    pub suite: String,
    pub k: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_outcome: Option<EvalRunOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_outcome: Option<EvalRunOutcome>,
    pub delta: EvalReportMetricDelta,
    #[serde(default)]
    pub transition_counts: EvalCaseTransitionCounts,
    #[serde(default)]
    pub regression_count: u64,
    #[serde(default)]
    pub regression_ids: Vec<String>,
    pub transitions: Vec<EvalCaseTransition>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalReportMetricDelta {
    pub pass_at_1_delta: f64,
    pub pass_to_k_delta: f64,
    pub total_tokens_delta: i128,
    pub total_cost_usd_micros_delta: i128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalCaseTransition {
    pub case_id: String,
    pub transition: EvalCaseTransitionKind,
    pub baseline_status: Option<EvalReportCaseStatus>,
    pub candidate_status: Option<EvalReportCaseStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_attestation_trust: Option<EvalAttestationTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_attestation_trust: Option<EvalAttestationTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_attestation_decision: Option<EvalAttestationDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_attestation_decision: Option<EvalAttestationDecision>,
    #[serde(default)]
    pub baseline_source_commit: Option<String>,
    #[serde(default)]
    pub candidate_source_commit: Option<String>,
    #[serde(default)]
    pub baseline_verify_commands: Vec<String>,
    #[serde(default)]
    pub candidate_verify_commands: Vec<String>,
    #[serde(default)]
    pub baseline_verification_evidence: Vec<EvalValidationCommandEvidence>,
    #[serde(default)]
    pub candidate_verification_evidence: Vec<EvalValidationCommandEvidence>,
    #[serde(default)]
    pub baseline_terminal_state: Option<String>,
    #[serde(default)]
    pub candidate_terminal_state: Option<String>,
    #[serde(default)]
    pub baseline_infrastructure_status: Option<EvalCaseInfrastructureStatus>,
    #[serde(default)]
    pub candidate_infrastructure_status: Option<EvalCaseInfrastructureStatus>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseTransitionKind {
    Added,
    Removed,
    UnchangedPass,
    UnchangedFail,
    UnchangedSkip,
    PassToFail,
    FailToPass,
    PassToSkip,
    SkipToPass,
    FailToSkip,
    SkipToFail,
    StatusChanged,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalCaseTransitionCounts {
    pub added: u64,
    pub removed: u64,
    pub unchanged_pass: u64,
    pub unchanged_fail: u64,
    #[serde(default)]
    pub unchanged_skip: u64,
    pub pass_to_fail: u64,
    pub fail_to_pass: u64,
    #[serde(default)]
    pub pass_to_skip: u64,
    #[serde(default)]
    pub skip_to_pass: u64,
    #[serde(default)]
    pub fail_to_skip: u64,
    #[serde(default)]
    pub skip_to_fail: u64,
    pub status_changed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalReportError {
    message: String,
}

impl EvalReportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EvalReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for EvalReportError {}

pub fn eval_report_dry_run(
    manifest: &EvalBenchmarkManifest,
    run_id: impl Into<String>,
    k: u32,
) -> Result<EvalRunReport, EvalReportError> {
    validate_k(k)?;
    let cases = manifest
        .cases
        .iter()
        .map(|case| EvalReportCase {
            case_id: case.case_id.clone(),
            repo: case.repo.clone(),
            issue: case.issue,
            base_commit: case.base_commit.clone(),
            source_commit: case.base_commit.clone(),
            verify_commands: case.verify_commands.clone(),
            verification_evidence: Vec::new(),
            status: EvalReportCaseStatus::Pending,
            outcome: None,
            passed: false,
            attestation_trust: EvalAttestationTrust::Unsigned,
            attestation_decision: None,
            explicit_evidence: false,
            final_grade: None,
            failed_hard_gates: Vec::new(),
            workflow_id: None,
            terminal_state: None,
            infrastructure_status: EvalCaseInfrastructureStatus::Unknown,
            total_tokens: 0,
            cost_usd_micros: 0,
            missing_evidence: Vec::new(),
        })
        .collect::<Vec<_>>();
    Ok(report_from_cases(manifest, run_id, k, cases))
}

pub fn eval_report_from_evidence(
    manifest: &EvalBenchmarkManifest,
    run_id: impl Into<String>,
    k: u32,
    evidence: Vec<EvalCaseEvidence>,
) -> Result<EvalRunReport, EvalReportError> {
    validate_k(k)?;
    let mut known_cases = BTreeMap::new();
    for case in &manifest.cases {
        known_cases.insert(case.case_id.as_str(), case);
    }

    let mut evidence_by_case = BTreeMap::new();
    for case_evidence in evidence {
        let Some(case) = known_cases.get(case_evidence.case_id.as_str()) else {
            return Err(EvalReportError::new(format!(
                "evidence references unknown case_id `{}`",
                case_evidence.case_id
            )));
        };
        if let Some(blocker) = case.replay_blocker() {
            return Err(EvalReportError::new(format!(
                "evidence references non-replayable case_id `{}`: {blocker}",
                case_evidence.case_id
            )));
        }
        if evidence_by_case
            .insert(case_evidence.case_id.clone(), case_evidence)
            .is_some()
        {
            return Err(EvalReportError::new("duplicate evidence case_id"));
        }
    }

    let cases = manifest
        .cases
        .iter()
        .map(|case| match evidence_by_case.remove(&case.case_id) {
            Some(evidence) => report_case_from_evidence(case, evidence),
            None if let Some(blocker) = case.replay_blocker() => EvalReportCase {
                case_id: case.case_id.clone(),
                repo: case.repo.clone(),
                issue: case.issue,
                base_commit: case.base_commit.clone(),
                source_commit: case.base_commit.clone(),
                verify_commands: case.verify_commands.clone(),
                verification_evidence: Vec::new(),
                status: EvalReportCaseStatus::Pending,
                outcome: None,
                passed: false,
                attestation_trust: EvalAttestationTrust::Unsigned,
                attestation_decision: None,
                explicit_evidence: false,
                final_grade: None,
                failed_hard_gates: Vec::new(),
                workflow_id: None,
                terminal_state: None,
                infrastructure_status: EvalCaseInfrastructureStatus::Unknown,
                total_tokens: 0,
                cost_usd_micros: 0,
                missing_evidence: vec![blocker.to_string()],
            },
            None => EvalReportCase {
                case_id: case.case_id.clone(),
                repo: case.repo.clone(),
                issue: case.issue,
                base_commit: case.base_commit.clone(),
                source_commit: case.base_commit.clone(),
                verify_commands: case.verify_commands.clone(),
                verification_evidence: Vec::new(),
                status: EvalReportCaseStatus::Skipped,
                outcome: None,
                passed: false,
                attestation_trust: EvalAttestationTrust::Unsigned,
                attestation_decision: None,
                explicit_evidence: false,
                final_grade: None,
                failed_hard_gates: Vec::new(),
                workflow_id: None,
                terminal_state: None,
                infrastructure_status: EvalCaseInfrastructureStatus::MissingEvidence,
                total_tokens: 0,
                cost_usd_micros: 0,
                missing_evidence: vec!["case_evidence".to_string()],
            },
        })
        .collect::<Vec<_>>();
    Ok(report_from_cases(manifest, run_id, k, cases))
}

pub fn diff_eval_run_reports(
    baseline: &EvalRunReport,
    candidate: &EvalRunReport,
) -> EvalRunReportDiff {
    let baseline_cases = baseline
        .cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let candidate_cases = candidate
        .cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let mut case_ids = BTreeSet::new();
    case_ids.extend(baseline_cases.keys().copied());
    case_ids.extend(candidate_cases.keys().copied());

    let transitions = case_ids
        .into_iter()
        .map(|case_id| {
            let baseline_case = baseline_cases.get(case_id).copied();
            let candidate_case = candidate_cases.get(case_id).copied();
            EvalCaseTransition {
                case_id: case_id.to_string(),
                transition: transition_kind(baseline_case, candidate_case),
                baseline_status: baseline_case.map(|case| case.status),
                candidate_status: candidate_case.map(|case| case.status),
                baseline_attestation_trust: baseline_case.map(|case| case.attestation_trust),
                candidate_attestation_trust: candidate_case.map(|case| case.attestation_trust),
                baseline_attestation_decision: baseline_case
                    .and_then(|case| case.attestation_decision),
                candidate_attestation_decision: candidate_case
                    .and_then(|case| case.attestation_decision),
                baseline_source_commit: baseline_case.map(case_source_commit),
                candidate_source_commit: candidate_case.map(case_source_commit),
                baseline_verify_commands: baseline_case
                    .map(|case| case.verify_commands.clone())
                    .unwrap_or_default(),
                candidate_verify_commands: candidate_case
                    .map(|case| case.verify_commands.clone())
                    .unwrap_or_default(),
                baseline_verification_evidence: baseline_case
                    .map(|case| case.verification_evidence.clone())
                    .unwrap_or_default(),
                candidate_verification_evidence: candidate_case
                    .map(|case| case.verification_evidence.clone())
                    .unwrap_or_default(),
                baseline_terminal_state: baseline_case.and_then(|case| case.terminal_state.clone()),
                candidate_terminal_state: candidate_case
                    .and_then(|case| case.terminal_state.clone()),
                baseline_infrastructure_status: baseline_case
                    .map(|case| case.infrastructure_status),
                candidate_infrastructure_status: candidate_case
                    .map(|case| case.infrastructure_status),
            }
        })
        .collect::<Vec<_>>();
    let transition_counts = transition_counts_for(&transitions);
    let regression_ids = regression_ids_for(&transitions);
    let regression_count = regression_ids.len() as u64;

    EvalRunReportDiff {
        baseline_run_id: baseline.run_id.clone(),
        candidate_run_id: candidate.run_id.clone(),
        suite: candidate.suite.clone(),
        k: candidate.k,
        baseline_outcome: eval_report_effective_outcome(baseline),
        candidate_outcome: eval_report_effective_outcome(candidate),
        delta: EvalReportMetricDelta {
            pass_at_1_delta: candidate.metrics.pass_at_1 - baseline.metrics.pass_at_1,
            pass_to_k_delta: candidate.metrics.pass_to_k - baseline.metrics.pass_to_k,
            total_tokens_delta: i128::from(candidate.metrics.total_tokens)
                - i128::from(baseline.metrics.total_tokens),
            total_cost_usd_micros_delta: i128::from(candidate.metrics.total_cost_usd_micros)
                - i128::from(baseline.metrics.total_cost_usd_micros),
        },
        transition_counts,
        regression_count,
        regression_ids,
        transitions,
    }
}

fn report_case_from_evidence(
    case: &super::manifest::EvalBenchmarkCase,
    evidence: EvalCaseEvidence,
) -> EvalReportCase {
    let (total_tokens, cost_usd_micros) = evidence_usage_totals(&evidence);
    let passed = evidence.status == EvalEvidenceStatus::Passed;
    let status = evidence_case_status(&evidence, passed);
    let outcome = (evidence.status == EvalEvidenceStatus::BudgetExhausted)
        .then_some(EvalReportCaseOutcome::BudgetExhausted);
    let terminal_state = evidence_terminal_state(&evidence);
    let infrastructure_status = evidence_infrastructure_status(&evidence, status);
    let (final_grade, failed_hard_gates) = quality_summary(evidence.quality.as_ref());
    let verification_evidence = evidence
        .quality_gate
        .as_ref()
        .map(|quality_gate| quality_gate.validation_evidence.clone())
        .unwrap_or_default();
    EvalReportCase {
        case_id: case.case_id.clone(),
        repo: case.repo.clone(),
        issue: case.issue,
        base_commit: case.base_commit.clone(),
        source_commit: case.base_commit.clone(),
        verify_commands: case.verify_commands.clone(),
        verification_evidence,
        status,
        outcome,
        passed,
        attestation_trust: evidence.attestation.trust(),
        attestation_decision: evidence.attestation.decision(),
        explicit_evidence: true,
        final_grade,
        failed_hard_gates,
        workflow_id: evidence.workflow_id,
        terminal_state,
        infrastructure_status,
        total_tokens,
        cost_usd_micros,
        missing_evidence: evidence.missing_evidence,
    }
}

fn quality_summary(
    quality: Option<&QualitySnapshot>,
) -> (Option<EvalGrade>, Vec<EvalReportFailedGate>) {
    let Some(quality) = quality else {
        return (None, Vec::new());
    };
    let failed_hard_gates = quality
        .hard_gates
        .iter()
        .filter(|gate| gate.status == GateStatus::Fail)
        .map(|gate| EvalReportFailedGate {
            name: gate.name,
            grade_cap: gate.grade_cap,
        })
        .collect();
    (Some(quality.final_grade), failed_hard_gates)
}

fn evidence_case_status(evidence: &EvalCaseEvidence, passed: bool) -> EvalReportCaseStatus {
    if passed {
        return EvalReportCaseStatus::Passed;
    }
    if evidence.status == EvalEvidenceStatus::Skipped {
        return EvalReportCaseStatus::Skipped;
    }
    if evidence.status == EvalEvidenceStatus::BudgetExhausted {
        return EvalReportCaseStatus::InfraFailed;
    }
    if matches!(
        evidence.status,
        EvalEvidenceStatus::DispatchFailed | EvalEvidenceStatus::EvidenceIncomplete
    ) {
        return EvalReportCaseStatus::InfraFailed;
    }
    if evidence.status == EvalEvidenceStatus::TimedOut {
        return EvalReportCaseStatus::Failed;
    }
    if evidence.missing_evidence.iter().any(|missing| {
        matches!(
            missing.as_str(),
            "workflow_instance" | "terminal_runtime_state"
        )
    }) {
        return EvalReportCaseStatus::InfraFailed;
    }
    EvalReportCaseStatus::Failed
}

fn evidence_terminal_state(evidence: &EvalCaseEvidence) -> Option<String> {
    let runtime = evidence.runtime.as_ref()?;
    runtime.terminal_state.clone().or_else(|| {
        runtime
            .runtime_jobs
            .iter()
            .find_map(|job| job.terminal_state.clone())
    })
}

fn evidence_infrastructure_status(
    evidence: &EvalCaseEvidence,
    status: EvalReportCaseStatus,
) -> EvalCaseInfrastructureStatus {
    if status == EvalReportCaseStatus::InfraFailed {
        return EvalCaseInfrastructureStatus::InfraFailed;
    }
    if evidence.missing_evidence.is_empty() {
        EvalCaseInfrastructureStatus::Healthy
    } else {
        EvalCaseInfrastructureStatus::MissingEvidence
    }
}

fn evidence_usage_totals(evidence: &EvalCaseEvidence) -> (u64, u64) {
    evidence.usage.iter().fold((0_u64, 0_u64), |acc, usage| {
        let tokens = usage.total_tokens.unwrap_or_else(|| {
            usage
                .input_tokens
                .unwrap_or(0)
                .saturating_add(usage.output_tokens.unwrap_or(0))
                .saturating_add(usage.cached_input_tokens.unwrap_or(0))
        });
        (
            acc.0.saturating_add(tokens),
            acc.1.saturating_add(usage.cost_usd_micros.unwrap_or(0)),
        )
    })
}

fn report_from_cases(
    manifest: &EvalBenchmarkManifest,
    run_id: impl Into<String>,
    k: u32,
    cases: Vec<EvalReportCase>,
) -> EvalRunReport {
    let metrics = metrics_for_cases(k, &cases);
    let outcome = inferred_run_outcome(&cases, &metrics);
    EvalRunReport {
        run_id: run_id.into(),
        suite: manifest.suite.clone(),
        k,
        metrics,
        outcome,
        cases,
    }
}

fn metrics_for_cases(k: u32, cases: &[EvalReportCase]) -> EvalReportMetrics {
    let total_cases = cases.len() as u64;
    let scored_cases = cases
        .iter()
        .filter(|case| {
            case.explicit_evidence
                && matches!(
                    case.status,
                    EvalReportCaseStatus::Passed | EvalReportCaseStatus::Failed
                )
        })
        .count() as u64;
    let passed_cases = cases
        .iter()
        .filter(|case| case.explicit_evidence && case.status == EvalReportCaseStatus::Passed)
        .count() as u64;
    let failed_cases = cases
        .iter()
        .filter(|case| case.explicit_evidence && case.status == EvalReportCaseStatus::Failed)
        .count() as u64;
    let skipped_cases = cases
        .iter()
        .filter(|case| case.status == EvalReportCaseStatus::Skipped)
        .count() as u64;
    let pending_cases = cases
        .iter()
        .filter(|case| case.status == EvalReportCaseStatus::Pending)
        .count() as u64;
    let infra_failed_cases = cases
        .iter()
        .filter(|case| matches!(case.status, EvalReportCaseStatus::InfraFailed))
        .count() as u64;
    let pass_at_1 = if scored_cases == 0 {
        0.0
    } else {
        passed_cases as f64 / scored_cases as f64
    };
    let total_tokens = cases
        .iter()
        .fold(0_u64, |sum, case| sum.saturating_add(case.total_tokens));
    let total_cost_usd_micros = cases
        .iter()
        .fold(0_u64, |sum, case| sum.saturating_add(case.cost_usd_micros));

    EvalReportMetrics {
        total_cases,
        scored_cases,
        passed_cases,
        failed_cases,
        skipped_cases,
        pending_cases,
        infra_failed_cases,
        pass_at_1,
        pass_to_k: pass_to_k(pass_at_1, k),
        total_tokens,
        avg_tokens_per_scored_case: average_u64(total_tokens, scored_cases),
        total_cost_usd_micros,
        avg_cost_usd_micros_per_scored_case: average_u64(total_cost_usd_micros, scored_cases),
    }
}

fn pass_to_k(pass_at_1: f64, k: u32) -> f64 {
    1.0 - (1.0 - pass_at_1).powi(k as i32)
}

fn average_u64(value: u64, count: u64) -> Option<f64> {
    (count > 0).then(|| value as f64 / count as f64)
}

fn transition_kind(
    baseline: Option<&EvalReportCase>,
    candidate: Option<&EvalReportCase>,
) -> EvalCaseTransitionKind {
    match (baseline, candidate) {
        (None, Some(_)) => EvalCaseTransitionKind::Added,
        (Some(_), None) => EvalCaseTransitionKind::Removed,
        (None, None) => EvalCaseTransitionKind::StatusChanged,
        (Some(baseline), Some(candidate)) => match (
            transition_outcome(baseline.status),
            transition_outcome(candidate.status),
        ) {
            (EvalCaseTransitionOutcome::Pass, EvalCaseTransitionOutcome::Pass) => {
                EvalCaseTransitionKind::UnchangedPass
            }
            (EvalCaseTransitionOutcome::Fail, EvalCaseTransitionOutcome::Fail) => {
                EvalCaseTransitionKind::UnchangedFail
            }
            (EvalCaseTransitionOutcome::Skip, EvalCaseTransitionOutcome::Skip) => {
                EvalCaseTransitionKind::UnchangedSkip
            }
            (EvalCaseTransitionOutcome::Pass, EvalCaseTransitionOutcome::Fail) => {
                EvalCaseTransitionKind::PassToFail
            }
            (EvalCaseTransitionOutcome::Fail, EvalCaseTransitionOutcome::Pass) => {
                EvalCaseTransitionKind::FailToPass
            }
            (EvalCaseTransitionOutcome::Pass, EvalCaseTransitionOutcome::Skip) => {
                EvalCaseTransitionKind::PassToSkip
            }
            (EvalCaseTransitionOutcome::Skip, EvalCaseTransitionOutcome::Pass) => {
                EvalCaseTransitionKind::SkipToPass
            }
            (EvalCaseTransitionOutcome::Fail, EvalCaseTransitionOutcome::Skip) => {
                EvalCaseTransitionKind::FailToSkip
            }
            (EvalCaseTransitionOutcome::Skip, EvalCaseTransitionOutcome::Fail) => {
                EvalCaseTransitionKind::SkipToFail
            }
            _ => EvalCaseTransitionKind::StatusChanged,
        },
    }
}

fn case_source_commit(case: &EvalReportCase) -> String {
    if case.source_commit.is_empty() {
        case.base_commit.clone()
    } else {
        case.source_commit.clone()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EvalCaseTransitionOutcome {
    Pass,
    Fail,
    Skip,
    Other,
}

fn transition_outcome(status: EvalReportCaseStatus) -> EvalCaseTransitionOutcome {
    match status {
        EvalReportCaseStatus::Passed => EvalCaseTransitionOutcome::Pass,
        EvalReportCaseStatus::Failed => EvalCaseTransitionOutcome::Fail,
        EvalReportCaseStatus::Skipped => EvalCaseTransitionOutcome::Skip,
        EvalReportCaseStatus::Pending | EvalReportCaseStatus::InfraFailed => {
            EvalCaseTransitionOutcome::Other
        }
    }
}

fn transition_counts_for(transitions: &[EvalCaseTransition]) -> EvalCaseTransitionCounts {
    let mut counts = EvalCaseTransitionCounts::default();
    for transition in transitions {
        match transition.transition {
            EvalCaseTransitionKind::Added => counts.added += 1,
            EvalCaseTransitionKind::Removed => counts.removed += 1,
            EvalCaseTransitionKind::UnchangedPass => counts.unchanged_pass += 1,
            EvalCaseTransitionKind::UnchangedFail => counts.unchanged_fail += 1,
            EvalCaseTransitionKind::UnchangedSkip => counts.unchanged_skip += 1,
            EvalCaseTransitionKind::PassToFail => counts.pass_to_fail += 1,
            EvalCaseTransitionKind::FailToPass => counts.fail_to_pass += 1,
            EvalCaseTransitionKind::PassToSkip => counts.pass_to_skip += 1,
            EvalCaseTransitionKind::SkipToPass => counts.skip_to_pass += 1,
            EvalCaseTransitionKind::FailToSkip => counts.fail_to_skip += 1,
            EvalCaseTransitionKind::SkipToFail => counts.skip_to_fail += 1,
            EvalCaseTransitionKind::StatusChanged => counts.status_changed += 1,
        }
    }
    counts
}

fn regression_ids_for(transitions: &[EvalCaseTransition]) -> Vec<String> {
    transitions
        .iter()
        .filter(|transition| transition.transition == EvalCaseTransitionKind::PassToFail)
        .map(|transition| transition.case_id.clone())
        .collect()
}

fn validate_k(k: u32) -> Result<(), EvalReportError> {
    if k == 0 {
        return Err(EvalReportError::new("k must be greater than zero"));
    }
    if k > i32::MAX as u32 {
        return Err(EvalReportError::new(
            "k must be less than or equal to i32::MAX",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
