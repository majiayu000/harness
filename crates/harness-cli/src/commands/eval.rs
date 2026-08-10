use anyhow::Context;
use clap::{Args, Subcommand};
use harness_workflow::runtime::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence,
    parse_benchmark_manifest_str, EvalBenchmarkManifest, EvalCaseEvidence, EvalCaseTransitionKind,
    EvalReportCaseStatus, EvalRunReport, EvalRunReportDiff,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum EvalCommand {
    /// Emit an eval report from a manifest and collected case evidence
    Run(EvalRunArgs),
    /// Compare two saved eval run reports
    Diff(EvalDiffArgs),
    /// Render a stable promotion decision summary for CI gates
    PromotionSummary(EvalPromotionSummaryArgs),
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

#[derive(Args)]
pub struct EvalPromotionSummaryArgs {
    /// Promotion decision input JSON
    #[arg(long)]
    pub input: PathBuf,
    /// Print JSON instead of Markdown
    #[arg(long)]
    pub json: bool,
    /// Also write the JSON summary to this path
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Also write the Markdown summary to this path
    #[arg(long)]
    pub markdown_output: Option<PathBuf>,
}

pub async fn run(cmd: EvalCommand) -> anyhow::Result<()> {
    match cmd {
        EvalCommand::Run(args) => run_eval_report(args).await,
        EvalCommand::Diff(args) => diff_eval_reports(args),
        EvalCommand::PromotionSummary(args) => {
            let summary = promotion_summary(args)?;
            let exit_code = summary.exit_code;
            if exit_code == 0 {
                Ok(())
            } else {
                std::process::exit(exit_code);
            }
        }
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
    emit_diff(&diff, args.json, args.output.as_deref())
}

fn promotion_summary(args: EvalPromotionSummaryArgs) -> anyhow::Result<PromotionSummary> {
    let input = read_promotion_summary_input(&args.input)?;
    let summary = build_promotion_summary(input);
    let markdown = render_promotion_summary_markdown(&summary);
    write_json_output(&summary, args.output.as_deref())?;
    write_string_output(&markdown, args.markdown_output.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{markdown}");
    }
    Ok(summary)
}

fn read_promotion_summary_input(path: &Path) -> anyhow::Result<PromotionSummaryInput> {
    let content = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read promotion summary input at {}",
            path.display()
        )
    })?;
    serde_json::from_str(&content)
        .with_context(|| format!("invalid promotion summary input {}", path.display()))
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

fn write_string_output(content: &str, output: Option<&Path>) -> anyhow::Result<()> {
    if let Some(output) = output {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create output directory {}", parent.display())
            })?;
        }
        fs::write(output, content)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PromotionSummaryInput {
    #[serde(default)]
    suite: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    baseline: Option<String>,
    #[serde(default)]
    candidate: Option<String>,
    decision: PromotionDecision,
    #[serde(default)]
    no_change: bool,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    regressions: Vec<String>,
    #[serde(default)]
    gaps: Vec<String>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    engine_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PromotionSummary {
    suite: Option<String>,
    subject: Option<String>,
    baseline: Option<String>,
    candidate: Option<String>,
    decision: PromotionDecision,
    exit_code: i32,
    no_change: bool,
    changes: Vec<String>,
    regressions: Vec<String>,
    gaps: Vec<String>,
    rules: Vec<String>,
    engine_error: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PromotionDecision {
    #[serde(rename = "PROMOTE", alias = "promote")]
    Promote,
    #[serde(rename = "REVIEW", alias = "review")]
    Review,
    #[serde(rename = "BLOCK", alias = "block")]
    Block,
    #[serde(rename = "NO_BASELINE", alias = "no_baseline", alias = "no-baseline")]
    NoBaseline,
    #[serde(
        rename = "ENGINE_ERROR",
        alias = "engine_error",
        alias = "engine-error"
    )]
    EngineError,
}

