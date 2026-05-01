use super::model::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, ActivityStatus,
    RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
    WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowEvent, WorkflowEvidence, WorkflowInstance, WorkflowSubject,
};
use super::store::RuntimeJobEnqueueOutcome;
use super::validator::{DecisionValidator, ValidationContext, WorkflowDecisionRejectionKind};
use super::{
    build_merged_pr_decision, build_open_issue_without_workflow_decision,
    build_plan_issue_decision, build_pr_detected_decision, build_pr_feedback_decision,
    build_quality_gate_run_decision, build_stale_active_workflow_decision,
    reduce_runtime_job_completed, CommandDispatchOutcome, InMemoryWorkflowBus,
    MergedPrDecisionInput, OpenIssueDecisionInput, PlanIssueDecisionInput, PlanIssueWorkflowAction,
    PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackOutcome, PrFeedbackWorkflowAction,
    QualityGateDecisionInput, QualityGateWorkflowAction, RepoBacklogWorkflowAction,
    RuntimeCommandDispatcher, RuntimeJobExecutor, RuntimeProfileSelector, RuntimeWorker,
    StaleWorkflowDecisionInput, WorkflowRuntimeStore, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, REPO_BACKLOG_DEFINITION_ID,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

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

fn quality_gate_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("quality_gate", "issue:123"),
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
    let instance = issue_instance("pr_open");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "pr_open",
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
fn runtime_completion_reducer_returns_invalid_structured_workflow_decision_for_audit() {
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
        .expect("invalid structured workflow decision should still be returned");

    let error = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("validator should reject stale observed state");
    assert_eq!(error.kind, WorkflowDecisionRejectionKind::StateMismatch);
}

#[test]
fn runtime_completion_reducer_retries_failed_activity_when_policy_allows() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::enqueue_activity("implement_issue", "implement-1");
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation failed.",
        "codex stdin not available",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency);
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
        .expect("failed activity should produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(decision.commands[0].command["activity"], "implement_issue");
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    assert_eq!(
        decision.commands[0].command["previous_command_id"],
        "command-1"
    );
    assert_eq!(
        decision.commands[0].command["previous_error_kind"],
        "external_dependency"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("retry decision should validate");
}

#[test]
fn runtime_completion_reducer_does_not_retry_fatal_activity_failure() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 3
        }
    }));
    let command = WorkflowCommand::enqueue_activity("implement_issue", "implement-1");
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation cannot continue.",
        "repository instructions forbid this operation",
    )
    .with_error_kind(ActivityErrorKind::Fatal);
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
        .expect("fatal failure should fail the workflow immediately");

    assert_eq!(decision.decision, "fail_after_runtime_activity");
    assert_eq!(decision.next_state, "failed");
    assert_eq!(decision.commands[0].command["error_kind"], "fatal");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("fatal failure decision should validate");
}

#[test]
fn runtime_completion_reducer_fails_after_retry_policy_exhausted() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "implement-retry-1",
        json!({
            "activity": "implement_issue",
            "retry_attempt": 1
        }),
    );
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation failed again.",
        "codex stdin not available",
    );
    let event = WorkflowEvent::new(
        &instance.id,
        2,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-2",
        "command": command,
        "runtime_job_id": "job-2",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("exhausted retry policy should fail the workflow");

    assert_eq!(decision.decision, "fail_after_runtime_activity");
    assert_eq!(decision.next_state, "failed");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkFailed
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("failed decision should validate");
}

#[test]
fn runtime_completion_reducer_uses_activity_retry_override() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1,
            "activity_retries": {
                "implement_issue": {
                    "max_failed_activity_retries": 2
                }
            }
        }
    }));
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "implement-retry-1",
        json!({
            "activity": "implement_issue",
            "retry_attempt": 1
        }),
    );
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation failed again.",
        "codex stdin not available",
    );
    let event = WorkflowEvent::new(
        &instance.id,
        2,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-2",
        "command": command,
        "runtime_job_id": "job-2",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("activity override should allow a second retry");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.commands[0].command["retry_attempt"], 2);
    assert_eq!(
        decision.commands[0].command["max_failed_activity_retries"],
        2
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("retry override decision should validate");
}

#[test]
fn runtime_completion_reducer_adds_retry_cooldown_metadata() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 3,
            "retry_delay_secs": 30,
            "max_retry_delay_secs": 60,
            "activity_retries": {
                "implement_issue": {
                    "retry_delay_secs": 20,
                    "max_retry_delay_secs": 35
                }
            }
        }
    }));
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "implement-retry-1",
        json!({
            "activity": "implement_issue",
            "retry_attempt": 1
        }),
    );
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation failed again.",
        "codex stdin not available",
    );
    let event = WorkflowEvent::new(
        &instance.id,
        2,
        super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-2",
        "command": command,
        "runtime_job_id": "job-2",
        "activity_result": result,
    }));
    let before = Utc::now();

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("cooldown policy should still produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.commands[0].command["retry_attempt"], 2);
    assert_eq!(decision.commands[0].command["retry_delay_secs"], 35);
    let not_before = decision.commands[0].command["retry_not_before"]
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .expect("retry_not_before should be an RFC3339 timestamp");
    assert!(not_before >= before + Duration::seconds(34));
    assert!(not_before <= Utc::now() + Duration::seconds(36));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("retry cooldown decision should validate");
}

