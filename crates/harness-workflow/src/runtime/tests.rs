use super::model::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, ActivityStatus,
    RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
    WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowSubject,
};
use super::store::RuntimeJobEnqueueOutcome;
use super::validator::{DecisionValidator, ValidationContext, WorkflowDecisionRejectionKind};
use super::{
    build_issue_submission_decision, build_plan_issue_decision, build_pr_detected_decision,
    build_pr_feedback_decision, build_pr_feedback_sweep_decision, build_prompt_submission_decision,
    build_quality_gate_run_decision, reduce_runtime_job_completed, CommandDispatchOutcome,
    InMemoryWorkflowBus, IssueSubmissionDecisionInput, IssueSubmissionWorkflowAction,
    PlanIssueDecisionInput, PlanIssueWorkflowAction, PrDetectedDecisionInput,
    PrFeedbackDecisionInput, PrFeedbackOutcome, PrFeedbackSweepDecisionInput,
    PrFeedbackWorkflowAction, PromptSubmissionDecisionInput, QualityGateDecisionInput,
    QualityGateWorkflowAction, RuntimeCommandDispatcher, RuntimeJobExecutor,
    RuntimeProfileSelector, RuntimeWorker, WorkflowCommandStatus, WorkflowDecisionTransition,
    WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID, LOCAL_REVIEW_ACTIVITY,
    PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL,
    QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
    SCOPE_TOO_LARGE_SIGNAL,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

mod issue_planning;
mod local_review;
mod p1_followups;
mod pr_repair_evidence;
mod replay_determinism;
mod retry;
mod runtime_store;

fn issue_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", "123"),
    )
}

fn quality_gate_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("quality_gate", "issue:123"),
    )
}

fn prompt_task_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("prompt", "task-123"),
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

async fn enqueue_test_runtime_job(
    store: &WorkflowRuntimeStore,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
) -> anyhow::Result<RuntimeJob> {
    enqueue_test_runtime_job_with_not_before(
        store,
        command_key,
        runtime_kind,
        runtime_profile,
        input,
        None,
    )
    .await
}

async fn enqueue_test_runtime_job_with_not_before(
    store: &WorkflowRuntimeStore,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
    not_before: Option<DateTime<Utc>>,
) -> anyhow::Result<RuntimeJob> {
    let workflow = issue_instance("implementing").with_id(format!("test-workflow-{command_key}"));
    store.upsert_instance(&workflow).await?;
    enqueue_workflow_runtime_job(
        store,
        &workflow.id,
        command_key,
        runtime_kind,
        runtime_profile,
        input,
        not_before,
    )
    .await
}

async fn enqueue_workflow_runtime_job(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
    not_before: Option<DateTime<Utc>>,
) -> anyhow::Result<RuntimeJob> {
    let activity = input
        .get("activity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("test_activity")
        .to_string();
    let command =
        WorkflowCommand::enqueue_activity(activity, format!("test-command-{command_key}"));
    let command_id = store.enqueue_command(workflow_id, None, &command).await?;
    store
        .enqueue_runtime_job_with_not_before(
            &command_id,
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
}

fn runtime_completion_event(
    instance: &WorkflowInstance,
    activity: &str,
    activity_result: ActivityResult,
) -> WorkflowEvent {
    WorkflowEvent::new(
        &instance.id,
        1,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": WorkflowCommand::enqueue_activity(activity, "activity-1"),
        "runtime_job_id": "job-1",
        "activity_result": activity_result,
    }))
}

#[test]
fn issue_submission_decision_starts_discovered_issue_planning() {
    let labels = vec!["bug".to_string(), "p1".to_string()];
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-1",
            repo: Some("owner/repo"),
            issue_number: 123,
            labels: &labels,
            force_execute: false,
            additional_prompt: Some("prefer a minimal patch"),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
        },
    );

    assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
    assert_eq!(output.decision.decision, "submit_issue");
    assert_eq!(output.decision.next_state, "planning");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("plan_issue")
    );
    assert_eq!(
        output.decision.commands[0].dedupe_key,
        "issue-submit:owner/repo:issue:123:task:task-1:plan"
    );
    assert_eq!(
        output.decision.commands[0].command["additional_prompt"],
        "prefer a minimal patch"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("issue submission decision should validate");
}

#[test]
fn issue_submission_decision_can_reopen_failed_issue_when_requested() {
    let labels = Vec::new();
    let instance = issue_instance("failed");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-2",
            repo: None,
            issue_number: 124,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
        },
    );

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()).allow_terminal_reopen(),
        )
        .expect("explicit issue submission should reopen failed workflows");
}

#[test]
fn issue_submission_decision_can_reopen_terminal_issue_for_planning() {
    let labels = Vec::new();
    for state in ["failed", "cancelled"] {
        let instance = issue_instance(state);
        let output = build_issue_submission_decision(
            &instance,
            IssueSubmissionDecisionInput {
                task_id: "task-terminal",
                repo: Some("owner/repo"),
                issue_number: 124,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                remote_fact_hash: None,
            },
        );

        assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
        assert_eq!(output.decision.next_state, "planning");
        assert_eq!(
            output.decision.commands[0].activity_name(),
            Some("plan_issue")
        );
        let validation = DecisionValidator::github_issue_pr().validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()).allow_terminal_reopen(),
        );
        assert!(
            validation.is_ok(),
            "terminal issue in {state} should reopen for planning: {validation:?}"
        );
    }
}

#[test]
fn issue_submission_decision_waits_for_dependencies_without_runtime_command() {
    let labels = Vec::new();
    let depends_on = vec!["task-upstream".to_string()];
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-3",
            repo: Some("owner/repo"),
            issue_number: 125,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &depends_on,
            dependencies_blocked: true,
            remote_fact_hash: None,
        },
    );

    assert_eq!(output.decision.next_state, "awaiting_dependencies");
    assert!(output.decision.commands.is_empty());
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked issue submission should validate without dispatching");
}

#[test]
fn issue_submission_decision_releases_dependencies_to_planning() {
    let labels = Vec::new();
    let instance = issue_instance("awaiting_dependencies");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-4",
            repo: Some("owner/repo"),
            issue_number: 126,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
        },
    );

    assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
    assert_eq!(output.decision.next_state, "planning");
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("plan_issue")
    );
    let validation = DecisionValidator::github_issue_pr().validate(
        &instance,
        &output.decision,
        &ValidationContext::new("workflow-policy", Utc::now()),
    );
    assert!(
        validation.is_ok(),
        "dependency release should allow planning: {validation:?}"
    );
}

