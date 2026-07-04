use clap::{Args, Subcommand};
use harness_workflow::runtime::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence,
    parse_benchmark_manifest_str, EvalBenchmarkManifest, EvalCaseEvidence, EvalCaseTransitionKind,
    EvalReportCaseStatus, EvalRunReport, EvalRunReportDiff,
};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

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
    let diff = diff_eval_run_reports(&baseline, &candidate);
    emit_diff(&diff, args.json, args.output.as_deref())
}

fn read_eval_manifest(path: &Path) -> anyhow::Result<EvalBenchmarkManifest> {
    let content = fs::read_to_string(path)?;
    parse_benchmark_manifest_str(&content)
        .map_err(|error| anyhow::anyhow!("invalid eval manifest {}: {error}", path.display()))
}

fn read_evidence(path: &Path) -> anyhow::Result<Vec<EvalCaseEvidence>> {
    let content = fs::read_to_string(path)?;
    let input: EvidenceInput = serde_json::from_str(&content)?;
    Ok(match input {
        EvidenceInput::Cases(cases) => cases,
        EvidenceInput::Wrapped { cases } => cases,
    })
}

fn read_run_report(path: &Path) -> anyhow::Result<EvalRunReport> {
    let content = fs::read_to_string(path)?;
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
        "cases: total={} scored={} passed={} failed={} pending={} infra_failed={}\n",
        metrics.total_cases,
        metrics.scored_cases,
        metrics.passed_cases,
        metrics.failed_cases,
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
            "- {} {}#{} status={} tokens={} cost_usd_micros={} base_commit={}\n",
            case.case_id,
            case.repo,
            case.issue,
            case_status_label(case.status),
            case.total_tokens,
            case.cost_usd_micros,
            case.base_commit
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
    output.push_str("transitions:\n");
    for transition in &diff.transitions {
        output.push_str(&format!(
            "- {} {}\n",
            transition.case_id,
            transition_kind_label(transition.transition)
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
        EvalReportCaseStatus::InfraFailed => "infra_failed",
    }
}

fn transition_kind_label(kind: EvalCaseTransitionKind) -> &'static str {
    match kind {
        EvalCaseTransitionKind::Added => "added",
        EvalCaseTransitionKind::Removed => "removed",
        EvalCaseTransitionKind::UnchangedPass => "unchanged_pass",
        EvalCaseTransitionKind::UnchangedFail => "unchanged_fail",
        EvalCaseTransitionKind::PassToFail => "pass_to_fail",
        EvalCaseTransitionKind::FailToPass => "fail_to_pass",
        EvalCaseTransitionKind::StatusChanged => "status_changed",
    }
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
    use harness_workflow::runtime::eval::model::{Confidence, UsageSnapshot};
    use harness_workflow::runtime::{
        EvalEvidenceStatus, EvalQualityGateEvidence, EvalSubmissionEvidence,
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
            "--json",
        ])
        .unwrap_or_else(|error| panic!("eval diff command should parse: {error}"));
        match cli.command {
            Command::Eval {
                cmd: EvalCommand::Diff(args),
            } => {
                assert_eq!(args.baseline, PathBuf::from("baseline.json"));
                assert_eq!(args.candidate, PathBuf::from("candidate.json"));
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
        assert_eq!(report.metrics.scored_cases, 2);
        assert_eq!(report.metrics.passed_cases, 1);
        assert_eq!(report.metrics.failed_cases, 1);
        assert_eq!(report.metrics.total_tokens, 120);
        assert_eq!(report.metrics.total_cost_usd_micros, 50);
        assert!(rendered.contains("pass@1: 0.5000"));
        assert!(rendered.contains("pass^3: 0.8750"));
        assert!(rendered.contains("missing_evidence: case_evidence"));
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
        assert!(rendered.contains("tokens delta: -20"));
        assert!(rendered.contains("cost_usd_micros delta: -10"));
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
}