#[test]
fn runtime_completion_reducer_preserves_start_child_workflow_retry_type() {
    let instance = repo_backlog_instance("dispatching").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:123",
        "repo-backlog:owner/repo:issue:123:start",
    );
    let result = ActivityResult::failed(
        "workflow_activity",
        "Child workflow dispatch failed.",
        "runtime host unavailable",
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

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("start child workflow failure should retry");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        decision.commands[0].command["definition_id"],
        "github_issue_pr"
    );
    assert_eq!(decision.commands[0].command["subject_key"], "issue:123");
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("start child workflow retry should validate");
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

#[test]
fn runtime_completion_reducer_idles_repo_backlog_after_dispatch() {
    let instance = repo_backlog_instance("dispatching");
    let command = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:123",
        "repo-backlog:owner/repo:issue:123:start",
    );
    let result = ActivityResult::succeeded("workflow_activity", "Child workflow started.");
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
        .expect("dispatch completion should idle repo backlog");

    assert_eq!(decision.decision, "finish_issue_workflow_dispatch");
    assert_eq!(decision.next_state, "idle");
    assert!(decision.commands.is_empty());
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("repo backlog dispatch completion should validate");
}

#[test]
fn runtime_completion_reducer_idles_repo_backlog_after_reconciliation() {
    let instance = repo_backlog_instance("reconciling");
    let command = WorkflowCommand::enqueue_activity(
        "mark_bound_issue_done",
        "repo-backlog:owner/repo:pr:77:merged",
    );
    let result = ActivityResult::succeeded(
        "mark_bound_issue_done",
        "Bound issue workflow was marked done.",
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

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("reconciliation completion should idle repo backlog");

    assert_eq!(decision.decision, "finish_bound_issue_reconciliation");
    assert_eq!(decision.next_state, "idle");
    assert!(decision.commands.is_empty());
    DecisionValidator::repo_backlog()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("repo backlog reconciliation completion should validate");
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
async fn runtime_worker_skips_runtime_jobs_before_not_before() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let not_before = Utc::now() + Duration::minutes(5);
    let job = store
        .enqueue_runtime_job_with_not_before(
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
    let job = store
        .enqueue_runtime_job(
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

    let reclaimed = store
        .claim_next_runtime_job("runtime-2", Utc::now() + Duration::minutes(5))
        .await?
        .expect("expired running job should be reclaimable");
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.status, RuntimeJobStatus::Running);
    assert_eq!(
        reclaimed.lease.as_ref().expect("lease should exist").owner,
        "runtime-2"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_does_not_reclaim_unexpired_running_job() -> anyhow::Result<()> {
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
    assert_eq!(commands[0].status, "completed");
    assert_eq!(commands[1].status, "blocked");
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
        "dispatched"
    );
    assert!(dispatcher.dispatch_once().await?.is_none());
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
        "dispatched"
    );
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
    let backlog = repo_backlog_instance("scanning").with_id("repo-backlog");
    store.upsert_instance(&issue).await?;
    store.upsert_instance(&backlog).await?;

    let issue_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-profile");
    let issue_implement_command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-profile");
    let backlog_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "repo-backlog-replan-profile");
    let backlog_scan_command =
        WorkflowCommand::enqueue_activity("scan_repo", "repo-backlog-scan-profile");
    let issue_replan_command_id = store
        .enqueue_command(&issue.id, None, &issue_replan_command)
        .await?;
    let issue_implement_command_id = store
        .enqueue_command(&issue.id, None, &issue_implement_command)
        .await?;
    let backlog_replan_command_id = store
        .enqueue_command(&backlog.id, None, &backlog_replan_command)
        .await?;
    let backlog_scan_command_id = store
        .enqueue_command(&backlog.id, None, &backlog_scan_command)
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

    let backlog_replan_jobs = store
        .runtime_jobs_for_command(&backlog_replan_command_id)
        .await?;
    assert_eq!(backlog_replan_jobs.len(), 1);
    assert_eq!(
        backlog_replan_jobs[0].runtime_kind,
        RuntimeKind::CodexJsonrpc
    );
    assert_eq!(backlog_replan_jobs[0].runtime_profile, "codex-replan");
    assert_eq!(
        backlog_replan_jobs[0].input["runtime_profile"]["model"],
        "gpt-replan"
    );
    assert_eq!(
        backlog_replan_jobs[0].input["runtime_profile"]["timeout_secs"],
        300
    );

    let backlog_scan_jobs = store
        .runtime_jobs_for_command(&backlog_scan_command_id)
        .await?;
    assert_eq!(backlog_scan_jobs.len(), 1);
    assert_eq!(backlog_scan_jobs[0].runtime_kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(backlog_scan_jobs[0].runtime_profile, "codex-default");
    assert_eq!(
        backlog_scan_jobs[0].input["runtime_profile"]["model"],
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
    let backlog = repo_backlog_instance("dispatching").with_id("repo-backlog");
    store.upsert_instance(&backlog).await?;

    let child_command = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:123",
        "repo-backlog:owner/repo:issue:123:start",
    );
    let child_command_id = store
        .enqueue_command(&backlog.id, None, &child_command)
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
    assert_eq!(store.commands_for(&instance.id).await?[0].status, "skipped");
    Ok(())
}
