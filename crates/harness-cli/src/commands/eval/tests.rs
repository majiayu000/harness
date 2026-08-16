use super::super::{Cli, Command};
use super::*;
use clap::Parser;
use harness_workflow::runtime::eval::model::{Confidence, EvalGrade, HardGateName, UsageSnapshot};
use harness_workflow::runtime::{
    EvalAttestationSummary, EvalEvidenceStatus, EvalIsolationEvidence, EvalQualityGateEvidence,
    EvalReportCase, EvalReportFailedGate, EvalReportMetrics, EvalSubmissionEvidence,
};

fn sample_eval_manifest() -> EvalBenchmarkManifest {
    parse_benchmark_manifest_str(
        r#"
suite = "harness-core"

[[cases]]
case_id = "case-pass"
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test -p harness-workflow eval_report"]

[[cases]]
case_id = "case-fail"
repo = "majiayu000/harness"
issue = 1447
base_commit = "69f5e113"
verify_commands = ["cargo test -p harness-cli eval_report"]
"#,
    )
    .unwrap_or_else(|error| panic!("manifest should parse: {error}"))
}

#[test]
fn eval_report_cli_parses_run_and_diff_commands() {
    let cli = Cli::try_parse_from([
        "harness",
        "eval",
        "run",
        "--manifest",
        "evals/benchmarks/harness-core.toml",
        "--evidence",
        "evidence.json",
        "--run-id",
        "run-1",
        "--k",
        "5",
        "--json",
        "--output",
        "report.json",
    ])
    .unwrap_or_else(|error| panic!("eval run command should parse: {error}"));
    match cli.command {
        Command::Eval {
            cmd: EvalCommand::Run(args),
        } => {
            assert_eq!(
                args.manifest,
                PathBuf::from("evals/benchmarks/harness-core.toml")
            );
            assert_eq!(args.evidence, Some(PathBuf::from("evidence.json")));
            assert_eq!(args.run_id.as_deref(), Some("run-1"));
            assert_eq!(args.k, 5);
            assert!(!args.execute);
            assert!(args.json);
            assert_eq!(args.output, Some(PathBuf::from("report.json")));
        }
        _ => panic!("expected eval run command"),
    }

    let cli = Cli::try_parse_from([
        "harness",
        "eval",
        "run",
        "--manifest",
        "evals/benchmarks/harness-core.toml",
        "--execute",
        "--case-timeout-secs",
        "30",
        "--poll-interval-ms",
        "250",
        "--dispatch-timeout-secs",
        "12",
        "--max-total-tokens",
        "10000",
        "--max-cost-usd-micros",
        "500000",
    ])
    .unwrap_or_else(|error| panic!("eval execute command should parse: {error}"));
    match cli.command {
        Command::Eval {
            cmd: EvalCommand::Run(args),
        } => {
            assert!(args.execute);
            assert_eq!(args.case_timeout_secs, Some(30));
            assert_eq!(args.poll_interval_ms, 250);
            assert_eq!(args.dispatch_timeout_secs, 12);
            assert_eq!(args.max_total_tokens, Some(10_000));
            assert_eq!(args.max_cost_usd_micros, Some(500_000));
        }
        _ => panic!("expected eval run command"),
    }

    let cli = Cli::try_parse_from([
        "harness",
        "eval",
        "diff",
        "baseline.json",
        "candidate.json",
        "--max-pass-drop",
        "0.1",
        "--fail-on-new-f-gate",
        "--json",
    ])
    .unwrap_or_else(|error| panic!("eval diff command should parse: {error}"));
    match cli.command {
        Command::Eval {
            cmd: EvalCommand::Diff(args),
        } => {
            assert_eq!(args.baseline, PathBuf::from("baseline.json"));
            assert_eq!(args.candidate, PathBuf::from("candidate.json"));
            assert_eq!(args.max_pass_drop, Some(0.1));
            assert!(args.fail_on_new_f_gate);
            assert!(args.json);
        }
        _ => panic!("expected eval diff command"),
    }
}

