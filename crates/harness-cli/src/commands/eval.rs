use anyhow::Context;
use clap::{Args, Subcommand};
use harness_core::config::HarnessConfig;
use harness_observe::event_store::EventStore;
use harness_workflow::runtime::eval::model::EvalGrade;
use harness_workflow::runtime::{
    diff_eval_run_reports, eval_report_dry_run, eval_report_from_evidence,
    execute_manifest_with_cancellation, parse_benchmark_manifest_str, EvalAttestationDecision,
    EvalAttestationTrust, EvalBenchmarkManifest, EvalCaseEvidence, EvalCaseInfrastructureStatus,
    EvalCaseTransition, EvalCaseTransitionKind, EvalExecuteConfig, EvalReportCaseStatus,
    EvalRunReport, EvalRunReportDiff, WorkflowRuntimeStore,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod output;
use output::{default_run_id, reserve_eval_output};

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
    /// Statistical pass^k estimate parameter; live execution still runs each case once
    #[arg(long, default_value_t = 3)]
    pub k: u32,
    /// Validate the manifest and list cases without requiring collected evidence
    #[arg(long)]
    pub dry_run: bool,
    /// Dispatch each replayable case through the workflow runtime and collect evidence in-process
    #[arg(long)]
    pub execute: bool,
    /// Override each case timeout while executing
    #[arg(long)]
    pub case_timeout_secs: Option<u64>,
    /// Poll interval while waiting for executed cases
    #[arg(long, default_value_t = 5_000)]
    pub poll_interval_ms: u64,
    /// Maximum time to wait for the server dispatcher to create a runtime job
    #[arg(long, default_value_t = 30)]
    pub dispatch_timeout_secs: u64,
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

pub async fn run(cmd: EvalCommand, config: &HarnessConfig) -> anyhow::Result<()> {
    match cmd {
        EvalCommand::Run(args) => run_eval_report(args, config).await,
        EvalCommand::Diff(args) => diff_eval_reports(args),
    }
}

async fn run_eval_report(args: EvalRunArgs, config: &HarnessConfig) -> anyhow::Result<()> {
    let selected_modes =
        u8::from(args.dry_run) + u8::from(args.evidence.is_some()) + u8::from(args.execute);
    if selected_modes > 1 {
        anyhow::bail!("use only one of --execute, --dry-run, or --evidence");
    }

    let manifest = read_eval_manifest(&args.manifest)?;
    let run_id = args
        .run_id
        .clone()
        .unwrap_or_else(|| default_run_id(&manifest.suite));
    let output = eval_report_output_path(args.output.as_ref(), args.execute, &run_id)?;
    let mut output_reservation = if args.execute {
        output.as_deref().map(reserve_eval_output).transpose()?
    } else {
        None
    };
    let mut interrupted = false;
    let report = if args.dry_run {
        eval_report_dry_run(&manifest, run_id, args.k)?
    } else if let Some(evidence_path) = args.evidence.as_ref() {
        let evidence = read_evidence(evidence_path)?;
        eval_report_from_evidence(&manifest, run_id, args.k, evidence)?
    } else if args.execute {
        let outcome = execute_eval_report(&manifest, &run_id, args.k, &args, config).await?;
        interrupted = outcome.interrupted;
        outcome.report
    } else {
        anyhow::bail!(
            "choose an eval run mode: pass --execute to dispatch cases, --evidence to report collected evidence, or --dry-run to validate the manifest"
        );
    };

    if let Some(reservation) = output_reservation.as_mut() {
        reservation.write_report(&report)?;
        emit_report(&report, args.json, None)?;
    } else {
        emit_report(&report, args.json, output.as_deref())?;
    }
    if interrupted {
        anyhow::bail!("eval execution was interrupted after writing its partial report");
    }
    Ok(())
}

struct EvalExecutionOutcome {
    report: EvalRunReport,
    interrupted: bool,
}

async fn execute_eval_report(
    manifest: &EvalBenchmarkManifest,
    run_id: &str,
    k: u32,
    args: &EvalRunArgs,
    config: &HarnessConfig,
) -> anyhow::Result<EvalExecutionOutcome> {
    if args.poll_interval_ms == 0 {
        anyhow::bail!("--poll-interval-ms must be greater than zero");
    }
    if args.case_timeout_secs == Some(0) {
        anyhow::bail!("--case-timeout-secs must be greater than zero");
    }
    if args.dispatch_timeout_secs == 0 {
        anyhow::bail!("--dispatch-timeout-secs must be greater than zero");
    }
    if config.server.database_url.is_none() {
        anyhow::bail!(
            "--execute requires server.database_url or HARNESS_DATABASE_URL so the evaluator can use the workflow runtime store"
        );
    }
    let workflow_config =
        harness_core::config::workflow::load_workflow_config(&config.server.project_root)
            .with_context(|| {
                format!(
                    "failed to load workflow config for {}",
                    config.server.project_root.display()
                )
            })?;
    let runtime_schema = format!("{}_runtime", workflow_config.storage.schema_namespace);
    let store = WorkflowRuntimeStore::open_with_database_url_and_schema(
        config.server.database_url.as_deref(),
        &runtime_schema,
    )
    .await
    .with_context(|| format!("failed to open workflow runtime store schema {runtime_schema}"))?;
    let observe = EventStore::with_policies_and_otel_with_database_url(
        &config.server.data_dir,
        config.server.database_url.as_deref(),
        config.observe.session_renewal_secs,
        config.observe.log_retention_days,
        &config.otel,
    )
    .await
    .context("failed to open observe event store for eval execution")?;

    let mut execute_config = EvalExecuteConfig::new(
        run_id,
        config.server.project_root.to_string_lossy().into_owned(),
        k,
    );
    execute_config.poll_interval = Duration::from_millis(args.poll_interval_ms);
    execute_config.dispatch_timeout = Duration::from_secs(args.dispatch_timeout_secs);
    execute_config.case_timeout_override = args.case_timeout_secs.map(Duration::from_secs);

    let (cancellation_tx, cancellation_rx) = tokio::sync::watch::channel(false);
    let mut execution = Box::pin(execute_manifest_with_cancellation(
        &store,
        &observe,
        manifest,
        execute_config,
        cancellation_rx,
    ));
    let outcome = tokio::select! {
        result = &mut execution => result.map(|report| EvalExecutionOutcome {
            report,
            interrupted: false,
        }),
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => {
                    tracing::warn!(
                        run_id,
                        "eval interruption requested; waiting for the active case to reach durable cleanup"
                    );
                    if cancellation_tx.send(true).is_err() {
                        tracing::debug!(
                            run_id,
                            "eval execution completed while interruption was being delivered"
                        );
                    }
                    execution.await.map(|report| EvalExecutionOutcome {
                        report,
                        interrupted: true,
                    }).context("eval interruption cleanup failed")
                }
                Err(error) => Err(error).context("failed to listen for eval execution interruption"),
            }
        }
    };
    observe.shutdown().await;
    outcome
}

fn eval_report_output_path(
    requested: Option<&PathBuf>,
    execute: bool,
    run_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let output = requested
        .cloned()
        .or_else(|| execute.then(|| default_execute_output_path(run_id)));
    if execute {
        let existing = output
            .as_ref()
            .map(|path| {
                path.try_exists().with_context(|| {
                    format!("failed to inspect eval output path {}", path.display())
                })
            })
            .transpose()?
            .unwrap_or(false);
        if existing {
            let path = output
                .as_ref()
                .expect("existing output path should be present");
            anyhow::bail!(
                "eval execute report already exists at {}; choose a new --run-id or --output",
                path.display()
            );
        }
    }
    Ok(output)
}

fn default_execute_output_path(run_id: &str) -> PathBuf {
    PathBuf::from("artifacts")
        .join("eval")
        .join(run_id)
        .join("report.json")
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
#[path = "eval/tests.rs"]
mod tests;
