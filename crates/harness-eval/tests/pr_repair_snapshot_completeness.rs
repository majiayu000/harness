use harness_eval::{
    github_pr_snapshot_from_value, score_pr_repair_eval, Confidence, EvalGrade, EvalScenario,
    EvalTarget, GateStatus, HardGateName, PrRepairEvalInput, PullRequestSnapshot, ReviewerJudgment,
    ReviewerKind, RuntimeJobSnapshot, RuntimeSnapshot, UsageSnapshot,
};
use serde_json::json;

#[test]
fn incomplete_raw_review_threads_fail_closure_gate() {
    let baseline = raw_pr_snapshot("base-head", false);
    let final_pr = raw_pr_snapshot("final-head", true);
    let input = eval_input(
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:00:00Z", &baseline),
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:01:00Z", &final_pr),
    );

    let snapshot = score_pr_repair_eval(input).expect("score incomplete raw snapshot");
    let gate = snapshot_gate(&snapshot.hard_gates, HardGateName::ReviewThreadClosure);

    assert_eq!(gate.status, GateStatus::Fail);
    assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
    assert_eq!(
        snapshot
            .objective_score
            .feedback_discovery_and_prioritization
            .points,
        4
    );
}

#[test]
fn missing_raw_review_thread_page_info_fails_closure_gate() {
    let baseline = raw_pr_snapshot("base-head", false);
    let final_pr = raw_pr_snapshot_without_review_thread_page_info("final-head");
    let input = eval_input(
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:00:00Z", &baseline),
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:01:00Z", &final_pr),
    );

    let snapshot = score_pr_repair_eval(input).expect("score missing review page info");
    let gate = snapshot_gate(&snapshot.hard_gates, HardGateName::ReviewThreadClosure);

    assert_eq!(gate.status, GateStatus::Fail);
    assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
}

#[test]
fn incomplete_server_normalized_review_threads_fail_closure_gate() {
    let baseline = server_normalized_snapshot("base-head", true, true);
    let final_pr = server_normalized_snapshot("final-head", false, true);
    let input = eval_input(
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:00:00Z", &baseline),
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:01:00Z", &final_pr),
    );

    let snapshot = score_pr_repair_eval(input).expect("score incomplete server snapshot");
    let gate = snapshot_gate(&snapshot.hard_gates, HardGateName::ReviewThreadClosure);

    assert_eq!(gate.status, GateStatus::Fail);
    assert_eq!(snapshot.grade_cap, Some(EvalGrade::C));
}

#[test]
fn missing_raw_changed_files_page_info_penalizes_current_head_verification() {
    let baseline = raw_pr_snapshot("base-head", false);
    let final_pr = raw_pr_snapshot_without_files_page_info("final-head");
    let input = eval_input(
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:00:00Z", &baseline),
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:01:00Z", &final_pr),
    );

    let snapshot = score_pr_repair_eval(input).expect("score missing files page info");

    assert_eq!(snapshot.grade_cap, None);
    assert_eq!(
        snapshot
            .objective_score
            .verification_and_current_head_gates
            .points,
        4
    );
}

#[test]
fn incomplete_changed_files_penalize_current_head_verification() {
    let baseline = server_normalized_snapshot("base-head", true, true);
    let final_pr = server_normalized_snapshot("final-head", true, false);
    let input = eval_input(
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:00:00Z", &baseline),
        github_pr_snapshot_from_value("owner/repo", "2026-06-06T00:01:00Z", &final_pr),
    );

    let snapshot = score_pr_repair_eval(input).expect("score incomplete files snapshot");

    assert_eq!(snapshot.grade_cap, None);
    assert_eq!(
        snapshot
            .objective_score
            .verification_and_current_head_gates
            .points,
        4
    );
    assert_eq!(snapshot.final_score, 88);
}

fn raw_pr_snapshot(head_oid: &str, review_threads_has_next_page: bool) -> serde_json::Value {
    json!({
        "number": 42,
        "url": "https://github.com/owner/repo/pull/42",
        "title": "Fix PR repair",
        "baseRefName": "main",
        "headRefName": "fix/pr-42",
        "headRefOid": head_oid,
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {
            "pageInfo": {"hasNextPage": review_threads_has_next_page},
            "nodes": []
        },
        "files": {
            "pageInfo": {"hasNextPage": false},
            "nodes": [
                {"path": "src/lib.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}
            ]
        }
    })
}

fn raw_pr_snapshot_without_review_thread_page_info(head_oid: &str) -> serde_json::Value {
    json!({
        "number": 42,
        "url": "https://github.com/owner/repo/pull/42",
        "title": "Fix PR repair",
        "baseRefName": "main",
        "headRefName": "fix/pr-42",
        "headRefOid": head_oid,
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {"nodes": []},
        "files": {
            "pageInfo": {"hasNextPage": false},
            "nodes": [
                {"path": "src/lib.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}
            ]
        }
    })
}

fn raw_pr_snapshot_without_files_page_info(head_oid: &str) -> serde_json::Value {
    json!({
        "number": 42,
        "url": "https://github.com/owner/repo/pull/42",
        "title": "Fix PR repair",
        "baseRefName": "main",
        "headRefName": "fix/pr-42",
        "headRefOid": head_oid,
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {
            "pageInfo": {"hasNextPage": false},
            "nodes": []
        },
        "files": {
            "nodes": [
                {"path": "src/lib.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}
            ]
        }
    })
}

fn server_normalized_snapshot(
    head_oid: &str,
    review_threads_complete: bool,
    changed_files_complete: bool,
) -> serde_json::Value {
    json!({
        "pr_number": 42,
        "pr_url": "https://github.com/owner/repo/pull/42",
        "title": "Fix PR repair",
        "base_ref": "main",
        "head_ref": "fix/pr-42",
        "head_oid": head_oid,
        "is_draft": false,
        "merge_state_status": "CLEAN",
        "review_decision": "APPROVED",
        "status_check_rollup_state": "SUCCESS",
        "active_unresolved_review_threads": [],
        "review_threads_complete": review_threads_complete,
        "changed_files": [
            {"path": "src/lib.rs", "additions": 3, "deletions": 1, "status": "MODIFIED"}
        ],
        "changed_files_complete": changed_files_complete
    })
}

fn eval_input(
    baseline_pr: PullRequestSnapshot,
    final_pr: PullRequestSnapshot,
) -> PrRepairEvalInput {
    let final_head_oid = final_pr.head_oid.clone();
    PrRepairEvalInput {
        scenario: EvalScenario::PrRepair,
        target: EvalTarget::PullRequest {
            repo: "owner/repo".to_string(),
            pr_number: 42,
            base_ref: Some("main".to_string()),
            head_ref: Some("fix/pr-42".to_string()),
        },
        final_evidence_head_oid: Some(final_head_oid.clone()),
        baseline_pr,
        final_pr,
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
            collected_at: "2026-06-06T00:01:00Z".to_string(),
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
        reviewer_judgment: Some(ReviewerJudgment {
            reviewer_kind: ReviewerKind::Human,
            judged_head_oid: final_head_oid,
            code_quality_score: 100,
            trajectory_score: 100,
            findings: Vec::new(),
            residual_risks: Vec::new(),
        }),
        created_unrelated_pr: false,
        scope_violations: Vec::new(),
    }
}

fn snapshot_gate(
    gates: &[harness_eval::HardGateResult],
    name: HardGateName,
) -> &harness_eval::HardGateResult {
    gates
        .iter()
        .find(|gate| gate.name == name)
        .expect("gate should exist")
}
