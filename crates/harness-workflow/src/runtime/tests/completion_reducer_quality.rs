#[test]
fn runtime_completion_reducer_maps_pr_feedback_sweep_signal_to_address_command() {
    let instance = issue_instance("awaiting_feedback").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-1",
    }));
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent found actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "count": 2, "pr_number": 77 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("feedback sweep signal should reduce");

    assert_eq!(decision.decision, "address_pr_feedback");
    assert_eq!(decision.next_state, "addressing_feedback");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].activity_name(),
        Some("address_pr_feedback")
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("feedback sweep signal decision should validate");
}

#[test]
fn runtime_completion_reducer_maps_pr_feedback_child_signal_to_parent_decision() {
    let instance = issue_instance("awaiting_feedback").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-1",
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "count": 2, "pr_number": 77 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "child-command-1",
        "runtime_job_id": "child-job-1",
        "child_workflow_id": "pr-feedback-child-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("child feedback signal should reduce on parent issue workflow");

    assert_eq!(decision.decision, "address_pr_feedback");
    assert_eq!(decision.next_state, "addressing_feedback");
    assert_eq!(
        decision.commands[0].activity_name(),
        Some("address_pr_feedback")
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("child feedback signal decision should validate on parent");
}

#[test]
fn runtime_completion_reducer_updates_pr_feedback_child_from_inspection_signal() {
    let instance = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-1");
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found no actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "NoFeedbackFound",
        json!({ "pr_number": 77 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "child-command-1",
        "runtime_job_id": "child-job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("child feedback inspection signal should reduce");

    assert_eq!(decision.decision, "record_no_actionable_feedback");
    assert_eq!(decision.next_state, "no_actionable_feedback");
    assert!(decision.commands.is_empty());
    DecisionValidator::pr_feedback()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("child feedback inspection decision should validate");
}

#[test]
fn runtime_completion_reducer_uses_pr_feedback_child_signal_when_structured_decision_is_invalid() {
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
        "invalid_child_feedback_decision",
        "addressing_feedback",
        "This child decision targets the parent workflow state.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "address_pr_feedback",
        "invalid-child-feedback",
    ));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found actionable PR feedback.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ))
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "pr_number": 77 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "child-command-1",
        "runtime_job_id": "child-job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("child feedback inspection signal should reduce");

    assert_eq!(decision.decision, "record_feedback_found");
    assert_eq!(decision.next_state, "feedback_found");
    assert!(decision.commands.is_empty());
    DecisionValidator::pr_feedback()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("child feedback inspection decision should validate");
}

#[test]
fn runtime_completion_reducer_prefers_blocking_pr_feedback_over_ready_signal() {
    let instance = issue_instance("awaiting_feedback").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-1",
    }));
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Runtime agent emitted mixed PR feedback signals.",
    )
    .with_signal(ActivitySignal::new(
        "PrReadyToMerge",
        json!({ "pr_number": 77 }),
    ))
    .with_signal(ActivitySignal::new(
        "ChecksFailed",
        json!({ "pr_number": 77, "failed": 1 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("mixed feedback sweep signals should reduce conservatively");

    assert_eq!(decision.decision, "address_pr_feedback");
    assert_eq!(decision.next_state, "addressing_feedback");
    assert_eq!(
        decision.commands[0].activity_name(),
        Some("address_pr_feedback")
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocking feedback should validate");
}

#[test]
fn runtime_completion_reducer_blocks_invalid_structured_workflow_decision() {
    let instance = issue_instance("pr_open");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "This decision observed the wrong state.",
    )
    .with_command(WorkflowCommand::wait(
        "Waiting for fresh PR feedback.",
        "wait-feedback-1",
    ));
    let result = ActivityResult::succeeded(
        "inspect_pr_feedback",
        "No actionable PR feedback was found.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("invalid structured workflow decision should be blocked");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    assert!(decision
        .commands
        .iter()
        .any(|command| { command.command_type == WorkflowCommandType::RequestOperatorAttention }));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("invalid structured workflow decision should reduce to a valid blocked decision");
}

#[test]
fn runtime_completion_reducer_blocks_unknown_success_without_structured_decision() {
    let instance = issue_instance("awaiting_feedback");
    let result = ActivityResult::succeeded(
        "unexpected_activity",
        "Unexpected activity completed without a workflow decision.",
    );
    let event = runtime_completion_event(&instance, "unexpected_activity", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("unknown success should block instead of silently producing no decision");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .reason
        .contains("no reducer fallback was available"));
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("unknown successful activity should reduce to a valid blocked decision");
}

#[test]
fn runtime_completion_reducer_ignores_stale_pr_feedback_sweep_after_parent_advances() {
    let instance = issue_instance("ready_to_merge").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        "sweep_pr_feedback",
        "Late feedback sweep found actionable feedback after the parent advanced.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "count": 1, "pr_number": 77 }),
    ));
    let event = runtime_completion_event(&instance, "sweep_pr_feedback", result);

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "late feedback sweep completion should be treated as obsolete"
    );
}