#[test]
fn prompt_submission_decision_starts_runtime_implementation() {
    let instance = prompt_task_instance("submitted");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-1",
            prompt: "Fix the flaky login test.",
            prompt_ref: "prompt-ref-1",
            source: None,
            external_id: Some("manual-prompt-1"),
            depends_on: &[],
            dependencies_blocked: false,
        },
    );

    assert_eq!(output.decision.decision, "submit_prompt");
    assert_eq!(output.decision.next_state, "implementing");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some(PROMPT_TASK_IMPLEMENT_ACTIVITY)
    );
    assert_eq!(
        output.decision.commands[0].command["prompt_ref"],
        "prompt-ref-1"
    );
    assert!(output.decision.commands[0].command.get("prompt").is_none());
    assert_eq!(
        output.decision.commands[0].command["prompt_chars"],
        "Fix the flaky login test.".chars().count()
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("prompt submission decision should validate");
}

#[test]
fn prompt_submission_decision_waits_for_dependencies_without_runtime_command() {
    let depends_on = vec!["task-upstream".to_string()];
    let instance = prompt_task_instance("submitted");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-2",
            prompt: "Refactor the settings panel.",
            prompt_ref: "prompt-ref-2",
            source: None,
            external_id: None,
            depends_on: &depends_on,
            dependencies_blocked: true,
        },
    );

    assert_eq!(output.decision.next_state, "awaiting_dependencies");
    assert!(output.decision.commands.is_empty());
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked prompt submission should validate without dispatching");
}

#[test]
fn prompt_submission_decision_reopens_blocked_prompt_task() {
    let instance = prompt_task_instance("blocked");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-retry",
            prompt: "Retry the prompt task after payload loss.",
            prompt_ref: "prompt-ref-retry",
            source: None,
            external_id: Some("manual-prompt-retry"),
            depends_on: &[],
            dependencies_blocked: false,
        },
    );

    assert_eq!(output.decision.next_state, "implementing");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some(PROMPT_TASK_IMPLEMENT_ACTIVITY)
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked prompt task resubmission should validate");
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
fn plan_issue_decision_replans_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
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
    assert_eq!(output.decision.next_state, "replanning");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should be allowed to replan");
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
fn plan_issue_force_continue_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
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
    assert_eq!(output.decision.next_state, "implementing");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should allow force continue");
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
fn decision_validator_lists_allowed_transitions_from_state() {
    let validator = DecisionValidator::github_issue_pr();
    let rules = validator
        .transition_rules_from("ready_to_merge")
        .collect::<Vec<_>>();

    assert!(rules.iter().any(|rule| rule.to_state == "done"
        && rule
            .allowed_commands
            .contains(&WorkflowCommandType::MarkDone)));
    assert!(rules.iter().any(|rule| rule.to_state == "blocked"
        && rule
            .allowed_commands
            .contains(&WorkflowCommandType::MarkBlocked)));
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
fn pr_detected_decision_binds_pr_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::BindPr);
    assert_eq!(output.decision.next_state, "pr_open");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should bind a produced PR");
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
fn pr_feedback_sweep_decision_starts_child_workflow() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_sweep_decision(
        &instance,
        PrFeedbackSweepDecisionInput {
            dedupe_key: "pr-feedback-sweep:123:77",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            issue_number: Some(123),
            repo: Some("owner/repo"),
            summary: "Runtime workflow requested a PR feedback sweep.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::SweepFeedback);
    assert_eq!(output.decision.decision, "sweep_pr_feedback");
    assert_eq!(output.decision.next_state, "awaiting_feedback");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        output.decision.commands[0].command["definition_id"],
        PR_FEEDBACK_DEFINITION_ID
    );
    assert_eq!(
        output.decision.commands[0].command["child_activity"],
        PR_FEEDBACK_INSPECT_ACTIVITY
    );
    assert_eq!(output.decision.commands[0].command["pr_number"], 77);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("PR feedback sweep decision should validate");
}

#[test]
fn pr_feedback_decision_waits_when_no_actionable_feedback_exists() {
    let instance = issue_instance("awaiting_feedback");
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
fn pr_feedback_decision_starts_quality_gate_before_ready_to_merge() {
    let instance = issue_instance("awaiting_feedback");
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

    assert_eq!(output.action, PrFeedbackWorkflowAction::RequestQualityGate);
    assert_eq!(output.decision.decision, "start_quality_gate");
    assert_eq!(output.decision.next_state, "quality_gate_pending");
    assert_eq!(
        output.decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        output.decision.commands[0].command["definition_id"],
        QUALITY_GATE_DEFINITION_ID
    );
    assert_eq!(output.decision.commands[0].command["subject_key"], "pr:77");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("quality gate request decision should validate");
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
fn runtime_completion_reducer_blocks_issue_implementation_success_without_pr() {
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

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("implementation success without PR evidence should block");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
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
        .expect("blocked implementation decision should validate");
}

#[test]
fn runtime_completion_reducer_blocks_scope_too_large_signal() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Scope guard rejected the implementation before PR creation.",
    )
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 31,
            "lines_added": 200,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "Split workflow contract changes",
                    "summary": "Handle prompt/schema changes separately from reducer behavior.",
                    "target_files": ["crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs"]
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("scope guard signal should block for decomposition");

    assert_eq!(decision.decision, "block_scope_too_large");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    let operator_command = decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention)
        .expect("operator attention should carry decomposition evidence");
    assert_eq!(operator_command.command["scope_guard"]["files_changed"], 31);
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "scope_too_large"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("scope-too-large block decision should validate");
}

