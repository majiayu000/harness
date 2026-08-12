use anyhow::Context;
use clap::{Args, Subcommand};
use harness_workflow::runtime::eval::model::EvalGrade;
use harness_workflow::runtime::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence,
    parse_benchmark_manifest_str, EvalAttestationDecision, EvalAttestationTrust,
    EvalBenchmarkManifest, EvalCaseEvidence, EvalCaseInfrastructureStatus, EvalCaseTransition,
    EvalCaseTransitionKind, EvalReportCaseStatus, EvalRunReport, EvalRunReportDiff,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const PASS_DROP_EPSILON: f64 = 1e-9;

#[derive(Subcommand)]
pub enum EvalCommand {
    /// Emit an eval report from a manifest and collected case evidence
    Run(EvalRunArgs),
    /// Compare two saved eval run reports
    Diff(EvalDiffArgs),
}

#[derive(Args)]
pub struct EvalRunArgs {
    /// Benchmark manifest path
    #[arg(long)]
    pub manifest: PathBuf,
    /// Collected EvalCaseEvidence JSON path. Accepts either an array or {"cases": [...]}.
    #[arg(long)]
    pub evidence: Option<PathBuf>,
    /// Stable run identifier. Defaults to suite plus current UTC timestamp.
    #[arg(long)]
    pub run_id: Option<String>,
    /// pass^k retry count used for aggregate reporting
    #[arg(long, default_value_t = 3)]
    pub k: u32,
    /// Validate the manifest and list cases without requiring collected evidence
    #[arg(long)]
    pub dry_run: bool,
    /// Print JSON instead of the compact text report
    #[arg(long)]
    pub json: bool,
    /// Also write the JSON report to this path
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Args)]
pub struct EvalDiffArgs {
    /// Baseline eval run report JSON
    pub baseline: PathBuf,
    /// Candidate eval run report JSON
    pub candidate: PathBuf,
    /// Fail when pass@1 or pass^k drops by more than this amount
    #[arg(long)]
    pub max_pass_drop: Option<f64>,
    /// Fail when a previously passing case newly fails an F-cap hard gate
    #[arg(long)]
    pub fail_on_new_f_gate: bool,
    /// Print JSON instead of the compact text diff
    #[arg(long)]
    pub json: bool,
    /// Also write the JSON diff to this path
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub async fn run(cmd: EvalCommand) -> anyhow::Result<()> {
    match cmd {
        EvalCommand::Run(args) => run_eval_report(args).await,
        EvalCommand::Diff(args) => diff_eval_reports(args),
    }
}

async fn run_eval_report(args: EvalRunArgs) -> anyhow::Result<()> {
    if args.dry_run && args.evidence.is_some() {
        anyhow::bail!("use either --dry-run or --evidence, not both");
    }

    let manifest = read_eval_manifest(&args.manifest)?;
    let run_id = args
        .run_id
        .unwrap_or_else(|| default_run_id(&manifest.suite));
    let report = if args.dry_run {
        eval_report_dry_run(&manifest, run_id, args.k)?
    } else if let Some(evidence_path) = args.evidence.as_ref() {
        let evidence = read_evidence(evidence_path)?;
        eval_report_from_evidence(&manifest, run_id, args.k, evidence)?
    } else {
        anyhow::bail!(
            "live eval execution is not wired to the CLI yet; pass --evidence to report collected evidence or --dry-run to validate the manifest"
        );
    };

    emit_report(&report, args.json, args.output.as_deref())
}

fn diff_eval_reports(args: EvalDiffArgs) -> anyhow::Result<()> {
    let baseline = read_run_report(&args.baseline)?;
    let candidate = read_run_report(&args.candidate)?;
    if baseline.suite != candidate.suite {
        anyhow::bail!(
            "cannot diff reports from different suites: baseline={}, candidate={}",
            baseline.suite,
            candidate.suite
        );
    }
    if baseline.k != candidate.k {
        anyhow::bail!(
            "cannot diff reports with different k values: baseline={}, candidate={}",
            baseline.k,
            candidate.k
        );
    }
    let diff = diff_eval_run_reports(&baseline, &candidate);
    let regressions = eval_diff_regressions(&baseline, &candidate, &diff, &args)?;
    emit_diff(&diff, args.json, args.output.as_deref())?;
    if regressions.is_empty() {
        return Ok(());
    }
    anyhow::bail!("eval regression gate failed:\n{}", regressions.join("\n"))
}

fn read_eval_manifest(path: &Path) -> anyhow::Result<EvalBenchmarkManifest> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read eval manifest at {}", path.display()))?;
    parse_benchmark_manifest_str(&content)
        .map_err(|error| anyhow::anyhow!("invalid eval manifest {}: {error}", path.display()))
}

