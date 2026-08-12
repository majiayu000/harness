use super::report::{diff_eval_run_reports, EvalRunReport, EvalRunReportDiff};
use chrono::DateTime;
use harness_core::stack::Sha256Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

pub const EVAL_BASELINE_RECORD_SCHEMA_VERSION: &str = "eval-baseline/v0.1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalBaselineCreatorObservation {
    pub observer: String,
    pub observed_at: String,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalBaselineProvenance {
    pub suite_digest: String,
    pub stack_id: String,
    pub source_commit: String,
    pub evidence_ids: Vec<String>,
    pub creator_observation: EvalBaselineCreatorObservation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalBaselineRecord {
    pub schema_version: String,
    pub provenance: EvalBaselineProvenance,
    pub report: EvalRunReport,
    pub report_digest: String,
    pub record_digest: String,
}

#[derive(Debug, Error)]
pub enum EvalBaselineError {
    #[error("baseline record already exists; compare against it or use explicit migration")]
    BaselineAlreadyExists,
    #[error("invalid eval baseline JSON: {source}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported eval baseline schema version `{actual}`; expected `{expected}`")]
    UnsupportedSchemaVersion {
        expected: &'static str,
        actual: String,
    },
    #[error("invalid eval baseline provenance `{field}`: {reason}")]
    InvalidProvenance {
        field: &'static str,
        reason: &'static str,
    },
    #[error("invalid eval baseline report `{field}`: {reason}")]
    InvalidReport {
        field: &'static str,
        reason: &'static str,
    },
    #[error("eval baseline {field} mismatch: expected `{expected}`, got `{actual}`")]
    DigestMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("cannot compare baseline suite `{baseline}` to candidate suite `{candidate}`")]
    SuiteMismatch { baseline: String, candidate: String },
    #[error("cannot compare baseline k `{baseline}` to candidate k `{candidate}`")]
    KMismatch { baseline: u32, candidate: u32 },
    #[error("failed to serialize baseline digest payload: {source}")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
}

pub fn record_eval_baseline(
    existing: Option<&EvalBaselineRecord>,
    report: EvalRunReport,
    provenance: EvalBaselineProvenance,
) -> Result<EvalBaselineRecord, EvalBaselineError> {
    if existing.is_some() {
        return Err(EvalBaselineError::BaselineAlreadyExists);
    }
    build_eval_baseline_record(report, provenance)
}

pub fn migrate_eval_baseline_report(
    report: EvalRunReport,
    provenance: EvalBaselineProvenance,
) -> Result<EvalBaselineRecord, EvalBaselineError> {
    build_eval_baseline_record(report, provenance)
}

pub fn parse_eval_baseline_record_str(
    input: &str,
) -> Result<EvalBaselineRecord, EvalBaselineError> {
    let record: EvalBaselineRecord =
        serde_json::from_str(input).map_err(|source| EvalBaselineError::InvalidJson { source })?;
    validate_eval_baseline(&record)?;
    Ok(record)
}

pub fn validate_eval_baseline(record: &EvalBaselineRecord) -> Result<(), EvalBaselineError> {
    if record.schema_version != EVAL_BASELINE_RECORD_SCHEMA_VERSION {
        return Err(EvalBaselineError::UnsupportedSchemaVersion {
            expected: EVAL_BASELINE_RECORD_SCHEMA_VERSION,
            actual: record.schema_version.clone(),
        });
    }
    validate_provenance(&record.provenance)?;
    validate_report(&record.report)?;
    let expected_report_digest = eval_baseline_report_digest(&record.report)?;
    if record.report_digest != expected_report_digest {
        return Err(EvalBaselineError::DigestMismatch {
            field: "report_digest",
            expected: expected_report_digest,
            actual: record.report_digest.clone(),
        });
    }
    validate_prefixed_sha256(&record.record_digest, "record_digest")?;
    let expected_record_digest = eval_baseline_record_digest(
        &record.schema_version,
        &record.provenance,
        &record.report,
        &record.report_digest,
    )?;
    if record.record_digest != expected_record_digest {
        return Err(EvalBaselineError::DigestMismatch {
            field: "record_digest",
            expected: expected_record_digest,
            actual: record.record_digest.clone(),
        });
    }
    Ok(())
}

