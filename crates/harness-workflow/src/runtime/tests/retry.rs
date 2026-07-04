use super::*;

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
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
fn runtime_completion_reducer_retries_local_review_failure_when_policy_allows() {
    let instance = issue_instance("local_review_gate").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::enqueue_activity(LOCAL_REVIEW_ACTIVITY, "local-review-1");
    let result = ActivityResult::failed(
        LOCAL_REVIEW_ACTIVITY,
        "Local review failed.",
        "codex stdin not available",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency);
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        .expect("failed local review should produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.next_state, "local_review_gate");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(
        decision.commands[0].command["activity"],
        LOCAL_REVIEW_ACTIVITY
    );
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("local review retry decision should validate");
}

#[test]
fn runtime_completion_reducer_retries_timeout_activity_failure_when_policy_allows() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::enqueue_activity("implement_issue", "implement-timeout-1");
    let result = ActivityResult::failed(
        "implement_issue",
        "Implementation timed out.",
        "Agent turn timed out after 30s",
    )
    .with_error_kind(ActivityErrorKind::Timeout);
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-timeout-1",
        "command": command,
        "runtime_job_id": "job-timeout-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("timeout activity should produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(
        decision.commands[0].command["previous_error_kind"],
        "timeout"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("timeout retry decision should validate");
}

#[test]
fn runtime_completion_reducer_retries_spawn_failure_when_policy_allows() {
    let instance = issue_instance("implementing").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let command = WorkflowCommand::enqueue_activity("implement_issue", "implement-spawn-1");
    let result = ActivityResult::failed(
        "implement_issue",
        "Agent turn completed without observable activity.",
        "agent completed with no observable activity",
    )
    .with_error_kind(ActivityErrorKind::SpawnFailure);
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-spawn-1",
        "command": command,
        "runtime_job_id": "job-spawn-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("spawn failure should produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(
        decision.commands[0].command["previous_error_kind"],
        "spawn_failure"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("spawn failure retry decision should validate");
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
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
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