fn read_evidence(path: &Path) -> anyhow::Result<Vec<EvalCaseEvidence>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read eval evidence at {}", path.display()))?;
    let input: EvidenceInput = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse eval evidence at {}", path.display()))?;
    Ok(match input {
        EvidenceInput::Cases(cases) => cases,
        EvidenceInput::Wrapped { cases } => cases,
    })
}

fn read_run_report(path: &Path) -> anyhow::Result<EvalRunReport> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read eval report at {}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|error| anyhow::anyhow!("invalid eval report {}: {error}", path.display()))
}

fn emit_report(report: &EvalRunReport, json: bool, output: Option<&Path>) -> anyhow::Result<()> {
    write_json_output(report, output)?;
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print!("{}", render_run_report(report));
    }
    Ok(())
}

fn emit_diff(diff: &EvalRunReportDiff, json: bool, output: Option<&Path>) -> anyhow::Result<()> {
    write_json_output(diff, output)?;
    if json {
        println!("{}", serde_json::to_string_pretty(diff)?);
    } else {
        print!("{}", render_diff_report(diff));
    }
    Ok(())
}

fn write_json_output<T: serde::Serialize>(value: &T, output: Option<&Path>) -> anyhow::Result<()> {
    if let Some(output) = output {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create output directory {}", parent.display())
            })?;
        }
        fs::write(output, serde_json::to_string_pretty(value)?)?;
    }
    Ok(())
}

fn default_run_id(suite: &str) -> String {
    format!("{}-{}", suite, chrono::Utc::now().format("%Y%m%dT%H%M%SZ"))
}

pub(crate) fn render_run_report(report: &EvalRunReport) -> String {
    let metrics = &report.metrics;
    let mut output = String::new();
    output.push_str(&format!(
        "Eval report {} ({})\n",
        report.run_id, report.suite
    ));
    output.push_str(&format!(
        "cases: total={} scored={} passed={} failed={} skipped={} pending={} infra_failed={}\n",
        metrics.total_cases,
        metrics.scored_cases,
        metrics.passed_cases,
        metrics.failed_cases,
        metrics.skipped_cases,
        metrics.pending_cases,
        metrics.infra_failed_cases
    ));
    output.push_str(&format!(
        "pass@1: {:.4}  pass^{}: {:.4}\n",
        metrics.pass_at_1, report.k, metrics.pass_to_k
    ));
    output.push_str(&format!(
        "tokens: total={} avg/scored={}\n",
        metrics.total_tokens,
        format_optional_float(metrics.avg_tokens_per_scored_case)
    ));
    output.push_str(&format!(
        "cost_usd_micros: total={} avg/scored={}\n",
        metrics.total_cost_usd_micros,
        format_optional_float(metrics.avg_cost_usd_micros_per_scored_case)
    ));
    output.push_str("cases:\n");
    for case in &report.cases {
        output.push_str(&format!(
            "- {} {}#{} status={} attestation={} infra={} tokens={} cost_usd_micros={} source_commit={} terminal_state={}\n",
            case.case_id,
            case.repo,
            case.issue,
            case_status_label(case.status),
            attestation_label(case.attestation_trust, case.attestation_decision),
            infrastructure_status_label(case.infrastructure_status),
            case.total_tokens,
            case.cost_usd_micros,
            case_source_commit(case),
            case.terminal_state.as_deref().unwrap_or("n/a")
        ));
        if !case.verify_commands.is_empty() {
            output.push_str(&format!(
                "  verify: {}\n",
                case.verify_commands.join(" && ")
            ));
        }
        if !case.missing_evidence.is_empty() {
            output.push_str(&format!(
                "  missing_evidence: {}\n",
                case.missing_evidence.join(", ")
            ));
        }
        if let Some(final_grade) = case.final_grade {
            output.push_str(&format!("  final_grade: {}\n", grade_label(final_grade)));
        }
        if !case.failed_hard_gates.is_empty() {
            let gates = case
                .failed_hard_gates
                .iter()
                .map(|gate| {
                    let cap = gate.grade_cap.map(grade_label).unwrap_or("none");
                    format!("{:?}(cap={cap})", gate.name)
                })
                .collect::<Vec<_>>();
            output.push_str(&format!("  failed_hard_gates: {}\n", gates.join(", ")));
        }
    }
    output
}

