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
    pub status: EvalReportCaseStatus,
    pub passed: bool,
    #[serde(default)]
    pub explicit_evidence: bool,
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub terminal_state: Option<String>,
    #[serde(default)]
    pub infrastructure_status: EvalCaseInfrastructureStatus,
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
    #[serde(default)]
    pub baseline_source_commit: Option<String>,
    #[serde(default)]
    pub candidate_source_commit: Option<String>,
    #[serde(default)]
    pub baseline_verify_commands: Vec<String>,
    #[serde(default)]
    pub candidate_verify_commands: Vec<String>,
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
            status: EvalReportCaseStatus::Pending,
            passed: false,
            explicit_evidence: false,
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
                source_commit: case.base_commit.clone(),
                verify_commands: case.verify_commands.clone(),
                status: EvalReportCaseStatus::Skipped,
                passed: false,
                explicit_evidence: false,
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
                baseline_source_commit: baseline_case.map(case_source_commit),
                candidate_source_commit: candidate_case.map(case_source_commit),
                baseline_verify_commands: baseline_case
                    .map(|case| case.verify_commands.clone())
                    .unwrap_or_default(),
                candidate_verify_commands: candidate_case
                    .map(|case| case.verify_commands.clone())
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
    let terminal_state = evidence_terminal_state(&evidence);
    let infrastructure_status = evidence_infrastructure_status(&evidence, status);
    EvalReportCase {
        case_id: case.case_id.clone(),
        repo: case.repo.clone(),
        issue: case.issue,
        base_commit: case.base_commit.clone(),
        source_commit: case.base_commit.clone(),
        verify_commands: case.verify_commands.clone(),
        status,
        passed,
        explicit_evidence: true,
        workflow_id: evidence.workflow_id,
        terminal_state,
        infrastructure_status,
        total_tokens,
        cost_usd_micros,
        missing_evidence: evidence.missing_evidence,
    }
}

