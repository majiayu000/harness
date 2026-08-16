use super::super::attestation::EvalAttestationSummary;
use super::super::manifest::{
    EvalBenchmarkCase, EvalCaseVerdict, EvalCommitResolution, EvalVerifyCommandMode,
};
use super::super::model::RuntimeSnapshot;
use super::super::EvalCaseEvidence;
use super::*;
use crate::runtime::{
    parse_benchmark_manifest_str, EvalQualityGateEvidence, EvalTrustedVerifier,
    EvalValidationCommandEvidence,
};

#[test]
fn historical_replay_manifest_pending_case_stays_pending_without_evidence() {
    let manifest = manifest_with_case(non_replayable_case());

    let report =
        eval_report_from_evidence(&manifest, "run-1", 1, Vec::new()).expect("report builds");

    assert_eq!(report.cases[0].status, EvalReportCaseStatus::Pending);
    assert!(!report.cases[0].passed);
    assert_eq!(report.metrics.pending_cases, 1);
    assert_eq!(report.metrics.failed_cases, 0);
    assert_eq!(
        report.cases[0].missing_evidence,
        vec!["commit_resolution is pending"]
    );
    assert_eq!(report.outcome, Some(EvalRunOutcome::Incomplete));
}

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
    assert_eq!(report.outcome, Some(EvalRunOutcome::Incomplete));

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
fn eval_execute_timeout_is_a_scored_failure() {
    let report = eval_report_from_evidence(
        &manifest(&["case-fail"]),
        "candidate",
        1,
        vec![evidence(
            "case-fail",
            EvalEvidenceStatus::TimedOut,
            vec!["case_timeout".to_string()],
            None,
        )],
    )
    .expect("report should build");

    assert_eq!(report.cases[0].status, EvalReportCaseStatus::Failed);
    assert_eq!(report.metrics.scored_cases, 1);
    assert_eq!(report.metrics.failed_cases, 1);
    assert_eq!(report.metrics.infra_failed_cases, 0);
}

#[test]
fn eval_execute_verification_failure_is_scored() {
    let report = eval_report_from_evidence(
        &manifest(&["case-fail"]),
        "candidate",
        1,
        vec![evidence(
            "case-fail",
            EvalEvidenceStatus::Failed,
            vec!["quality_gate_pass".to_string()],
            Some("failed"),
        )],
    )
    .expect("report should build");

    assert_eq!(report.cases[0].status, EvalReportCaseStatus::Failed);
    assert_eq!(report.metrics.scored_cases, 1);
    assert_eq!(report.metrics.failed_cases, 1);
}

#[test]
fn eval_execute_infrastructure_statuses_are_not_scored() {
    for status in [
        EvalEvidenceStatus::DispatchFailed,
        EvalEvidenceStatus::EvidenceIncomplete,
    ] {
        let report = eval_report_from_evidence(
            &manifest(&["case-fail"]),
            "candidate",
            1,
            vec![evidence(
                "case-fail",
                status,
                vec!["case_timeout".to_string()],
                None,
            )],
        )
        .expect("report should build");

        assert_eq!(report.cases[0].status, EvalReportCaseStatus::InfraFailed);
        assert_eq!(report.metrics.scored_cases, 0);
        assert_eq!(report.metrics.failed_cases, 0);
        assert_eq!(report.metrics.infra_failed_cases, 1);
        assert_eq!(report.outcome, Some(EvalRunOutcome::Incomplete));
    }
}

