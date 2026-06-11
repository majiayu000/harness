use super::*;
use crate::model::{
    ChangedFileSnapshot, MergeState, PullRequestSnapshot, ReviewerJudgment, ReviewerKind,
    RuntimeErrorKind, RuntimeJobSnapshot, UsageSnapshot,
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
        "expired",
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
fn expired_runtime_job_state_caps_at_b_when_terminal_state_is_missing() {
    let mut input = perfect_input();
    let Some(runtime) = input.runtime.as_mut() else {
        panic!("runtime should exist");
    };
    runtime.terminal_state = Some("succeeded".to_string());
    runtime.runtime_jobs[0].artifact_count = 1;
    runtime.runtime_jobs[0].state = "expired".to_string();
    runtime.runtime_jobs[0].terminal_state = None;

    let snapshot = match score_pr_repair_eval(input) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("score expired job runtime input failed: {error}"),
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
fn repeated_non_retryable_runtime_failures_zero_cost_efficiency() {
    let mut input = perfect_input();
    let Some(runtime) = input.runtime.as_mut() else {
        panic!("runtime should exist");
    };
    runtime.runtime_jobs = vec![
        failed_job("job-1", RuntimeErrorKind::Configuration),
        failed_job("job-2", RuntimeErrorKind::Configuration),
    ];

    let snapshot = score_pr_repair_eval(input).expect("score repeated configuration failures");

    assert_eq!(snapshot.objective_score.cost_and_time_efficiency.points, 0);
}

#[test]
fn single_failed_runtime_job_partially_penalizes_cost_efficiency() {
    let mut input = perfect_input();
    let Some(runtime) = input.runtime.as_mut() else {
        panic!("runtime should exist");
    };
    runtime.runtime_jobs = vec![failed_job("job-1", RuntimeErrorKind::Timeout)];

    let snapshot = score_pr_repair_eval(input).expect("score single failed runtime job");

    assert_eq!(snapshot.objective_score.cost_and_time_efficiency.points, 6);
}

#[test]
fn expired_runtime_job_state_partially_penalizes_cost_efficiency() {
    let mut input = perfect_input();
    let Some(runtime) = input.runtime.as_mut() else {
        panic!("runtime should exist");
    };
    runtime.runtime_jobs[0].state = "expired".to_string();
    runtime.runtime_jobs[0].terminal_state = None;

    let snapshot = match score_pr_repair_eval(input) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("score expired runtime job failed: {error}"),
    };

    assert_eq!(snapshot.objective_score.cost_and_time_efficiency.points, 6);
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
        run_mode: crate::model::EvalRunMode::LiveRun,
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
                activity: Some("implement_issue".to_string()),
                artifact_count: 1,
                terminal_state: Some("succeeded".to_string()),
                error_kind: None,
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

fn failed_job(id: &str, error_kind: RuntimeErrorKind) -> RuntimeJobSnapshot {
    RuntimeJobSnapshot {
        runtime_job_id: id.to_string(),
        state: "failed".to_string(),
        activity: Some("implement_issue".to_string()),
        artifact_count: 1,
        terminal_state: Some("failed".to_string()),
        error_kind: Some(error_kind),
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
        review_threads_complete: true,
        changed_files,
        changed_files_complete: true,
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
