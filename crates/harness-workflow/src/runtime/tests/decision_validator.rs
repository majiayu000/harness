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
