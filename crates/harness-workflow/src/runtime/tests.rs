use super::model::{
    ActivityArtifact, ActivityResult, ActivitySignal, RuntimeJob, RuntimeJobStatus, RuntimeKind,
    ValidationRecord, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvidence, WorkflowInstance,
    WorkflowSubject,
};
use super::validator::{DecisionValidator, ValidationContext, WorkflowDecisionRejectionKind};
use super::{
    build_plan_issue_decision, build_pr_detected_decision, build_pr_feedback_decision,
    InMemoryWorkflowBus, PlanIssueDecisionInput, PlanIssueWorkflowAction, PrDetectedDecisionInput,
    PrFeedbackDecisionInput, PrFeedbackOutcome, PrFeedbackWorkflowAction, WorkflowRuntimeStore,
};
use chrono::{Duration, Utc};
use harness_core::db::resolve_database_url;
use serde_json::json;

fn issue_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", "123"),
    )
}

#[test]
fn plan_issue_decision_runs_replan_when_policy_allows() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: false,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::RunReplan);
    assert_eq!(output.decision.decision, "run_replan");
    assert_eq!(output.decision.next_state, "replanning");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("replan decision should validate");
}

#[test]
fn plan_issue_decision_force_execute_continues_implementation() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: true,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::ForceContinue);
    assert_eq!(output.decision.decision, "continue_force_execute");
    assert_eq!(output.decision.next_state, "implementing");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("force continue decision should validate");
}

#[test]
fn plan_issue_decision_blocks_repeated_replan() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan still invalid",
            force_execute: false,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: true,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::Block);
    assert_eq!(output.decision.decision, "block_replan_loop");
    assert_eq!(output.decision.next_state, "blocked");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("block decision should validate");
}

#[test]
fn pr_detected_decision_binds_pr_from_implementation() {
    let instance = issue_instance("implementing");
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::BindPr);
    assert_eq!(output.decision.decision, "bind_pr");
    assert_eq!(output.decision.next_state, "pr_open");
    assert_eq!(output.decision.commands.len(), 1);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("PR binding decision should validate");
}

#[test]
fn pr_feedback_decision_addresses_blocking_feedback() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::BlockingFeedback,
            summary: "Reviewer found blocking feedback.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::AddressFeedback);
    assert_eq!(output.decision.decision, "address_pr_feedback");
    assert_eq!(output.decision.next_state, "addressing_feedback");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocking feedback decision should validate");
}

#[test]
fn pr_feedback_decision_waits_when_no_actionable_feedback_exists() {
    let instance = issue_instance("pr_open");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: None,
            outcome: PrFeedbackOutcome::NoActionableFeedback,
            summary: "Review bot has not produced actionable feedback yet.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::AwaitFeedback);
    assert_eq!(output.decision.decision, "wait_for_pr_feedback");
    assert_eq!(output.decision.next_state, "awaiting_feedback");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("wait decision should validate");
}

#[test]
fn pr_feedback_decision_marks_ready_to_merge() {
    let instance = issue_instance("addressing_feedback");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::ReadyToMerge,
            summary: "Reviewer approved and validation passed.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::ReadyToMerge);
    assert_eq!(output.decision.decision, "mark_ready_to_merge");
    assert_eq!(output.decision.next_state, "ready_to_merge");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("ready-to-merge decision should validate");
}

fn leased_issue_instance(state: &str) -> WorkflowInstance {
    issue_instance(state).with_lease("controller-1", Utc::now() + Duration::minutes(5))
}

fn validation_context() -> ValidationContext {
    ValidationContext::new("controller-1", Utc::now())
}

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

