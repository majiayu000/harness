#[test]
fn accepts_replan_decision_from_plan_issue() {
    let instance = leased_issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The implementation agent reported a missing migration step.",
    )
    .high_confidence()
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RecordPlanConcern,
        "issue-123-plan-concern-1",
        json!({ "concern": "missing migration step" }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "agent_signal",
        "PLAN_ISSUE was emitted by the implementation activity.",
    ));

    DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect("valid replan decision should be accepted");
}

#[test]
fn accepts_operator_recovery_from_blocked_issue_to_implementation() {
    let instance = issue_instance("blocked");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "operator_runtime_unblock",
        "implementing",
        "operator requested workflow runtime unblock",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "implement_issue",
        "operator-recovery:issue-123",
    ));

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow_runtime_operator_action", Utc::now()),
        )
        .expect("operator recovery should re-dispatch blocked issue workflows");
}

#[test]
fn rejects_ordinary_runtime_actor_for_stopped_state_recovery_transition() {
    let instance = issue_instance("blocked");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "agent_merge",
        "merging",
        "ordinary runtime actor tried to bypass the merge gate",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "merge_pr",
        "issue-123-merge",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("ordinary actors must not use operator recovery transitions");

    assert_eq!(err.kind, WorkflowDecisionRejectionKind::TransitionNotAllowed);
}

#[test]
fn rejects_decision_with_stale_observed_state() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "planning",
        "run_implement",
        "implementing",
        "The policy agent used stale state.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_implement",
        "issue-123-implement-1",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("stale observed state must be rejected");

    assert_eq!(err.kind, WorkflowDecisionRejectionKind::StateMismatch);
}

#[test]
fn rejects_unallowed_command_for_transition() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent requested an invalid command.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::BindPr,
        "issue-123-bind-pr",
        json!({ "pr": 77 }),
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("BindPr should not be allowed on implementing -> replanning");

    assert_eq!(err.kind, WorkflowDecisionRejectionKind::CommandNotAllowed);
}

#[test]
fn rejects_pr_open_transition_without_bind_pr_command() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "agent_reported_pr_open",
        "pr_open",
        "The agent reported a PR without binding PR metadata.",
    )
    .with_command(WorkflowCommand::wait(
        "PR metadata was omitted.",
        "issue-123-pr-open-wait",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("pr_open transition should require BindPr");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::RequiredCommandMissing
    );
}

#[test]
fn rejects_pr_open_transition_with_invalid_bind_pr_payload() {
    let cases = [
        ("missing pr_url", json!({ "pr_number": 77 })),
        (
            "missing pr_number",
            json!({ "pr_url": "https://github.com/owner/repo/pull/77" }),
        ),
        (
            "non-numeric pr_number",
            json!({
                "pr_number": "77",
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ),
        (
            "empty pr_url",
            json!({
                "pr_number": 77,
                "pr_url": "  "
            }),
        ),
    ];

    for (case_name, payload) in cases {
        let instance = issue_instance("implementing");
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            "implementing",
            "agent_reported_pr_open",
            "pr_open",
            "The agent reported a PR with incomplete metadata.",
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::BindPr,
            "issue-123-bind-pr",
            payload,
        ));

        let err = DecisionValidator::github_issue_pr()
            .validate(&instance, &decision, &validation_context())
            .expect_err("BindPr must include a valid pr_number and pr_url payload");

        assert_eq!(
            err.kind,
            WorkflowDecisionRejectionKind::InvalidCommandPayload,
            "failed case: {case_name}"
        );
    }
}

#[test]
fn rejects_scheduled_pr_open_transition_without_bind_pr_command() {
    let instance = issue_instance("scheduled");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "scheduled",
        "agent_reported_pr_open",
        "pr_open",
        "The legacy implementation produced a PR without binding PR metadata.",
    )
    .with_command(WorkflowCommand::wait(
        "PR metadata was omitted.",
        "issue-123-pr-open-wait",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("scheduled -> pr_open should require BindPr");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::RequiredCommandMissing
    );
}

