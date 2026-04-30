use super::model::{
    ActivityArtifact, ActivityResult, ActivitySignal, ActivityStatus, RuntimeJob, RuntimeJobStatus,
    RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand, WorkflowCommandRecord,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord, WorkflowDefinition,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowSubject,
};
use super::validator::{DecisionValidator, ValidationContext, WorkflowDecisionRejectionKind};
use super::{
    build_merged_pr_decision, build_open_issue_without_workflow_decision,
    build_plan_issue_decision, build_pr_detected_decision, build_pr_feedback_decision,
    build_stale_active_workflow_decision, reduce_runtime_job_completed, CommandDispatchOutcome,
    InMemoryWorkflowBus, MergedPrDecisionInput, OpenIssueDecisionInput, PlanIssueDecisionInput,
    PlanIssueWorkflowAction, PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackOutcome,
    PrFeedbackWorkflowAction, RepoBacklogWorkflowAction, RuntimeCommandDispatcher,
    RuntimeJobExecutor, RuntimeWorker, StaleWorkflowDecisionInput, WorkflowRuntimeStore,
    REPO_BACKLOG_DEFINITION_ID,
};
use async_trait::async_trait;
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

fn repo_backlog_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        REPO_BACKLOG_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("repo", "owner/repo"),
    )
}

fn project_issue_instance(project_id: &str, issue_number: u64, state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(format!("{project_id}::issue:{issue_number}"))
    .with_data(json!({
        "project_id": project_id,
        "issue_number": issue_number,
    }))
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

#[test]
fn runtime_completion_reducer_resumes_after_replan() {
    let instance = issue_instance("replanning");
    let result = ActivityResult::succeeded("replan_issue", "Replan completed.");
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("replan completion should produce a decision");

    assert_eq!(decision.decision, "resume_implementation_after_replan");
    assert_eq!(decision.next_state, "implementing");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("runtime completion decision should validate");
}

#[test]
fn runtime_completion_reducer_returns_none_for_unmapped_success() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.");
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    assert!(reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .is_none());
}

#[test]
fn repo_backlog_open_issue_without_workflow_starts_child_workflow() {
    let instance = repo_backlog_instance("idle");
    let output = build_open_issue_without_workflow_decision(
        &instance,
        OpenIssueDecisionInput {
            repo: Some("owner/repo"),
            issue_number: 42,
            issue_url: Some("https://github.com/owner/repo/issues/42"),
        },
    );

    assert_eq!(output.action, RepoBacklogWorkflowAction::StartIssueWorkflow);
    assert_eq!(output.decision.decision, "start_issue_workflow");
    assert_eq!(output.decision.next_state, "dispatching");
    assert_eq!(
        output.decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("repo backlog start decision should validate");
}

#[test]
fn repo_backlog_merged_pr_marks_bound_issue_done() {
    let instance = repo_backlog_instance("idle");
    let output = build_merged_pr_decision(
        &instance,
        MergedPrDecisionInput {
            repo: Some("owner/repo"),
            issue_number: Some(42),
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
        },
    );

    assert_eq!(output.action, RepoBacklogWorkflowAction::MarkBoundIssueDone);
    assert_eq!(output.decision.decision, "mark_bound_issue_done");
    assert_eq!(output.decision.next_state, "reconciling");
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("mark_bound_issue_done")
    );
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("repo backlog merged PR decision should validate");
}

#[test]
fn repo_backlog_merged_pr_can_reconcile_after_dispatch() {
    let instance = repo_backlog_instance("dispatching");
    let output = build_merged_pr_decision(
        &instance,
        MergedPrDecisionInput {
            repo: Some("owner/repo"),
            issue_number: Some(42),
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
        },
    );

    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("repo backlog can reconcile after dispatching work");
}

#[test]
fn repo_backlog_stale_workflow_requests_recovery() {
    let instance = repo_backlog_instance("idle");
    let output = build_stale_active_workflow_decision(
        &instance,
        StaleWorkflowDecisionInput {
            repo: Some("owner/repo"),
            issue_number: 42,
            active_task_id: Some("task-1"),
            observed_state: "implementing",
            reason: "active task disappeared during startup reconciliation",
        },
    );

    assert_eq!(output.action, RepoBacklogWorkflowAction::RequestRecovery);
    assert_eq!(output.decision.decision, "request_workflow_recovery");
    assert_eq!(output.decision.next_state, "reconciling");
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("recover_issue_workflow")
    );
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("repo backlog recovery decision should validate");
}

fn leased_issue_instance(state: &str) -> WorkflowInstance {
    issue_instance(state).with_lease("controller-1", Utc::now() + Duration::minutes(5))
}

fn validation_context() -> ValidationContext {
    ValidationContext::new("controller-1", Utc::now())
}

struct StaticRuntimeExecutor {
    result: ActivityResult,
}

