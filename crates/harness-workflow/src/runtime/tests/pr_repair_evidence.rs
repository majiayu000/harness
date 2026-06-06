use super::*;
use crate::runtime::PR_REPAIR_SNAPSHOT_ARTIFACT;

fn pr_workflow_state(state: &str) -> WorkflowInstance {
    issue_instance(state).with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-1",
    }))
}

fn event_for_result(result: ActivityResult) -> WorkflowEvent {
    WorkflowEvent::new(
        "workflow-1",
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }))
}

fn ready_snapshot_artifact() -> ActivityArtifact {
    ActivityArtifact::new(
        PR_REPAIR_SNAPSHOT_ARTIFACT,
        json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads": [],
            "active_unresolved_review_threads_count": 0,
            "status_check_rollup_state": "SUCCESS",
            "merge_state_status": "CLEAN",
            "review_decision": "APPROVED",
            "is_draft": false,
            "changed_files": [],
            "action_taken": "confirmed_ready",
            "validation": [
                {"command": "cargo test -p harness-workflow pr_repair_evidence", "status": "passed"}
            ]
        }),
    )
}

fn repair_snapshot_artifact() -> ActivityArtifact {
    ActivityArtifact::new(
        PR_REPAIR_SNAPSHOT_ARTIFACT,
        json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_sha": "def456",
            "observed_at": "2026-06-06T00:05:00Z",
            "changed_files": ["crates/harness-workflow/src/runtime/reducer/pr_feedback_completion.rs"],
            "action_taken": "pushed_commit",
            "validation_commands": [
                {"command": "cargo test -p harness-workflow pr_repair_evidence", "status": "passed"}
            ]
        }),
    )
}

#[test]
fn ready_to_merge_signal_without_current_pr_snapshot_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent claimed the PR is ready to merge.",
    )
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing snapshot should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("missing snapshot block should validate");
}

#[test]
fn structured_ready_decision_without_current_pr_snapshot_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "Agent reported the PR is ready.",
    )
    .high_confidence();
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime agent emitted a ready workflow decision.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing snapshot should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
}

#[test]
fn structured_ready_decision_with_blocking_signal_uses_blocking_feedback() {
    let instance = pr_workflow_state("awaiting_feedback");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "Agent reported the PR is ready.",
    )
    .high_confidence();
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime agent emitted both ready and blocking feedback.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ))
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocking signal should override structured ready output");

    assert_eq!(decision.decision, "address_pr_feedback");
    assert_eq!(decision.next_state, "addressing_feedback");
}

#[test]
fn address_pr_feedback_success_without_repair_evidence_blocks() {
    let instance = pr_workflow_state("addressing_feedback");
    let result = ActivityResult::succeeded(
        "address_pr_feedback",
        "Runtime agent claimed review feedback was addressed.",
    );
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing repair evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR repair evidence is missing"));
}

#[test]
fn address_pr_feedback_success_with_repair_snapshot_requests_local_review() {
    let instance = pr_workflow_state("addressing_feedback");
    let result = ActivityResult::succeeded(
        "address_pr_feedback",
        "Runtime agent addressed review feedback and pushed validation-backed changes.",
    )
    .with_artifact(repair_snapshot_artifact())
    .with_validation(ValidationRecord::new(
        "cargo test -p harness-workflow pr_repair_evidence",
        "passed",
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("valid repair snapshot should request local review");

    assert_eq!(decision.decision, "run_local_review_after_rework");
    assert_eq!(decision.next_state, "local_review_gate");
    assert_eq!(
        decision.commands[0].activity_name(),
        Some(LOCAL_REVIEW_ACTIVITY)
    );
}

#[test]
fn address_pr_feedback_snapshot_for_different_pr_blocks() {
    let instance = pr_workflow_state("addressing_feedback");
    let result = ActivityResult::succeeded(
        "address_pr_feedback",
        "Runtime agent attached repair evidence copied from another PR.",
    )
    .with_artifact(ActivityArtifact::new(
        PR_REPAIR_SNAPSHOT_ARTIFACT,
        json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/88",
            "head_sha": "def456",
            "observed_at": "2026-06-06T00:05:00Z",
            "changed_files": ["crates/harness-workflow/src/runtime/reducer/pr_feedback_completion.rs"],
            "validation_commands": [
                {"command": "cargo test -p harness-workflow pr_repair_evidence", "status": "passed"}
            ]
        }),
    ))
    .with_validation(ValidationRecord::new(
        "cargo test -p harness-workflow pr_repair_evidence",
        "passed",
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("wrong PR repair evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR repair evidence is missing"));
}

#[test]
fn ready_to_merge_signal_with_current_pr_snapshot_can_mark_ready() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent verified the PR is ready to merge.",
    )
    .with_artifact(ready_snapshot_artifact())
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("valid snapshot should reduce");

    assert_eq!(decision.decision, "mark_ready_to_merge");
    assert_eq!(decision.next_state, "ready_to_merge");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("ready snapshot decision should validate");
}

#[test]
fn ready_to_merge_snapshot_for_different_pr_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent attached ready evidence copied from another PR.",
    )
    .with_artifact(ActivityArtifact::new(
        PR_REPAIR_SNAPSHOT_ARTIFACT,
        json!({
            "pr_number": 88,
            "pr_url": "https://github.com/owner/repo/pull/88",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "status_check_rollup_state": "SUCCESS",
            "merge_state_status": "CLEAN",
            "review_decision": "APPROVED",
            "is_draft": false
        }),
    ))
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("wrong PR ready evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
}

#[test]
fn ready_to_merge_snapshot_with_required_review_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent claimed the PR is ready without an approval.",
    )
    .with_artifact(ActivityArtifact::new(
        PR_REPAIR_SNAPSHOT_ARTIFACT,
        json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "status_check_rollup_state": "SUCCESS",
            "merge_state_status": "CLEAN",
            "review_decision": "REVIEW_REQUIRED",
            "is_draft": false
        }),
    ))
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing approval should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn child_pr_feedback_ready_signal_without_snapshot_blocks() {
    let instance = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-1");
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow claimed the PR is ready to merge.",
    )
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("child missing snapshot should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    DecisionValidator::pr_feedback()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("child missing snapshot block should validate");
}