#[test]
fn runtime_completion_reducer_blocks_scope_too_large_signal_from_blocked_result() {
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Scope guard rejected the implementation before PR creation.".to_string(),
        artifacts: Vec::new(),
        signals: Vec::new(),
        validation: Vec::new(),
        error: None,
        error_kind: None,
    }
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 42,
            "lines_added": 1600,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "Split reducer behavior",
                    "summary": "Keep workflow reducer changes separate from worker prompt updates.",
                    "target_files": ["crates/harness-workflow/src/runtime/reducer.rs"]
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocked scope guard signal should block with decomposition evidence");

    assert_eq!(decision.decision, "block_scope_too_large");
    assert_eq!(decision.next_state, "blocked");
    let operator_command = decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention)
        .expect("operator attention should carry decomposition evidence");
    assert_eq!(operator_command.command["scope_guard"]["files_changed"], 42);
    assert_eq!(
        operator_command.command["scope_guard"]["decomposition_skeleton"][0]["title"],
        "Split reducer behavior"
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "scope_too_large"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked scope-too-large decision should validate");
}

#[test]
fn runtime_completion_reducer_rejects_scope_too_large_signal_without_exceeded_threshold() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Scope guard payload did not exceed configured thresholds.",
    )
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 30,
            "lines_added": 1500,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "No split needed",
                    "summary": "The diff is within configured guard thresholds."
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("non-exceeded scope guard signal should not satisfy implement_issue");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn runtime_completion_reducer_finishes_prompt_task_after_implementation() {
    let instance = prompt_task_instance("implementing");
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Prompt implementation completed.",
    )
    .with_validation(ValidationRecord::new("cargo test", "passed"));
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
        .expect("prompt implementation completion should produce a decision");

    assert_eq!(decision.decision, "finish_prompt_task");
    assert_eq!(decision.next_state, "done");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("prompt completion decision should validate");
}

#[test]
fn runtime_completion_reducer_binds_pr_from_structured_pull_request_artifact() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_url": "missing number"
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ));
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
        .expect("structured pull request artifact should bind the PR");

    assert_eq!(decision.decision, "bind_pr");
    assert_eq!(decision.next_state, "pr_open");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::BindPr
    );
    assert_eq!(decision.commands[0].command["pr_number"], 77);
    assert_eq!(
        decision.commands[0].command["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("structured PR binding should validate");
}

#[test]
fn runtime_completion_reducer_finishes_merge_pr_with_merged_pull_request_artifact() {
    let instance = issue_instance("merging");
    let result = ActivityResult::succeeded("merge_pr", "PR was merged.").with_artifact(
        ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
                "state": "merged",
                "merged": true,
                "merge_commit_sha": "abc123",
                "head_sha": "head123"
            }),
        ),
    );
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
        .expect("merged PR evidence should finish the workflow");

    assert_eq!(decision.decision, "record_pr_merged");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(decision.commands[0].command["pr_number"], 77);
    assert_eq!(decision.commands[0].command["merge_commit_sha"], "abc123");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("merged PR decision should validate");
}

#[test]
fn runtime_completion_reducer_finishes_closed_issue_signal_without_pr() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Issue was already closed before implementation created a PR.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ));
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
        .expect("closed issue signal should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(
        decision.commands[0].command["closed_issue_evidence"]["state"],
        "closed"
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "closed_issue"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_closed_issue_during_quality_gate() {
    let instance = issue_instance("quality_gate_pending");
    let result = ActivityResult::succeeded(
        QUALITY_GATE_ACTIVITY,
        "Issue was closed before quality gate completed.",
    )
    .with_artifact(ActivityArtifact::new(
        "issue_state",
        json!({
            "issue_number": 123,
            "state": "closed"
        }),
    ));
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
        .expect("closed issue artifact should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue quality gate completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_blocked_closed_issue_signal_without_pr() {
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Issue was already resolved upstream before implementation.".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "IssueAlreadyResolved",
            json!({
                "issue_number": 123,
                "state": "resolved",
                "issue_url": "https://github.com/owner/repo/issues/123"
            }),
        )],
        validation: Vec::new(),
        error: Some("No implementation PR is needed for an already resolved issue.".to_string()),
        error_kind: None,
    };
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
        .expect("blocked closed issue signal should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "closed_issue"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_feedback_closed_issue_signal_without_pr() {
    let instance = issue_instance("addressing_feedback");
    let result = ActivityResult {
        activity: "address_pr_feedback".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Issue was closed while addressing PR feedback.".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "IssueClosed",
            json!({
                "issue_number": 123,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/123"
            }),
        )],
        validation: Vec::new(),
        error: Some("No further feedback work is needed because the issue is closed.".to_string()),
        error_kind: None,
    };
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
        .expect("closed issue signal from feedback work should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(
        decision.commands[0].command["activity"],
        "address_pr_feedback"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("feedback closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_succeeded_feedback_closed_issue_signal_without_pr() {
    let instance = issue_instance("addressing_feedback");
    let result = ActivityResult::succeeded(
        "address_pr_feedback",
        "Issue was closed while addressing PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ));
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
        .expect("closed issue signal from successful feedback work should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("successful feedback closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_uses_issue_state_artifact_as_closed_issue_evidence() {
    let instance = issue_instance("implementing");
    let result =
        ActivityResult::succeeded("implement_issue", "Issue state confirms no PR is needed.")
            .with_artifact(ActivityArtifact::new(
                "issue_state",
                json!({
                    "issue_number": 123,
                    "state": "closed"
                }),
            ));
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
        .expect("issue_state artifact should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue artifact completion should validate");
}

#[test]
fn runtime_completion_reducer_rejects_closed_issue_signal_without_closed_state() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Issue signal omitted explicit closed evidence.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "open"
        }),
    ));
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
        .expect("malformed closed issue signal should block");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn runtime_completion_reducer_blocks_structured_done_without_closed_issue_evidence() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "finish_closed_issue",
        "done",
        "The agent claimed the issue was closed without structured evidence.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "agent-claimed-done",
        json!({ "reason": "missing structured issue state" }),
    ));
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ));
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
        .expect("missing terminal evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
}

#[test]
fn runtime_completion_reducer_binds_pr_when_structured_workflow_decision_is_invalid() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "planning",
        "run_replan",
        "replanning",
        "This decision observed a stale workflow state.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "stale-replan-1",
    ));
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ))
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ));
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
        .expect("structured pull request artifact should bind the PR");

    assert_eq!(decision.decision, "bind_pr");
    assert_eq!(decision.next_state, "pr_open");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::BindPr
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("fallback PR binding should validate");
}