pub(crate) fn render_diff_report(diff: &EvalRunReportDiff) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "Eval diff {} -> {} ({})\n",
        diff.baseline_run_id, diff.candidate_run_id, diff.suite
    ));
    output.push_str(&format!(
        "pass@1 delta: {:+.4}  pass^{} delta: {:+.4}\n",
        diff.delta.pass_at_1_delta, diff.k, diff.delta.pass_to_k_delta
    ));
    output.push_str(&format!(
        "tokens delta: {:+}  cost_usd_micros delta: {:+}\n",
        diff.delta.total_tokens_delta, diff.delta.total_cost_usd_micros_delta
    ));
    output.push_str(&format!(
        "regressions: count={} ids={}\n",
        diff.regression_count,
        if diff.regression_ids.is_empty() {
            "none".to_string()
        } else {
            diff.regression_ids.join(",")
        }
    ));
    output.push_str(&format!(
        "transition_counts: added={} removed={} pass_to_fail={} fail_to_pass={} pass_to_skip={} skip_to_pass={} fail_to_skip={} skip_to_fail={} status_changed={}\n",
        diff.transition_counts.added,
        diff.transition_counts.removed,
        diff.transition_counts.pass_to_fail,
        diff.transition_counts.fail_to_pass,
        diff.transition_counts.pass_to_skip,
        diff.transition_counts.skip_to_pass,
        diff.transition_counts.fail_to_skip,
        diff.transition_counts.skip_to_fail,
        diff.transition_counts.status_changed
    ));
    output.push_str("transitions:\n");
    for transition in &diff.transitions {
        let attestation_change = render_attestation_change(transition)
            .map(|change| format!(" {change}"))
            .unwrap_or_default();
        output.push_str(&format!(
            "- {} {} baseline_status={} candidate_status={} infra={} terminal={} source_commit={}{}\n",
            transition.case_id,
            transition_kind_label(transition.transition),
            optional_case_status_label(transition.baseline_status),
            optional_case_status_label(transition.candidate_status),
            format_optional_infrastructure_transition(
                transition.baseline_infrastructure_status,
                transition.candidate_infrastructure_status
            ),
            format_optional_transition(
                transition.baseline_terminal_state.as_deref(),
                transition.candidate_terminal_state.as_deref()
            ),
            format_optional_transition(
                transition.baseline_source_commit.as_deref(),
                transition.candidate_source_commit.as_deref()
            ),
            attestation_change
        ));
    }
    output
}

fn format_optional_float(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn case_status_label(status: EvalReportCaseStatus) -> &'static str {
    match status {
        EvalReportCaseStatus::Pending => "pending",
        EvalReportCaseStatus::Passed => "passed",
        EvalReportCaseStatus::Failed => "failed",
        EvalReportCaseStatus::Skipped => "skipped",
        EvalReportCaseStatus::InfraFailed => "infra_failed",
    }
}

