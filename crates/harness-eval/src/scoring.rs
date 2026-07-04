use crate::model::{
    CheckState, Confidence, EvalGrade, EvalScenario, EvalTarget, GateStatus, HardGateName,
    HardGateResult, MergeState, PrRepairEvalInput, QualitySnapshot, RuntimeErrorKind,
    RuntimeJobSnapshot, RuntimeSnapshot, ScoreBreakdown, ScoreComponent, ScoreDimensionName,
};
use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScoringError {
    UnsupportedTarget,
}

impl fmt::Display for ScoringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget => {
                f.write_str("PR repair scoring requires a pull request target")
            }
        }
    }
}

impl Error for ScoringError {}

pub fn score_pr_repair_eval(input: PrRepairEvalInput) -> Result<QualitySnapshot, ScoringError> {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::HarnessEval,
    );
    let target_matches = target_matches_pr(&input)?;
    let branch_safe = branch_safe(&input);
    let no_unrelated_pr = !input.created_unrelated_pr;
    let scope_contained = input.scope_violations.is_empty();
    let head_changed = input.baseline_pr.head_oid != input.final_pr.head_oid;
    let final_evidence_fresh = input
        .final_evidence_head_oid
        .as_deref()
        .is_some_and(|head_oid| head_oid == input.final_pr.head_oid);
    let checks_passing = input.final_pr.check_state == CheckState::Passing;
    let mergeability_clean = input.final_pr.merge_state == MergeState::Clean;
    let review_threads_closed = input.final_pr.active_unresolved_review_threads.is_empty()
        && input.final_pr.review_threads_complete
        && (input.scenario == EvalScenario::PrRepair || input.baseline_pr.review_threads_complete);
    let final_remote_evidence_complete =
        input.final_pr.review_threads_complete && input.final_pr.changed_files_complete;
    let runtime_complete = input.runtime.as_ref().is_some_and(runtime_has_artifact);

    let hard_gates = vec![
        gate(
            HardGateName::TargetCorrectness,
            target_matches,
            Some(EvalGrade::F),
            "final PR matches the requested target",
            "final PR does not match the requested target",
        ),
        gate(
            HardGateName::BranchSafety,
            branch_safe,
            Some(EvalGrade::F),
            "base and head refs stayed on the requested PR",
            "base or head refs changed during evaluation",
        ),
        gate(
            HardGateName::NoUnrelatedPrCreation,
            no_unrelated_pr,
            Some(EvalGrade::F),
            "evaluation did not create an unrelated PR",
            "evaluation created or reported an unrelated PR",
        ),
        gate(
            HardGateName::ScopeContainment,
            scope_contained,
            Some(EvalGrade::F),
            "final diff stayed within the repair scope",
            "destructive or unrelated scope changes were detected",
        ),
        head_change_gate(&input.scenario, head_changed),
        gate(
            HardGateName::HeadFreshness,
            final_evidence_fresh,
            Some(EvalGrade::C),
            "final evidence was collected for the final PR head",
            "final evidence is missing or stale for the final PR head",
        ),
        gate(
            HardGateName::RequiredChecks,
            checks_passing,
            Some(EvalGrade::C),
            "required checks passed on the final PR head",
            "required checks are not passing on the final PR head",
        ),
        gate(
            HardGateName::MergeabilityClean,
            mergeability_clean,
            Some(EvalGrade::C),
            "final PR mergeability is clean",
            "final PR mergeability is not clean",
        ),
        gate(
            HardGateName::ReviewThreadClosure,
            review_threads_closed,
            Some(EvalGrade::C),
            "review threads were fully enumerated and no active unresolved review threads remain",
            "active unresolved review threads remain or review thread enumeration is incomplete",
        ),
        gate(
            HardGateName::RuntimeArtifactCompleteness,
            runtime_complete,
            Some(EvalGrade::B),
            "usable runtime task, workflow, or job artifact is present",
            "runtime artifact is missing, failed, or empty",
        ),
        reviewer_gate(&input),
    ];
    let gate_signals = GateSignals {
        target_matches,
        branch_safe,
        no_unrelated_pr,
        scope_contained,
        final_evidence_fresh,
        checks_passing,
        mergeability_clean,
        review_threads_closed,
        final_remote_evidence_complete,
        runtime_complete,
    };
    let objective_score = score_breakdown(&input, gate_signals);
    let final_score = objective_score.total();
    let raw_grade = EvalGrade::from_score(final_score);
    let grade_cap = most_restrictive_cap(&hard_gates);
    let final_grade = grade_cap.map_or(raw_grade, |cap| raw_grade.cap_at(cap));
    let blocker_summary = hard_gates
        .iter()
        .filter(|gate| gate.status == GateStatus::Fail)
        .map(|gate| gate.message.clone())
        .collect();

    Ok(QualitySnapshot {
        scenario: input.scenario,
        run_mode: input.run_mode,
        target: input.target,
        baseline_pr: Some(input.baseline_pr),
        final_pr: Some(input.final_pr),
        runtime: input.runtime,
        usage: input.usage,
        hard_gates,
        objective_score,
        reviewer_judgment: input.reviewer_judgment,
        final_score,
        final_grade,
        grade_cap,
        blocker_summary,
    })
}