#[test]
fn eval_usage_ceiling_requires_execute_mode() {
    let error = match Cli::try_parse_from([
        "harness",
        "eval",
        "run",
        "--manifest",
        "evals/benchmarks/harness-core.toml",
        "--dry-run",
        "--max-total-tokens",
        "1000",
    ]) {
        Ok(_) => panic!("suite usage ceilings must only apply to live execution"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("--execute"));
}

#[test]
fn eval_report_dry_run_text_lists_manifest_cases() {
    let report = eval_report_dry_run(&sample_eval_manifest(), "run-dry", 3)
        .unwrap_or_else(|error| panic!("dry run report should build: {error}"));
    let rendered = render_run_report(&report);

    assert!(rendered.contains("pass@1: 0.0000"));
    assert!(rendered.contains("pass^3: 0.0000"));
    assert!(rendered.contains("case-pass"));
    assert!(rendered.contains("status=pending"));
}

#[test]
fn eval_report_rejects_invalid_k_values() {
    let zero_error = match eval_report_dry_run(&sample_eval_manifest(), "run-zero", 0) {
        Ok(_) => panic!("zero k should be rejected"),
        Err(error) => error,
    };
    assert!(zero_error.to_string().contains("greater than zero"));

    let excessive_k = i32::MAX as u32 + 1;
    let excessive_error =
        match eval_report_dry_run(&sample_eval_manifest(), "run-excessive", excessive_k) {
            Ok(_) => panic!("k values above i32::MAX should be rejected"),
            Err(error) => error,
        };
    assert!(excessive_error
        .to_string()
        .contains("less than or equal to i32::MAX"));
}

#[test]
fn eval_report_evidence_text_includes_pass_cost_and_tokens() {
    let report = eval_report_from_evidence(
        &sample_eval_manifest(),
        "run-1",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(120, 50)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("evidence report should build: {error}"));
    let rendered = render_run_report(&report);

    assert_eq!(report.metrics.total_cases, 2);
    assert_eq!(report.metrics.scored_cases, 1);
    assert_eq!(report.metrics.passed_cases, 1);
    assert_eq!(report.metrics.failed_cases, 0);
    assert_eq!(report.metrics.skipped_cases, 1);
    assert_eq!(report.metrics.total_tokens, 120);
    assert_eq!(report.metrics.total_cost_usd_micros, 50);
    assert!(rendered.contains("pass@1: 1.0000"));
    assert!(rendered.contains("pass^3: 1.0000"));
    assert!(rendered.contains("status=skipped"));
    assert!(rendered.contains("attestation=unsigned"));
    assert!(rendered.contains("missing_evidence: case_evidence"));
}

#[test]
fn eval_report_text_distinguishes_verified_rejections() {
    let mut report = eval_report_dry_run(&sample_eval_manifest(), "run-dry", 3)
        .unwrap_or_else(|error| panic!("dry run report should build: {error}"));
    report.cases[0].attestation_trust = EvalAttestationTrust::Verified;
    report.cases[0].attestation_decision = Some(EvalAttestationDecision::Rejected);

    let rendered = render_run_report(&report);

    assert!(rendered.contains("attestation=verified:rejected"));
}

#[test]
fn eval_report_marks_missing_runtime_evidence_as_infra_failure() {
    let report = eval_report_from_evidence(
        &sample_eval_manifest(),
        "run-1",
        3,
        vec![
            case_evidence(
                "case-pass",
                EvalEvidenceStatus::Failed,
                vec![usage_snapshot(10, 5)],
                vec!["terminal_runtime_state".to_string()],
            ),
            case_evidence(
                "case-fail",
                EvalEvidenceStatus::Passed,
                vec![usage_snapshot(20, 10)],
                Vec::new(),
            ),
        ],
    )
    .unwrap_or_else(|error| panic!("evidence report should build: {error}"));
    let rendered = render_run_report(&report);

    assert_eq!(report.metrics.scored_cases, 1);
    assert_eq!(report.metrics.passed_cases, 1);
    assert_eq!(report.metrics.failed_cases, 0);
    assert_eq!(report.metrics.infra_failed_cases, 1);
    assert_eq!(report.metrics.pass_at_1, 1.0);
    assert!(rendered.contains("status=infra_failed"));
}

#[test]
fn eval_report_diff_text_includes_status_transitions() {
    let baseline = eval_report_from_evidence(
        &sample_eval_manifest(),
        "baseline",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(100, 40)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    let candidate = eval_report_from_evidence(
        &sample_eval_manifest(),
        "candidate",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Failed,
            vec![usage_snapshot(80, 30)],
            vec!["quality_gate_pass".to_string()],
        )],
    )
    .unwrap_or_else(|error| panic!("candidate report should build: {error}"));
    let diff = diff_eval_run_reports(&baseline, &candidate);
    let rendered = render_diff_report(&diff);

    assert!(rendered.contains("pass_to_fail"));
    assert!(rendered.contains("regressions: count=1 ids=case-pass"));
    assert!(rendered.contains("tokens delta: -20"));
    assert!(rendered.contains("cost_usd_micros delta: -10"));
}