pub fn compare_eval_baseline(
    baseline: &EvalBaselineRecord,
    candidate: &EvalRunReport,
) -> Result<EvalRunReportDiff, EvalBaselineError> {
    validate_eval_baseline(baseline)?;
    if baseline.report.suite != candidate.suite {
        return Err(EvalBaselineError::SuiteMismatch {
            baseline: baseline.report.suite.clone(),
            candidate: candidate.suite.clone(),
        });
    }
    if baseline.report.k != candidate.k {
        return Err(EvalBaselineError::KMismatch {
            baseline: baseline.report.k,
            candidate: candidate.k,
        });
    }
    Ok(diff_eval_run_reports(&baseline.report, candidate))
}

pub fn eval_baseline_report_digest(report: &EvalRunReport) -> Result<String, EvalBaselineError> {
    sha256_json(report)
}

fn build_eval_baseline_record(
    report: EvalRunReport,
    provenance: EvalBaselineProvenance,
) -> Result<EvalBaselineRecord, EvalBaselineError> {
    validate_provenance(&provenance)?;
    validate_report(&report)?;
    let report_digest = eval_baseline_report_digest(&report)?;
    let record_digest = eval_baseline_record_digest(
        EVAL_BASELINE_RECORD_SCHEMA_VERSION,
        &provenance,
        &report,
        &report_digest,
    )?;
    let record = EvalBaselineRecord {
        schema_version: EVAL_BASELINE_RECORD_SCHEMA_VERSION.to_string(),
        provenance,
        report,
        report_digest,
        record_digest,
    };
    validate_eval_baseline(&record)?;
    Ok(record)
}

#[derive(Serialize)]
struct EvalBaselineRecordDigestPayload<'a> {
    schema_version: &'a str,
    provenance: &'a EvalBaselineProvenance,
    report: &'a EvalRunReport,
    report_digest: &'a str,
}

fn eval_baseline_record_digest(
    schema_version: &str,
    provenance: &EvalBaselineProvenance,
    report: &EvalRunReport,
    report_digest: &str,
) -> Result<String, EvalBaselineError> {
    sha256_json(&EvalBaselineRecordDigestPayload {
        schema_version,
        provenance,
        report,
        report_digest,
    })
}

fn sha256_json(value: &impl Serialize) -> Result<String, EvalBaselineError> {
    let encoded =
        serde_json::to_vec(value).map_err(|source| EvalBaselineError::Serialize { source })?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn validate_provenance(provenance: &EvalBaselineProvenance) -> Result<(), EvalBaselineError> {
    validate_prefixed_sha256(&provenance.suite_digest, "suite_digest")?;
    validate_non_empty_line(&provenance.stack_id, "stack_id")?;
    if !is_valid_source_commit(&provenance.source_commit) {
        return Err(EvalBaselineError::InvalidProvenance {
            field: "source_commit",
            reason: "must be a 40- or 64-character hexadecimal commit",
        });
    }
    if provenance.evidence_ids.is_empty() {
        return Err(EvalBaselineError::InvalidProvenance {
            field: "evidence_ids",
            reason: "must contain at least one evidence id",
        });
    }
    let mut seen = BTreeSet::new();
    for evidence_id in &provenance.evidence_ids {
        validate_non_empty_line(evidence_id, "evidence_ids")?;
        if !seen.insert(evidence_id.as_str()) {
            return Err(EvalBaselineError::InvalidProvenance {
                field: "evidence_ids",
                reason: "must not contain duplicates",
            });
        }
    }
    validate_non_empty_line(
        &provenance.creator_observation.observer,
        "creator_observation",
    )?;
    validate_non_empty_line(&provenance.creator_observation.note, "creator_observation")?;
    if DateTime::parse_from_rfc3339(&provenance.creator_observation.observed_at).is_err() {
        return Err(EvalBaselineError::InvalidProvenance {
            field: "creator_observation",
            reason: "observed_at must be RFC3339",
        });
    }
    Ok(())
}

fn validate_report(report: &EvalRunReport) -> Result<(), EvalBaselineError> {
    validate_non_empty_report_field(&report.run_id, "run_id")?;
    validate_non_empty_report_field(&report.suite, "suite")?;
    if report.k == 0 {
        return Err(EvalBaselineError::InvalidReport {
            field: "k",
            reason: "must be greater than zero",
        });
    }
    if report.cases.is_empty() {
        return Err(EvalBaselineError::InvalidReport {
            field: "cases",
            reason: "must contain at least one case",
        });
    }
    if report.metrics.total_cases != report.cases.len() as u64 {
        return Err(EvalBaselineError::InvalidReport {
            field: "metrics.total_cases",
            reason: "must match cases length",
        });
    }
    for case in &report.cases {
        validate_non_empty_report_field(&case.case_id, "case_id")?;
        validate_non_empty_report_field(&case.repo, "repo")?;
        validate_non_empty_report_field(&case.base_commit, "base_commit")?;
        validate_non_empty_report_field(&case.source_commit, "source_commit")?;
        if case.verify_commands.is_empty()
            || case
                .verify_commands
                .iter()
                .any(|command| command.trim().is_empty() || contains_newline(command))
        {
            return Err(EvalBaselineError::InvalidReport {
                field: "verify_commands",
                reason: "must contain single-line commands",
            });
        }
        if case.passed != matches!(case.status, super::report::EvalReportCaseStatus::Passed) {
            return Err(EvalBaselineError::InvalidReport {
                field: "passed",
                reason: "must match case status",
            });
        }
    }
    Ok(())
}

fn validate_prefixed_sha256(value: &str, field: &'static str) -> Result<(), EvalBaselineError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(EvalBaselineError::InvalidProvenance {
            field,
            reason: "must use sha256:<64-hex> format",
        });
    };
    Sha256Digest::parse(digest).map_err(|_| EvalBaselineError::InvalidProvenance {
        field,
        reason: "must use sha256:<64-hex> format",
    })?;
    Ok(())
}