fn attestation_label(
    trust: EvalAttestationTrust,
    decision: Option<EvalAttestationDecision>,
) -> String {
    let trust = attestation_trust_label(trust);
    decision
        .map(|decision| format!("{trust}:{}", attestation_decision_label(decision)))
        .unwrap_or_else(|| trust.to_string())
}

fn attestation_trust_label(trust: EvalAttestationTrust) -> &'static str {
    match trust {
        EvalAttestationTrust::Unsigned => "unsigned",
        EvalAttestationTrust::Unverified => "unverified",
        EvalAttestationTrust::Verified => "verified",
    }
}

fn attestation_decision_label(decision: EvalAttestationDecision) -> &'static str {
    match decision {
        EvalAttestationDecision::Approved => "approved",
        EvalAttestationDecision::Rejected => "rejected",
    }
}

fn render_attestation_change(transition: &EvalCaseTransition) -> Option<String> {
    if transition.baseline_attestation_trust == transition.candidate_attestation_trust
        && transition.baseline_attestation_decision == transition.candidate_attestation_decision
    {
        return None;
    }
    Some(format!(
        "attestation={}->{}",
        optional_attestation_label(
            transition.baseline_attestation_trust,
            transition.baseline_attestation_decision
        ),
        optional_attestation_label(
            transition.candidate_attestation_trust,
            transition.candidate_attestation_decision
        )
    ))
}

fn optional_attestation_label(
    trust: Option<EvalAttestationTrust>,
    decision: Option<EvalAttestationDecision>,
) -> String {
    trust
        .map(|trust| attestation_label(trust, decision))
        .unwrap_or_else(|| "none".to_string())
}

fn optional_case_status_label(status: Option<EvalReportCaseStatus>) -> &'static str {
    status.map(case_status_label).unwrap_or("n/a")
}

fn infrastructure_status_label(status: EvalCaseInfrastructureStatus) -> &'static str {
    match status {
        EvalCaseInfrastructureStatus::Unknown => "unknown",
        EvalCaseInfrastructureStatus::Healthy => "healthy",
        EvalCaseInfrastructureStatus::MissingEvidence => "missing_evidence",
        EvalCaseInfrastructureStatus::InfraFailed => "infra_failed",
    }
}

fn case_source_commit(case: &harness_workflow::runtime::EvalReportCase) -> &str {
    if case.source_commit.is_empty() {
        &case.base_commit
    } else {
        &case.source_commit
    }
}

fn format_optional_infrastructure_transition(
    baseline: Option<EvalCaseInfrastructureStatus>,
    candidate: Option<EvalCaseInfrastructureStatus>,
) -> String {
    format!(
        "{}->{}",
        baseline.map(infrastructure_status_label).unwrap_or("n/a"),
        candidate.map(infrastructure_status_label).unwrap_or("n/a")
    )
}

fn format_optional_transition(baseline: Option<&str>, candidate: Option<&str>) -> String {
    format!(
        "{}->{}",
        baseline.unwrap_or("n/a"),
        candidate.unwrap_or("n/a")
    )
}

fn transition_kind_label(kind: EvalCaseTransitionKind) -> &'static str {
    match kind {
        EvalCaseTransitionKind::Added => "added",
        EvalCaseTransitionKind::Removed => "removed",
        EvalCaseTransitionKind::UnchangedPass => "unchanged_pass",
        EvalCaseTransitionKind::UnchangedFail => "unchanged_fail",
        EvalCaseTransitionKind::UnchangedSkip => "unchanged_skip",
        EvalCaseTransitionKind::PassToFail => "pass_to_fail",
        EvalCaseTransitionKind::FailToPass => "fail_to_pass",
        EvalCaseTransitionKind::PassToSkip => "pass_to_skip",
        EvalCaseTransitionKind::SkipToPass => "skip_to_pass",
        EvalCaseTransitionKind::FailToSkip => "fail_to_skip",
        EvalCaseTransitionKind::SkipToFail => "skip_to_fail",
        EvalCaseTransitionKind::StatusChanged => "status_changed",
    }
}

