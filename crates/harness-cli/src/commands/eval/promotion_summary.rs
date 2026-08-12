use anyhow::Context;
use clap::{Args, ValueEnum};
use harness_workflow::runtime::{
    diff_eval_run_reports, EvalAttestationDecision, EvalAttestationTrust,
    EvalCaseInfrastructureStatus, EvalCaseTransition, EvalCaseTransitionCounts,
    EvalCaseTransitionKind, EvalReportCaseStatus, EvalReportMetricDelta, EvalRunReport,
};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const PROMOTION_SUMMARY_SCHEMA_VERSION: &str = "harness.eval.promotion_summary.v1";
const RULE_NO_BASELINE: &str = "ASC020-NO-BASELINE";
const RULE_EVIDENCE_GAP: &str = "ASC020-EVIDENCE-GAP";
const RULE_PASS_TO_FAIL: &str = "ASC020-PASS-TO-FAIL";
const RULE_PASS_DROP: &str = "ASC020-PASS-DROP";
const RULE_NEW_F_GATE: &str = "ASC020-NEW-F-GATE";
const RULE_PROMOTE: &str = "ASC020-PROMOTE";

#[derive(Args)]
pub(crate) struct EvalPromotionSummaryArgs {
    /// Candidate eval run report JSON.
    #[arg(long)]
    pub candidate: PathBuf,
    /// Baseline eval run report JSON. Omit to emit REVIEW with no-baseline evidence.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Block when pass@1 or pass^k drops by more than this amount.
    #[arg(long)]
    pub max_pass_drop: Option<f64>,
    /// Block when a previously passing case newly fails an F-cap hard gate.
    #[arg(long)]
    pub fail_on_new_f_gate: bool,
    /// Stdout format for the rendered summary.
    #[arg(long, value_enum, default_value_t = PromotionOutputFormat::Markdown)]
    pub format: PromotionOutputFormat,
    /// Also write the machine-readable JSON summary to this path.
    #[arg(long)]
    pub json_output: Option<PathBuf>,
    /// Also write the human-readable Markdown summary to this path.
    #[arg(long)]
    pub markdown_output: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PromotionOutputFormat {
    Json,
    Markdown,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PromotionVerdict {
    Promote,
    Review,
    Block,
}

impl PromotionVerdict {
    fn label(self) -> &'static str {
        match self {
            Self::Promote => "PROMOTE",
            Self::Review => "REVIEW",
            Self::Block => "BLOCK",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Promote => 0,
            Self::Review => 2,
            Self::Block => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PromotionDecision {
    verdict: PromotionVerdict,
    exit_code: i32,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct PromotionSummary {
    schema_version: &'static str,
    decision: PromotionDecision,
    no_change: bool,
    baseline_run_id: Option<String>,
    candidate_run_id: String,
    suite: String,
    k: u32,
    changes: PromotionChanges,
    regressions: Vec<PromotionRegression>,
    gaps: Vec<PromotionGap>,
    rules: Vec<PromotionRuleMatch>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct PromotionChanges {
    transition_counts: Option<EvalCaseTransitionCounts>,
    metric_delta: Option<EvalReportMetricDelta>,
    changed_cases: Vec<PromotionCaseChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PromotionCaseChange {
    case_id: String,
    transition: EvalCaseTransitionKind,
    baseline_status: Option<EvalReportCaseStatus>,
    candidate_status: Option<EvalReportCaseStatus>,
    baseline_attestation_trust: Option<EvalAttestationTrust>,
    candidate_attestation_trust: Option<EvalAttestationTrust>,
    baseline_attestation_decision: Option<EvalAttestationDecision>,
    candidate_attestation_decision: Option<EvalAttestationDecision>,
    baseline_source_commit: Option<String>,
    candidate_source_commit: Option<String>,
    baseline_verify_commands: Vec<String>,
    candidate_verify_commands: Vec<String>,
    baseline_terminal_state: Option<String>,
    candidate_terminal_state: Option<String>,
    baseline_infrastructure_status: Option<EvalCaseInfrastructureStatus>,
    candidate_infrastructure_status: Option<EvalCaseInfrastructureStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PromotionRegression {
    rule_id: &'static str,
    case_id: Option<String>,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PromotionGap {
    gap_type: &'static str,
    case_id: Option<String>,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PromotionRuleMatch {
    rule_id: &'static str,
    outcome: PromotionVerdict,
    reason: String,
    matched_facts: Vec<String>,
}

struct PromotionPolicy {
    max_pass_drop: Option<f64>,
    fail_on_new_f_gate: bool,
}

pub(super) fn run_promotion_summary(args: EvalPromotionSummaryArgs) -> anyhow::Result<i32> {
    let candidate = super::read_run_report(&args.candidate)?;
    let baseline = args
        .baseline
        .as_ref()
        .map(|path| super::read_run_report(path))
        .transpose()?;
    let policy = PromotionPolicy {
        max_pass_drop: args.max_pass_drop,
        fail_on_new_f_gate: args.fail_on_new_f_gate,
    };
    let summary = promotion_summary_from_reports(baseline.as_ref(), &candidate, &policy)?;

    if let Some(path) = args.json_output.as_deref() {
        super::write_json_output(&summary, Some(path))?;
    }
    if let Some(path) = args.markdown_output.as_deref() {
        write_text_output(path, &render_promotion_summary_markdown(&summary))?;
    }

    match args.format {
        PromotionOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        PromotionOutputFormat::Markdown => {
            print!("{}", render_promotion_summary_markdown(&summary));
        }
    }

    Ok(summary.decision.exit_code)
}

fn promotion_summary_from_reports(
    baseline: Option<&EvalRunReport>,
    candidate: &EvalRunReport,
    policy: &PromotionPolicy,
) -> anyhow::Result<PromotionSummary> {
    if let Some(max_pass_drop) = policy.max_pass_drop {
        if !max_pass_drop.is_finite() || !(0.0..=1.0).contains(&max_pass_drop) {
            anyhow::bail!("--max-pass-drop must be between 0.0 and 1.0");
        }
    }

    let mut rules = Vec::new();
    let mut gaps = candidate_evidence_gaps(candidate);
    let mut regressions = Vec::new();

    let (baseline_run_id, changes, no_change) = if let Some(baseline) = baseline {
        if baseline.suite != candidate.suite {
            anyhow::bail!(
                "cannot summarize promotion reports from different suites: baseline={}, candidate={}",
                baseline.suite,
                candidate.suite
            );
        }
        if baseline.k != candidate.k {
            anyhow::bail!(
                "cannot summarize promotion reports with different k values: baseline={}, candidate={}",
                baseline.k,
                candidate.k
            );
        }
        let diff = diff_eval_run_reports(baseline, candidate);
        regressions.extend(pass_to_fail_regressions(&diff.transitions));
        let policy_rule_matches = blocking_policy_rules(baseline, candidate, &diff, policy);
        for rule_match in &policy_rule_matches {
            regressions.push(PromotionRegression {
                rule_id: rule_match.rule_id,
                case_id: None,
                message: rule_match.reason.clone(),
            });
        }
        rules.extend(policy_rule_matches);
        let no_change = diff_has_no_change(&diff);
        (
            Some(baseline.run_id.clone()),
            PromotionChanges {
                transition_counts: Some(diff.transition_counts),
                metric_delta: Some(diff.delta),
                changed_cases: changed_cases(&diff.transitions),
            },
            no_change,
        )
    } else {
        gaps.push(PromotionGap {
            gap_type: "no_baseline",
            case_id: None,
            reason: "no baseline report was supplied".to_string(),
        });
        rules.push(PromotionRuleMatch {
            rule_id: RULE_NO_BASELINE,
            outcome: PromotionVerdict::Review,
            reason: "candidate cannot be promoted without a baseline comparison".to_string(),
            matched_facts: vec![format!("candidate_run_id={}", candidate.run_id)],
        });
        (
            None,
            PromotionChanges {
                transition_counts: None,
                metric_delta: None,
                changed_cases: Vec::new(),
            },
            false,
        )
    };

    let candidate_gap_facts = gaps
        .iter()
        .filter(|gap| gap.gap_type != "no_baseline")
        .map(|gap| match gap.case_id.as_deref() {
            Some(case_id) => format!("{}:{case_id}", gap.gap_type),
            None => gap.gap_type.to_string(),
        })
        .collect::<Vec<_>>();

    if !candidate_gap_facts.is_empty()
        && !rules
            .iter()
            .any(|rule_match| rule_match.rule_id == RULE_EVIDENCE_GAP)
    {
        rules.push(PromotionRuleMatch {
            rule_id: RULE_EVIDENCE_GAP,
            outcome: PromotionVerdict::Review,
            reason:
                "candidate report has missing, pending, skipped, or failed infrastructure evidence"
                    .to_string(),
            matched_facts: candidate_gap_facts,
        });
    }

    for regression in &regressions {
        if regression.rule_id == RULE_PASS_TO_FAIL
            && !rules
                .iter()
                .any(|rule_match| rule_match.rule_id == RULE_PASS_TO_FAIL)
        {
            rules.push(PromotionRuleMatch {
                rule_id: RULE_PASS_TO_FAIL,
                outcome: PromotionVerdict::Block,
                reason: "one or more previously passing cases now fail".to_string(),
                matched_facts: regressions
                    .iter()
                    .filter(|item| item.rule_id == RULE_PASS_TO_FAIL)
                    .filter_map(|item| item.case_id.as_ref())
                    .map(|case_id| format!("case_id={case_id}"))
                    .collect(),
            });
        }
    }

    if rules.is_empty() {
        rules.push(PromotionRuleMatch {
            rule_id: RULE_PROMOTE,
            outcome: PromotionVerdict::Promote,
            reason: "no blocking regressions or review gaps matched".to_string(),
            matched_facts: vec![format!("candidate_run_id={}", candidate.run_id)],
        });
    }

    let decision_verdict = if rules
        .iter()
        .any(|rule_match| rule_match.outcome == PromotionVerdict::Block)
    {
        PromotionVerdict::Block
    } else if rules
        .iter()
        .any(|rule_match| rule_match.outcome == PromotionVerdict::Review)
    {
        PromotionVerdict::Review
    } else {
        PromotionVerdict::Promote
    };
    let decision = PromotionDecision {
        verdict: decision_verdict,
        exit_code: decision_verdict.exit_code(),
        reason: decision_reason(decision_verdict, &rules),
    };

    Ok(PromotionSummary {
        schema_version: PROMOTION_SUMMARY_SCHEMA_VERSION,
        decision,
        no_change,
        baseline_run_id,
        candidate_run_id: candidate.run_id.clone(),
        suite: candidate.suite.clone(),
        k: candidate.k,
        changes,
        regressions,
        gaps,
        rules,
    })
}

fn candidate_evidence_gaps(candidate: &EvalRunReport) -> Vec<PromotionGap> {
    candidate
        .cases
        .iter()
        .filter_map(|case| {
            let mut reasons = Vec::new();
            if !case.missing_evidence.is_empty() {
                reasons.push(format!(
                    "missing_evidence={}",
                    case.missing_evidence.join(",")
                ));
            }
            match case.status {
                EvalReportCaseStatus::Pending => reasons.push("status=pending".to_string()),
                EvalReportCaseStatus::Skipped => reasons.push("status=skipped".to_string()),
                EvalReportCaseStatus::InfraFailed => {
                    reasons.push("status=infra_failed".to_string());
                }
                EvalReportCaseStatus::Passed | EvalReportCaseStatus::Failed => {}
            }
            match case.infrastructure_status {
                EvalCaseInfrastructureStatus::MissingEvidence => {
                    reasons.push("infrastructure_status=missing_evidence".to_string());
                }
                EvalCaseInfrastructureStatus::InfraFailed => {
                    reasons.push("infrastructure_status=infra_failed".to_string());
                }
                EvalCaseInfrastructureStatus::Unknown | EvalCaseInfrastructureStatus::Healthy => {}
            }
            (!reasons.is_empty()).then(|| PromotionGap {
                gap_type: if case.infrastructure_status == EvalCaseInfrastructureStatus::InfraFailed
                    || case.status == EvalReportCaseStatus::InfraFailed
                {
                    "candidate_infra_failed"
                } else {
                    "candidate_evidence_gap"
                },
                case_id: Some(case.case_id.clone()),
                reason: reasons.join("; "),
            })
        })
        .collect()
}

fn pass_to_fail_regressions(transitions: &[EvalCaseTransition]) -> Vec<PromotionRegression> {
    transitions
        .iter()
        .filter(|transition| transition.transition == EvalCaseTransitionKind::PassToFail)
        .map(|transition| PromotionRegression {
            rule_id: RULE_PASS_TO_FAIL,
            case_id: Some(transition.case_id.clone()),
            message: "case changed from passed baseline to failed candidate".to_string(),
        })
        .collect()
}

fn blocking_policy_rules(
    baseline: &EvalRunReport,
    candidate: &EvalRunReport,
    diff: &harness_workflow::runtime::EvalRunReportDiff,
    policy: &PromotionPolicy,
) -> Vec<PromotionRuleMatch> {
    let mut rules = Vec::new();
    if let Some(max_pass_drop) = policy.max_pass_drop {
        let mut matched_facts = Vec::new();
        if super::pass_drop_exceeds(diff.delta.pass_at_1_delta, max_pass_drop) {
            matched_facts.push(format!(
                "pass@1_delta={:+.4} max_pass_drop={:.4}",
                diff.delta.pass_at_1_delta, max_pass_drop
            ));
        }
        if super::pass_drop_exceeds(diff.delta.pass_to_k_delta, max_pass_drop) {
            matched_facts.push(format!(
                "pass^{}_delta={:+.4} max_pass_drop={:.4}",
                diff.k, diff.delta.pass_to_k_delta, max_pass_drop
            ));
        }
        if !matched_facts.is_empty() {
            rules.push(PromotionRuleMatch {
                rule_id: RULE_PASS_DROP,
                outcome: PromotionVerdict::Block,
                reason: "pass-rate drop exceeded the configured threshold".to_string(),
                matched_facts,
            });
        }
    }

    if policy.fail_on_new_f_gate {
        let failures = super::new_f_cap_gate_regressions(baseline, candidate);
        if !failures.is_empty() {
            rules.push(PromotionRuleMatch {
                rule_id: RULE_NEW_F_GATE,
                outcome: PromotionVerdict::Block,
                reason: "candidate newly failed an F-cap hard gate".to_string(),
                matched_facts: failures,
            });
        }
    }

    rules
}

fn diff_has_no_change(diff: &harness_workflow::runtime::EvalRunReportDiff) -> bool {
    diff.transitions
        .iter()
        .all(|transition| !material_transition_changed(transition))
        && diff.delta.pass_at_1_delta.abs() <= super::PASS_DROP_EPSILON
        && diff.delta.pass_to_k_delta.abs() <= super::PASS_DROP_EPSILON
        && diff.delta.total_tokens_delta == 0
        && diff.delta.total_cost_usd_micros_delta == 0
}

fn is_unchanged_transition(kind: EvalCaseTransitionKind) -> bool {
    matches!(
        kind,
        EvalCaseTransitionKind::UnchangedPass
            | EvalCaseTransitionKind::UnchangedFail
            | EvalCaseTransitionKind::UnchangedSkip
    )
}

fn changed_cases(transitions: &[EvalCaseTransition]) -> Vec<PromotionCaseChange> {
    transitions
        .iter()
        .filter(|transition| material_transition_changed(transition))
        .map(|transition| PromotionCaseChange {
            case_id: transition.case_id.clone(),
            transition: transition.transition,
            baseline_status: transition.baseline_status,
            candidate_status: transition.candidate_status,
            baseline_attestation_trust: transition.baseline_attestation_trust,
            candidate_attestation_trust: transition.candidate_attestation_trust,
            baseline_attestation_decision: transition.baseline_attestation_decision,
            candidate_attestation_decision: transition.candidate_attestation_decision,
            baseline_source_commit: transition.baseline_source_commit.clone(),
            candidate_source_commit: transition.candidate_source_commit.clone(),
            baseline_verify_commands: transition.baseline_verify_commands.clone(),
            candidate_verify_commands: transition.candidate_verify_commands.clone(),
            baseline_terminal_state: transition.baseline_terminal_state.clone(),
            candidate_terminal_state: transition.candidate_terminal_state.clone(),
            baseline_infrastructure_status: transition.baseline_infrastructure_status,
            candidate_infrastructure_status: transition.candidate_infrastructure_status,
        })
        .collect()
}

fn material_transition_changed(transition: &EvalCaseTransition) -> bool {
    !is_unchanged_transition(transition.transition)
        || transition.baseline_attestation_trust != transition.candidate_attestation_trust
        || transition.baseline_attestation_decision != transition.candidate_attestation_decision
        || transition.baseline_source_commit != transition.candidate_source_commit
        || transition.baseline_verify_commands != transition.candidate_verify_commands
        || transition.baseline_terminal_state != transition.candidate_terminal_state
        || transition.baseline_infrastructure_status != transition.candidate_infrastructure_status
}

fn decision_reason(verdict: PromotionVerdict, rules: &[PromotionRuleMatch]) -> String {
    match verdict {
        PromotionVerdict::Promote => "candidate satisfies promotion summary rules".to_string(),
        PromotionVerdict::Review => format!(
            "manual review required by {}",
            matching_rule_ids(rules, PromotionVerdict::Review).join(",")
        ),
        PromotionVerdict::Block => format!(
            "promotion blocked by {}",
            matching_rule_ids(rules, PromotionVerdict::Block).join(",")
        ),
    }
}

fn matching_rule_ids(rules: &[PromotionRuleMatch], outcome: PromotionVerdict) -> Vec<&'static str> {
    rules
        .iter()
        .filter(|rule_match| rule_match.outcome == outcome)
        .map(|rule_match| rule_match.rule_id)
        .collect()
}

fn render_promotion_summary_markdown(summary: &PromotionSummary) -> String {
    let mut output = String::new();
    output.push_str("# Promotion Summary\n\n");
    output.push_str(&format!(
        "Decision: {}\n\n",
        summary.decision.verdict.label()
    ));
    output.push_str(&format!("Exit code: {}\n\n", summary.decision.exit_code));
    output.push_str(&format!("Reason: {}\n\n", summary.decision.reason));
    output.push_str(&format!("No change: {}\n\n", summary.no_change));
    output.push_str(&format!("Suite: {}\n\n", summary.suite));
    output.push_str(&format!(
        "Runs: {} -> {}\n\n",
        summary.baseline_run_id.as_deref().unwrap_or("n/a"),
        summary.candidate_run_id
    ));

    output.push_str("## Changes\n\n");
    match (
        &summary.changes.transition_counts,
        &summary.changes.metric_delta,
    ) {
        (Some(counts), Some(delta)) => {
            output.push_str(&format!(
                "- transition_counts: added={} removed={} pass_to_fail={} fail_to_pass={} pass_to_skip={} skip_to_pass={} fail_to_skip={} skip_to_fail={} status_changed={}\n",
                counts.added,
                counts.removed,
                counts.pass_to_fail,
                counts.fail_to_pass,
                counts.pass_to_skip,
                counts.skip_to_pass,
                counts.fail_to_skip,
                counts.skip_to_fail,
                counts.status_changed
            ));
            output.push_str(&format!(
                "- metric_delta: pass@1={:+.4} pass^{}={:+.4} tokens={:+} cost_usd_micros={:+}\n",
                delta.pass_at_1_delta,
                summary.k,
                delta.pass_to_k_delta,
                delta.total_tokens_delta,
                delta.total_cost_usd_micros_delta
            ));
        }
        _ => output.push_str("- baseline comparison unavailable\n"),
    }
    if summary.changes.changed_cases.is_empty() {
        output.push_str("- changed_cases: none\n\n");
    } else {
        output.push_str("- changed_cases:\n");
        for case in &summary.changes.changed_cases {
            output.push_str(&format!(
                "  - {} {} baseline_status={} candidate_status={} infra={} source_commit={} terminal={} attestation={}\n",
                case.case_id,
                super::transition_kind_label(case.transition),
                case.baseline_status
                    .map(super::case_status_label)
                    .unwrap_or("n/a"),
                case.candidate_status
                    .map(super::case_status_label)
                    .unwrap_or("n/a"),
                super::format_optional_infrastructure_transition(
                    case.baseline_infrastructure_status,
                    case.candidate_infrastructure_status
                ),
                super::format_optional_transition(
                    case.baseline_source_commit.as_deref(),
                    case.candidate_source_commit.as_deref()
                ),
                super::format_optional_transition(
                    case.baseline_terminal_state.as_deref(),
                    case.candidate_terminal_state.as_deref()
                ),
                format_optional_attestation_transition(case)
            ));
            if case.baseline_verify_commands != case.candidate_verify_commands {
                output.push_str(&format!(
                    "    verify: {} -> {}\n",
                    format_commands(&case.baseline_verify_commands),
                    format_commands(&case.candidate_verify_commands)
                ));
            }
        }
        output.push('\n');
    }

    output.push_str("## Regressions\n\n");
    if summary.regressions.is_empty() {
        output.push_str("- none\n\n");
    } else {
        for regression in &summary.regressions {
            output.push_str(&format!(
                "- {} {}: {}\n",
                regression.rule_id,
                regression.case_id.as_deref().unwrap_or("n/a"),
                regression.message
            ));
        }
        output.push('\n');
    }

    output.push_str("## Gaps\n\n");
    if summary.gaps.is_empty() {
        output.push_str("- none\n\n");
    } else {
        for gap in &summary.gaps {
            output.push_str(&format!(
                "- {} {}: {}\n",
                gap.gap_type,
                gap.case_id.as_deref().unwrap_or("n/a"),
                gap.reason
            ));
        }
        output.push('\n');
    }

    output.push_str("## Rules\n\n");
    for rule_match in &summary.rules {
        output.push_str(&format!(
            "- {} -> {}: {}\n",
            rule_match.rule_id,
            rule_match.outcome.label(),
            rule_match.reason
        ));
        if !rule_match.matched_facts.is_empty() {
            output.push_str(&format!(
                "  facts: {}\n",
                rule_match.matched_facts.join("; ")
            ));
        }
    }

    output
}

fn write_text_output(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn format_optional_attestation_transition(case: &PromotionCaseChange) -> String {
    format!(
        "{}->{}",
        super::optional_attestation_label(
            case.baseline_attestation_trust,
            case.baseline_attestation_decision
        ),
        super::optional_attestation_label(
            case.candidate_attestation_trust,
            case.candidate_attestation_decision
        )
    )
}

fn format_commands(commands: &[String]) -> String {
    if commands.is_empty() {
        "none".to_string()
    } else {
        commands.join(" && ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::eval::model::EvalGrade;
    use harness_workflow::runtime::eval::model::HardGateName;
    use harness_workflow::runtime::{
        EvalAttestationTrust, EvalReportCase, EvalReportFailedGate, EvalReportMetrics,
    };

    #[test]
    fn promotion_summary_promotes_clean_diff_and_marks_no_change() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect("clean diff should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Promote);
        assert_eq!(summary.decision.exit_code, 0);
        assert!(summary.no_change);
        assert!(summary.regressions.is_empty());
        assert!(summary.gaps.is_empty());

        let markdown = render_promotion_summary_markdown(&summary);
        assert!(markdown.contains("Decision: PROMOTE"));
        assert!(markdown.contains("No change: true"));

        let json = serde_json::to_string(&summary).expect("summary should serialize");
        assert!(json.contains(r#""verdict":"PROMOTE""#));
        assert!(json.contains(r#""no_change":true"#));
    }

    #[test]
    fn promotion_summary_no_change_tracks_material_metadata_changes() {
        let baseline = report_with_source(
            "baseline",
            "source-a",
            &[("case-a", EvalReportCaseStatus::Passed)],
        );
        let candidate = report_with_source(
            "candidate",
            "source-b",
            &[("case-a", EvalReportCaseStatus::Passed)],
        );
        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect("source commit change should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Promote);
        assert!(!summary.no_change);
        assert_eq!(summary.changes.changed_cases[0].case_id, "case-a");
        assert_eq!(
            summary.changes.changed_cases[0]
                .baseline_source_commit
                .as_deref(),
            Some("source-a")
        );
        assert_eq!(
            summary.changes.changed_cases[0]
                .candidate_source_commit
                .as_deref(),
            Some("source-b")
        );
    }

    #[test]
    fn promotion_summary_reviews_missing_baseline() {
        let candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        let summary = promotion_summary_from_reports(
            None,
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect("missing baseline should summarize as review");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Review);
        assert_eq!(summary.decision.exit_code, 2);
        assert!(!summary.no_change);
        assert!(summary.gaps.iter().any(|gap| gap.gap_type == "no_baseline"));
        assert!(summary
            .rules
            .iter()
            .any(|rule_match| rule_match.rule_id == RULE_NO_BASELINE));
    }

    #[test]
    fn promotion_summary_blocks_pass_to_fail_regression() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Failed)]);
        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect("pass-to-fail should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Block);
        assert_eq!(summary.decision.exit_code, 3);
        assert!(summary
            .regressions
            .iter()
            .any(|regression| regression.case_id.as_deref() == Some("case-a")));
        assert!(render_promotion_summary_markdown(&summary).contains("ASC020-PASS-TO-FAIL"));
    }

    #[test]
    fn promotion_summary_reviews_candidate_evidence_gaps() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Skipped)]);
        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect("candidate evidence gap should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Review);
        assert_eq!(summary.decision.exit_code, 2);
        assert!(summary
            .gaps
            .iter()
            .any(|gap| gap.gap_type == "candidate_evidence_gap"));
        assert!(summary
            .rules
            .iter()
            .any(|rule_match| rule_match.rule_id == RULE_EVIDENCE_GAP));
    }

    #[test]
    fn promotion_summary_blocks_configured_pass_drop_without_case_regression() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let mut candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        candidate.metrics.pass_at_1 = 0.8;
        candidate.metrics.pass_to_k = 0.8;

        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: Some(0.1),
                fail_on_new_f_gate: false,
            },
        )
        .expect("pass drop should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Block);
        assert_eq!(summary.decision.exit_code, 3);
        assert!(summary
            .rules
            .iter()
            .any(|rule_match| rule_match.rule_id == RULE_PASS_DROP));
    }

    #[test]
    fn promotion_summary_blocks_new_f_cap_gate_when_configured() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let mut candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        candidate.cases[0].failed_hard_gates = vec![EvalReportFailedGate {
            name: HardGateName::TargetCorrectness,
            grade_cap: Some(EvalGrade::F),
        }];

        let summary = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: true,
            },
        )
        .expect("new F-cap gate should summarize");

        assert_eq!(summary.decision.verdict, PromotionVerdict::Block);
        assert!(summary
            .rules
            .iter()
            .any(|rule_match| rule_match.rule_id == RULE_NEW_F_GATE));
    }

    #[test]
    fn promotion_summary_invalid_threshold_is_engine_error() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        let error = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: Some(1.1),
                fail_on_new_f_gate: false,
            },
        )
        .expect_err("invalid threshold should fail through engine-error path");

        assert!(error.to_string().contains("--max-pass-drop"));
    }

    #[test]
    fn promotion_summary_suite_mismatch_is_engine_error() {
        let baseline = report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]);
        let mut candidate = report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]);
        candidate.suite = "different-suite".to_string();
        let error = promotion_summary_from_reports(
            Some(&baseline),
            &candidate,
            &PromotionPolicy {
                max_pass_drop: None,
                fail_on_new_f_gate: false,
            },
        )
        .expect_err("suite mismatch should fail through engine-error path");