#[test]
fn runtime_completion_reducer_ignores_stale_pr_feedback_child_after_parent_advances() {
    let instance = issue_instance("ready_to_merge").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Late feedback child reported no actionable feedback after the parent advanced.",
    )
    .with_signal(ActivitySignal::new(
        "NoFeedbackFound",
        json!({ "pr_number": 77 }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "child-command-1",
        "runtime_job_id": "child-job-1",
        "child_workflow_id": "pr-feedback-child-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "late feedback child completion should be treated as obsolete"
    );
}

#[test]
fn runtime_completion_reducer_ignores_stale_local_review_after_pass_advances_parent() {
    let instance = issue_instance("awaiting_feedback").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Late local review passed after the parent advanced.",
    )
    .with_signal(ActivitySignal::new(
        "LocalReviewPassed",
        json!({ "pr_number": 77 }),
    ));
    let event = runtime_completion_event(&instance, LOCAL_REVIEW_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "late local review pass should be treated as obsolete"
    );
}

#[test]
fn runtime_completion_reducer_ignores_stale_local_review_after_changes_advance_parent() {
    let instance = issue_instance("addressing_feedback").with_data(json!({
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Late local review requested changes after the parent advanced.",
    )
    .with_signal(ActivitySignal::new(
        "LocalReviewChangesRequested",
        json!({ "pr_number": 77 }),
    ));
    let event = runtime_completion_event(&instance, LOCAL_REVIEW_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "late local review change request should be treated as obsolete"
    );
}

#[test]
fn runtime_completion_reducer_ignores_child_workflow_start_ack_without_parent_decision() {
    let instance = issue_instance("quality_gate_pending");
    let command = WorkflowCommand::start_child_workflow(
        QUALITY_GATE_DEFINITION_ID,
        "pr:77",
        "quality-gate:issue-123:77",
    );
    let result = ActivityResult::succeeded(
        WorkflowCommandType::StartChildWorkflow.as_str(),
        "Quality gate child workflow started.",
    );
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "child workflow start acknowledgement should wait for child completion"
    );
}

#[test]
fn runtime_completion_reducer_ignores_success_for_already_terminal_workflow() {
    let instance = issue_instance("done");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Workflow was already terminal before runtime execution.",
    );
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event).expect("event should parse");

    assert!(
        decision.is_none(),
        "stale terminal workflow completion should not produce a new decision"
    );
}

#[test]
fn quality_gate_run_decision_starts_runtime_activity() {
    let instance = quality_gate_instance("pending");
    let commands = vec!["cargo check".to_string(), "cargo test".to_string()];
    let output = build_quality_gate_run_decision(
        &instance,
        QualityGateDecisionInput {
            reason: "Run validation before merge.",
            validation_commands: &commands,
        },
    );

    assert_eq!(output.action, QualityGateWorkflowAction::RunQualityGate);
    assert_eq!(output.decision.decision, "run_quality_gate");
    assert_eq!(output.decision.next_state, "checking");
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some(QUALITY_GATE_ACTIVITY)
    );
    assert_eq!(
        output.decision.commands[0].command["validation_commands"][1],
        "cargo test"
    );
    DecisionValidator::quality_gate()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("quality gate run decision should validate");
}