fn grade_label(grade: EvalGrade) -> &'static str {
    match grade {
        EvalGrade::F => "F",
        EvalGrade::D => "D",
        EvalGrade::C => "C",
        EvalGrade::B => "B",
        EvalGrade::A => "A",
    }
}

fn eval_diff_regressions(
    baseline: &EvalRunReport,
    candidate: &EvalRunReport,
    diff: &EvalRunReportDiff,
    args: &EvalDiffArgs,
) -> anyhow::Result<Vec<String>> {
    let mut regressions = Vec::new();
    if let Some(max_pass_drop) = args.max_pass_drop {
        if !max_pass_drop.is_finite() || !(0.0..=1.0).contains(&max_pass_drop) {
            anyhow::bail!("--max-pass-drop must be between 0.0 and 1.0");
        }
        if pass_drop_exceeds(diff.delta.pass_at_1_delta, max_pass_drop) {
            regressions.push(format!(
                "pass@1 drop {:+.4} exceeded max drop {:.4}",
                diff.delta.pass_at_1_delta, max_pass_drop
            ));
        }
        if pass_drop_exceeds(diff.delta.pass_to_k_delta, max_pass_drop) {
            regressions.push(format!(
                "pass^{} drop {:+.4} exceeded max drop {:.4}",
                diff.k, diff.delta.pass_to_k_delta, max_pass_drop
            ));
        }
    }

    if args.fail_on_new_f_gate {
        regressions.extend(new_f_cap_gate_regressions(baseline, candidate));
    }
    Ok(regressions)
}

fn pass_drop_exceeds(delta: f64, max_pass_drop: f64) -> bool {
    -delta > max_pass_drop + PASS_DROP_EPSILON
}

fn new_f_cap_gate_regressions(baseline: &EvalRunReport, candidate: &EvalRunReport) -> Vec<String> {
    let baseline_by_case = baseline
        .cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect::<BTreeMap<_, _>>();

    candidate
        .cases
        .iter()
        .filter_map(|candidate_case| {
            let baseline_case = baseline_by_case.get(candidate_case.case_id.as_str())?;
            if !baseline_case.passed {
                return None;
            }
            let gates = candidate_case
                .failed_hard_gates
                .iter()
                .filter(|gate| gate.grade_cap == Some(EvalGrade::F))
                .filter(|gate| {
                    !baseline_case.failed_hard_gates.iter().any(|baseline_gate| {
                        baseline_gate.name == gate.name
                            && baseline_gate.grade_cap == Some(EvalGrade::F)
                    })
                })
                .map(|gate| format!("{:?}", gate.name))
                .collect::<Vec<_>>();
            (!gates.is_empty()).then(|| {
                format!(
                    "{} newly failed F-cap hard gate(s): {}",
                    candidate_case.case_id,
                    gates.join(", ")
                )
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EvidenceInput {
    Cases(Vec<EvalCaseEvidence>),
    Wrapped { cases: Vec<EvalCaseEvidence> },
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, Command};
    use super::*;
    use clap::Parser;
    use harness_workflow::runtime::eval::model::{
        Confidence, EvalGrade, HardGateName, UsageSnapshot,
    };
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
                assert!(args.json);
                assert_eq!(args.output, Some(PathBuf::from("report.json")));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
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
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
        let output = tempdir.path().join("nested").join("report.json");
        let report = eval_report_dry_run(&sample_eval_manifest(), "run-dry", 3)
            .unwrap_or_else(|error| panic!("dry run report should build: {error}"));

        write_json_output(&report, Some(&output))
            .unwrap_or_else(|error| panic!("nested output should write: {error}"));

        assert!(output.exists());
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
                    attestation_trust: EvalAttestationTrust::Unsigned,
                    attestation_decision: None,
                    status: if passed {
                        EvalReportCaseStatus::Passed
                    } else {
                        EvalReportCaseStatus::Failed
                    },
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
}