impl PromotionDecision {
    const fn exit_code(self) -> i32 {
        match self {
            Self::Promote => 0,
            Self::Review | Self::NoBaseline => 2,
            Self::Block => 3,
            Self::EngineError => 1,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Promote => "PROMOTE",
            Self::Review => "REVIEW",
            Self::Block => "BLOCK",
            Self::NoBaseline => "NO_BASELINE",
            Self::EngineError => "ENGINE_ERROR",
        }
    }
}

fn build_promotion_summary(input: PromotionSummaryInput) -> PromotionSummary {
    let mut gaps = input.gaps;
    let mut rules = input.rules;
    if input.decision == PromotionDecision::NoBaseline && gaps.is_empty() {
        gaps.push("baseline evidence is missing".to_string());
    }
    if input.decision == PromotionDecision::EngineError
        && input
            .engine_error
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        rules.push("engine error was reported without details".to_string());
    }
    PromotionSummary {
        suite: input.suite,
        subject: input.subject,
        baseline: input.baseline,
        candidate: input.candidate,
        decision: input.decision,
        exit_code: input.decision.exit_code(),
        no_change: input.no_change,
        changes: input.changes,
        regressions: input.regressions,
        gaps,
        rules,
        engine_error: input.engine_error,
    }
}

fn render_promotion_summary_markdown(summary: &PromotionSummary) -> String {
    let mut output = String::new();
    output.push_str("# Agent Stack Regression\n\n");
    output.push_str(&format!("decision: `{}`\n", summary.decision.label()));
    output.push_str(&format!("exit_code: `{}`\n", summary.exit_code));
    output.push_str(&format!("no_change: `{}`\n", summary.no_change));
    append_optional_line(&mut output, "suite", summary.suite.as_deref());
    append_optional_line(&mut output, "subject", summary.subject.as_deref());
    append_optional_line(&mut output, "baseline", summary.baseline.as_deref());
    append_optional_line(&mut output, "candidate", summary.candidate.as_deref());
    append_optional_line(&mut output, "engine_error", summary.engine_error.as_deref());
    append_markdown_list(&mut output, "Changes", &summary.changes);
    append_markdown_list(&mut output, "Regressions", &summary.regressions);
    append_markdown_list(&mut output, "Gaps", &summary.gaps);
    append_markdown_list(&mut output, "Rules", &summary.rules);
    output
}

fn append_optional_line(output: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        output.push_str(&format!("{label}: `{}`\n", value.trim()));
    }
}