fn validate_non_empty_line(value: &str, field: &'static str) -> Result<(), EvalBaselineError> {
    if value.trim().is_empty() || contains_newline(value) {
        return Err(EvalBaselineError::InvalidProvenance {
            field,
            reason: "must be a non-empty single-line value",
        });
    }
    Ok(())
}

fn validate_non_empty_report_field(
    value: &str,
    field: &'static str,
) -> Result<(), EvalBaselineError> {
    if value.trim().is_empty() {
        return Err(EvalBaselineError::InvalidReport {
            field,
            reason: "must be non-empty",
        });
    }
    Ok(())
}

fn contains_newline(value: &str) -> bool {
    value.contains('\n') || value.contains('\r')
}

fn is_valid_source_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::super::report::{
        EvalCaseInfrastructureStatus, EvalReportCase, EvalReportCaseStatus, EvalReportMetrics,
    };
    use super::*;

    #[test]
    fn record_eval_baseline_binds_provenance_and_compares() {
        let baseline = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("baseline should record");

        assert_eq!(baseline.provenance.stack_id, "agent-stack-v1");
        assert_eq!(
            baseline.provenance.evidence_ids,
            vec!["issue:1744", "run:baseline-1"]
        );
        validate_eval_baseline(&baseline).expect("recorded baseline should validate");

        let candidate = report("candidate", EvalReportCaseStatus::Failed);
        let diff = compare_eval_baseline(&baseline, &candidate).expect("baseline should compare");

        assert_eq!(diff.baseline_run_id, "baseline");
        assert_eq!(diff.candidate_run_id, "candidate");
        assert_eq!(diff.regression_ids, vec!["case-1"]);
    }

    #[test]
    fn validate_eval_baseline_rejects_report_tampering() {
        let mut baseline = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("baseline should record");
        baseline.report.cases[0].status = EvalReportCaseStatus::Failed;
        baseline.report.cases[0].passed = false;

        let error =
            validate_eval_baseline(&baseline).expect_err("tampered report must be rejected");

        assert!(matches!(
            error,
            EvalBaselineError::DigestMismatch {
                field: "report_digest",
                ..
            }
        ));
    }

    #[test]
    fn validate_eval_baseline_rejects_provenance_tampering() {
        let mut baseline = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("baseline should record");
        baseline.provenance.stack_id = "agent-stack-v2".to_string();

        let error =
            validate_eval_baseline(&baseline).expect_err("tampered provenance must be rejected");

        assert!(matches!(
            error,
            EvalBaselineError::DigestMismatch {
                field: "record_digest",
                ..
            }
        ));
    }

    #[test]
    fn record_eval_baseline_rejects_candidate_overwrite() {
        let baseline = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("baseline should record");

        let error = record_eval_baseline(
            Some(&baseline),
            report("candidate", EvalReportCaseStatus::Failed),
            provenance(),
        )
        .expect_err("candidate record must not overwrite existing baseline");

        assert!(matches!(error, EvalBaselineError::BaselineAlreadyExists));
    }

    #[test]
    fn eval_baseline_requires_complete_provenance() {
        let mut provenance = provenance();
        provenance.evidence_ids.clear();

        let error = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance,
        )
        .expect_err("missing evidence ids must be rejected");

        assert!(matches!(
            error,
            EvalBaselineError::InvalidProvenance {
                field: "evidence_ids",
                ..
            }
        ));
    }

    #[test]
    fn migrate_eval_baseline_report_requires_explicit_operation() {
        let migrated = migrate_eval_baseline_report(
            report("legacy-baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("legacy report should migrate through explicit operation");

        assert_eq!(migrated.schema_version, EVAL_BASELINE_RECORD_SCHEMA_VERSION);
        validate_eval_baseline(&migrated).expect("migrated baseline should validate");
    }

    #[test]
    fn parse_eval_baseline_record_str_rejects_incomplete_record() {
        let baseline = record_eval_baseline(
            None,
            report("baseline", EvalReportCaseStatus::Passed),
            provenance(),
        )
        .expect("baseline should record");
        let mut json =
            serde_json::to_value(&baseline).expect("baseline record should serialize to JSON");
        json.as_object_mut()
            .expect("record JSON should be an object")
            .remove("provenance");
        let input = serde_json::to_string(&json).expect("incomplete JSON should serialize");

        let error =
            parse_eval_baseline_record_str(&input).expect_err("incomplete record must fail");

        assert!(matches!(error, EvalBaselineError::InvalidJson { .. }));
    }

    fn provenance() -> EvalBaselineProvenance {
        EvalBaselineProvenance {
            suite_digest: format!("sha256:{}", "a".repeat(64)),
            stack_id: "agent-stack-v1".to_string(),
            source_commit: "1".repeat(40),
            evidence_ids: vec!["issue:1744".to_string(), "run:baseline-1".to_string()],
            creator_observation: EvalBaselineCreatorObservation {
                observer: "maintainer@example.com".to_string(),
                observed_at: "2026-08-12T20:00:00Z".to_string(),
                note: "baseline approved from collected eval evidence".to_string(),
            },
        }
    }

    fn report(run_id: &str, status: EvalReportCaseStatus) -> EvalRunReport {
        let case = EvalReportCase {
            case_id: "case-1".to_string(),
            repo: "majiayu000/harness".to_string(),
            issue: 1744,
            base_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            verify_commands: vec!["cargo test -p harness-workflow eval_baseline".to_string()],
            status,
            passed: status == EvalReportCaseStatus::Passed,
            attestation_trust: super::super::attestation::EvalAttestationTrust::Verified,
            attestation_decision: Some(
                super::super::attestation::EvalAttestationDecision::Approved,
            ),
            explicit_evidence: true,
            final_grade: None,
            failed_hard_gates: Vec::new(),
            workflow_id: Some("workflow-1".to_string()),
            terminal_state: Some("done".to_string()),
            infrastructure_status: EvalCaseInfrastructureStatus::Healthy,
            total_tokens: 100,
            cost_usd_micros: 50,
            missing_evidence: Vec::new(),
        };
        EvalRunReport {
            run_id: run_id.to_string(),
            suite: "harness-core".to_string(),
            k: 3,
            metrics: EvalReportMetrics {
                total_cases: 1,
                scored_cases: 1,
                passed_cases: u64::from(status == EvalReportCaseStatus::Passed),
                failed_cases: u64::from(status == EvalReportCaseStatus::Failed),
                skipped_cases: 0,
                pending_cases: 0,
                infra_failed_cases: 0,
                pass_at_1: if status == EvalReportCaseStatus::Passed {
                    1.0
                } else {
                    0.0
                },
                pass_to_k: if status == EvalReportCaseStatus::Passed {
                    1.0
                } else {
                    0.0
                },
                total_tokens: 100,
                avg_tokens_per_scored_case: Some(100.0),
                total_cost_usd_micros: 50,
                avg_cost_usd_micros_per_scored_case: Some(50.0),
            },
            cases: vec![case],
        }
    }
}