#[async_trait]
impl RuntimeJobExecutor for StaticRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.result.clone()
    }
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
async fn runtime_worker_claims_one_job_once_and_records_events() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = store
        .enqueue_runtime_job(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "check" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
    assert!(worker.run_once(&executor).await?.is_none());

    let events = store.runtime_events_for(&completed.id).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].event_type, "ActivityResultReady");
    assert_eq!(events[1].sequence, 2);
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn runtime_worker_records_completion_event_and_command_status() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("replanning");
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "replan_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("replan_issue", "Replan completed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "completed"
    );
    let workflow_events = store.events_for(&workflow.id).await?;
    let event = workflow_events
        .iter()
        .find(|event| event.event_type == "RuntimeJobCompleted")
        .expect("completion event should be appended");
    assert_eq!(event.source, "runtime-1");
    assert_eq!(event.event["command_id"], command_id);
    assert_eq!(event.event["runtime_job_id"], job.id);
    assert_eq!(event.event["runtime_job_status"], "succeeded");
    assert_eq!(
        event.event["activity_result"]["summary"],
        "Replan completed."
    );
    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "implementing");
    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions
        .iter()
        .any(|record| record.decision.decision == "resume_implementation_after_replan"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_records_failed_activity_result() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    store
        .enqueue_runtime_job(
            "command-2",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            "implement",
            "Codex execution failed.",
            "codex stdin not available",
        ),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete failed job");
    assert_eq!(completed.status, RuntimeJobStatus::Failed);
    assert_eq!(
        completed.error.as_deref(),
        Some("codex stdin not available")
    );
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Failed);
    assert_eq!(output.error.as_deref(), Some("codex stdin not available"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_cancelled_result_releases_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    store
        .enqueue_runtime_job(
            "command-3",
            RuntimeKind::ClaudeCode,
            "claude-default",
            json!({ "activity": "review" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::cancelled("review", "Runtime job was cancelled."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete cancelled job");
    assert_eq!(completed.status, RuntimeJobStatus::Cancelled);
    assert!(completed.lease.is_none());
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Cancelled);
    Ok(())
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

#[tokio::test]
async fn durable_store_lists_workflow_runtime_tree_inputs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let parent = repo_backlog_instance("dispatching").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let child =
        project_issue_instance("/project-a", 123, "replanning").with_parent(parent.id.clone());
    let other_project = project_issue_instance("/project-b", 456, "implementing");
    store.upsert_instance(&parent).await?;
    store.upsert_instance(&child).await?;
    store.upsert_instance(&other_project).await?;
    let event = store
        .append_event(
            &child.id,
            "PlanIssueRaised",
            "workflow-runtime-test",
            json!({ "issue_number": 123 }),
        )
        .await?;
    let rejected_decision = WorkflowDecision::new(
        child.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan requested after the budget was exhausted.",
    );
    let rejected = WorkflowDecisionRecord::rejected(
        rejected_decision,
        Some(event.id),
        "replan limit exhausted",
    );
    store.record_decision(&rejected).await?;

    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-2");
    let command_id = store
        .enqueue_command(&child.id, Some(&rejected.id), &command)
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "workflow_id": child.id }),
        )
        .await?;

    let listed = store.list_instances(Some("/project-a"), 25).await?;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|instance| {
        instance
            .data
            .get("project_id")
            .and_then(|value| value.as_str())
            == Some("/project-a")
    }));

    let decisions = store.decisions_for(&child.id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].accepted);
    assert_eq!(
        decisions[0].rejection_reason.as_deref(),
        Some("replan limit exhausted")
    );

    let commands: Vec<WorkflowCommandRecord> = store.commands_for(&child.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].command.activity_name(), Some("replan_issue"));

    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job.id);
    assert_eq!(jobs[0].status, RuntimeJobStatus::Pending);

    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_enqueues_runtime_job_for_activity_command() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan before continuing.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), None);
    store.record_decision(&record).await?;
    let command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id: dispatched_command_id,
            runtime_job,
        } => {
            assert_eq!(dispatched_command_id, command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };
    assert_eq!(runtime_job.command_id, command_id);
    assert_eq!(runtime_job.runtime_kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(runtime_job.runtime_profile, "codex-high");
    assert_eq!(runtime_job.status, RuntimeJobStatus::Pending);
    assert_eq!(
        runtime_job
            .input
            .get("command")
            .and_then(|command| command.get("activity"))
            .and_then(|activity| activity.as_str()),
        Some("replan_issue")
    );
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        "dispatched"
    );
    assert!(dispatcher.dispatch_once().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_skips_non_runtime_commands() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "pr_open");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::bind_pr(
        77,
        "https://github.com/owner/repo/pull/77",
        "issue-123-pr-77",
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should be inspected");
    match outcome {
        CommandDispatchOutcome::Skipped {
            command_id: skipped_command_id,
            reason,
        } => {
            assert_eq!(skipped_command_id, command_id);
            assert_eq!(reason, "command does not require runtime execution");
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(store.commands_for(&instance.id).await?[0].status, "skipped");
    Ok(())
}