fn target_matches_pr(input: &PrRepairEvalInput) -> Result<bool, ScoringError> {
    let EvalTarget::PullRequest {
        repo,
        pr_number,
        base_ref,
        head_ref,
    } = &input.target
    else {
        return Err(ScoringError::UnsupportedTarget);
    };

    let target_matches = input.baseline_pr.repo == repo.as_str()
        && input.final_pr.repo == repo.as_str()
        && input.baseline_pr.pr_number == *pr_number
        && input.final_pr.pr_number == *pr_number
        && base_ref
            .as_deref()
            .is_none_or(|expected_base_ref| input.final_pr.base_ref == expected_base_ref)
        && head_ref
            .as_deref()
            .is_none_or(|expected_head_ref| input.final_pr.head_ref == expected_head_ref);

    Ok(target_matches)
}

fn branch_safe(input: &PrRepairEvalInput) -> bool {
    input.baseline_pr.repo == input.final_pr.repo
        && input.baseline_pr.pr_number == input.final_pr.pr_number
        && input.baseline_pr.base_ref == input.final_pr.base_ref
        && input.baseline_pr.head_ref == input.final_pr.head_ref
}

fn runtime_has_artifact(runtime: &RuntimeSnapshot) -> bool {
    let has_runtime_identity = runtime.task_id.is_some() && runtime.workflow_id.is_some();
    if !has_runtime_identity || runtime_has_failed_terminal_state(runtime) {
        return false;
    }

    let has_terminal_or_job_evidence = runtime.terminal_state.is_some()
        || runtime
            .runtime_jobs
            .iter()
            .any(|job| job.artifact_count > 0 || job.terminal_state.is_some());

    has_terminal_or_job_evidence
}

fn runtime_has_failed_terminal_state(runtime: &RuntimeSnapshot) -> bool {
    runtime
        .terminal_state
        .as_deref()
        .is_some_and(is_failed_runtime_terminal_state)
        || runtime.runtime_jobs.iter().any(runtime_job_failed)
}

fn is_failed_runtime_terminal_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "failed"
            | "cancelled"
            | "canceled"
            | "timed_out"
            | "timeout"
            | "errored"
            | "error"
            | "expired"
    )
}

fn gate(
    name: HardGateName,
    passed: bool,
    grade_cap: Option<EvalGrade>,
    pass_message: &str,
    fail_message: &str,
) -> HardGateResult {
    HardGateResult {
        name,
        status: if passed {
            GateStatus::Pass
        } else {
            GateStatus::Fail
        },
        grade_cap: if passed { None } else { grade_cap },
        message: if passed { pass_message } else { fail_message }.to_string(),
    }
}

fn head_change_gate(scenario: &EvalScenario, head_changed: bool) -> HardGateResult {
    match scenario {
        EvalScenario::ReadyNoopControl => gate(
            HardGateName::HeadChange,
            !head_changed,
            Some(EvalGrade::C),
            "ready/no-op control kept the PR head unchanged",
            "ready/no-op control changed the PR head",
        ),
        EvalScenario::PrRepair => gate(
            HardGateName::HeadChange,
            head_changed,
            Some(EvalGrade::C),
            "PR repair changed the PR head",
            "PR repair did not change the PR head",
        ),
    }
}

fn reviewer_gate(input: &PrRepairEvalInput) -> HardGateResult {
    match &input.reviewer_judgment {
        Some(judgment) if judgment.judged_head_oid == input.final_pr.head_oid => HardGateResult {
            name: HardGateName::ReviewerJudgmentFreshness,
            status: GateStatus::Pass,
            grade_cap: None,
            message: "reviewer judgment matches the final PR head".to_string(),
        },
        Some(_) => HardGateResult {
            name: HardGateName::ReviewerJudgmentFreshness,
            status: GateStatus::Fail,
            grade_cap: Some(EvalGrade::C),
            message: "reviewer judgment is stale for the final PR head".to_string(),
        },
        None => HardGateResult {
            name: HardGateName::ReviewerJudgmentFreshness,
            status: GateStatus::NotApplicable,
            grade_cap: Some(EvalGrade::B),
            message: "reviewer judgment was not provided; grade is capped at B".to_string(),
        },
    }
}

fn most_restrictive_cap(gates: &[HardGateResult]) -> Option<EvalGrade> {
    gates
        .iter()
        .filter_map(|gate| gate.grade_cap)
        .min_by_key(|grade| grade.rank())
}

#[derive(Copy, Clone)]
struct GateSignals {
    target_matches: bool,
    branch_safe: bool,
    no_unrelated_pr: bool,
    scope_contained: bool,
    final_evidence_fresh: bool,
    checks_passing: bool,
    mergeability_clean: bool,
    review_threads_closed: bool,
    final_remote_evidence_complete: bool,
    runtime_complete: bool,
}