#[test]
fn eval_budget_exhaustion_has_an_explicit_non_scored_status() {
    let report = eval_report_from_evidence(
        &manifest(&["case-budget"]),
        "candidate",
        1,
        vec![evidence(
            "case-budget",
            EvalEvidenceStatus::BudgetExhausted,
            vec!["suite_usage_ceiling_exceeded".to_string()],
            None,
        )],
    )
    .expect("report should build");

    assert_eq!(report.cases[0].status, EvalReportCaseStatus::InfraFailed);
    assert_eq!(
        report.cases[0].outcome,
        Some(EvalReportCaseOutcome::BudgetExhausted)
    );
    assert_eq!(report.metrics.scored_cases, 0);
    assert_eq!(report.metrics.failed_cases, 0);
    assert_eq!(report.metrics.infra_failed_cases, 1);
    assert_eq!(report.outcome, Some(EvalRunOutcome::BudgetExhausted));
}

#[test]
fn budget_report_remains_readable_by_legacy_case_status_consumers() {
    #[derive(serde::Deserialize)]
    struct LegacyReport {
        cases: Vec<LegacyCase>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyCase {
        status: LegacyCaseStatus,
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyCaseStatus {
        Pending,
        Passed,
        Failed,
        Skipped,
        InfraFailed,
    }

    let report = eval_report_from_evidence(
        &manifest(&["case-budget"]),
        "candidate",
        1,
        vec![evidence(
            "case-budget",
            EvalEvidenceStatus::BudgetExhausted,
            vec!["suite_usage_ceiling_exceeded".to_string()],
            None,
        )],
    )
    .expect("report should build");
    let legacy: LegacyReport =
        serde_json::from_value(serde_json::to_value(report).expect("report should serialize"))
            .expect("legacy consumer should ignore additive outcome fields");

    assert_eq!(legacy.cases[0].status, LegacyCaseStatus::InfraFailed);
}

#[test]
fn passed_then_budget_exhausted_run_is_explicitly_incomplete() {
    let report = eval_report_from_evidence(
        &manifest(&["case-pass", "case-budget"]),
        "candidate",
        1,
        vec![
            evidence("case-pass", EvalEvidenceStatus::Passed, Vec::new(), None),
            evidence(
                "case-budget",
                EvalEvidenceStatus::BudgetExhausted,
                vec!["suite_usage_ceiling_exceeded".to_string()],
                None,
            ),
        ],
    )
    .expect("report should build");

    assert_eq!(report.metrics.pass_at_1, 1.0);
    assert_eq!(report.outcome, Some(EvalRunOutcome::BudgetExhausted));
}

#[test]
fn effective_outcome_reconstructs_legacy_incomplete_reports() {
    let report = report(
        "legacy-incomplete",
        vec![case("case-infra", EvalReportCaseStatus::InfraFailed)],
    );

    assert_eq!(report.outcome, None);
    assert_eq!(
        eval_report_effective_outcome(&report),
        Some(EvalRunOutcome::Incomplete)
    );
}

#[test]
fn gh1454_report_and_diff_retain_verifier_provenance() {
    let manifest = parse_benchmark_manifest_str(
        r#"
suite = "trusted"

[[cases]]
case_id = "gh1454-scoped-ci-jobs"
repo = "majiayu000/harness"
issue = 1454
base_commit = "9c0099ad458e82fd377fd20a8e288a46722762ef"
"#,
    )
    .expect("trusted manifest should parse");
    let verifier = EvalTrustedVerifier::Gh1454CiContractV1;
    let mut case_evidence = evidence(
        "gh1454-scoped-ci-jobs",
        EvalEvidenceStatus::Passed,
        vec![],
        None,
    );
    case_evidence.quality_gate = Some(EvalQualityGateEvidence {
        command_id: Some("command-1".to_string()),
        runtime_job_id: Some("job-1".to_string()),
        status: "succeeded".to_string(),
        validation_passed: true,
        validation_commands: Vec::new(),
        validation_evidence: vec![EvalValidationCommandEvidence {
            command: "harness eval verify-trusted".to_string(),
            argv: verifier.validation_argv(),
            exit_code: Some(0),
            output_sha256: Some("a".repeat(64)),
            duration_ms: Some(10),
            verifier_id: Some(verifier.id().to_string()),
            verifier_sha256: Some(verifier.sha256().to_string()),
        }],
    });

    let candidate = eval_report_from_evidence(&manifest, "candidate", 1, vec![case_evidence])
        .expect("report should build");
    let baseline = eval_report_dry_run(&manifest, "baseline", 1).expect("baseline should build");
    let diff = diff_eval_run_reports(&baseline, &candidate);

    assert_eq!(
        candidate.cases[0].verification_evidence[0]
            .verifier_sha256
            .as_deref(),
        Some(verifier.sha256())
    );
    assert_eq!(
        diff.transitions[0].candidate_verification_evidence[0]
            .output_sha256
            .as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );

    let mut old_json = serde_json::to_value(&candidate).expect("report should serialize");
    old_json
        .as_object_mut()
        .expect("report object")
        .remove("outcome");
    old_json["cases"][0]
        .as_object_mut()
        .expect("case object")
        .remove("verification_evidence");
    let old_report: EvalRunReport =
        serde_json::from_value(old_json).expect("older report should remain readable");
    assert!(old_report.cases[0].verification_evidence.is_empty());
}

#[test]
fn historical_replay_manifest_rejects_evidence_for_pending_case() {
    let manifest = manifest_with_case(non_replayable_case());

    let err =
        eval_report_from_evidence(&manifest, "run-1", 1, vec![replay_evidence("pending-case")])
            .expect_err("pending case evidence must fail");

    assert!(err.to_string().contains("non-replayable case_id"));
    assert!(err.to_string().contains("commit_resolution is pending"));
}

fn manifest_with_case(case: EvalBenchmarkCase) -> EvalBenchmarkManifest {
    EvalBenchmarkManifest {
        suite: "harness-historical-replay".to_string(),
        cases: vec![case],
    }
}

fn non_replayable_case() -> EvalBenchmarkCase {
    EvalBenchmarkCase {
        case_id: "pending-case".to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test".to_string()],
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: Some(EvalCommitResolution::Pending),
        verdict: Some(EvalCaseVerdict::Pending),
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())
            .expect("default resource limits should be valid"),
        isolation: crate::runtime::eval::manifest::EvalIsolationProfile::default(),
    }
}

fn replay_evidence(case_id: &str) -> EvalCaseEvidence {
    EvalCaseEvidence {
        eval_run_id: "run-1".to_string(),
        case_id: case_id.to_string(),
        workflow_id: Some("workflow-1".to_string()),
        status: EvalEvidenceStatus::Passed,
        runtime: None,
        usage: Vec::new(),
        submission: None,
        attestation: EvalAttestationSummary::unsigned(),
        quality_gate: None,
        quality: None,
        isolation: None,
        missing_evidence: Vec::new(),
    }
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
                verify_command_mode: EvalVerifyCommandMode::Argv,
                paths: Vec::new(),
                risk: None,
                evidence: Vec::new(),
                resolution_prs: Vec::new(),
                resolution_commits: Vec::new(),
                commit_resolution: None,
                verdict: None,
                timeout_secs: 3600,
                resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(3600)
                    .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())
                    .expect("default resource limits should be valid"),
                isolation: crate::runtime::eval::manifest::EvalIsolationProfile::default(),
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
        attestation: EvalAttestationSummary::unsigned(),
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
        quality: None,
        isolation: None,
        missing_evidence,
    }
}

fn report(run_id: &str, cases: Vec<EvalReportCase>) -> EvalRunReport {
    EvalRunReport {
        run_id: run_id.to_string(),
        suite: "harness-core".to_string(),
        k: 3,
        metrics: metrics_for_cases(3, &cases),
        outcome: None,
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
        verification_evidence: Vec::new(),
        attestation_trust: EvalAttestationTrust::Unsigned,
        attestation_decision: None,
        status,
        outcome: None,
        passed: status == EvalReportCaseStatus::Passed,
        explicit_evidence,
        final_grade: None,
        failed_hard_gates: Vec::new(),
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