#[test]
fn rejects_done_transition_without_mark_done_command() {
    let instance = issue_instance("ready_to_merge");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "ready_to_merge",
        "agent_reported_done",
        "done",
        "The agent reported merge completion without a terminal command.",
    );

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("done transition should require MarkDone");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::RequiredCommandMissing
    );
}

#[test]
fn rejects_active_duplicate_command() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent requested a duplicate command.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let context = validation_context().with_active_dedupe_key("issue-123-replan-1");

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &context)
        .expect_err("active dedupe keys should reject duplicate commands");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::ActiveDuplicateCommand
    );
}

#[test]
fn rejects_duplicate_dedupe_key_inside_one_decision() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent emitted duplicate commands.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RecordPlanConcern,
        "issue-123-replan-1",
        json!({ "concern": "same dedupe key" }),
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("duplicate dedupe keys should be rejected");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::DuplicateCommandDedupeKey
    );
}

#[test]
fn rejects_terminal_reopen_by_default() {
    let instance = issue_instance("done");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "done",
        "recover",
        "implementing",
        "The policy agent tried to reopen a terminal workflow.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_implement",
        "issue-123-reopen",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &validation_context())
        .expect_err("terminal reopen should require explicit recovery override");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::TerminalReopenDenied
    );
}

#[test]
fn rejects_wrong_lease_owner() {
    let instance = leased_issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent holds a stale controller lease.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let context = ValidationContext::new("controller-2", Utc::now());

    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &context)
        .expect_err("wrong lease owner should be rejected");

    assert_eq!(err.kind, WorkflowDecisionRejectionKind::LeaseOwnerMismatch);
}

#[test]
fn rejects_runtime_command_when_resource_budget_is_unavailable() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent requested runtime work with no capacity.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &validation_context().without_resource_budget(),
        )
        .expect_err("runtime command should be rejected without resource budget");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::ResourceBudgetUnavailable
    );
}

#[test]
fn rejects_replan_after_replan_budget_is_exhausted() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "The policy agent requested another replan.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-2",
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &validation_context().with_replan_exhausted(),
        )
        .expect_err("replan command should be rejected after replan budget is exhausted");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::ReplanLimitExhausted
    );
}

#[test]
fn rejects_driverless_command_driven_transitions() {
    let cases = [
        (
            "github_issue_pr",
            DecisionValidator::github_issue_pr(),
            issue_instance("implementing"),
            "implementing",
            "replanning",
            WorkflowDecisionRejectionKind::ProgressDriverMissing,
        ),
        (
            "prompt_task",
            DecisionValidator::prompt_task(),
            prompt_task_instance("implementing"),
            "implementing",
            "implementing",
            WorkflowDecisionRejectionKind::InvalidDecisionContract,
        ),
        (
            "quality_gate",
            DecisionValidator::quality_gate(),
            quality_gate_instance("pending"),
            "pending",
            "checking",
            WorkflowDecisionRejectionKind::ProgressDriverMissing,
        ),
        (
            "pr_feedback",
            DecisionValidator::pr_feedback(),
            WorkflowInstance::new(
                PR_FEEDBACK_DEFINITION_ID,
                1,
                "pending",
                WorkflowSubject::new("pull_request", "issue:123"),
            ),
            "pending",
            "inspecting",
            WorkflowDecisionRejectionKind::ProgressDriverMissing,
        ),
    ];

    for (case_name, validator, instance, observed_state, next_state, expected_kind) in cases {
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            observed_state,
            "driverless_transition",
            next_state,
            "no runtime work was created",
        );
        let err = validator
            .validate(&instance, &decision, &validation_context())
            .expect_err("command-driven targets must reject driverless decisions");
        assert_eq!(
            err.kind,
            expected_kind,
            "failed case: {case_name}"
        );
    }
}