#[test]
fn eval_report_diff_text_and_json_include_attestation_changes() {
    let mut baseline = eval_report_from_evidence(
        &sample_eval_manifest(),
        "baseline",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(100, 40)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    baseline.cases[0].attestation_trust = EvalAttestationTrust::Verified;
    baseline.cases[0].attestation_decision = Some(EvalAttestationDecision::Approved);

    let candidate = eval_report_from_evidence(
        &sample_eval_manifest(),
        "candidate",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(100, 40)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("candidate report should build: {error}"));

    let diff = diff_eval_run_reports(&baseline, &candidate);
    let transition = diff
        .transitions
        .iter()
        .find(|transition| transition.case_id == "case-pass")
        .expect("case-pass transition should exist");
    assert_eq!(
        transition.baseline_attestation_trust,
        Some(EvalAttestationTrust::Verified)
    );
    assert_eq!(
        transition.candidate_attestation_trust,
        Some(EvalAttestationTrust::Unsigned)
    );
    assert_eq!(
        transition.baseline_attestation_decision,
        Some(EvalAttestationDecision::Approved)
    );

    let rendered = render_diff_report(&diff);

    assert!(rendered.contains("case-pass unchanged_pass"));
    assert!(rendered.contains("attestation=verified:approved->unsigned"));
}

#[test]
fn eval_report_evidence_input_downgrades_forged_verified_summary() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let evidence_path = tempdir.path().join("evidence.json");
    fs::write(
        &evidence_path,
        r#"[
            {
                "eval_run_id": "run-1",
                "case_id": "case-pass",
                "workflow_id": "workflow-case-pass",
                "status": "passed",
                "attestation": {
                    "trust": "verified",
                    "provider": "offline-oidc",
                    "decision": "approved"
                },
                "runtime": null,
                "usage": [],
                "submission": null,
                "quality_gate": null,
                "missing_evidence": []
            }
        ]"#,
    )
    .unwrap_or_else(|error| panic!("evidence should write: {error}"));

    let evidence = read_evidence(&evidence_path)
        .unwrap_or_else(|error| panic!("evidence should parse: {error}"));
    let report = eval_report_from_evidence(&sample_eval_manifest(), "run-1", 3, evidence)
        .unwrap_or_else(|error| panic!("report should build: {error}"));

    assert_eq!(
        report.cases[0].attestation_trust,
        EvalAttestationTrust::Unverified
    );
    assert_eq!(
        report.cases[0].attestation_decision,
        Some(EvalAttestationDecision::Approved)
    );
    assert!(!render_run_report(&report).contains("attestation=verified"));
}

