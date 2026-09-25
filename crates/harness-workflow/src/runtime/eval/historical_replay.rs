use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::{error::Error, fmt};

pub const HISTORICAL_REPLAY_COHORT_SCHEMA: &str = "harness.eval.historical_replay_cohort.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayCohort {
    pub schema: String,
    pub cohort_id: String,
    pub issue_number: u64,
    pub generated_at: String,
    pub cases: Vec<HistoricalReplayCase>,
    pub verdict: HistoricalReplayCohortVerdict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayCase {
    pub case_id: String,
    pub issue: HistoricalReplayIssueSnapshot,
    pub closing_pr: HistoricalReplayPullRequestSnapshot,
    pub artifacts: Vec<String>,
    pub replay: HistoricalReplayCommandEvidence,
    pub comparison: HistoricalReplayComparison,
    pub verification: HistoricalReplayVerification,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayIssueSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub closed_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayPullRequestSnapshot {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub merged_at: String,
    pub base_ref: String,
    pub github_base_ref_oid: String,
    pub merge_parent_base_commit: String,
    pub head_commit: String,
    pub merge_commit: String,
    pub closing_issue_numbers: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayCommandEvidence {
    pub command: String,
    pub baseline: HistoricalReplayCommandRun,
    pub candidate: HistoricalReplayCommandRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayCommandRun {
    pub commit: String,
    pub exit_code: i32,
    pub tests_run: u64,
    pub status: String,
    pub output_summary: String,
    pub evidence_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayComparison {
    pub baseline_outcome: String,
    pub candidate_outcome: String,
    pub false_positives: Vec<String>,
    pub false_negatives: Vec<String>,
    pub infrastructure_failures: Vec<String>,
    pub verdict: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayVerification {
    pub closing_reference_verified: bool,
    pub commit_objects_verified: bool,
    pub ancestry_verified: bool,
    pub command_digests_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalReplayCohortVerdict {
    pub cases_total: u64,
    pub candidate_passed: u64,
    pub baseline_false_positives: u64,
    pub infrastructure_failures: u64,
    pub verdict: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoricalReplayError {
    message: String,
}

impl HistoricalReplayError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HistoricalReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for HistoricalReplayError {}

pub fn parse_historical_replay_cohort_str(
    input: &str,
) -> Result<HistoricalReplayCohort, HistoricalReplayError> {
    let cohort = serde_json::from_str::<HistoricalReplayCohort>(input)
        .map_err(|error| HistoricalReplayError::new(format!("invalid JSON: {error}")))?;
    validate_historical_replay_cohort(&cohort)?;
    Ok(cohort)
}

pub fn validate_historical_replay_cohort(
    cohort: &HistoricalReplayCohort,
) -> Result<(), HistoricalReplayError> {
    if cohort.schema != HISTORICAL_REPLAY_COHORT_SCHEMA {
        return Err(HistoricalReplayError::new(format!(
            "unsupported historical replay schema: {}",
            cohort.schema
        )));
    }
    non_empty(&cohort.cohort_id, "cohort_id")?;
    non_empty(&cohort.generated_at, "generated_at")?;
    if cohort.issue_number == 0 {
        return Err(HistoricalReplayError::new(
            "issue_number must be greater than zero",
        ));
    }
    if cohort.cases.is_empty() {
        return Err(HistoricalReplayError::new(
            "historical replay cohort must include at least one case",
        ));
    }

    let mut seen_cases = BTreeSet::new();
    let mut candidate_passed = 0;
    let mut baseline_false_positives = 0;
    let mut infrastructure_failures = 0;
    for case in &cohort.cases {
        validate_case(case)?;
        if !seen_cases.insert(case.case_id.as_str()) {
            return Err(HistoricalReplayError::new(format!(
                "duplicate historical replay case_id: {}",
                case.case_id
            )));
        }
        if case.replay.candidate.exit_code == 0 && case.replay.candidate.tests_run > 0 {
            candidate_passed += 1;
        }
        if !case.comparison.false_positives.is_empty() {
            baseline_false_positives += 1;
        }
        if !case.comparison.infrastructure_failures.is_empty() {
            infrastructure_failures += 1;
        }
    }

    validate_count(
        cohort.verdict.cases_total,
        cohort.cases.len() as u64,
        "verdict.cases_total",
    )?;
    validate_count(
        cohort.verdict.candidate_passed,
        candidate_passed,
        "verdict.candidate_passed",
    )?;
    validate_count(
        cohort.verdict.baseline_false_positives,
        baseline_false_positives,
        "verdict.baseline_false_positives",
    )?;
    validate_count(
        cohort.verdict.infrastructure_failures,
        infrastructure_failures,
        "verdict.infrastructure_failures",
    )?;
    non_empty(&cohort.verdict.verdict, "verdict.verdict")?;

    Ok(())
}

pub fn historical_replay_command_digest(
    case_id: &str,
    phase: &str,
    command: &str,
    run: &HistoricalReplayCommandRun,
) -> String {
    let mut hasher = Sha256::new();
    let exit_code = run.exit_code.to_string();
    let tests_run = run.tests_run.to_string();
    for part in [
        "harness.historical-replay.command.v1",
        case_id,
        phase,
        command,
        &run.commit,
        &exit_code,
        &tests_run,
        &run.status,
        &run.output_summary,
    ] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    hex
}

fn validate_case(case: &HistoricalReplayCase) -> Result<(), HistoricalReplayError> {
    non_empty(&case.case_id, "case_id")?;
    if case.issue.number == 0 {
        return Err(HistoricalReplayError::new("issue.number must be nonzero"));
    }
    non_empty(&case.issue.title, "issue.title")?;
    non_empty(&case.issue.url, "issue.url")?;
    if case.issue.state != "closed" {
        return Err(HistoricalReplayError::new(format!(
            "{} issue state must be closed",
            case.case_id
        )));
    }
    non_empty(&case.issue.closed_at, "issue.closed_at")?;

    let pr = &case.closing_pr;
    if pr.number == 0 {
        return Err(HistoricalReplayError::new(
            "closing_pr.number must be nonzero",
        ));
    }
    non_empty(&pr.title, "closing_pr.title")?;
    non_empty(&pr.url, "closing_pr.url")?;
    if pr.state != "merged" {
        return Err(HistoricalReplayError::new(format!(
            "{} closing PR must be merged",
            case.case_id
        )));
    }
    non_empty(&pr.merged_at, "closing_pr.merged_at")?;
    non_empty(&pr.base_ref, "closing_pr.base_ref")?;
    validate_sha1(&pr.github_base_ref_oid, "closing_pr.github_base_ref_oid")?;
    validate_sha1(
        &pr.merge_parent_base_commit,
        "closing_pr.merge_parent_base_commit",
    )?;
    validate_sha1(&pr.head_commit, "closing_pr.head_commit")?;
    validate_sha1(&pr.merge_commit, "closing_pr.merge_commit")?;
    if !pr.closing_issue_numbers.contains(&case.issue.number) {
        return Err(HistoricalReplayError::new(format!(
            "{} closing PR does not reference issue {}",
            case.case_id, case.issue.number
        )));
    }
    if case.artifacts.is_empty() {
        return Err(HistoricalReplayError::new(format!(
            "{} must retain at least one artifact path",
            case.case_id
        )));
    }
    for artifact in &case.artifacts {
        non_empty(artifact, "artifact path")?;
    }

    let replay = &case.replay;
    non_empty(&replay.command, "replay.command")?;
    validate_run_digest(case, "baseline", &replay.baseline)?;
    validate_run_digest(case, "candidate", &replay.candidate)?;
    validate_declared_run_outcome(
        &case.case_id,
        "baseline",
        "comparison.baseline_outcome",
        &case.comparison.baseline_outcome,
        &replay.baseline,
    )?;
    validate_declared_run_outcome(
        &case.case_id,
        "candidate",
        "comparison.candidate_outcome",
        &case.comparison.candidate_outcome,
        &replay.candidate,
    )?;
    if replay.baseline.commit != pr.merge_parent_base_commit {
        return Err(HistoricalReplayError::new(format!(
            "{} baseline commit does not match merge parent",
            case.case_id
        )));
    }
    if replay.candidate.commit != pr.merge_commit {
        return Err(HistoricalReplayError::new(format!(
            "{} candidate commit does not match merge commit",
            case.case_id
        )));
    }
    if replay.candidate.exit_code != 0 || replay.candidate.tests_run == 0 {
        return Err(HistoricalReplayError::new(format!(
            "{} candidate replay must pass at least one test",
            case.case_id
        )));
    }
    if replay.baseline.exit_code == 0
        && replay.baseline.tests_run == 0
        && case.comparison.false_positives.is_empty()
    {
        return Err(HistoricalReplayError::new(format!(
            "{} baseline zero-test success must be recorded as a false positive",
            case.case_id
        )));
    }
    non_empty(
        &case.comparison.baseline_outcome,
        "comparison.baseline_outcome",
    )?;
    non_empty(
        &case.comparison.candidate_outcome,
        "comparison.candidate_outcome",
    )?;
    non_empty(&case.comparison.verdict, "comparison.verdict")?;
    if !case.comparison.false_negatives.is_empty() {
        return Err(HistoricalReplayError::new(format!(
            "{} retained unexpected false negatives",
            case.case_id
        )));
    }
    if !case.verification.closing_reference_verified
        || !case.verification.commit_objects_verified
        || !case.verification.ancestry_verified
        || !case.verification.command_digests_verified
    {
        return Err(HistoricalReplayError::new(format!(
            "{} verification flags must all be true",
            case.case_id
        )));
    }
    non_empty(&case.summary, "summary")?;
    Ok(())
}

fn validate_declared_run_outcome(
    case_id: &str,
    phase: &str,
    outcome_field: &str,
    declared_outcome: &str,
    run: &HistoricalReplayCommandRun,
) -> Result<(), HistoricalReplayError> {
    let expected = expected_run_outcome(run);
    if run.status != expected {
        return Err(HistoricalReplayError::new(format!(
            "{case_id} {phase} status `{}` contradicts command result; expected `{expected}`",
            run.status
        )));
    }
    if declared_outcome != expected {
        return Err(HistoricalReplayError::new(format!(
            "{case_id} {outcome_field} `{declared_outcome}` contradicts command result; expected `{expected}`"
        )));
    }
    Ok(())
}

fn expected_run_outcome(run: &HistoricalReplayCommandRun) -> &'static str {
    if run.exit_code == 0 {
        if run.tests_run > 0 {
            "passed"
        } else {
            "passed_zero_tests"
        }
    } else {
        "failed"
    }
}

fn validate_run_digest(
    case: &HistoricalReplayCase,
    phase: &str,
    run: &HistoricalReplayCommandRun,
) -> Result<(), HistoricalReplayError> {
    validate_sha1(&run.commit, "command run commit")?;
    non_empty(&run.status, "command run status")?;
    non_empty(&run.output_summary, "command run output_summary")?;
    validate_sha256(&run.evidence_sha256, "command run evidence_sha256")?;
    let expected =
        historical_replay_command_digest(&case.case_id, phase, &case.replay.command, run);
    if run.evidence_sha256 != expected {
        return Err(HistoricalReplayError::new(format!(
            "{} {phase} command digest mismatch",
            case.case_id
        )));
    }
    Ok(())
}

fn validate_count(actual: u64, expected: u64, field: &str) -> Result<(), HistoricalReplayError> {
    if actual != expected {
        return Err(HistoricalReplayError::new(format!(
            "{field} mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn non_empty(value: &str, field: &str) -> Result<(), HistoricalReplayError> {
    if value.trim().is_empty() {
        return Err(HistoricalReplayError::new(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

fn validate_sha1(value: &str, field: &str) -> Result<(), HistoricalReplayError> {
    validate_hex_digest(value, 40, field)
}

fn validate_sha256(value: &str, field: &str) -> Result<(), HistoricalReplayError> {
    validate_hex_digest(value, 64, field)
}

fn validate_hex_digest(value: &str, len: usize, field: &str) -> Result<(), HistoricalReplayError> {
    if value.len() != len || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(HistoricalReplayError::new(format!(
            "{field} must be a {len}-character hex digest"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASC_030_COHORT: &str =
        include_str!("../../../../../evals/historical-replay/asc-030-cohort.json");

    #[test]
    fn historical_replay_asc030_cohort_evidence_validates() {
        let cohort = parse_historical_replay_cohort_str(ASC_030_COHORT)
            .expect("ASC-030 cohort should validate");

        assert_eq!(cohort.issue_number, 1759);
        assert_eq!(cohort.cases.len(), 3);
        assert!(cohort
            .cases
            .iter()
            .any(|case| case.case_id == "gh1715-lifecycle-transitions"));
        assert!(cohort
            .cases
            .iter()
            .any(|case| case.case_id == "gh1716-recovery-cas"));
        assert!(cohort
            .cases
            .iter()
            .any(|case| case.case_id == "gh1707-coverage-recovery"));
    }

    #[test]
    fn historical_replay_rejects_candidate_status_that_contradicts_passing_run() {
        let mut cohort: serde_json::Value =
            serde_json::from_str(ASC_030_COHORT).expect("fixture should be JSON");
        cohort["cases"][0]["replay"]["candidate"]["status"] = "failed".into();
        let case_id = cohort["cases"][0]["case_id"]
            .as_str()
            .expect("case id")
            .to_string();
        let command = cohort["cases"][0]["replay"]["command"]
            .as_str()
            .expect("command")
            .to_string();
        let run = serde_json::from_value::<HistoricalReplayCommandRun>(
            cohort["cases"][0]["replay"]["candidate"].clone(),
        )
        .expect("candidate run should deserialize");
        cohort["cases"][0]["replay"]["candidate"]["evidence_sha256"] =
            historical_replay_command_digest(&case_id, "candidate", &command, &run).into();

        let input = serde_json::to_string(&cohort).expect("fixture should serialize");
        let error = parse_historical_replay_cohort_str(&input)
            .expect_err("contradictory candidate status should fail validation");

        assert!(error.to_string().contains("candidate status"));
    }

    #[test]
    fn historical_replay_rejects_candidate_outcome_that_contradicts_passing_run() {
        let mut cohort: serde_json::Value =
            serde_json::from_str(ASC_030_COHORT).expect("fixture should be JSON");
        cohort["cases"][0]["comparison"]["candidate_outcome"] = "failed".into();

        let input = serde_json::to_string(&cohort).expect("fixture should serialize");
        let error = parse_historical_replay_cohort_str(&input)
            .expect_err("contradictory candidate outcome should fail validation");

        assert!(error.to_string().contains("comparison.candidate_outcome"));
    }
}