#[test]
fn runtime_completion_reducer_passes_quality_gate_after_success() {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ))
        .with_validation(ValidationRecord::new("cargo check", "passed"))
        .with_validation(ValidationRecord::new("cargo test", "passed"));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate completion should pass the workflow");

    assert_eq!(decision.decision, "quality_passed");
    assert_eq!(decision.next_state, "passed");
    assert!(decision.commands.is_empty());
    DecisionValidator::quality_gate()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("quality gate pass transition should validate");
}

#[test]
fn runtime_completion_reducer_marks_issue_pr_ready_after_quality_gate_pass() {
    let instance = issue_instance("quality_gate_pending");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ))
        .with_validation(ValidationRecord::new("cargo check", "passed"));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate completion should mark parent ready");

    assert_eq!(decision.decision, "quality_gate_passed");
    assert_eq!(decision.next_state, "ready_to_merge");
    assert!(decision.commands.is_empty());
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("parent quality gate pass transition should validate");
}

#[test]
fn runtime_completion_reducer_blocks_issue_pr_quality_gate_without_validation() {
    let instance = issue_instance("quality_gate_pending");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation was claimed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing validation evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("validation evidence"));
}

#[test]
fn runtime_completion_reducer_blocks_quality_gate_success_without_status_signal() {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_validation(ValidationRecord::new("cargo check", "passed"));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate missing status signal should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkBlocked
    );
    assert!(decision.reason.contains("without a quality status signal"));
    DecisionValidator::quality_gate()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("quality gate invalid output should validate as blocked");
}

#[test]
fn runtime_completion_reducer_blocks_quality_gate_success_without_validation_evidence() {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate missing validation evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("without validation evidence"));
}

#[test]
fn runtime_completion_reducer_blocks_quality_gate_success_with_conflicting_status_signals() {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation was inconsistent.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ))
        .with_signal(ActivitySignal::new(
            QUALITY_FAILED_SIGNAL,
            json!({
                "validation": "failed"
            }),
        ))
        .with_signal(ActivitySignal::new(
            QUALITY_BLOCKED_SIGNAL,
            json!({
                "blocker": "manual review needed"
            }),
        ))
        .with_validation(ValidationRecord::new("cargo test", "passed"));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate conflicting status signals should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("ambiguous quality status signals"));
}

#[test]
fn runtime_completion_reducer_blocks_quality_gate_structured_pass_without_contract_evidence() {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "checking",
        "quality_passed",
        "passed",
        "The agent claimed the quality gate passed.",
    )
    .with_evidence(WorkflowEvidence::new("quality_gate", "claimed pass"));
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate structured pass without contract evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("without a quality status signal"));
}

#[test]
fn runtime_completion_reducer_blocks_quality_gate_invalid_structured_decision_with_contract_evidence(
) {
    let instance = quality_gate_instance("checking");
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "pending",
        "quality_passed",
        "passed",
        "The agent observed a stale quality gate state.",
    );
    let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({
                "validation": "passed"
            }),
        ))
        .with_validation(ValidationRecord::new("cargo test", "passed"))
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("invalid quality gate workflow_decision should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("did not validate"));
}

#[test]
fn runtime_completion_reducer_retries_quality_gate_transient_failure() {
    let instance = quality_gate_instance("checking").with_data(json!({
        "runtime_retry_policy": {
            "activity_retries": {
                "run_quality_gate": {
                    "max_failed_activity_retries": 2,
                    "retry_delay_secs": 1
                }
            }
        }
    }));
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate:run");
    let result = ActivityResult::failed(
        QUALITY_GATE_ACTIVITY,
        "Validation command could not start.",
        "temporary process error",
    )
    .with_error_kind(ActivityErrorKind::Retryable);
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": command,
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("quality gate transient failure should retry");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.next_state, "checking");
    assert_eq!(
        decision.commands[0].activity_name(),
        Some(QUALITY_GATE_ACTIVITY)
    );
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    DecisionValidator::quality_gate()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("quality gate retry transition should validate");
}