#[test]
fn runtime_completion_reducer_accepts_structured_workflow_decision_artifact() {
    let instance = issue_instance("awaiting_feedback");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "PR feedback check completed without actionable feedback.",
    )
    .with_command(WorkflowCommand::wait(
        "Waiting for fresh PR feedback.",
        "wait-feedback-1",
    ))
    .with_evidence(WorkflowEvidence::new(
        "pr_feedback",
        "No actionable feedback found.",
    ))
    .high_confidence();
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
        .expect("structured workflow decision artifact should reduce");

    assert_eq!(decision.decision, "wait_for_pr_feedback");
    assert_eq!(decision.next_state, "awaiting_feedback");
    assert_eq!(decision.commands.len(), 1);
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "runtime_completion"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("structured workflow decision should validate");
}

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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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

struct CountingRuntimeExecutor {
    result: ActivityResult,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeJobExecutor for CountingRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

struct BlockingRuntimeExecutor {
    result: ActivityResult,
    calls: Arc<AtomicUsize>,
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    finish: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl RuntimeJobExecutor for BlockingRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        let finish = self
            .finish
            .lock()
            .unwrap()
            .take()
            .expect("blocking executor should only run once");
        let _ = finish.await;
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
    let job = enqueue_test_runtime_job(
        &store,
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
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].event_type, "RuntimeTurnStarted");
    assert_eq!(events[1].sequence, 2);
    assert_eq!(events[2].event_type, "ActivityResultReady");
    assert_eq!(events[2].sequence, 3);
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_get_instance_by_pr_filters_by_project_repo_and_pr() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let matching = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:77"),
    )
    .with_id("project-a::owner/repo::issue:77")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/repo",
        "issue_number": 77,
        "pr_number": 880,
    }));
    let wrong_repo = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:78"),
    )
    .with_id("project-a::owner/other::issue:78")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/other",
        "issue_number": 78,
        "pr_number": 880,
    }));
    let wrong_project = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:79"),
    )
    .with_id("project-b::owner/repo::issue:79")
    .with_data(json!({
        "project_id": "project-b",
        "repo": "owner/repo",
        "issue_number": 79,
        "pr_number": 880,
    }));
    store.upsert_instance(&matching).await?;
    store.upsert_instance(&wrong_repo).await?;
    store.upsert_instance(&wrong_project).await?;

    let found = store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 880)
        .await?
        .expect("matching runtime issue workflow should be found");
    assert_eq!(found.id, matching.id);
    assert!(store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 881)
        .await?
        .is_none());
    assert!(store
        .get_instance_by_pr("github_issue_pr", "project-b", Some("owner/other"), 880)
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_worker_skips_runtime_jobs_before_not_before() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let not_before = Utc::now() + Duration::minutes(5);
    let job = enqueue_test_runtime_job_with_not_before(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
        Some(not_before),
    )
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
        calls: calls.clone(),
    };
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));

    assert!(worker.run_once(&executor).await?.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should still exist");
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert_eq!(persisted.not_before, Some(not_before));
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_reclaims_expired_running_job() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;

    let first_claim = store
        .claim_next_runtime_job("runtime-1", Utc::now() - Duration::minutes(1))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(first_claim.id, job.id);
    assert_eq!(first_claim.status, RuntimeJobStatus::Running);
    assert_eq!(
        first_claim
            .lease
            .as_ref()
            .expect("lease should exist")
            .owner,
        "runtime-1"
    );
    let first_lease_expires_at = first_claim
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;

    let reclaimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("expired running job should be reclaimable");
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.status, RuntimeJobStatus::Running);
    assert_eq!(
        reclaimed.lease.as_ref().expect("lease should exist").owner,
        "runtime-1"
    );
    let reclaimed_lease_expires_at = reclaimed
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;
    assert_ne!(first_lease_expires_at, reclaimed_lease_expires_at);

    let stale_result = ActivityResult::succeeded("check", "Stale worker completed.");
    assert!(
        store
            .complete_runtime_job_if_owned(
                &first_claim.id,
                "runtime-1",
                first_lease_expires_at,
                &stale_result
            )
            .await?
            .is_none(),
        "stale lease completion should be ignored even when owner name is reused"
    );
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should still exist");
    assert_eq!(persisted.status, RuntimeJobStatus::Running);
    assert_eq!(
        persisted.lease.as_ref().expect("lease should exist").owner,
        "runtime-1"
    );
    assert!(persisted.output.is_none());

    let current_result = ActivityResult::succeeded("check", "Current worker completed.");
    let completed = store
        .complete_runtime_job_if_owned(
            &reclaimed.id,
            "runtime-1",
            reclaimed_lease_expires_at,
            &current_result,
        )
        .await?
        .expect("current owner completion should be accepted");
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_store_prioritizes_ready_work_over_other_activity_jobs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let background_poll = enqueue_test_runtime_job(
        &store,
        "command-background-poll",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "github_issue_poll" }),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let dependency_analysis = enqueue_test_runtime_job(
        &store,
        "command-dependency-analysis",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "analyze_dependencies" }),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let implementation = enqueue_test_runtime_job(
        &store,
        "command-implement",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    )
    .await?;

    let first = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| anyhow::anyhow!("ready implementation job should be claimed first"))?;
    assert_eq!(first.id, implementation.id);

    let second = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("older non-priority job should be claimed after implementation")
        })?;
    assert_eq!(second.id, background_poll.id);

    let third = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| anyhow::anyhow!("remaining non-priority job should still run"))?;
    assert_eq!(third.id, dependency_analysis.id);
    Ok(())
}

