
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