#[test]
fn scalar_and_array_command_payloads_are_rejected() {
    for (case_name, payload) in [("scalar", json!("wait")), ("array", json!([]))] {
        let instance = issue_instance("discovered");
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            "discovered",
            "await_dependencies",
            "awaiting_dependencies",
            "dependencies remain unresolved",
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::Wait,
            format!("invalid-payload:{case_name}"),
            payload,
        ));
        let err = DecisionValidator::github_issue_pr()
            .validate(&instance, &decision, &validation_context())
            .expect_err("non-object command payloads must be rejected");
        assert_eq!(
            err.kind,
            WorkflowDecisionRejectionKind::InvalidCommandPayload,
            "failed case: {case_name}"
        );
        assert!(err.message.contains("must be a JSON object"));
    }
}

#[test]
fn empty_and_omitted_commands_share_progress_rejection() {
    let instance = issue_instance("implementing");
    let decision_json = json!({
        "workflow_id": instance.id,
        "observed_state": "implementing",
        "decision": "run_replan",
        "next_state": "replanning",
        "reason": "replanning was requested without runtime work",
        "confidence": "medium"
    });
    let omitted: WorkflowDecision =
        serde_json::from_value(decision_json.clone()).expect("deserialize omitted commands");
    let mut empty_json = decision_json;
    empty_json["commands"] = json!([]);
    let empty: WorkflowDecision =
        serde_json::from_value(empty_json).expect("deserialize empty commands");

    for decision in [&omitted, &empty] {
        let err = DecisionValidator::github_issue_pr()
            .validate(&instance, decision, &validation_context())
            .expect_err("omitted and empty commands must both be rejected");
        assert_eq!(
            err.kind,
            WorkflowDecisionRejectionKind::ProgressDriverMissing
        );
    }
}

#[test]
fn wait_and_inline_commands_do_not_drive_progress() {
    let validator = DecisionValidator::new(
        super::validator::TransitionAllowlist::default().allow(
            "implementing",
            "implementing",
            [
                WorkflowCommandType::Wait,
                WorkflowCommandType::RecordPlanConcern,
                WorkflowCommandType::BindPr,
                WorkflowCommandType::RequestOperatorAttention,
            ],
        ),
    );
    let cases = [
        WorkflowCommand::wait("still waiting", "non-driver:wait"),
        WorkflowCommand::record_plan_concern("audit concern", "non-driver:audit"),
        WorkflowCommand::bind_pr(
            77,
            "https://github.com/owner/repo/pull/77",
            "non-driver:bind-pr",
        ),
        WorkflowCommand::new(
            WorkflowCommandType::RequestOperatorAttention,
            "non-driver:operator-attention",
            json!({ "reason": "operator review requested" }),
        ),
    ];

    for command in cases {
        let instance = issue_instance("implementing");
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            "implementing",
            "inline_only",
            "implementing",
            "no runtime job-producing command was included",
        )
        .with_command(command);
        let err = validator
            .validate(&instance, &decision, &validation_context())
            .expect_err("non-runtime commands must not drive command-driven progress");
        assert_eq!(
            err.kind,
            WorkflowDecisionRejectionKind::ProgressDriverMissing
        );
    }
}

#[test]
fn allows_declared_non_command_progress_modes() {
    let external_wait = issue_instance("discovered");
    let external_wait_decision = WorkflowDecision::new(
        external_wait.id.clone(),
        "discovered",
        "await_dependencies",
        "awaiting_dependencies",
        "dependency observer owns the wake-up",
    )
    .with_command(WorkflowCommand::wait(
        "dependencies unresolved",
        "progress-mode:external-wait",
    ));
    DecisionValidator::github_issue_pr()
        .validate(
            &external_wait,
            &external_wait_decision,
            &validation_context(),
        )
        .expect("external waits may remain without a runtime-job-producing command");

    let operator_gate = issue_instance("implementing");
    let operator_gate_decision = WorkflowDecision::new(
        operator_gate.id.clone(),
        "implementing",
        "block",
        "blocked",
        "operator recovery owns resolution",
    )
    .with_command(WorkflowCommand::mark_blocked(
        "operator action required",
        "progress-mode:operator-gate",
    ));
    DecisionValidator::github_issue_pr()
        .validate(
            &operator_gate,
            &operator_gate_decision,
            &validation_context(),
        )
        .expect("operator gates do not require runtime-job-producing commands");

    let parent_handoff = issue_instance("quality_gate_pending");
    let parent_handoff_decision = WorkflowDecision::new(
        parent_handoff.id.clone(),
        "quality_gate_pending",
        "quality_passed",
        "ready_to_merge",
        "the child result was propagated to its parent",
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &parent_handoff,
            &parent_handoff_decision,
            &validation_context(),
        )
        .expect("parent handoffs do not require runtime-job-producing commands");

    let terminal = quality_gate_instance("checking");
    let terminal_decision = WorkflowDecision::new(
        terminal.id.clone(),
        "checking",
        "quality_passed",
        "passed",
        "quality checks passed",
    );
    DecisionValidator::quality_gate()
        .validate(&terminal, &terminal_decision, &validation_context())
        .expect("terminal targets do not require runtime-job-producing commands");
}