        assert!(error.to_string().contains("different suites"));
    }

    #[test]
    fn promotion_summary_writes_json_and_markdown_outputs() {
        let tempdir = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be creatable: {error}"));
        let baseline_path = tempdir.path().join("baseline.json");
        let candidate_path = tempdir.path().join("candidate.json");
        let json_output = tempdir.path().join("nested").join("summary.json");
        let markdown_output = tempdir.path().join("nested").join("summary.md");
        write_report(
            &baseline_path,
            &report("baseline", &[("case-a", EvalReportCaseStatus::Passed)]),
        );
        write_report(
            &candidate_path,
            &report("candidate", &[("case-a", EvalReportCaseStatus::Passed)]),
        );

        let exit_code = run_promotion_summary(EvalPromotionSummaryArgs {
            candidate: candidate_path,
            baseline: Some(baseline_path),
            max_pass_drop: None,
            fail_on_new_f_gate: false,
            format: PromotionOutputFormat::Json,
            json_output: Some(json_output.clone()),
            markdown_output: Some(markdown_output.clone()),
        })
        .expect("clean summary should render");

        assert_eq!(exit_code, 0);
        assert!(std::fs::read_to_string(json_output)
            .expect("json output should read")
            .contains(r#""schema_version": "harness.eval.promotion_summary.v1""#));
        assert!(std::fs::read_to_string(markdown_output)
            .expect("markdown output should read")
            .contains("Decision: PROMOTE"));
    }

    fn report(run_id: &str, statuses: &[(&str, EvalReportCaseStatus)]) -> EvalRunReport {
        report_with_source(run_id, "same-source", statuses)
    }

    fn report_with_source(
        run_id: &str,
        source_commit: &str,
        statuses: &[(&str, EvalReportCaseStatus)],
    ) -> EvalRunReport {
        let cases = statuses
            .iter()
            .enumerate()
            .map(|(index, (case_id, status))| report_case(index, case_id, source_commit, *status))
            .collect::<Vec<_>>();
        let total_cases = cases.len() as u64;
        let passed_cases = cases.iter().filter(|case| case.passed).count() as u64;
        let failed_cases = cases
            .iter()
            .filter(|case| case.status == EvalReportCaseStatus::Failed)
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
        let scored_cases = passed_cases + failed_cases;
        let pass_at_1 = if scored_cases == 0 {
            0.0
        } else {
            passed_cases as f64 / scored_cases as f64
        };

        EvalRunReport {
            run_id: run_id.to_string(),
            suite: "harness-core".to_string(),
            k: 3,
            metrics: EvalReportMetrics {
                total_cases,
                scored_cases,
                passed_cases,
                failed_cases,
                pending_cases,
                skipped_cases,
                infra_failed_cases,
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

    fn report_case(
        index: usize,
        case_id: &str,
        source_commit: &str,
        status: EvalReportCaseStatus,
    ) -> EvalReportCase {
        let missing_evidence = match status {
            EvalReportCaseStatus::Pending | EvalReportCaseStatus::Skipped => {
                vec!["case_evidence".to_string()]
            }
            _ => Vec::new(),
        };
        let infrastructure_status = match status {
            EvalReportCaseStatus::Pending | EvalReportCaseStatus::Skipped => {
                EvalCaseInfrastructureStatus::MissingEvidence
            }
            EvalReportCaseStatus::InfraFailed => EvalCaseInfrastructureStatus::InfraFailed,
            EvalReportCaseStatus::Passed | EvalReportCaseStatus::Failed => {
                EvalCaseInfrastructureStatus::Healthy
            }
        };

        EvalReportCase {
            case_id: case_id.to_string(),
            repo: "majiayu000/harness".to_string(),
            issue: 1749 + index as u64,
            base_commit: "baseline".to_string(),
            source_commit: source_commit.to_string(),
            verify_commands: vec!["cargo test".to_string()],
            attestation_trust: EvalAttestationTrust::Unsigned,
            attestation_decision: None,
            status,
            passed: status == EvalReportCaseStatus::Passed,
            explicit_evidence: missing_evidence.is_empty(),
            final_grade: None,
            failed_hard_gates: Vec::new(),
            workflow_id: Some(format!("workflow-{case_id}")),
            terminal_state: None,
            infrastructure_status,
            total_tokens: 0,
            cost_usd_micros: 0,
            missing_evidence,
        }
    }

    fn write_report(path: &Path, report: &EvalRunReport) {
        std::fs::write(
            path,
            serde_json::to_string_pretty(report)
                .unwrap_or_else(|error| panic!("report should serialize: {error}")),
        )
        .unwrap_or_else(|error| panic!("report should write: {error}"));
    }
}
