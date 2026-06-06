use crate::model::{
    CheckState, Confidence, EvalGrade, EvalScenario, EvalTarget, GateStatus, HardGateName,
    HardGateResult, MergeState, PrRepairEvalInput, QualitySnapshot, RuntimeSnapshot,
    ScoreBreakdown, ScoreComponent, ScoreDimensionName,
};
use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScoringError {
    UnsupportedTarget,
}

impl fmt::Display for ScoringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget => {
                write!(f, "PR repair scoring requires a pull request target")
            }
        }
    }
}

impl Error for ScoringError {}

pub fn score_pr_repair_eval(input: PrRepairEvalInput) -> Result<QualitySnapshot, ScoringError> {
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
    let review_threads_closed = input.final_pr.active_unresolved_review_threads.is_empty();
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
            "no active unresolved review threads remain",
            "active unresolved review threads remain",
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
        || runtime.runtime_jobs.iter().any(|job| {
            job.terminal_state
                .as_deref()
                .is_some_and(is_failed_runtime_terminal_state)
        })
}

fn is_failed_runtime_terminal_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "failed" | "cancelled" | "canceled" | "timed_out" | "timeout" | "errored" | "error"
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
    runtime_complete: bool,
}

fn score_breakdown(input: &PrRepairEvalInput, gates: GateSignals) -> ScoreBreakdown {
    let head_changed = input.baseline_pr.head_oid != input.final_pr.head_oid;
    let scenario_head_behavior_ok = match input.scenario {
        EvalScenario::PrRepair => head_changed,
        EvalScenario::ReadyNoopControl => !head_changed,
    };
    let current_head_verified =
        gates.final_evidence_fresh && gates.checks_passing && gates.mergeability_clean;
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
            if has_usage { 8 } else { 4 },
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
mod tests {
    use super::*;
    use crate::model::{
        ChangedFileSnapshot, MergeState, PullRequestSnapshot, ReviewerJudgment, ReviewerKind,
        RuntimeJobSnapshot, UsageSnapshot,
    };

    #[test]
    fn ready_noop_control_accepts_unchanged_head() {
        let mut input = perfect_input();
        input.scenario = EvalScenario::ReadyNoopControl;
        input.baseline_pr.head_oid = "same-head".to_string();
        input.final_pr.head_oid = "same-head".to_string();
        input.final_pr.changed_files.clear();
        input.final_evidence_head_oid = Some("same-head".to_string());
        input.reviewer_judgment = Some(reviewer_judgment("same-head"));

        let snapshot = score_pr_repair_eval(input).expect("score ready/no-op input");
        let gate = find_gate(&snapshot, HardGateName::HeadChange);

        assert_eq!(gate.status, GateStatus::Pass);
        assert_eq!(gate.grade_cap, None);
        assert_eq!(snapshot.grade_cap, None);
    }

    #[test]
    fn ready_noop_control_changed_head_caps_and_penalizes() {
        let mut input = perfect_input();
        input.scenario = EvalScenario::ReadyNoopControl;
        input.baseline_pr.head_oid = "base-head".to_string();
        input.final_pr.head_oid = "changed-head".to_string();
        input.final_evidence_head_oid = Some("changed-head".to_string());
        input.reviewer_judgment = None;

        let snapshot = match score_pr_repair_eval(input) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("score changed ready/no-op input failed: {error}"),
        };
        let gate = find_gate(&snapshot, HardGateName::HeadChange);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.final_grade, EvalGrade::C);
        assert_eq!(snapshot.objective_score.fix_correctness_and_scope.points, 8);
    }

    #[test]
    fn unresolved_active_review_threads_cap_and_fail() {
        let mut input = perfect_input();
        input
            .final_pr
            .active_unresolved_review_threads
            .push(crate::model::ReviewThreadSnapshot {
                id: "thread-1".to_string(),
                path: Some("src/lib.rs".to_string()),
                is_resolved: false,
                is_outdated: false,
            });

        let snapshot = score_pr_repair_eval(input).expect("score input with unresolved thread");
        let gate = find_gate(&snapshot, HardGateName::ReviewThreadClosure);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.final_grade, EvalGrade::C);
    }

    #[test]
    fn non_clean_mergeability_caps_and_fails() {
        let mut input = perfect_input();
        input.final_pr.merge_state = MergeState::Blocked;

        let snapshot = match score_pr_repair_eval(input) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("score non-clean mergeability input failed: {error}"),
        };
        let gate = find_gate(&snapshot, HardGateName::MergeabilityClean);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.final_grade, EvalGrade::C);
        assert!(snapshot
            .blocker_summary
            .contains(&"final PR mergeability is not clean".to_string()));
    }

    #[test]
    fn stale_final_evidence_caps_and_fails() {
        let mut input = perfect_input();
        input.final_evidence_head_oid = Some("old-head".to_string());

        let snapshot = score_pr_repair_eval(input).expect("score stale final evidence");
        let gate = find_gate(&snapshot, HardGateName::HeadFreshness);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.final_grade, EvalGrade::C);
    }

    #[test]
    fn missing_runtime_artifact_caps_at_b() {
        let mut input = perfect_input();
        input.runtime = None;

        let snapshot = score_pr_repair_eval(input).expect("score missing runtime input");
        let gate = find_gate(&snapshot, HardGateName::RuntimeArtifactCompleteness);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.final_grade, EvalGrade::B);
    }

    #[test]
    fn partial_runtime_artifact_without_workflow_identity_caps_at_b() {
        let mut input = perfect_input();
        input.runtime = Some(RuntimeSnapshot {
            task_id: Some("task-1".to_string()),
            workflow_id: None,
            workflow_state: None,
            runtime_jobs: Vec::new(),
            latest_activity: None,
            terminal_state: Some("succeeded".to_string()),
            collected_at: "2026-06-05T00:10:00Z".to_string(),
        });

        let snapshot = match score_pr_repair_eval(input) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("score partial runtime input failed: {error}"),
        };
        let gate = find_gate(&snapshot, HardGateName::RuntimeArtifactCompleteness);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::B));
    }

    #[test]
    fn failed_runtime_terminal_states_cap_at_b() {
        for state in [
            "failed",
            "cancelled",
            "canceled",
            "timed_out",
            "timeout",
            "errored",
            "error",
        ] {
            let mut input = perfect_input();
            let Some(runtime) = input.runtime.as_mut() else {
                panic!("runtime should exist");
            };
            runtime.terminal_state = Some(state.to_string());
            runtime.runtime_jobs[0].artifact_count = 1;
            runtime.runtime_jobs[0].terminal_state = Some("succeeded".to_string());

            let snapshot = match score_pr_repair_eval(input) {
                Ok(snapshot) => snapshot,
                Err(error) => panic!("score failed runtime input failed: {error}"),
            };
            let gate = find_gate(&snapshot, HardGateName::RuntimeArtifactCompleteness);

            assert_eq!(gate.status, GateStatus::Fail);
            assert_eq!(gate.grade_cap, Some(EvalGrade::B));
            assert_eq!(snapshot.grade_cap, Some(EvalGrade::B));
            assert_eq!(snapshot.final_grade, EvalGrade::B);
        }
    }

    #[test]
    fn failed_runtime_job_terminal_state_caps_at_b_even_with_artifacts() {
        let mut input = perfect_input();
        let Some(runtime) = input.runtime.as_mut() else {
            panic!("runtime should exist");
        };
        runtime.terminal_state = Some("succeeded".to_string());
        runtime.runtime_jobs[0].artifact_count = 1;
        runtime.runtime_jobs[0].terminal_state = Some("failed".to_string());

        let snapshot = match score_pr_repair_eval(input) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("score failed job runtime input failed: {error}"),
        };
        let gate = find_gate(&snapshot, HardGateName::RuntimeArtifactCompleteness);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::B));
    }

    #[test]
    fn auditable_runtime_terminal_states_count_as_runtime_evidence() {
        for state in ["ready_to_merge", "blocked"] {
            let mut input = perfect_input();
            let Some(runtime) = input.runtime.as_mut() else {
                panic!("runtime should exist");
            };
            runtime.terminal_state = Some(state.to_string());
            runtime.runtime_jobs.clear();

            let snapshot = match score_pr_repair_eval(input) {
                Ok(snapshot) => snapshot,
                Err(error) => panic!("score auditable runtime input failed: {error}"),
            };
            let gate = find_gate(&snapshot, HardGateName::RuntimeArtifactCompleteness);

            assert_eq!(gate.status, GateStatus::Pass);
            assert_eq!(gate.grade_cap, None);
            assert_eq!(snapshot.grade_cap, None);
        }
    }

    #[test]
    fn stale_reviewer_judgment_caps_and_fails() {
        let mut input = perfect_input();
        input.reviewer_judgment = Some(reviewer_judgment("old-head"));

        let snapshot = score_pr_repair_eval(input).expect("score stale reviewer input");
        let gate = find_gate(&snapshot, HardGateName::ReviewerJudgmentFreshness);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
        assert_eq!(snapshot.final_grade, EvalGrade::C);
        assert!(snapshot
            .blocker_summary
            .contains(&"reviewer judgment is stale for the final PR head".to_string()));
    }

    #[test]
    fn missing_reviewer_judgment_caps_at_b() {
        let mut input = perfect_input();
        input.reviewer_judgment = None;

        let snapshot = score_pr_repair_eval(input).expect("score missing reviewer input");
        let gate = find_gate(&snapshot, HardGateName::ReviewerJudgmentFreshness);

        assert_eq!(gate.status, GateStatus::NotApplicable);
        assert_eq!(gate.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::B));
        assert_eq!(snapshot.final_grade, EvalGrade::B);
    }

    #[test]
    fn unrelated_pr_creation_caps_at_f() {
        let mut input = perfect_input();
        input.created_unrelated_pr = true;

        let snapshot = score_pr_repair_eval(input).expect("score unrelated PR input");
        let gate = find_gate(&snapshot, HardGateName::NoUnrelatedPrCreation);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::F));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::F));
        assert_eq!(snapshot.final_grade, EvalGrade::F);
    }

    #[test]
    fn destructive_or_unrelated_scope_changes_cap_at_f() {
        let mut input = perfect_input();
        input
            .scope_violations
            .push("changed unrelated dashboard files".to_string());

        let snapshot = score_pr_repair_eval(input).expect("score scope violation input");
        let gate = find_gate(&snapshot, HardGateName::ScopeContainment);

        assert_eq!(gate.status, GateStatus::Fail);
        assert_eq!(gate.grade_cap, Some(EvalGrade::F));
        assert_eq!(snapshot.grade_cap, Some(EvalGrade::F));
        assert_eq!(snapshot.final_grade, EvalGrade::F);
    }

    #[test]
    fn trajectory_score_contributes_to_fix_correctness() {
        let mut input = perfect_input();
        input.reviewer_judgment = Some(ReviewerJudgment {
            code_quality_score: 100,
            trajectory_score: 0,
            ..reviewer_judgment("final-head")
        });

        let snapshot = score_pr_repair_eval(input).expect("score low trajectory input");

        assert_eq!(
            snapshot.objective_score.fix_correctness_and_scope.points,
            17
        );
    }

    #[test]
    fn basic_score_breakdown_returns_deterministic_total() {
        let snapshot = score_pr_repair_eval(perfect_input()).expect("score perfect input");

        assert_eq!(snapshot.objective_score.max_total(), 100);
        assert_eq!(snapshot.objective_score.total(), 100);
        assert_eq!(snapshot.final_score, 100);
        assert_eq!(snapshot.final_grade, EvalGrade::A);
        assert_eq!(snapshot.grade_cap, None);
    }

    fn find_gate(snapshot: &QualitySnapshot, name: HardGateName) -> &HardGateResult {
        snapshot
            .hard_gates
            .iter()
            .find(|gate| gate.name == name)
            .expect("gate should exist")
    }

    fn perfect_input() -> PrRepairEvalInput {
        PrRepairEvalInput {
            scenario: EvalScenario::PrRepair,
            target: EvalTarget::PullRequest {
                repo: "owner/repo".to_string(),
                pr_number: 42,
                base_ref: Some("main".to_string()),
                head_ref: Some("fix/pr-42".to_string()),
            },
            baseline_pr: pr_snapshot("base-head", Vec::new()),
            final_pr: pr_snapshot(
                "final-head",
                vec![ChangedFileSnapshot {
                    path: "src/lib.rs".to_string(),
                    additions: 12,
                    deletions: 3,
                    status: "modified".to_string(),
                }],
            ),
            final_evidence_head_oid: Some("final-head".to_string()),
            runtime: Some(RuntimeSnapshot {
                task_id: Some("task-1".to_string()),
                workflow_id: Some("workflow-1".to_string()),
                workflow_state: Some("completed".to_string()),
                runtime_jobs: vec![RuntimeJobSnapshot {
                    runtime_job_id: "job-1".to_string(),
                    state: "completed".to_string(),
                    artifact_count: 1,
                    terminal_state: Some("succeeded".to_string()),
                }],
                latest_activity: Some("completed".to_string()),
                terminal_state: Some("succeeded".to_string()),
                collected_at: "2026-06-05T00:10:00Z".to_string(),
            }),
            usage: vec![UsageSnapshot {
                agent_invocation_id: Some("invoke-1".to_string()),
                runtime_job_id: Some("job-1".to_string()),
                workflow_id: Some("workflow-1".to_string()),
                model: Some("codex".to_string()),
                reasoning_effort: Some("medium".to_string()),
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cached_input_tokens: Some(100),
                total_tokens: Some(1200),
                cost_usd_micros: Some(120_000),
                token_confidence: Confidence::Exact,
                cost_confidence: Confidence::Estimated,
            }],
            reviewer_judgment: Some(reviewer_judgment("final-head")),
            created_unrelated_pr: false,
            scope_violations: Vec::new(),
        }
    }

    fn pr_snapshot(head_oid: &str, changed_files: Vec<ChangedFileSnapshot>) -> PullRequestSnapshot {
        PullRequestSnapshot {
            repo: "owner/repo".to_string(),
            pr_number: 42,
            url: Some("https://github.com/owner/repo/pull/42".to_string()),
            title: Some("Fix issue".to_string()),
            base_ref: "main".to_string(),
            head_ref: "fix/pr-42".to_string(),
            head_oid: head_oid.to_string(),
            is_draft: false,
            merge_state: MergeState::Clean,
            check_state: CheckState::Passing,
            review_decision: Some(crate::model::ReviewDecision::Approved),
            active_unresolved_review_threads: Vec::new(),
            changed_files,
            collected_at: "2026-06-05T00:00:00Z".to_string(),
        }
    }

    fn reviewer_judgment(head_oid: &str) -> ReviewerJudgment {
        ReviewerJudgment {
            reviewer_kind: ReviewerKind::Human,
            judged_head_oid: head_oid.to_string(),
            code_quality_score: 100,
            trajectory_score: 100,
            findings: Vec::new(),
            residual_risks: Vec::new(),
        }
    }
}