#[test]
fn runtime_recovery_requires_progress_driver() {
    let instance = issue_instance("blocked");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "operator_runtime_unblock",
        "implementing",
        "operator recovery omitted runtime work",
    );
    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow_runtime_operator_action", Utc::now()),
        )
        .expect_err("operator recovery cannot bypass the progress contract");
    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::ProgressDriverMissing
    );
}

#[test]
fn stale_active_work_does_not_satisfy_progress() {
    let instance = issue_instance("implementing");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "the decision attempted to rely on older active work",
    );
    let context = validation_context().with_active_dedupe_key("stale:replan-command");
    let err = DecisionValidator::github_issue_pr()
        .validate(&instance, &decision, &context)
        .expect_err("active work outside the decision must not satisfy progress");
    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::ProgressDriverMissing
    );
}

#[test]
fn runtime_job_producing_commands_drive_progress() {
    let enqueue_instance = issue_instance("implementing");
    let enqueue_decision = WorkflowDecision::new(
        enqueue_instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "enqueue a replan activity",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "progress-driver:enqueue",
    ));
    DecisionValidator::github_issue_pr()
        .validate(
            &enqueue_instance,
            &enqueue_decision,
            &validation_context(),
        )
        .expect("EnqueueActivity should drive command-driven progress");

    let child_instance = issue_instance("awaiting_feedback");
    let child_decision = WorkflowDecision::new(
        child_instance.id.clone(),
        "awaiting_feedback",
        "address_feedback",
        "addressing_feedback",
        "start the feedback child workflow",
    )
    .with_command(WorkflowCommand::start_child_workflow(
        PR_FEEDBACK_DEFINITION_ID,
        "issue:123",
        "progress-driver:child",
    ));
    DecisionValidator::github_issue_pr()
        .validate(&child_instance, &child_decision, &validation_context())
        .expect("StartChildWorkflow should drive command-driven progress");
}

#[test]
fn runtime_job_and_activity_result_round_trip_through_json() {
    let result = ActivityResult::succeeded(
        "replan_issue",
        "Produced a revised migration-aware implementation plan.",
    )
    .with_artifact(ActivityArtifact::new(
        "plan_v1",
        json!({ "steps": ["migrate", "validate"] }),
    ))
    .with_signal(ActivitySignal::new(
        "ReplanCompleted",
        json!({ "detail": "migration path covered" }),
    ))
    .with_validation(
        ValidationRecord::new("cargo check -p harness-workflow", "not_run")
            .with_reason("plan-only activity"),
    );

    let result_json = serde_json::to_string(&result).expect("serialize activity result");
    let decoded_result: ActivityResult =
        serde_json::from_str(&result_json).expect("deserialize activity result");
    assert_eq!(decoded_result, result);

    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-high",
        json!({ "prompt_packet": { "workflow": "github_issue_pr" } }),
    );
    let job_json = serde_json::to_string(&job).expect("serialize runtime job");
    let decoded_job: RuntimeJob = serde_json::from_str(&job_json).expect("deserialize runtime job");
    assert_eq!(decoded_job, job);
}
