use super::evidence::{EvalCaseEvidence, EvalEvidenceStatus};
use super::manifest::EvalBenchmarkManifest;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalRunReport {
    pub run_id: String,
    pub suite: String,
    pub k: u32,
    pub metrics: EvalReportMetrics,
    pub cases: Vec<EvalReportCase>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalReportMetrics {
    pub total_cases: u64,
    pub scored_cases: u64,
    pub passed_cases: u64,
    pub failed_cases: u64,
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
    pub verify_commands: Vec<String>,
    pub status: EvalReportCaseStatus,
    pub passed: bool,
    pub workflow_id: Option<String>,
    pub total_tokens: u64,
    pub cost_usd_micros: u64,
    pub missing_evidence: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalReportCaseStatus {
    Pending,
    Passed,
    Failed,
    InfraFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalRunReportDiff {
    pub baseline_run_id: String,
    pub candidate_run_id: String,
    pub suite: String,
    pub k: u32,
    pub delta: EvalReportMetricDelta,
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
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseTransitionKind {
    Added,
    Removed,
    UnchangedPass,
    UnchangedFail,
    PassToFail,
    FailToPass,
    StatusChanged,
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
            verify_commands: case.verify_commands.clone(),
            status: EvalReportCaseStatus::Pending,
            passed: false,
            workflow_id: None,
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
    let mut known_case_ids = BTreeSet::new();
    for case in &manifest.cases {
        known_case_ids.insert(case.case_id.as_str());
    }

    let mut evidence_by_case = BTreeMap::new();
    for case_evidence in evidence {
        if !known_case_ids.contains(case_evidence.case_id.as_str()) {
            return Err(EvalReportError::new(format!(
                "evidence references unknown case_id `{}`",
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
            None => EvalReportCase {
                case_id: case.case_id.clone(),
                repo: case.repo.clone(),
                issue: case.issue,
                base_commit: case.base_commit.clone(),
                verify_commands: case.verify_commands.clone(),
                status: EvalReportCaseStatus::Failed,
                passed: false,
                workflow_id: None,
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
            }
        })
        .collect::<Vec<_>>();

    EvalRunReportDiff {
        baseline_run_id: baseline.run_id.clone(),
        candidate_run_id: candidate.run_id.clone(),
        suite: candidate.suite.clone(),
        k: candidate.k,
        delta: EvalReportMetricDelta {
            pass_at_1_delta: candidate.metrics.pass_at_1 - baseline.metrics.pass_at_1,
            pass_to_k_delta: candidate.metrics.pass_to_k - baseline.metrics.pass_to_k,
            total_tokens_delta: i128::from(candidate.metrics.total_tokens)
                - i128::from(baseline.metrics.total_tokens),
            total_cost_usd_micros_delta: i128::from(candidate.metrics.total_cost_usd_micros)
                - i128::from(baseline.metrics.total_cost_usd_micros),
        },
        transitions,
    }
}

fn report_case_from_evidence(
    case: &super::manifest::EvalBenchmarkCase,
    evidence: EvalCaseEvidence,
) -> EvalReportCase {
    let (total_tokens, cost_usd_micros) = evidence_usage_totals(&evidence);
    let passed = evidence.status == EvalEvidenceStatus::Passed;
    EvalReportCase {
        case_id: case.case_id.clone(),
        repo: case.repo.clone(),
        issue: case.issue,
        base_commit: case.base_commit.clone(),
        verify_commands: case.verify_commands.clone(),
        status: if passed {
            EvalReportCaseStatus::Passed
        } else {
            EvalReportCaseStatus::Failed
        },
        passed,
        workflow_id: evidence.workflow_id,
        total_tokens,
        cost_usd_micros,
        missing_evidence: evidence.missing_evidence,
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
    EvalRunReport {
        run_id: run_id.into(),
        suite: manifest.suite.clone(),
        k,
        metrics: metrics_for_cases(k, &cases),
        cases,
    }
}

fn metrics_for_cases(k: u32, cases: &[EvalReportCase]) -> EvalReportMetrics {
    let total_cases = cases.len() as u64;
    let scored_cases = cases
        .iter()
        .filter(|case| {
            matches!(
                case.status,
                EvalReportCaseStatus::Passed | EvalReportCaseStatus::Failed
            )
        })
        .count() as u64;
    let passed_cases = cases.iter().filter(|case| case.passed).count() as u64;
    let failed_cases = cases
        .iter()
        .filter(|case| case.status == EvalReportCaseStatus::Failed)
        .count() as u64;
    let pending_cases = cases
        .iter()
        .filter(|case| case.status == EvalReportCaseStatus::Pending)
        .count() as u64;
    let infra_failed_cases = cases
        .iter()
        .filter(|case| case.status == EvalReportCaseStatus::InfraFailed)
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
        (Some(baseline), Some(candidate)) => match (baseline.passed, candidate.passed) {
            (true, true) => EvalCaseTransitionKind::UnchangedPass,
            (false, false) if baseline.status == candidate.status => {
                EvalCaseTransitionKind::UnchangedFail
            }
            (true, false) => EvalCaseTransitionKind::PassToFail,
            (false, true) => EvalCaseTransitionKind::FailToPass,
            (false, false) => EvalCaseTransitionKind::StatusChanged,
        },
    }
}

fn validate_k(k: u32) -> Result<(), EvalReportError> {
    if k == 0 {
        return Err(EvalReportError::new("k must be greater than zero"));
    }
    Ok(())
}