#[test]
fn in_memory_bus_passes_workflow_commands_to_runtime_jobs() {
    let mut bus = InMemoryWorkflowBus::default();
    let instance = issue_instance("pr_open");
    bus.insert_instance(instance.clone());

    let event = bus.append_event(
        instance.id.clone(),
        "PrReviewUpdated",
        "github_webhook",
        json!({ "pr": 77 }),
    );
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "pr_open",
        "sweep_feedback",
        "awaiting_feedback",
        "A PR update should trigger feedback sweep.",
    )
    .with_command(WorkflowCommand::start_child_workflow(
        "pr_feedback",
        "pr:77",
        "issue-123-pr-77-feedback",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()));
    bus.record_decision(record);

    let command_id = bus.enqueue_command(decision.commands[0].clone());
    let job = bus.enqueue_runtime_job(
        command_id.clone(),
        RuntimeKind::CodexJsonrpc,
        "codex-feedback",
        json!({ "workflow_id": instance.id, "subject": "pr:77" }),
    );

    assert_eq!(
        bus.command(&command_id).expect("command exists").dedupe_key,
        "issue-123-pr-77-feedback"
    );

    let claimed = bus
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, RuntimeJobStatus::Running);

    let first_event = bus.record_runtime_event(
        claimed.id.clone(),
        "TurnStarted",
        json!({ "runtime": "codex_jsonrpc" }),
    );
    let second_event = bus.record_runtime_event(
        claimed.id.clone(),
        "ActivityResultReady",
        json!({ "activity": "sweep_feedback" }),
    );
    assert_eq!(first_event.sequence, 1);
    assert_eq!(second_event.sequence, 2);

    let result = ActivityResult::succeeded("sweep_feedback", "Found actionable feedback.")
        .with_signal(ActivitySignal::new("FeedbackFound", json!({ "count": 1 })));
    let completed = bus
        .complete_runtime_job(&claimed.id, &result)
        .expect("runtime job should complete");
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(bus.runtime_events_for(&claimed.id).len(), 2);
}

#[tokio::test]
async fn durable_store_persists_workflow_runtime_bus_contract() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let definition = WorkflowDefinition::new("github_issue_pr", 1, "GitHub Issue PR");
    store.upsert_definition(&definition).await?;

    let instance = issue_instance("implementing");
    store.upsert_instance(&instance).await?;
    let loaded = store
        .get_instance(&instance.id)
        .await?
        .expect("workflow instance should load");
    assert_eq!(loaded.id, instance.id);
    assert_eq!(loaded.state, "implementing");

    let first = store
        .append_event(
            &instance.id,
            "PlanIssueRaised",
            "runtime_job",
            json!({ "detail": "missing migration path" }),
        )
        .await?;
    let second = store
        .append_event(
            &instance.id,
            "PolicyDecisionRequested",
            "workflow_controller",
            json!({ "reason": "plan issue needs policy" }),
        )
        .await?;
    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    assert_eq!(store.events_for(&instance.id).await?.len(), 2);

    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "Replan before continuing implementation.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(first.id.clone()));
    store.record_decision(&record).await?;

    let command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    let duplicate_command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    assert_eq!(
        command_id, duplicate_command_id,
        "dedupe key should make command enqueue idempotent"
    );

    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "prompt_packet": { "workflow_id": instance.id } }),
        )
        .await?;
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        1
    );

    let claimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, RuntimeJobStatus::Running);

    let runtime_first = store
        .record_runtime_event(&claimed.id, "TurnStarted", json!({ "runtime": "codex" }))
        .await?;
    let runtime_second = store
        .record_runtime_event(
            &claimed.id,
            "ActivityResultReady",
            json!({ "activity": "replan_issue" }),
        )
        .await?;
    assert_eq!(runtime_first.sequence, 1);
    assert_eq!(runtime_second.sequence, 2);
    assert_eq!(store.runtime_events_for(&claimed.id).await?.len(), 2);

    let result = ActivityResult::succeeded("replan_issue", "Replan completed.")
        .with_signal(ActivitySignal::new("ReplanCompleted", json!({})));
    let completed = store.complete_runtime_job(&claimed.id, &result).await?;
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(
        store
            .get_runtime_job(&claimed.id)
            .await?
            .expect("completed job should load")
            .status,
        RuntimeJobStatus::Succeeded
    );

    Ok(())
}