#[tokio::test]
async fn runtime_store_does_not_reclaim_unexpired_running_job() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;

    let first_claim = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(first_claim.id, job.id);

    assert!(store
        .claim_next_runtime_job("runtime-2", Utc::now() + Duration::minutes(10))
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_worker_renews_running_job_lease_until_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);
    let job = enqueue_test_runtime_job(
        store.as_ref(),
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let blocking_calls = Arc::new(AtomicUsize::new(0));
    let blocking_executor = BlockingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
        calls: blocking_calls.clone(),
        started: Mutex::new(Some(started_tx)),
        finish: Mutex::new(Some(finish_rx)),
    };
    let worker_store = store.clone();
    let worker_handle = tokio::spawn(async move {
        let worker = RuntimeWorker::new(worker_store.as_ref(), "runtime-1")
            .with_lease_ttl(Duration::seconds(2));
        worker.run_once(&blocking_executor).await
    });

    started_rx.await?;
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Second worker should not run."),
        calls: second_calls.clone(),
    };
    let second_worker =
        RuntimeWorker::new(store.as_ref(), "runtime-2").with_lease_ttl(Duration::seconds(2));
    let second_claim = second_worker.run_once(&second_executor).await?;
    let _ = finish_tx.send(());
    let completed = worker_handle
        .await??
        .expect("first worker should complete the runtime job");

    assert!(second_claim.is_none());
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    assert_eq!(blocking_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
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
        WorkflowCommandStatus::Completed
    );
    let workflow_events = store.events_for(&workflow.id).await?;
    let event = workflow_events
        .iter()
        .find(|event| event.event_type == "RuntimeJobCompleted")
        .expect("completion event should be appended");
    assert_eq!(event.source, "runtime-1");
    assert_eq!(event.event["command_id"], command_id);
    assert_eq!(
        event.event["command"]["command"]["activity"],
        "replan_issue"
    );
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
async fn runtime_worker_persists_bind_pr_payload_for_pr_open_transition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                }),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "pr_open");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 123);
    assert_eq!(reloaded.data["pr_number"], 77);
    assert_eq!(
        reloaded.data["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert_eq!(
        commands[1].command.command_type,
        WorkflowCommandType::BindPr
    );
    assert_ne!(commands[1].status, WorkflowCommandStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_invalid_inline_bind_pr_before_persisting_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let malformed_decision = WorkflowDecision::new(
        &workflow.id,
        "implementing",
        "agent_reported_pr_open",
        "pr_open",
        "The agent reported a PR with incomplete metadata.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::BindPr,
        "issue-123-bind-pr",
        json!({ "pr_number": 77 }),
    ));
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "workflow_decision",
                serde_json::to_value(&malformed_decision).expect("decision should serialize"),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert!(reloaded.data.get("pr_number").is_none());
    assert!(reloaded.data.get("pr_url").is_none());

    let commands = store.commands_for(&workflow.id).await?;
    assert!(commands.iter().all(|record| {
        record.command.command_type != WorkflowCommandType::BindPr
            && record.command.dedupe_key != "issue-123-bind-pr"
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_invalid_agent_output"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_implementation_success_without_pr_evidence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 124, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 124,
        "task_id": "task-124",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-124-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 124);

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::RequestOperatorAttention
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_missing_implementation_result"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_finishes_closed_issue_success_without_pr() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 125, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 125,
        "task_id": "task-125",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-125-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            "implement_issue",
            "Issue was already resolved upstream.",
        )
        .with_signal(ActivitySignal::new(
            "IssueAlreadyResolved",
            json!({
                "issue_number": 125,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/125",
            }),
        )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "done");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 125);
    assert_eq!(
        reloaded.data["closed_issue_evidence"]["issue_url"],
        "https://github.com/owner/repo/issues/125"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkDone
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions
        .iter()
        .any(|record| record.accepted && record.decision.decision == "finish_closed_issue"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_pr_feedback_child_completion_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let malformed_parent_decision = WorkflowDecision::new(
        &parent.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "Child inspection emitted a stale parent decision.",
    )
    .high_confidence();
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback child found actionable feedback.",
        )
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&malformed_parent_decision)?,
        ))
        .with_signal(ActivitySignal::new(
            "FeedbackFound",
            json!({ "pr_number": 77, "count": 1 }),
        )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "feedback_found");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    let parent_events = store.events_for(&parent.id).await?;
    let propagated_event = parent_events
        .iter()
        .find(|event| {
            event.event_type == "RuntimeJobCompleted"
                && event.event["child_workflow_id"] == "pr-feedback-child"
        })
        .expect("child completion should propagate to parent");
    assert!(propagated_event.event["activity_result"]["artifacts"]
        .as_array()
        .expect("activity artifacts should be an array")
        .iter()
        .all(|artifact| artifact["artifact_type"] != "workflow_decision"));
    Ok(())
}

#[tokio::test]
async fn runtime_store_commits_parent_completion_event_decision_and_command() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-transaction")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "pr_number": 77, "count": 1 }),
    ));

    let record = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-1",
                "runtime_job_id": "child-job-1",
                "child_workflow_id": "pr-feedback-child-transaction",
                "activity_result": result,
            }),
        )
        .await?
        .expect("parent completion should produce a decision");

    assert!(record.accepted);
    assert_eq!(record.decision.decision, "address_pr_feedback");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(parent_events.iter().any(|event| {
        event.event_type == "RuntimeJobCompleted"
            && event.event["child_workflow_id"] == "pr-feedback-child-transaction"
    }));
    let parent_decisions = store.decisions_for(&parent.id).await?;
    assert_eq!(parent_decisions.len(), 1);
    assert_eq!(parent_decisions[0].id, record.id);
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    assert_eq!(
        parent_commands[0].decision_id.as_deref(),
        Some(record.id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_rolls_back_parent_completion_when_reducer_fails() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-rollback")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;

    let error = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-rollback",
                "runtime_job_id": "child-job-rollback",
                "child_workflow_id": "pr-feedback-child-rollback",
                "activity_result": "not an activity result",
            }),
        )
        .await
        .expect_err("malformed activity_result should fail the parent completion transaction");

    assert!(
        error.to_string().contains("invalid type"),
        "unexpected error: {error:#}"
    );
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    assert!(store.events_for(&parent.id).await?.is_empty());
    assert!(store.decisions_for(&parent.id).await?.is_empty());
    assert!(store.commands_for(&parent.id).await?.is_empty());
    Ok(())
}