fn evidence_case_status(evidence: &EvalCaseEvidence, passed: bool) -> EvalReportCaseStatus {
    if passed {
        return EvalReportCaseStatus::Passed;
    }
    if evidence.status == EvalEvidenceStatus::Skipped {
        return EvalReportCaseStatus::Skipped;
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
mod tests {
    use super::super::model::RuntimeSnapshot;
    use super::super::{EvalBenchmarkCase, EvalCaseEvidence};
    use super::*;

    #[test]
    fn eval_report_scores_only_explicit_cases_and_skips_missing_evidence() {
        let report = eval_report_from_evidence(
            &manifest(&["case-pass", "case-missing"]),
            "candidate",
            3,
            vec![evidence(
                "case-pass",
                EvalEvidenceStatus::Passed,
                Vec::new(),
                Some("done"),
            )],
        )
        .expect("report should build from partial evidence");

        assert_eq!(report.metrics.total_cases, 2);
        assert_eq!(report.metrics.scored_cases, 1);
        assert_eq!(report.metrics.passed_cases, 1);
        assert_eq!(report.metrics.failed_cases, 0);
        assert_eq!(report.metrics.skipped_cases, 1);
        assert_eq!(report.metrics.pass_at_1, 1.0);

        let missing = report
            .cases
            .iter()
            .find(|case| case.case_id == "case-missing")
            .expect("missing case should still be reported");
        assert_eq!(missing.status, EvalReportCaseStatus::Skipped);
        assert!(!missing.explicit_evidence);
        assert_eq!(
            missing.infrastructure_status,
            EvalCaseInfrastructureStatus::MissingEvidence
        );
        assert_eq!(missing.missing_evidence, vec!["case_evidence"]);

        let passed = report
            .cases
            .iter()
            .find(|case| case.case_id == "case-pass")
            .expect("explicit case should be reported");
        assert_eq!(passed.source_commit, "abcdef1");
        assert_eq!(passed.terminal_state.as_deref(), Some("done"));
        assert_eq!(
            passed.infrastructure_status,
            EvalCaseInfrastructureStatus::Healthy
        );
    }

    #[test]
    fn eval_report_diff_counts_every_transition_and_exposes_regressions() {
        let baseline = report(
            "baseline",
            vec![
                case("removed", EvalReportCaseStatus::Passed),
                case("unchanged-pass", EvalReportCaseStatus::Passed),
                case("unchanged-fail", EvalReportCaseStatus::Failed),
                case("unchanged-skip", EvalReportCaseStatus::Skipped),
                case("pass-to-fail", EvalReportCaseStatus::Passed),
                case("fail-to-pass", EvalReportCaseStatus::Failed),
                case("pass-to-skip", EvalReportCaseStatus::Passed),
                case("skip-to-pass", EvalReportCaseStatus::Skipped),
                case("fail-to-skip", EvalReportCaseStatus::Failed),
                case("skip-to-fail", EvalReportCaseStatus::Skipped),
                case("status-changed", EvalReportCaseStatus::Pending),
            ],
        );
        let candidate = report(
            "candidate",
            vec![
                case("added", EvalReportCaseStatus::Passed),
                case("unchanged-pass", EvalReportCaseStatus::Passed),
                case("unchanged-fail", EvalReportCaseStatus::Failed),
                case("unchanged-skip", EvalReportCaseStatus::Skipped),
                case("pass-to-fail", EvalReportCaseStatus::Failed),
                case("fail-to-pass", EvalReportCaseStatus::Passed),
                case("pass-to-skip", EvalReportCaseStatus::Skipped),
                case("skip-to-pass", EvalReportCaseStatus::Passed),
                case("fail-to-skip", EvalReportCaseStatus::Skipped),
                case("skip-to-fail", EvalReportCaseStatus::Failed),
                case("status-changed", EvalReportCaseStatus::InfraFailed),
            ],
        );

        let diff = diff_eval_run_reports(&baseline, &candidate);
        let kinds = diff
            .transitions
            .iter()
            .map(|transition| (transition.case_id.as_str(), transition.transition))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(kinds["added"], EvalCaseTransitionKind::Added);
        assert_eq!(kinds["removed"], EvalCaseTransitionKind::Removed);
        assert_eq!(
            kinds["unchanged-pass"],
            EvalCaseTransitionKind::UnchangedPass
        );
        assert_eq!(
            kinds["unchanged-fail"],
            EvalCaseTransitionKind::UnchangedFail
        );
        assert_eq!(
            kinds["unchanged-skip"],
            EvalCaseTransitionKind::UnchangedSkip
        );
        assert_eq!(kinds["pass-to-fail"], EvalCaseTransitionKind::PassToFail);
        assert_eq!(kinds["fail-to-pass"], EvalCaseTransitionKind::FailToPass);
        assert_eq!(kinds["pass-to-skip"], EvalCaseTransitionKind::PassToSkip);
        assert_eq!(kinds["skip-to-pass"], EvalCaseTransitionKind::SkipToPass);
        assert_eq!(kinds["fail-to-skip"], EvalCaseTransitionKind::FailToSkip);
        assert_eq!(kinds["skip-to-fail"], EvalCaseTransitionKind::SkipToFail);
        assert_eq!(
            kinds["status-changed"],
            EvalCaseTransitionKind::StatusChanged
        );

        assert_eq!(diff.transition_counts.added, 1);
        assert_eq!(diff.transition_counts.removed, 1);
        assert_eq!(diff.transition_counts.unchanged_pass, 1);
        assert_eq!(diff.transition_counts.unchanged_fail, 1);
        assert_eq!(diff.transition_counts.unchanged_skip, 1);
        assert_eq!(diff.transition_counts.pass_to_fail, 1);
        assert_eq!(diff.transition_counts.fail_to_pass, 1);
        assert_eq!(diff.transition_counts.pass_to_skip, 1);
        assert_eq!(diff.transition_counts.skip_to_pass, 1);
        assert_eq!(diff.transition_counts.fail_to_skip, 1);
        assert_eq!(diff.transition_counts.skip_to_fail, 1);
        assert_eq!(diff.transition_counts.status_changed, 1);
        assert_eq!(diff.regression_count, 1);
        assert_eq!(diff.regression_ids, vec!["pass-to-fail"]);

        let regression = diff
            .transitions
            .iter()
            .find(|transition| transition.case_id == "pass-to-fail")
            .expect("regression transition should be present");
        assert_eq!(
            regression.baseline_source_commit.as_deref(),
            Some("source-pass-to-fail")
        );
        assert_eq!(
            regression.candidate_terminal_state.as_deref(),
            Some("terminal-failed")
        );
        assert_eq!(
            regression.candidate_infrastructure_status,
            Some(EvalCaseInfrastructureStatus::Healthy)
        );
        assert_eq!(
            regression.candidate_verify_commands,
            vec!["cargo test pass-to-fail"]
        );
    }

    fn manifest(case_ids: &[&str]) -> EvalBenchmarkManifest {
        EvalBenchmarkManifest {
            suite: "harness-core".to_string(),
            cases: case_ids
                .iter()
                .map(|case_id| EvalBenchmarkCase {
                    case_id: (*case_id).to_string(),
                    repo: "majiayu000/harness".to_string(),
                    issue: 1447,
                    base_commit: "abcdef1".to_string(),
                    verify_commands: vec![format!("cargo test {case_id}")],
                    timeout_secs: 3600,
                })
                .collect(),
        }
    }

    fn evidence(
        case_id: &str,
        status: EvalEvidenceStatus,
        missing_evidence: Vec<String>,
        terminal_state: Option<&str>,
    ) -> EvalCaseEvidence {
        EvalCaseEvidence {
            eval_run_id: "run-1".to_string(),
            case_id: case_id.to_string(),
            workflow_id: Some(format!("workflow-{case_id}")),
            status,
            runtime: terminal_state.map(|terminal_state| RuntimeSnapshot {
                task_id: Some(format!("task-{case_id}")),
                workflow_id: Some(format!("workflow-{case_id}")),
                workflow_state: Some(terminal_state.to_string()),
                runtime_jobs: Vec::new(),
                latest_activity: Some("quality_gate".to_string()),
                terminal_state: Some(terminal_state.to_string()),
                collected_at: "2026-08-10T00:00:00Z".to_string(),
            }),
            usage: Vec::new(),
            submission: None,
            quality_gate: None,
            missing_evidence,
        }
    }

    fn report(run_id: &str, cases: Vec<EvalReportCase>) -> EvalRunReport {
        EvalRunReport {
            run_id: run_id.to_string(),
            suite: "harness-core".to_string(),
            k: 3,
            metrics: metrics_for_cases(3, &cases),
            cases,
        }
    }

    fn case(case_id: &str, status: EvalReportCaseStatus) -> EvalReportCase {
        let explicit_evidence = status != EvalReportCaseStatus::Pending;
        EvalReportCase {
            case_id: case_id.to_string(),
            repo: "majiayu000/harness".to_string(),
            issue: 1447,
            base_commit: format!("base-{case_id}"),
            source_commit: format!("source-{case_id}"),
            verify_commands: vec![format!("cargo test {case_id}")],
            status,
            passed: status == EvalReportCaseStatus::Passed,
            explicit_evidence,
            workflow_id: Some(format!("workflow-{case_id}")),
            terminal_state: Some(format!("terminal-{}", case_status_suffix(status))),
            infrastructure_status: match status {
                EvalReportCaseStatus::InfraFailed => EvalCaseInfrastructureStatus::InfraFailed,
                EvalReportCaseStatus::Pending => EvalCaseInfrastructureStatus::Unknown,
                _ => EvalCaseInfrastructureStatus::Healthy,
            },
            total_tokens: 0,
            cost_usd_micros: 0,
            missing_evidence: Vec::new(),
        }
    }

    fn case_status_suffix(status: EvalReportCaseStatus) -> &'static str {
        match status {
            EvalReportCaseStatus::Pending => "pending",
            EvalReportCaseStatus::Passed => "passed",
            EvalReportCaseStatus::Failed => "failed",
            EvalReportCaseStatus::Skipped => "skipped",
            EvalReportCaseStatus::InfraFailed => "infra-failed",
        }
    }
}
