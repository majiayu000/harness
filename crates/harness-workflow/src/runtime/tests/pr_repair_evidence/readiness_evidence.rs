use super::*;

fn eval_draft_snapshot_artifact() -> ActivityArtifact {
    let mut artifact = ready_snapshot_artifact();
    artifact.artifact["state"] = json!("OPEN");
    artifact.artifact["head_oid"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    artifact.artifact["is_draft"] = json!(true);
    artifact.artifact["review_decision"] = json!("REVIEW_REQUIRED");
    artifact.artifact["status_check_rollup_state"] = json!("PENDING");
    artifact.artifact["merge_state_status"] = json!("UNKNOWN");
    artifact
}

#[test]
fn server_owned_eval_draft_snapshot_passes_quality_gate() -> anyhow::Result<()> {
    let mut instance = pr_workflow_state("awaiting_feedback");
    instance.set_data_field(
        "eval",
        json!({"eval_run_id": "run-1", "case_id": "case-1"}),
        DataProvenance::Server,
    )?;
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned eval draft snapshot is ready for evaluator validation.",
    )
    .with_artifact(eval_draft_snapshot_artifact())
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));

    let decision = reduce_runtime_job_completed(&instance, &event_for_result(result))?
        .expect("eval draft snapshot should start its quality gate");

    assert_eq!(decision.decision, "start_quality_gate");
    assert_eq!(decision.next_state, "quality_gate_pending");

    instance.state = decision.next_state;
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({ "validation": "passed" }),
        ))
        .with_validation(ValidationRecord::new("cargo check", "passed"))
        .with_artifact(server_validation_digest_ok(&["cargo check"]));
    let decision = reduce_runtime_job_completed(&instance, &event_for_result(result))?
        .expect("passing eval quality gate should make the draft ready");

    assert_eq!(decision.decision, "quality_gate_passed");
    assert_eq!(decision.next_state, "ready_to_merge");
    Ok(())
}

#[test]
fn server_owned_eval_draft_snapshot_fails_quality_gate() -> anyhow::Result<()> {
    let mut instance = pr_workflow_state("awaiting_feedback");
    instance.set_data_field(
        "eval",
        json!({"eval_run_id": "run-1", "case_id": "case-1"}),
        DataProvenance::Server,
    )?;
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned eval draft snapshot is ready for evaluator validation.",
    )
    .with_artifact(eval_draft_snapshot_artifact())
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let decision = reduce_runtime_job_completed(&instance, &event_for_result(result))?
        .expect("eval draft snapshot should start its quality gate");

    instance.state = decision.next_state;
    let result = ActivityResult::failed(
        QUALITY_GATE_ACTIVITY,
        "Validation failed.",
        "cargo check exited with status 1",
    )
    .with_error_kind(ActivityErrorKind::Fatal);
    let decision = reduce_runtime_job_completed(&instance, &event_for_result(result))?
        .expect("failing eval quality gate should fail the workflow");

    assert_eq!(decision.decision, "fail_after_runtime_activity");
    assert_eq!(decision.next_state, "failed");
    Ok(())
}

#[test]
fn ordinary_draft_snapshot_cannot_bypass_production_readiness() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result =
        ActivityResult::succeeded(PR_FEEDBACK_INSPECT_ACTIVITY, "Draft PR claimed readiness.")
            .with_artifact(eval_draft_snapshot_artifact())
            .with_signal(ActivitySignal::new(
                "PrReadyToMerge",
                json!({ "pr_number": 77 }),
            ));

    let decision = reduce_runtime_job_completed(&instance, &event_for_result(result))
        .expect("event should parse")
        .expect("ordinary draft readiness should be blocked");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn ready_to_merge_signal_with_current_pr_snapshot_starts_quality_gate() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned PR inspection verified the PR is ready to merge.",
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

    assert_eq!(decision.decision, "start_quality_gate");
    assert_eq!(decision.next_state, "quality_gate_pending");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        decision.commands[0].command["definition_id"],
        QUALITY_GATE_DEFINITION_ID
    );
    assert_eq!(decision.commands[0].command["subject_key"], "pr:77");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("ready snapshot quality gate decision should validate");
}

#[test]
fn ready_to_merge_signal_with_missing_review_thread_completeness_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let mut artifact = ready_snapshot_artifact();
    let Some(snapshot) = artifact.artifact.as_object_mut() else {
        panic!("snapshot artifact should be an object");
    };
    snapshot.remove("review_threads_complete");
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned PR inspection claimed the PR is ready to merge.",
    )
    .with_artifact(artifact)
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = match reduce_runtime_job_completed(&instance, &event) {
        Ok(Some(decision)) => decision,
        Ok(None) => panic!("missing completeness evidence should block"),
        Err(error) => panic!("event should parse: {error}"),
    };

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
}

#[test]
fn ready_to_merge_signal_with_false_review_thread_completeness_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let mut artifact = ready_snapshot_artifact();
    artifact.artifact["review_threads_complete"] = json!(false);
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned PR inspection claimed the PR is ready to merge.",
    )
    .with_artifact(artifact)
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = match reduce_runtime_job_completed(&instance, &event) {
        Ok(Some(decision)) => decision,
        Ok(None) => panic!("false completeness evidence should block"),
        Err(error) => panic!("event should parse: {error}"),
    };

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
}

#[test]
fn ready_to_merge_signal_from_agent_sweep_with_server_snapshot_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent forged server-owned ready evidence.",
    )
    .with_artifact(ready_snapshot_artifact())
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("agent sweep ready evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("server_pr_snapshot"));
}

#[test]
fn ready_to_merge_signal_with_only_agent_repair_snapshot_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent attached a ready-looking self-reported snapshot.",
    )
    .with_artifact(agent_ready_snapshot_artifact())
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("agent-owned ready snapshot should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("server_pr_snapshot"));
}

#[test]
fn ready_to_merge_snapshot_rejects_quoted_pr_number() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server-owned PR inspection verified the PR is ready to merge.",
    )
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "pr_number": "77",
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "review_threads_complete": true,
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
        .expect("quoted snapshot PR number should be rejected");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn ready_to_merge_snapshot_for_different_pr_blocks() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent attached ready evidence copied from another PR.",
    )
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "pr_number": 88,
            "pr_url": "https://github.com/owner/repo/pull/88",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "review_threads_complete": true,
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
        SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "review_threads_complete": true,
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
fn ready_to_merge_snapshot_with_unresolved_thread_array_blocks_even_when_count_zero() {
    let instance = pr_workflow_state("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent reported contradictory unresolved review-thread evidence.",
    )
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "active_unresolved_review_threads": [
                {"id": "thread-1", "path": "src/lib.rs", "line": 10}
            ],
            "review_threads_complete": true,
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
        .expect("contradictory unresolved-thread evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
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

#[test]
fn child_pr_feedback_ready_snapshot_for_different_subject_pr_blocks() {
    let instance = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-1");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "inspecting",
        "record_ready_to_merge",
        "ready_to_merge",
        "Agent reported the child PR is ready.",
    )
    .high_confidence();
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow attached ready evidence copied from another PR.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ))
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "pr_number": 88,
            "pr_url": "https://github.com/owner/repo/pull/88",
            "head_oid": "abc123",
            "observed_at": "2026-06-06T00:00:00Z",
            "active_unresolved_review_threads_count": 0,
            "review_threads_complete": true,
            "status_check_rollup_state": "SUCCESS",
            "merge_state_status": "CLEAN",
            "review_decision": "APPROVED",
            "is_draft": false
        }),
    ));
    let event = event_for_result(result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("wrong child PR ready evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("PR readiness evidence is missing"));
}