#[test]
fn eval_report_diff_rejects_suite_or_k_mismatch() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let baseline_path = tempdir.path().join("baseline.json");
    let candidate_path = tempdir.path().join("candidate.json");
    let baseline = eval_report_dry_run(&sample_eval_manifest(), "baseline", 3)
        .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    let mut candidate = baseline.clone();
    candidate.run_id = "candidate".to_string();
    candidate.suite = "different-suite".to_string();
    fs::write(
        &baseline_path,
        serde_json::to_string_pretty(&baseline)
            .unwrap_or_else(|error| panic!("baseline should serialize: {error}")),
    )
    .unwrap_or_else(|error| panic!("baseline should write: {error}"));
    fs::write(
        &candidate_path,
        serde_json::to_string_pretty(&candidate)
            .unwrap_or_else(|error| panic!("candidate should serialize: {error}")),
    )
    .unwrap_or_else(|error| panic!("candidate should write: {error}"));

    let error = match diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path.clone(),
        candidate: candidate_path.clone(),
        max_pass_drop: None,
        fail_on_new_f_gate: false,
        json: false,
        output: None,
    }) {
        Ok(_) => panic!("different suites should be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("different suites"));

    candidate.suite = baseline.suite.clone();
    candidate.k = 5;
    fs::write(
        &candidate_path,
        serde_json::to_string_pretty(&candidate)
            .unwrap_or_else(|error| panic!("candidate should serialize: {error}")),
    )
    .unwrap_or_else(|error| panic!("candidate should write: {error}"));
    let error = match diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path,
        candidate: candidate_path,
        max_pass_drop: None,
        fail_on_new_f_gate: false,
        json: false,
        output: None,
    }) {
        Ok(_) => panic!("different k values should be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("different k values"));
}

#[test]
fn eval_report_diff_preserves_exit_zero_without_gate_flags() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let baseline_path = tempdir.path().join("baseline.json");
    let candidate_path = tempdir.path().join("candidate.json");
    let baseline = eval_report_from_evidence(
        &sample_eval_manifest(),
        "baseline",
        3,
        vec![
            case_evidence(
                "case-pass",
                EvalEvidenceStatus::Passed,
                vec![usage_snapshot(100, 40)],
                Vec::new(),
            ),
            case_evidence(
                "case-fail",
                EvalEvidenceStatus::Failed,
                Vec::new(),
                Vec::new(),
            ),
        ],
    )
    .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    let candidate = eval_report_from_evidence(
        &sample_eval_manifest(),
        "candidate",
        3,
        vec![
            case_evidence(
                "case-pass",
                EvalEvidenceStatus::Failed,
                vec![usage_snapshot(80, 30)],
                Vec::new(),
            ),
            case_evidence(
                "case-fail",
                EvalEvidenceStatus::Failed,
                Vec::new(),
                Vec::new(),
            ),
        ],
    )
    .unwrap_or_else(|error| panic!("candidate report should build: {error}"));
    write_report(&baseline_path, &baseline);
    write_report(&candidate_path, &candidate);

    diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path,
        candidate: candidate_path,
        max_pass_drop: None,
        fail_on_new_f_gate: false,
        json: false,
        output: None,
    })
    .unwrap_or_else(|error| panic!("ungated diff should stay exit-zero: {error}"));
}

#[test]
fn eval_report_diff_fails_when_pass_drop_exceeds_threshold() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let baseline_path = tempdir.path().join("baseline.json");
    let candidate_path = tempdir.path().join("candidate.json");
    let baseline = eval_report_from_evidence(
        &sample_eval_manifest(),
        "baseline",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(100, 40)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    let candidate = eval_report_from_evidence(
        &sample_eval_manifest(),
        "candidate",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Failed,
            vec![usage_snapshot(80, 30)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("candidate report should build: {error}"));
    write_report(&baseline_path, &baseline);
    write_report(&candidate_path, &candidate);

    let error = match diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path,
        candidate: candidate_path,
        max_pass_drop: Some(0.1),
        fail_on_new_f_gate: false,
        json: false,
        output: None,
    }) {
        Ok(_) => panic!("threshold breach should fail"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(message.contains("pass@1 drop"));
    assert!(message.contains("pass^3 drop"));
}

#[test]
fn eval_report_diff_allows_pass_drop_at_threshold() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let baseline_path = tempdir.path().join("baseline.json");
    let candidate_path = tempdir.path().join("candidate.json");
    let baseline = report_with_pass_count("baseline", 10, 8);
    let candidate = report_with_pass_count("candidate", 10, 7);
    write_report(&baseline_path, &baseline);
    write_report(&candidate_path, &candidate);

    diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path,
        candidate: candidate_path,
        max_pass_drop: Some(0.1),
        fail_on_new_f_gate: false,
        json: false,
        output: None,
    })
    .unwrap_or_else(|error| panic!("exact threshold drop should pass: {error}"));
}

