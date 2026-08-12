use super::manifest::{
    EvalBenchmarkCase, EvalBenchmarkManifest, EvalCaseRisk, EvalCaseVerdict, EvalCommitResolution,
    EvalIsolationProfile,
};
use harness_sandbox::{CappedResourceLimits, ResourceLimits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const EVAL_SUITE_DIGEST_SCHEMA_VERSION: &str = "eval-suite/v0.1";
pub const EVAL_SUITE_MIGRATION_RECORD_SCHEMA_VERSION: &str = "eval-suite-migration/v0.1";

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSuiteDriftPolicy {
    #[default]
    Block,
    NeedsHuman,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSuiteDriftDecision {
    NoDrift,
    Approved,
    Block,
    NeedsHuman,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSuiteChangeDirection {
    Changed,
    Strengthened,
    Weakened,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteDriftAssessment {
    pub baseline_suite_digest: String,
    pub candidate_suite_digest: String,
    pub drift_digest: String,
    pub policy: EvalSuiteDriftPolicy,
    pub decision: EvalSuiteDriftDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_migration_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    pub drift: EvalSuiteDriftSummary,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteDriftSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_name: Option<EvalSuiteStringChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_cases: Vec<EvalSuiteCaseChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_commands: Vec<EvalSuiteCommandsChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_expectations: Vec<EvalSuiteExpectationsChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_thresholds: Vec<EvalSuiteThresholdsChange>,
}

impl EvalSuiteDriftSummary {
    pub fn is_empty(&self) -> bool {
        self.suite_name.is_none()
            && self.changed_cases.is_empty()
            && self.changed_commands.is_empty()
            && self.changed_expectations.is_empty()
            && self.changed_thresholds.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteStringChange {
    pub baseline: String,
    pub candidate: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSuiteCaseChangeKind {
    Added,
    Removed,
    MetadataChanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteCaseChange {
    pub case_id: String,
    pub kind: EvalSuiteCaseChangeKind,
    pub direction: EvalSuiteChangeDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<EvalSuiteCaseDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<EvalSuiteCaseDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteCaseDefinition {
    pub case_id: String,
    pub repo: String,
    pub issue: u64,
    pub base_commit: String,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<EvalCaseRisk>,
    pub evidence: Vec<String>,
    pub isolation: EvalIsolationProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteCommandsChange {
    pub case_id: String,
    pub direction: EvalSuiteChangeDirection,
    pub baseline: Vec<String>,
    pub candidate: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteExpectationsChange {
    pub case_id: String,
    pub direction: EvalSuiteChangeDirection,
    pub baseline: EvalSuiteCaseExpectations,
    pub candidate: EvalSuiteCaseExpectations,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteCaseExpectations {
    pub resolution_prs: Vec<u64>,
    pub resolution_commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_resolution: Option<EvalCommitResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<EvalCaseVerdict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteThresholdsChange {
    pub case_id: String,
    pub direction: EvalSuiteChangeDirection,
    pub baseline: EvalSuiteCaseThresholds,
    pub candidate: EvalSuiteCaseThresholds,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteCaseThresholds {
    pub timeout_secs: u64,
    pub resource_limits: CappedResourceLimits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSuiteMigrationRecord {
    pub schema_version: String,
    pub migration_id: String,
    pub baseline_suite: String,
    pub candidate_suite: String,
    pub baseline_suite_digest: String,
    pub candidate_suite_digest: String,
    pub drift_digest: String,
    pub approver_kind: EvalSuiteMigrationApproverKind,
    pub approved_by: String,
    pub approval_url: String,
    pub approved_at: String,
    pub rationale: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSuiteMigrationApproverKind {
    Human,
    Agent,
    Automation,
}

pub fn assess_eval_suite_drift(
    baseline: &EvalBenchmarkManifest,
    candidate: &EvalBenchmarkManifest,
    migration: Option<&EvalSuiteMigrationRecord>,
    policy: EvalSuiteDriftPolicy,
) -> EvalSuiteDriftAssessment {
    let baseline_suite_digest = eval_suite_digest(baseline);
    let candidate_suite_digest = eval_suite_digest(candidate);
    let drift = diff_eval_suites(baseline, candidate);
    let drift_digest = eval_suite_drift_digest(&drift);
    let mut blockers = Vec::new();
    let approved_migration_id = if drift.is_empty() {
        None
    } else if let Some(record) = migration {
        blockers = validate_migration_record(
            record,
            baseline,
            candidate,
            &baseline_suite_digest,
            &candidate_suite_digest,
            &drift_digest,
        );
        blockers
            .is_empty()
            .then(|| record.migration_id.trim().to_string())
    } else {
        blockers
            .push("suite drift requires a versioned human-approved migration record".to_string());
        None
    };

    let decision = if drift.is_empty() {
        EvalSuiteDriftDecision::NoDrift
    } else if approved_migration_id.is_some() {
        EvalSuiteDriftDecision::Approved
    } else {
        match policy {
            EvalSuiteDriftPolicy::Block => EvalSuiteDriftDecision::Block,
            EvalSuiteDriftPolicy::NeedsHuman => EvalSuiteDriftDecision::NeedsHuman,
        }
    };

    EvalSuiteDriftAssessment {
        baseline_suite_digest,
        candidate_suite_digest,
        drift_digest,
        policy,
        decision,
        approved_migration_id,
        blockers,
        drift,
    }
}

pub fn eval_suite_digest(manifest: &EvalBenchmarkManifest) -> String {
    let snapshot = EvalSuiteDigestInput {
        schema_version: EVAL_SUITE_DIGEST_SCHEMA_VERSION,
        suite: manifest.suite.as_str(),
        cases: canonical_case_snapshots(manifest),
    };
    prefixed_sha256(&snapshot)
}

pub fn eval_suite_drift_digest(drift: &EvalSuiteDriftSummary) -> String {
    prefixed_sha256(drift)
}

fn diff_eval_suites(
    baseline: &EvalBenchmarkManifest,
    candidate: &EvalBenchmarkManifest,
) -> EvalSuiteDriftSummary {
    let baseline_cases = case_index(&baseline.cases);
    let candidate_cases = case_index(&candidate.cases);
    let mut case_ids = BTreeSet::new();
    case_ids.extend(baseline_cases.keys().copied());
    case_ids.extend(candidate_cases.keys().copied());

    let mut summary = EvalSuiteDriftSummary {
        suite_name: (baseline.suite != candidate.suite).then(|| EvalSuiteStringChange {
            baseline: baseline.suite.clone(),
            candidate: candidate.suite.clone(),
        }),
        ..EvalSuiteDriftSummary::default()
    };

    for case_id in case_ids {
        match (baseline_cases.get(case_id), candidate_cases.get(case_id)) {
            (None, Some(candidate_case)) => {
                summary.changed_cases.push(EvalSuiteCaseChange {
                    case_id: case_id.to_string(),
                    kind: EvalSuiteCaseChangeKind::Added,
                    direction: EvalSuiteChangeDirection::Strengthened,
                    baseline: None,
                    candidate: Some(case_definition(candidate_case)),
                });
            }
            (Some(baseline_case), None) => {
                summary.changed_cases.push(EvalSuiteCaseChange {
                    case_id: case_id.to_string(),
                    kind: EvalSuiteCaseChangeKind::Removed,
                    direction: EvalSuiteChangeDirection::Weakened,
                    baseline: Some(case_definition(baseline_case)),
                    candidate: None,
                });
            }
            (Some(baseline_case), Some(candidate_case)) => {
                let baseline_definition = case_definition(baseline_case);
                let candidate_definition = case_definition(candidate_case);
                if baseline_definition != candidate_definition {
                    summary.changed_cases.push(EvalSuiteCaseChange {
                        case_id: case_id.to_string(),
                        kind: EvalSuiteCaseChangeKind::MetadataChanged,
                        direction: EvalSuiteChangeDirection::Changed,
                        baseline: Some(baseline_definition),
                        candidate: Some(candidate_definition),
                    });
                }

                if baseline_case.verify_commands != candidate_case.verify_commands {
                    summary.changed_commands.push(EvalSuiteCommandsChange {
                        case_id: case_id.to_string(),
                        direction: sequence_change_direction(
                            &baseline_case.verify_commands,
                            &candidate_case.verify_commands,
                        ),
                        baseline: baseline_case.verify_commands.clone(),
                        candidate: candidate_case.verify_commands.clone(),
                    });
                }

                let baseline_expectations = case_expectations(baseline_case);
                let candidate_expectations = case_expectations(candidate_case);
                if baseline_expectations != candidate_expectations {
                    summary
                        .changed_expectations
                        .push(EvalSuiteExpectationsChange {
                            case_id: case_id.to_string(),
                            direction: expectation_change_direction(
                                &baseline_expectations,
                                &candidate_expectations,
                            ),
                            baseline: baseline_expectations,
                            candidate: candidate_expectations,
                        });
                }

                let baseline_thresholds = case_thresholds(baseline_case);
                let candidate_thresholds = case_thresholds(candidate_case);
                if baseline_thresholds != candidate_thresholds {
                    summary.changed_thresholds.push(EvalSuiteThresholdsChange {
                        case_id: case_id.to_string(),
                        direction: threshold_change_direction(
                            &baseline_thresholds,
                            &candidate_thresholds,
                        ),
                        baseline: baseline_thresholds,
                        candidate: candidate_thresholds,
                    });
                }
            }
            (None, None) => {}
        }
    }

    summary
}

fn validate_migration_record(
    record: &EvalSuiteMigrationRecord,
    baseline: &EvalBenchmarkManifest,
    candidate: &EvalBenchmarkManifest,
    baseline_suite_digest: &str,
    candidate_suite_digest: &str,
    drift_digest: &str,
) -> Vec<String> {
    let mut blockers = Vec::new();
    if record.schema_version != EVAL_SUITE_MIGRATION_RECORD_SCHEMA_VERSION {
        blockers.push(format!(
            "migration record schema_version must be {EVAL_SUITE_MIGRATION_RECORD_SCHEMA_VERSION}"
        ));
    }
    if record.migration_id.trim().is_empty() {
        blockers.push("migration record migration_id must not be empty".to_string());
    }
    if record.baseline_suite != baseline.suite {
        blockers.push(format!(
            "migration record baseline_suite `{}` does not match baseline suite `{}`",
            record.baseline_suite, baseline.suite
        ));
    }
    if record.candidate_suite != candidate.suite {
        blockers.push(format!(
            "migration record candidate_suite `{}` does not match candidate suite `{}`",
            record.candidate_suite, candidate.suite
        ));
    }
    if record.baseline_suite_digest != baseline_suite_digest {
        blockers.push(
            "migration record baseline_suite_digest does not match baseline manifest".to_string(),
        );
    }
    if record.candidate_suite_digest != candidate_suite_digest {
        blockers.push(
            "migration record candidate_suite_digest does not match candidate manifest".to_string(),
        );
    }
    if record.drift_digest != drift_digest {
        blockers.push("migration record drift_digest does not match manifest drift".to_string());
    }
    if record.approver_kind != EvalSuiteMigrationApproverKind::Human {
        blockers.push("suite migration approval must come from a human approver".to_string());
    }
    for (field, value) in [
        ("approved_by", record.approved_by.as_str()),
        ("approval_url", record.approval_url.as_str()),
        ("approved_at", record.approved_at.as_str()),
        ("rationale", record.rationale.as_str()),
    ] {
        if value.trim().is_empty() {
            blockers.push(format!("migration record {field} must not be empty"));
        }
    }
    if !record.approved_at.trim().is_empty()
        && chrono::DateTime::parse_from_rfc3339(&record.approved_at).is_err()
    {
        blockers.push("migration record approved_at must be an RFC3339 timestamp".to_string());
    }
    blockers
}

#[derive(Serialize)]
struct EvalSuiteDigestInput<'a> {
    schema_version: &'static str,
    suite: &'a str,
    cases: Vec<EvalSuiteCaseSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct EvalSuiteCaseSnapshot {
    definition: EvalSuiteCaseDefinition,
    verify_commands: Vec<String>,
    expectations: EvalSuiteCaseExpectations,
    thresholds: EvalSuiteCaseThresholds,
}

fn canonical_case_snapshots(manifest: &EvalBenchmarkManifest) -> Vec<EvalSuiteCaseSnapshot> {
    let mut snapshots = manifest
        .cases
        .iter()
        .map(|case| EvalSuiteCaseSnapshot {
            definition: case_definition(case),
            verify_commands: case.verify_commands.clone(),
            expectations: case_expectations(case),
            thresholds: case_thresholds(case),
        })
        .collect::<Vec<_>>();
    snapshots.sort_by(|left, right| left.definition.case_id.cmp(&right.definition.case_id));
    snapshots
}

fn case_index(cases: &[EvalBenchmarkCase]) -> BTreeMap<&str, &EvalBenchmarkCase> {
    cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect()
}

fn case_definition(case: &EvalBenchmarkCase) -> EvalSuiteCaseDefinition {
    EvalSuiteCaseDefinition {
        case_id: case.case_id.clone(),
        repo: case.repo.clone(),
        issue: case.issue,
        base_commit: case.base_commit.clone(),
        paths: case.paths.clone(),
        risk: case.risk,
        evidence: case.evidence.clone(),
        isolation: case.isolation.clone(),
    }
}

fn case_expectations(case: &EvalBenchmarkCase) -> EvalSuiteCaseExpectations {
    EvalSuiteCaseExpectations {
        resolution_prs: case.resolution_prs.clone(),
        resolution_commits: case.resolution_commits.clone(),
        commit_resolution: case.commit_resolution,
        verdict: case.verdict,
    }
}

fn case_thresholds(case: &EvalBenchmarkCase) -> EvalSuiteCaseThresholds {
    EvalSuiteCaseThresholds {
        timeout_secs: case.timeout_secs,
        resource_limits: case.resource_limits.clone(),
    }
}

fn sequence_change_direction(
    baseline: &[String],
    candidate: &[String],
) -> EvalSuiteChangeDirection {
    let baseline_set = baseline.iter().collect::<BTreeSet<_>>();
    let candidate_set = candidate.iter().collect::<BTreeSet<_>>();
    if baseline_set.is_subset(&candidate_set) && baseline.len() < candidate.len() {
        EvalSuiteChangeDirection::Strengthened
    } else if candidate_set.is_subset(&baseline_set) && candidate.len() < baseline.len() {
        EvalSuiteChangeDirection::Weakened
    } else {
        EvalSuiteChangeDirection::Changed
    }
}

fn expectation_change_direction(
    baseline: &EvalSuiteCaseExpectations,
    candidate: &EvalSuiteCaseExpectations,
) -> EvalSuiteChangeDirection {
    let baseline_strength = expectation_strength(baseline);
    let candidate_strength = expectation_strength(candidate);
    if candidate_strength > baseline_strength {
        EvalSuiteChangeDirection::Strengthened
    } else if candidate_strength < baseline_strength {
        EvalSuiteChangeDirection::Weakened
    } else {
        EvalSuiteChangeDirection::Changed
    }
}

fn expectation_strength(expectations: &EvalSuiteCaseExpectations) -> u8 {
    let mut strength = match expectations.commit_resolution {
        Some(EvalCommitResolution::Resolved) => 2,
        None => 1,
        Some(EvalCommitResolution::Pending) => 0,
    };
    strength += match expectations.verdict {
        Some(EvalCaseVerdict::Replayable) => 2,
        None => 1,
        Some(EvalCaseVerdict::Pending) => 0,
    };
    strength += u8::from(!expectations.resolution_prs.is_empty());
    strength += u8::from(!expectations.resolution_commits.is_empty());
    strength
}

fn threshold_change_direction(
    baseline: &EvalSuiteCaseThresholds,
    candidate: &EvalSuiteCaseThresholds,
) -> EvalSuiteChangeDirection {
    let mut strengthened = false;
    let mut weakened = false;
    record_limit_direction(
        limit_direction(Some(baseline.timeout_secs), Some(candidate.timeout_secs)),
        &mut strengthened,
        &mut weakened,
    );
    for (baseline, candidate) in resource_limit_pairs(
        baseline.resource_limits.effective,
        candidate.resource_limits.effective,
    ) {
        record_limit_direction(
            limit_direction(baseline, candidate),
            &mut strengthened,
            &mut weakened,
        );
    }
    match (strengthened, weakened) {
        (true, false) => EvalSuiteChangeDirection::Strengthened,
        (false, true) => EvalSuiteChangeDirection::Weakened,
        _ => EvalSuiteChangeDirection::Changed,
    }
}

fn resource_limit_pairs(
    baseline: ResourceLimits,
    candidate: ResourceLimits,
) -> [(Option<u64>, Option<u64>); 6] {
    [
        (baseline.cpu_time_secs, candidate.cpu_time_secs),
        (baseline.memory_bytes, candidate.memory_bytes),
        (baseline.pids, candidate.pids),
        (baseline.disk_bytes, candidate.disk_bytes),
        (baseline.output_bytes, candidate.output_bytes),
        (baseline.wall_time_secs, candidate.wall_time_secs),
    ]
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum LimitDirection {
    Same,
    Strengthened,
    Weakened,
}

fn limit_direction(baseline: Option<u64>, candidate: Option<u64>) -> LimitDirection {
    match (baseline, candidate) {
        (left, right) if left == right => LimitDirection::Same,
        (Some(left), Some(right)) if right < left => LimitDirection::Strengthened,
        (Some(_), None) => LimitDirection::Weakened,
        (None, Some(_)) => LimitDirection::Strengthened,
        (Some(_), Some(_)) => LimitDirection::Weakened,
        (None, None) => LimitDirection::Same,
    }
}

fn record_limit_direction(direction: LimitDirection, strengthened: &mut bool, weakened: &mut bool) {
    match direction {
        LimitDirection::Same => {}
        LimitDirection::Strengthened => *strengthened = true,
        LimitDirection::Weakened => *weakened = true,
    }
}

fn prefixed_sha256<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_vec(value).expect("eval suite digest serialization is infallible");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_drift_detects_added_and_removed_cases() {
        let baseline = manifest(
            "harness-core",
            vec![case("case-keep"), case("case-removed")],
        );
        let candidate = manifest("harness-core", vec![case("case-keep"), case("case-added")]);

        let assessment =
            assess_eval_suite_drift(&baseline, &candidate, None, EvalSuiteDriftPolicy::Block);

        assert_eq!(assessment.decision, EvalSuiteDriftDecision::Block);
        assert_eq!(assessment.drift.changed_cases.len(), 2);
        assert!(assessment.drift.changed_cases.iter().any(|change| {
            change.case_id == "case-added"
                && change.kind == EvalSuiteCaseChangeKind::Added
                && change.direction == EvalSuiteChangeDirection::Strengthened
        }));
        assert!(assessment.drift.changed_cases.iter().any(|change| {
            change.case_id == "case-removed"
                && change.kind == EvalSuiteCaseChangeKind::Removed
                && change.direction == EvalSuiteChangeDirection::Weakened
        }));
        assert_eq!(
            assessment.blockers,
            vec!["suite drift requires a versioned human-approved migration record"]
        );
    }

    #[test]
    fn suite_drift_emits_weakened_commands_and_expectations() {
        let mut baseline_case = resolved_case("case-drift");
        baseline_case.verify_commands = vec!["cargo test".to_string(), "cargo clippy".to_string()];
        let mut candidate_case = pending_case("case-drift");
        candidate_case.verify_commands = vec!["cargo test".to_string()];
        let baseline = manifest("harness-core", vec![baseline_case]);
        let candidate = manifest("harness-core", vec![candidate_case]);

        let assessment = assess_eval_suite_drift(
            &baseline,
            &candidate,
            None,
            EvalSuiteDriftPolicy::NeedsHuman,
        );

        assert_eq!(assessment.decision, EvalSuiteDriftDecision::NeedsHuman);
        assert_eq!(assessment.drift.changed_commands.len(), 1);
        assert_eq!(
            assessment.drift.changed_commands[0].direction,
            EvalSuiteChangeDirection::Weakened
        );
        assert_eq!(assessment.drift.changed_expectations.len(), 1);
        assert_eq!(
            assessment.drift.changed_expectations[0].direction,
            EvalSuiteChangeDirection::Weakened
        );
    }

    #[test]
    fn suite_drift_emits_strengthened_commands_expectations_and_thresholds() {
        let mut baseline_case = pending_case("case-drift");
        baseline_case.verify_commands = vec!["cargo test".to_string()];
        baseline_case.timeout_secs = 240;
        baseline_case.resource_limits = limits(240);
        let mut candidate_case = resolved_case("case-drift");
        candidate_case.verify_commands = vec!["cargo test".to_string(), "cargo clippy".to_string()];
        candidate_case.timeout_secs = 120;
        candidate_case.resource_limits = limits(120);
        let baseline = manifest("harness-core", vec![baseline_case]);
        let candidate = manifest("harness-core", vec![candidate_case]);

        let assessment =
            assess_eval_suite_drift(&baseline, &candidate, None, EvalSuiteDriftPolicy::Block);

        assert_eq!(
            assessment.drift.changed_commands[0].direction,
            EvalSuiteChangeDirection::Strengthened
        );
        assert_eq!(
            assessment.drift.changed_expectations[0].direction,
            EvalSuiteChangeDirection::Strengthened
        );
        assert_eq!(
            assessment.drift.changed_thresholds[0].direction,
            EvalSuiteChangeDirection::Strengthened
        );
    }

    #[test]
    fn suite_drift_approved_migration_record_allows_changed_suite() {
        let baseline = manifest("harness-core", vec![case("case-removed")]);
        let candidate = manifest("harness-core-v2", vec![case("case-added")]);
        let pending =
            assess_eval_suite_drift(&baseline, &candidate, None, EvalSuiteDriftPolicy::Block);
        let migration = migration_record(&baseline, &candidate, &pending.drift_digest);

        let assessment = assess_eval_suite_drift(
            &baseline,
            &candidate,
            Some(&migration),
            EvalSuiteDriftPolicy::Block,
        );

        assert_eq!(assessment.decision, EvalSuiteDriftDecision::Approved);
        assert_eq!(
            assessment.approved_migration_id.as_deref(),
            Some("eval-suite-migration-1")
        );
        assert!(assessment.blockers.is_empty());
        assert!(assessment.drift.suite_name.is_some());
    }

    #[test]
    fn suite_drift_rejects_non_human_migration_approval() {
        let baseline = manifest("harness-core", vec![case("case-removed")]);
        let candidate = manifest("harness-core", vec![case("case-added")]);
        let pending =
            assess_eval_suite_drift(&baseline, &candidate, None, EvalSuiteDriftPolicy::Block);
        let mut migration = migration_record(&baseline, &candidate, &pending.drift_digest);
        migration.approver_kind = EvalSuiteMigrationApproverKind::Agent;

        let assessment = assess_eval_suite_drift(
            &baseline,
            &candidate,
            Some(&migration),
            EvalSuiteDriftPolicy::Block,
        );

        assert_eq!(assessment.decision, EvalSuiteDriftDecision::Block);
        assert!(assessment
            .blockers
            .contains(&"suite migration approval must come from a human approver".to_string()));
    }

    fn manifest(suite: &str, cases: Vec<EvalBenchmarkCase>) -> EvalBenchmarkManifest {
        EvalBenchmarkManifest {
            suite: suite.to_string(),
            cases,
        }
    }

    fn case(case_id: &str) -> EvalBenchmarkCase {
        EvalBenchmarkCase {
            case_id: case_id.to_string(),
            repo: "majiayu000/harness".to_string(),
            issue: 1745,
            base_commit: "0123456789abcdef".to_string(),
            verify_commands: vec!["cargo test".to_string()],
            paths: vec!["crates/harness-workflow/src/runtime/eval/report.rs".to_string()],
            risk: Some(EvalCaseRisk::Medium),
            evidence: vec!["https://github.com/majiayu000/harness/issues/1745".to_string()],
            resolution_prs: Vec::new(),
            resolution_commits: Vec::new(),
            commit_resolution: None,
            verdict: None,
            timeout_secs: 120,
            resource_limits: limits(120),
            isolation: EvalIsolationProfile::default(),
        }
    }

    fn resolved_case(case_id: &str) -> EvalBenchmarkCase {
        let mut case = case(case_id);
        case.resolution_prs = vec![1900];
        case.resolution_commits = vec!["fedcba9876543210".to_string()];
        case.commit_resolution = Some(EvalCommitResolution::Resolved);
        case.verdict = Some(EvalCaseVerdict::Replayable);
        case
    }

    fn pending_case(case_id: &str) -> EvalBenchmarkCase {
        let mut case = case(case_id);
        case.commit_resolution = Some(EvalCommitResolution::Pending);
        case.verdict = Some(EvalCaseVerdict::Pending);
        case
    }

    fn limits(timeout_secs: u64) -> CappedResourceLimits {
        ResourceLimits::evaluation_defaults(timeout_secs)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("default resource limits should be valid")
    }

    fn migration_record(
        baseline: &EvalBenchmarkManifest,
        candidate: &EvalBenchmarkManifest,
        drift_digest: &str,
    ) -> EvalSuiteMigrationRecord {
        EvalSuiteMigrationRecord {
            schema_version: EVAL_SUITE_MIGRATION_RECORD_SCHEMA_VERSION.to_string(),
            migration_id: "eval-suite-migration-1".to_string(),
            baseline_suite: baseline.suite.clone(),
            candidate_suite: candidate.suite.clone(),
            baseline_suite_digest: eval_suite_digest(baseline),
            candidate_suite_digest: eval_suite_digest(candidate),
            drift_digest: drift_digest.to_string(),
            approver_kind: EvalSuiteMigrationApproverKind::Human,
            approved_by: "maintainer".to_string(),
            approval_url: "https://github.com/majiayu000/harness/pull/1#issuecomment-1".to_string(),
            approved_at: "2026-08-12T00:00:00Z".to_string(),
            rationale: "Approved suite maintenance.".to_string(),
        }
    }
}