fn score_breakdown(input: &PrRepairEvalInput, gates: GateSignals) -> ScoreBreakdown {
    let head_changed = input.baseline_pr.head_oid != input.final_pr.head_oid;
    let scenario_head_behavior_ok = match input.scenario {
        EvalScenario::PrRepair => head_changed,
        EvalScenario::ReadyNoopControl => !head_changed,
    };
    let current_head_verified = gates.final_evidence_fresh
        && gates.checks_passing
        && gates.mergeability_clean
        && gates.final_remote_evidence_complete;
    let has_usage = input.usage.iter().any(|usage| usage.total_tokens.is_some());
    let has_confident_usage = input.usage.iter().any(|usage| {
        usage.token_confidence != Confidence::Unknown
            || usage.cost_confidence != Confidence::Unknown
    });

    ScoreBreakdown {
        task_classification_and_baseline_evidence: component(
            ScoreDimensionName::TaskClassificationAndBaselineEvidence,
            if gates.target_matches && !input.baseline_pr.collected_at.is_empty() {
                12
            } else {
                4
            },
            12,
        ),
        feedback_discovery_and_prioritization: component(
            ScoreDimensionName::FeedbackDiscoveryAndPrioritization,
            if gates.review_threads_closed { 14 } else { 4 },
            14,
        ),
        branch_and_pr_safety: component(
            ScoreDimensionName::BranchAndPrSafety,
            if gates.target_matches
                && gates.branch_safe
                && gates.no_unrelated_pr
                && gates.scope_contained
            {
                10
            } else {
                0
            },
            10,
        ),
        fix_correctness_and_scope: component(
            ScoreDimensionName::FixCorrectnessAndScope,
            fix_correctness_points(input, scenario_head_behavior_ok),
            22,
        ),
        verification_and_current_head_gates: component(
            ScoreDimensionName::VerificationAndCurrentHeadGates,
            if current_head_verified { 16 } else { 4 },
            16,
        ),
        runtime_workflow_behavior_and_persistence: component(
            ScoreDimensionName::RuntimeWorkflowBehaviorAndPersistence,
            if gates.runtime_complete { 12 } else { 0 },
            12,
        ),
        cost_and_time_efficiency: component(
            ScoreDimensionName::CostAndTimeEfficiency,
            cost_efficiency_points(input.runtime.as_ref(), has_usage),
            8,
        ),
        reporting_and_attribution_quality: component(
            ScoreDimensionName::ReportingAndAttributionQuality,
            if gates.runtime_complete && gates.final_evidence_fresh && has_confident_usage {
                6
            } else {
                2
            },
            6,
        ),
    }
}

fn cost_efficiency_points(runtime: Option<&RuntimeSnapshot>, has_usage: bool) -> u8 {
    let Some(runtime) = runtime else {
        return if has_usage { 6 } else { 4 };
    };
    let failed_jobs = runtime
        .runtime_jobs
        .iter()
        .filter(|job| runtime_job_failed(job))
        .count();
    let repeated_non_retryable = runtime
        .runtime_jobs
        .iter()
        .filter(|job| runtime_job_failed(job) && runtime_job_error_is_non_retryable(job))
        .count()
        > 1;

    if repeated_non_retryable {
        0
    } else if failed_jobs > 1 {
        3
    } else if failed_jobs == 1 {
        6
    } else if has_usage {
        8
    } else {
        4
    }
}

fn runtime_job_failed(job: &RuntimeJobSnapshot) -> bool {
    job.terminal_state
        .as_deref()
        .is_some_and(is_failed_runtime_terminal_state)
        || is_failed_runtime_terminal_state(&job.state)
}

fn runtime_job_error_is_non_retryable(job: &RuntimeJobSnapshot) -> bool {
    matches!(
        job.error_kind,
        Some(RuntimeErrorKind::Fatal | RuntimeErrorKind::Configuration)
    )
}

fn fix_correctness_points(input: &PrRepairEvalInput, changed_or_noop: bool) -> u8 {
    if input.created_unrelated_pr || !input.scope_violations.is_empty() {
        return 4;
    }

    if let Some(judgment) = &input.reviewer_judgment {
        let code_quality = judgment.code_quality_score.min(100) as u16;
        let trajectory = judgment.trajectory_score.min(100) as u16;
        let bounded = (code_quality * 3 + trajectory + 2) / 4;
        return ((bounded * 22 + 50) / 100) as u8;
    }

    if changed_or_noop {
        13
    } else {
        8
    }
}

fn component(name: ScoreDimensionName, points: u8, max_points: u8) -> ScoreComponent {
    ScoreComponent {
        name,
        points: points.min(max_points),
        max_points,
    }
}

#[cfg(test)]
#[path = "scoring_tests.rs"]
mod tests;