#[test]
fn eval_report_diff_fails_on_new_f_cap_gate_even_when_candidate_passed() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let baseline_path = tempdir.path().join("baseline.json");
    let candidate_path = tempdir.path().join("candidate.json");
    let baseline = eval_report_from_evidence(
        &sample_eval_manifest(),
        "baseline",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(100, 40)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("baseline report should build: {error}"));
    let mut candidate = eval_report_from_evidence(
        &sample_eval_manifest(),
        "candidate",
        3,
        vec![case_evidence(
            "case-pass",
            EvalEvidenceStatus::Passed,
            vec![usage_snapshot(80, 30)],
            Vec::new(),
        )],
    )
    .unwrap_or_else(|error| panic!("candidate report should build: {error}"));
    candidate.cases[0].failed_hard_gates = vec![EvalReportFailedGate {
        name: HardGateName::TargetCorrectness,
        grade_cap: Some(EvalGrade::F),
    }];
    write_report(&baseline_path, &baseline);
    write_report(&candidate_path, &candidate);

    let error = match diff_eval_reports(EvalDiffArgs {
        baseline: baseline_path,
        candidate: candidate_path,
        max_pass_drop: None,
        fail_on_new_f_gate: true,
        json: false,
        output: None,
    }) {
        Ok(_) => panic!("new F-cap gate should fail"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(message.contains("case-pass"));
    assert!(message.contains("TargetCorrectness"));
}

#[test]
fn eval_report_output_creates_parent_directory() {
    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let output = tempdir.path().join("nested").join("report.json");
    let report = eval_report_dry_run(&sample_eval_manifest(), "run-dry", 3)
        .unwrap_or_else(|error| panic!("dry run report should build: {error}"));

    write_json_output(&report, Some(&output))
        .unwrap_or_else(|error| panic!("nested output should write: {error}"));

    assert!(output.exists());
}

#[test]
fn eval_execute_output_defaults_to_run_artifact_path_and_refuses_overwrite() {
    let default = eval_report_output_path(None, true, "run-1")
        .unwrap_or_else(|error| panic!("default execute output should resolve: {error}"));
    assert_eq!(
        default,
        Some(PathBuf::from("artifacts/eval/run-1/report.json"))
    );

    let tempdir =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
    let existing = tempdir.path().join("report.json");
    fs::write(&existing, "{}").unwrap_or_else(|error| panic!("report should write: {error}"));

    let error = eval_report_output_path(Some(&existing), true, "run-1")
        .expect_err("execute must refuse to overwrite an existing report");
    assert!(error.to_string().contains("already exists"));
}

fn case_evidence(
    case_id: &str,
    status: EvalEvidenceStatus,
    usage: Vec<UsageSnapshot>,
    missing_evidence: Vec<String>,
) -> EvalCaseEvidence {
    EvalCaseEvidence {
        eval_run_id: "run-1".to_string(),
        case_id: case_id.to_string(),
        workflow_id: Some(format!("workflow-{case_id}")),
        status,
        attestation: EvalAttestationSummary::unsigned(),
        runtime: None,
        usage,
        submission: Some(EvalSubmissionEvidence {
            repo: Some("majiayu000/harness".to_string()),
            issue_number: Some(1447),
            command_id: Some("cmd-1".to_string()),
            command_status: Some("completed".to_string()),
            runtime_job_ids: vec!["job-1".to_string()],
        }),
        quality_gate: Some(EvalQualityGateEvidence {
            command_id: Some("cmd-quality".to_string()),
            runtime_job_id: Some("job-quality".to_string()),
            status: "succeeded".to_string(),
            validation_passed: true,
            validation_commands: vec!["cargo test".to_string()],
            validation_evidence: Vec::new(),
        }),
        quality: None,
        isolation: Some(EvalIsolationEvidence {
            required_tier: Some("container".to_string()),
            selected_tier: Some("container".to_string()),
            runtime_kind: Some("remote_host".to_string()),
            runtime_profile: Some("eval-isolated-runtime-host".to_string()),
            sandbox: Some("workspace-write".to_string()),
            backend: Some("container_runtime_host".to_string()),
            image: Some("harness-eval-runner:local".to_string()),
            lifecycle: Some("ephemeral".to_string()),
            cleanup_required: true,
            cleanup_status: Some("cleaned".to_string()),
        }),
        missing_evidence,
    }
}

fn usage_snapshot(total_tokens: u64, cost_usd_micros: u64) -> UsageSnapshot {
    UsageSnapshot {
        agent_invocation_id: Some("agent-1".to_string()),
        runtime_job_id: Some("job-1".to_string()),
        workflow_id: Some("workflow-1".to_string()),
        model: Some("codex-test".to_string()),
        reasoning_effort: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        total_tokens: Some(total_tokens),
        cost_usd_micros: Some(cost_usd_micros),
        token_confidence: Confidence::Observed,
        cost_confidence: Confidence::Estimated,
    }
}

fn report_with_pass_count(run_id: &str, total_cases: u64, passed_cases: u64) -> EvalRunReport {
    let pass_at_1 = passed_cases as f64 / total_cases as f64;
    let cases = (0..total_cases)
        .map(|index| {
            let passed = index < passed_cases;
            EvalReportCase {
                case_id: format!("case-{index:02}"),
                repo: "majiayu000/harness".to_string(),
                issue: 1400 + index,
                base_commit: "b308b380".to_string(),
                source_commit: "b308b380".to_string(),
                verify_commands: vec!["cargo test".to_string()],
                verification_evidence: Vec::new(),
                attestation_trust: EvalAttestationTrust::Unsigned,
                attestation_decision: None,
                status: if passed {
                    EvalReportCaseStatus::Passed
                } else {
                    EvalReportCaseStatus::Failed
                },
                outcome: None,
                passed,
                explicit_evidence: true,
                final_grade: None,
                failed_hard_gates: Vec::new(),
                workflow_id: Some(format!("workflow-{index:02}")),
                terminal_state: None,
                infrastructure_status: EvalCaseInfrastructureStatus::Healthy,
                total_tokens: 0,
                cost_usd_micros: 0,
                missing_evidence: Vec::new(),
            }
        })
        .collect::<Vec<_>>();

    EvalRunReport {
        run_id: run_id.to_string(),
        suite: "harness-core".to_string(),
        k: 1,
        outcome: None,
        metrics: EvalReportMetrics {
            total_cases,
            scored_cases: total_cases,
            passed_cases,
            failed_cases: total_cases - passed_cases,
            pending_cases: 0,
            skipped_cases: 0,
            infra_failed_cases: 0,
            pass_at_1,
            pass_to_k: pass_at_1,
            total_tokens: 0,
            avg_tokens_per_scored_case: Some(0.0),
            total_cost_usd_micros: 0,
            avg_cost_usd_micros_per_scored_case: Some(0.0),
        },
        cases,
    }
}

fn write_report(path: &Path, report: &EvalRunReport) {
    fs::write(
        path,
        serde_json::to_string_pretty(report)
            .unwrap_or_else(|error| panic!("report should serialize: {error}")),
    )
    .unwrap_or_else(|error| panic!("report should write: {error}"));
}