async fn seed_quality_gate_child_job(
    store: &WorkflowRuntimeStore,
    parent_id: &str,
    child_id: &str,
) -> anyhow::Result<RuntimeJob> {
    let parent = issue_instance("quality_gate_pending").with_id(parent_id);
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", "pr:77"),
    )
    .with_id(child_id)
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": QUALITY_GATE_ACTIVITY }),
        )
        .await
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_pass_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = seed_quality_gate_child_job(&store, "issue-parent", "quality-gate-child").await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
            .with_signal(ActivitySignal::new(
                QUALITY_PASSED_SIGNAL,
                json!({ "validation": "passed" }),
            ))
            .with_validation(ValidationRecord::new("cargo check", "passed")),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "passed");
    let parent_after = store
        .get_instance("issue-parent")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "ready_to_merge");
    let parent_events = store.events_for("issue-parent").await?;
    assert!(parent_events.iter().any(|event| {
        event.event_type == "RuntimeJobCompleted"
            && event.event["child_workflow_id"] == "quality-gate-child"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_block_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job =
        seed_quality_gate_child_job(&store, "issue-parent-blocked", "quality-gate-child-blocked")
            .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation was claimed.")
            .with_signal(ActivitySignal::new(
                QUALITY_PASSED_SIGNAL,
                json!({ "validation": "passed" }),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child-blocked")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "blocked");
    let parent_after = store
        .get_instance("issue-parent-blocked")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "blocked");
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_failure_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job =
        seed_quality_gate_child_job(&store, "issue-parent-failed", "quality-gate-child-failed")
            .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            QUALITY_GATE_ACTIVITY,
            "Quality gate execution failed.",
            "validation command failed",
        )
        .with_error_kind(ActivityErrorKind::Fatal),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child-failed")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "failed");
    let parent_after = store
        .get_instance("issue-parent-failed")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "failed");
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_still_inspecting_pr_feedback_child() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-still-inspecting")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-still-inspecting")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection produced no accepted outcome signal.",
        ),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "inspecting");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "still-inspecting child success must not propagate to parent"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_retrying_pr_feedback_child() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-retry")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-retry")
    .with_parent(parent.id.clone())
    .with_data(json!({
        "runtime_retry_policy": {
            "activity_retries": {
                "inspect_pr_feedback": {
                    "max_failed_activity_retries": 2,
                    "retry_delay_secs": 1
                }
            }
        }
    }));
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection hit a transient API error.",
            "temporary GitHub API error",
        )
        .with_error_kind(ActivityErrorKind::Retryable),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "inspecting");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "retrying child failure must not propagate to parent"
    );
    let child_commands = store.commands_for(&child.id).await?;
    assert!(child_commands.iter().any(|record| {
        record.status == WorkflowCommandStatus::Pending
            && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
            && record.command.command["retry_attempt"] == 1
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_terminal_pr_feedback_child_failure() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-failed-child")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection failed permanently.",
            "invalid PR feedback response",
        )
        .with_error_kind(ActivityErrorKind::Fatal),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "failed");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "terminal child failure must not propagate to parent"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_when_profile_max_turns_is_exhausted() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("replanning");
    store.upsert_instance(&workflow).await?;
    let mut profile = RuntimeProfile::new("codex-budgeted", RuntimeKind::CodexJsonrpc);
    profile.max_turns = Some(1);

    let first_command = WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let first_command_id = store
        .enqueue_command(&workflow.id, None, &first_command)
        .await?;
    store
        .enqueue_runtime_job(
            &first_command_id,
            profile.kind,
            &profile.name,
            json!({
                "workflow_id": workflow.id,
                "command_id": first_command_id,
                "command": first_command.command.clone(),
                "runtime_profile": profile.clone(),
            }),
        )
        .await?;

    let second_command = WorkflowCommand::enqueue_activity("address_pr_feedback", "feedback-1");
    let second_command_id = store
        .enqueue_command(&workflow.id, None, &second_command)
        .await?;
    let mut second_profile = RuntimeProfile::new("codex-budgeted", RuntimeKind::CodexJsonrpc);
    second_profile.max_turns = Some(1);
    store
        .enqueue_runtime_job(
            &second_command_id,
            second_profile.kind,
            &second_profile.name,
            json!({
                "workflow_id": workflow.id,
                "command_id": second_command_id,
                "command": second_command.command.clone(),
                "runtime_profile": second_profile.clone(),
            }),
        )
        .await?;

    let calls = Arc::new(AtomicUsize::new(0));
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("replan_issue", "Replan completed."),
        calls: calls.clone(),
    };
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));

    let first_completed = worker
        .run_once(&executor)
        .await?
        .expect("first runtime job should run");
    assert_eq!(first_completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second_completed = worker
        .run_once(&executor)
        .await?
        .expect("second runtime job should be completed as blocked");
    assert_eq!(second_completed.status, RuntimeJobStatus::Failed);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "executor should not be called after max_turns is exhausted"
    );
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&workflow.id, None)
            .await?,
        1
    );

    let output: ActivityResult =
        serde_json::from_value(second_completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Blocked);
    assert_eq!(output.activity, "address_pr_feedback");
    assert!(output
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("exhausted max_turns"));
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert_eq!(commands[1].status, WorkflowCommandStatus::Blocked);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_records_failed_activity_result() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
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
    enqueue_test_runtime_job(
        &store,
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
    let lease_expires_at = claimed
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;
    let completed = store
        .complete_runtime_job_if_owned(&claimed.id, "runtime-1", lease_expires_at, &result)
        .await?
        .expect("current owner completion should be accepted");
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
async fn durable_store_apply_decision_transition_can_create_initial_instance() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = project_issue_instance("/project-a", 123, "implementing");
    let decision = WorkflowDecision::new(
        &initial.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "implementation produced a pull request for the issue workflow",
    )
    .with_command(WorkflowCommand::bind_pr(
        77,
        "https://github.com/owner/repo/pull/77",
        "pr-detected:task-1:77",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "pr_open".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = json!({
        "project_id": "/project-a",
        "issue_number": 123,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "last_decision": "bind_pr",
    });

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: "implementing",
            create_if_missing: Some(&initial),
            event_type: "PrDetected",
            source: "workflow-runtime-test",
            payload: json!({
                "issue_number": 123,
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
            }),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?
        .expect("missing initial instance should be created inside the transition");

    assert!(record.accepted);
    assert!(record.event_id.is_some());
    let loaded = store
        .get_instance(&initial.id)
        .await?
        .expect("transition should persist final instance");
    assert_eq!(loaded.state, "pr_open");
    assert_eq!(loaded.data["pr_number"], 77);
    assert_eq!(store.events_for(&initial.id).await?.len(), 1);
    assert_eq!(store.decisions_for(&initial.id).await?.len(), 1);
    let commands = store.commands_for(&initial.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].decision_id.as_deref(), Some(record.id.as_str()));
    Ok(())
}

#[tokio::test]
async fn durable_store_apply_decision_transition_does_not_rewind_existing_instance(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = project_issue_instance("/project-a", 124, "implementing");
    let mut existing = initial.clone();
    existing.state = "awaiting_feedback".to_string();
    existing.data = json!({
        "project_id": "/project-a",
        "issue_number": 124,
        "pr_number": 78,
        "last_decision": "record_feedback",
    });
    store.upsert_instance(&existing).await?;
    let decision = WorkflowDecision::new(
        &initial.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "implementation produced a pull request for the issue workflow",
    )
    .with_command(WorkflowCommand::bind_pr(
        79,
        "https://github.com/owner/repo/pull/79",
        "pr-detected:task-2:79",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "pr_open".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = json!({
        "project_id": "/project-a",
        "issue_number": 124,
        "pr_number": 79,
        "pr_url": "https://github.com/owner/repo/pull/79",
        "last_decision": "bind_pr",
    });

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: "implementing",
            create_if_missing: Some(&initial),
            event_type: "PrDetected",
            source: "workflow-runtime-test",
            payload: json!({
                "issue_number": 124,
                "pr_number": 79,
                "pr_url": "https://github.com/owner/repo/pull/79",
            }),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?;

    assert!(record.is_none());
    let loaded = store
        .get_instance(&initial.id)
        .await?
        .expect("existing instance should remain visible");
    assert_eq!(loaded.state, "awaiting_feedback");
    assert_eq!(loaded.data["pr_number"], 78);
    assert!(store.events_for(&initial.id).await?.is_empty());
    assert!(store.decisions_for(&initial.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn durable_store_lists_workflow_runtime_tree_inputs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let parent = quality_gate_instance("checking").with_data(json!({
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
async fn durable_store_lists_nonterminal_instances_by_definition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let active = project_issue_instance("/project-a", 123, "implementing");
    let queued = project_issue_instance("/project-a", 124, "ready_to_merge");
    let terminal = project_issue_instance("/project-a", 125, "done");
    let other_definition = prompt_task_instance("implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let other_project = project_issue_instance("/project-b", 126, "implementing");
    store.upsert_instance(&active).await?;
    store.upsert_instance(&queued).await?;
    store.upsert_instance(&terminal).await?;
    store.upsert_instance(&other_definition).await?;
    store.upsert_instance(&other_project).await?;

    let listed = store
        .list_nonterminal_instances_by_definition(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            Some("/project-a"),
            None,
        )
        .await?;
    let ids: std::collections::HashSet<_> =
        listed.into_iter().map(|instance| instance.id).collect();

    assert!(ids.contains(&active.id));
    assert!(ids.contains(&queued.id));
    assert!(!ids.contains(&terminal.id));
    assert!(!ids.contains(&other_definition.id));
    assert!(!ids.contains(&other_project.id));
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
            .get("activity")
            .and_then(|activity| activity.as_str()),
        Some("replan_issue")
    );
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
        WorkflowCommandStatus::Dispatched
    );
    assert!(dispatcher.dispatch_once().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_store_can_insert_non_pending_command_atomically() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "scheduled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-inline");
    let command_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::HandledInline);
    assert!(
        store.pending_commands(10).await?.is_empty(),
        "inline commands must never be visible to the dispatcher as pending"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_non_pending_status_updates_existing_pending_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "scheduled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-inline");
    let pending_id = store.enqueue_command(&instance.id, None, &command).await?;
    let inline_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    assert_eq!(inline_id, pending_id);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, WorkflowCommandStatus::HandledInline);
    assert!(
        store.pending_commands(10).await?.is_empty(),
        "dedupe conflict must not leave an inline command visible as pending"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_dedupe_status_update_does_not_regress_dispatched_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-dispatched");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let outcome = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "replan_issue"}),
            None,
        )
        .await?;
    assert!(matches!(outcome, RuntimeJobEnqueueOutcome::Enqueued(_)));

    let duplicate_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    assert_eq!(duplicate_id, command_id);
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Dispatched
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_pending_command_enqueue_is_idempotent_across_concurrent_claims(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-idempotent");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;

    let first = store.enqueue_runtime_job_for_pending_command(
        &command_id,
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "replan_issue"}),
        None,
    );
    let second = store.enqueue_runtime_job_for_pending_command(
        &command_id,
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "replan_issue"}),
        None,
    );
    let (first, second) = tokio::join!(first, second);
    let outcomes = [first?, second?];
    let enqueued: Vec<&RuntimeJobEnqueueOutcome> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RuntimeJobEnqueueOutcome::Enqueued(_)))
        .collect();
    let already_exists: Vec<&RuntimeJobEnqueueOutcome> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RuntimeJobEnqueueOutcome::AlreadyExists(_)))
        .collect();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(already_exists.len(), 1);

    let first_job_id = match enqueued[0] {
        RuntimeJobEnqueueOutcome::Enqueued(job) => job.id.clone(),
        other => panic!("unexpected enqueue outcome: {other:?}"),
    };
    match already_exists[0] {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, first_job_id),
        other => panic!("unexpected idempotent enqueue outcome: {other:?}"),
    }

    let third = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "replan_issue"}),
            None,
        )
        .await?;
    match third {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, first_job_id),
        other => panic!("unexpected third enqueue outcome: {other:?}"),
    }

    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, first_job_id);
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Dispatched
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_claims_pending_commands_with_dispatch_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-lease");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;

    let claimed = store
        .claim_pending_commands("dispatcher-a", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, command_id);
    assert_eq!(claimed[0].status, WorkflowCommandStatus::Dispatching);
    assert_eq!(claimed[0].dispatch_owner.as_deref(), Some("dispatcher-a"));
    assert!(claimed[0].dispatch_lease_expires_at.is_some());
    assert!(store.pending_commands(10).await?.is_empty());

    let concurrent = store
        .claim_pending_commands("dispatcher-b", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert!(
        concurrent.is_empty(),
        "an unexpired dispatch lease must hide the command from other dispatchers"
    );

    let stale_command =
        WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-stale-lease");
    let stale_command_id = store
        .enqueue_command(&instance.id, None, &stale_command)
        .await?;
    let stale_claim = store
        .claim_pending_commands("stale-dispatcher", Utc::now() - Duration::seconds(1), 10)
        .await?;
    assert_eq!(stale_claim.len(), 1);
    assert_eq!(stale_claim[0].id, stale_command_id);

    let reclaimed = store
        .claim_pending_commands("dispatcher-c", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, stale_command_id);
    assert_eq!(reclaimed[0].dispatch_owner.as_deref(), Some("dispatcher-c"));
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_prefers_workflow_activity_profile() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let issue = project_issue_instance("/project-a", 123, "replanning");
    let prompt = prompt_task_instance("implementing").with_id("prompt-task-profile");
    store.upsert_instance(&issue).await?;
    store.upsert_instance(&prompt).await?;

    let issue_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-profile");
    let issue_implement_command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-profile");
    let prompt_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "prompt-task-replan-profile");
    let prompt_scan_command =
        WorkflowCommand::enqueue_activity("scan_repo", "prompt-task-scan-profile");
    let issue_replan_command_id = store
        .enqueue_command(&issue.id, None, &issue_replan_command)
        .await?;
    let issue_implement_command_id = store
        .enqueue_command(&issue.id, None, &issue_implement_command)
        .await?;
    let prompt_replan_command_id = store
        .enqueue_command(&prompt.id, None, &prompt_replan_command)
        .await?;
    let prompt_scan_command_id = store
        .enqueue_command(&prompt.id, None, &prompt_scan_command)
        .await?;

    let mut default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    default_profile.model = Some("gpt-default".to_string());
    let mut issue_profile = RuntimeProfile::new("codex-issue-high", RuntimeKind::CodexExec);
    issue_profile.model = Some("gpt-issue".to_string());
    issue_profile.timeout_secs = Some(1200);
    let mut replan_profile = RuntimeProfile::new("codex-replan", RuntimeKind::CodexJsonrpc);
    replan_profile.model = Some("gpt-replan".to_string());
    replan_profile.timeout_secs = Some(300);
    let mut issue_replan_profile =
        RuntimeProfile::new("codex-issue-replan", RuntimeKind::CodexExec);
    issue_replan_profile.model = Some("gpt-issue-replan".to_string());
    issue_replan_profile.timeout_secs = Some(90);
    let profile_selector = RuntimeProfileSelector::new(default_profile)
        .with_workflow_profile("github_issue_pr", issue_profile)
        .with_activity_profile("replan_issue", replan_profile)
        .with_workflow_activity_profile("github_issue_pr", "replan_issue", issue_replan_profile);
    let dispatcher = RuntimeCommandDispatcher::with_profile_selector(&store, profile_selector);

    let outcomes = dispatcher.dispatch_pending().await?;
    assert_eq!(outcomes.len(), 4);

    let replan_jobs = store
        .runtime_jobs_for_command(&issue_replan_command_id)
        .await?;
    assert_eq!(replan_jobs.len(), 1);
    assert_eq!(replan_jobs[0].runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(replan_jobs[0].runtime_profile, "codex-issue-replan");
    assert_eq!(
        replan_jobs[0].input["runtime_profile"]["name"],
        "codex-issue-replan"
    );
    assert_eq!(
        replan_jobs[0].input["runtime_profile"]["model"],
        "gpt-issue-replan"
    );
    assert_eq!(replan_jobs[0].input["runtime_profile"]["timeout_secs"], 90);

    let implement_jobs = store
        .runtime_jobs_for_command(&issue_implement_command_id)
        .await?;
    assert_eq!(implement_jobs.len(), 1);
    assert_eq!(implement_jobs[0].runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(implement_jobs[0].runtime_profile, "codex-issue-high");
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["name"],
        "codex-issue-high"
    );
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["model"],
        "gpt-issue"
    );
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["timeout_secs"],
        1200
    );

    let prompt_replan_jobs = store
        .runtime_jobs_for_command(&prompt_replan_command_id)
        .await?;
    assert_eq!(prompt_replan_jobs.len(), 1);
    assert_eq!(
        prompt_replan_jobs[0].runtime_kind,
        RuntimeKind::CodexJsonrpc
    );
    assert_eq!(prompt_replan_jobs[0].runtime_profile, "codex-replan");
    assert_eq!(
        prompt_replan_jobs[0].input["runtime_profile"]["model"],
        "gpt-replan"
    );
    assert_eq!(
        prompt_replan_jobs[0].input["runtime_profile"]["timeout_secs"],
        300
    );

    let prompt_scan_jobs = store
        .runtime_jobs_for_command(&prompt_scan_command_id)
        .await?;
    assert_eq!(prompt_scan_jobs.len(), 1);
    assert_eq!(prompt_scan_jobs[0].runtime_kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(prompt_scan_jobs[0].runtime_profile, "codex-default");
    assert_eq!(
        prompt_scan_jobs[0].input["runtime_profile"]["model"],
        "gpt-default"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_preserves_retry_not_before() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "implementing");
    store.upsert_instance(&instance).await?;
    let not_before = Utc::now() + Duration::seconds(60);
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "issue-123-implement-retry",
        json!({
            "activity": "implement_issue",
            "retry_attempt": 1,
            "retry_not_before": not_before.to_rfc3339(),
        }),
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("retry command should dispatch");
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

    assert_eq!(runtime_job.not_before, Some(not_before));
    let persisted = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].not_before, Some(not_before));
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_uses_command_type_activity_key_for_child_workflow(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let prompt = prompt_task_instance("implementing").with_id("prompt-task-child");
    store.upsert_instance(&prompt).await?;

    let child_command = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:123",
        "prompt-task:issue:123:start",
    );
    let child_command_id = store
        .enqueue_command(&prompt.id, None, &child_command)
        .await?;

    let default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    let mut child_profile = RuntimeProfile::new("codex-child", RuntimeKind::CodexExec);
    child_profile.model = Some("gpt-child".to_string());
    let profile_selector = RuntimeProfileSelector::new(default_profile)
        .with_activity_profile("start_child_workflow", child_profile);
    let dispatcher = RuntimeCommandDispatcher::with_profile_selector(&store, profile_selector);

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("child workflow command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id,
            runtime_job,
        } => {
            assert_eq!(command_id, child_command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };

    assert_eq!(runtime_job.runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(runtime_job.runtime_profile, "codex-child");
    assert_eq!(runtime_job.input["activity"], "start_child_workflow");
    assert_eq!(runtime_job.input["runtime_profile"]["model"], "gpt-child");
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
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Skipped
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_skips_terminal_workflow_before_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "cancelled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-cancelled-implement");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
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
            assert!(reason.contains("terminal (cancelled)"));
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}