fn append_markdown_list(output: &mut String, heading: &str, items: &[String]) {
    output.push_str(&format!("\n## {heading}\n"));
    if items.is_empty() {
        output.push_str("- none\n");
    } else {
        for item in items {
            output.push_str(&format!("- {}\n", item.trim()));
        }
    }
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

        let cli = Cli::try_parse_from([
            "harness",
            "eval",
            "promotion-summary",
            "--input",
            "decision.json",
            "--json",
            "--output",
            "summary.json",
            "--markdown-output",
            "summary.md",
        ])
        .unwrap_or_else(|error| panic!("eval promotion-summary command should parse: {error}"));
        match cli.command {
            Command::Eval {
                cmd: EvalCommand::PromotionSummary(args),
            } => {
                assert_eq!(args.input, PathBuf::from("decision.json"));
                assert!(args.json);
                assert_eq!(args.output, Some(PathBuf::from("summary.json")));
                assert_eq!(args.markdown_output, Some(PathBuf::from("summary.md")));
            }
            _ => panic!("expected eval promotion-summary command"),
        }
    }

    #[test]
    fn promotion_summary_maps_decisions_to_stable_exit_codes() {
        let decisions = [
            (PromotionDecision::Promote, 0),
            (PromotionDecision::Review, 2),
            (PromotionDecision::NoBaseline, 2),
            (PromotionDecision::Block, 3),
            (PromotionDecision::EngineError, 1),
        ];
        for (decision, expected_code) in decisions {
            let summary = build_promotion_summary(PromotionSummaryInput {
                suite: Some("agent-stack".to_string()),
                subject: Some("regression".to_string()),
                baseline: Some("baseline.json".to_string()),
                candidate: Some("candidate.json".to_string()),
                decision,
                no_change: decision == PromotionDecision::Promote,
                changes: Vec::new(),
                regressions: Vec::new(),
                gaps: Vec::new(),
                rules: Vec::new(),
                engine_error: (decision == PromotionDecision::EngineError)
                    .then(|| "diff engine failed".to_string()),
            });

            assert_eq!(summary.exit_code, expected_code, "{decision:?}");
        }
    }

    #[test]
    fn promotion_summary_cli_parses_command() {
        let cli = Cli::try_parse_from([
            "harness",
            "eval",
            "promotion-summary",
            "--input",
            "decision.json",
            "--json",
            "--output",
            "summary.json",
            "--markdown-output",
            "summary.md",
        ])
        .unwrap_or_else(|error| panic!("eval promotion-summary command should parse: {error}"));

        match cli.command {
            Command::Eval {
                cmd: EvalCommand::PromotionSummary(args),
            } => {
                assert_eq!(args.input, PathBuf::from("decision.json"));
                assert!(args.json);
                assert_eq!(args.output, Some(PathBuf::from("summary.json")));
                assert_eq!(args.markdown_output, Some(PathBuf::from("summary.md")));
            }
            _ => panic!("expected eval promotion-summary command"),
        }
    }

    #[test]
    fn promotion_summary_keeps_no_change_separate_from_verdict() {
        let summary = build_promotion_summary(PromotionSummaryInput {
            suite: Some("agent-stack".to_string()),
            subject: None,
            baseline: None,
            candidate: None,
            decision: PromotionDecision::Review,
            no_change: true,
            changes: Vec::new(),
            regressions: vec!["policy check requires review".to_string()],
            gaps: Vec::new(),
            rules: vec!["review decisions fail the required check".to_string()],
            engine_error: None,
        });
        let rendered = render_promotion_summary_markdown(&summary);

        assert_eq!(summary.exit_code, 2);
        assert!(summary.no_change);
        assert!(rendered.contains("decision: `REVIEW`"));
        assert!(rendered.contains("no_change: `true`"));
        assert!(rendered.contains("policy check requires review"));
    }

    #[test]
    fn promotion_summary_writes_json_and_markdown_outputs() {
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
        let input_path = tempdir.path().join("input.json");
        let json_path = tempdir.path().join("nested").join("summary.json");
        let markdown_path = tempdir.path().join("nested").join("summary.md");
        fs::write(
            &input_path,
            r#"{
              "suite": "agent-stack",
              "subject": "regression",
              "decision": "no_baseline",
              "no_change": true
            }"#,
        )
        .unwrap_or_else(|error| panic!("input should write: {error}"));

        let summary = promotion_summary(EvalPromotionSummaryArgs {
            input: input_path,
            json: true,
            output: Some(json_path.clone()),
            markdown_output: Some(markdown_path.clone()),
        })
        .unwrap_or_else(|error| panic!("promotion summary should render: {error}"));

        assert_eq!(summary.decision, PromotionDecision::NoBaseline);
        assert_eq!(summary.exit_code, 2);
        assert!(summary
            .gaps
            .contains(&"baseline evidence is missing".to_string()));
        let json = fs::read_to_string(json_path)
            .unwrap_or_else(|error| panic!("json should read: {error}"));
        assert!(json.contains("\"decision\": \"NO_BASELINE\""));
        let markdown = fs::read_to_string(markdown_path)
            .unwrap_or_else(|error| panic!("markdown should read: {error}"));
        assert!(markdown.contains("# Agent Stack Regression"));
        assert!(markdown.contains("decision: `NO_BASELINE`"));
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
        assert!(rendered.contains("tokens delta: -20"));
        assert!(rendered.contains("cost_usd_micros delta: -10"));
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
            json: false,
            output: None,
        }) {
            Ok(_) => panic!("different k values should be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("different k values"));
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
