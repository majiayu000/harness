use super::super::{
    build_local_review_completed_decision, LocalReviewCompletedInput, LocalReviewOutcome,
};
use super::*;

#[test]
fn local_review_changes_requested_command_carries_review_findings() {
    let instance = issue_instance("local_review_gate");
    let summary = "Local review found a missing regression test for the PR feedback gate.";

    let output = build_local_review_completed_decision(
        &instance,
        LocalReviewCompletedInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            repair_dedupe_key: "local-review:workflow-1:77:address:command-1",
            outcome: LocalReviewOutcome::ChangesRequested,
            summary,
        },
    );

    assert_eq!(
        output.action,
        PrFeedbackWorkflowAction::LocalReviewChangesRequested
    );
    assert_eq!(output.decision.decision, "address_local_review_feedback");
    assert_eq!(output.decision.next_state, "addressing_feedback");
    assert_eq!(output.decision.commands.len(), 1);
    let command = &output.decision.commands[0];
    assert_eq!(command.activity_name(), Some("address_pr_feedback"));
    assert_eq!(command.command["source"], "local_review");
    assert_eq!(command.command["task_id"], "task-1");
    assert_eq!(command.command["pr_number"], 77);
    assert_eq!(
        command.command["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    assert_eq!(command.command["review_summary"], summary);
    assert_eq!(
        command.dedupe_key,
        "local-review:workflow-1:77:address:command-1"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("local review changes-requested decision should validate");
}

#[test]
fn local_review_after_rework_uses_completion_command_dedupe_key() {
    let instance = issue_instance("addressing_feedback");
    let command = WorkflowCommand::enqueue_activity("address_pr_feedback", "feedback-1");
    let result = ActivityResult::succeeded("address_pr_feedback", "Feedback addressed.");
    let first_event = WorkflowEvent::new(
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
    let second_event = WorkflowEvent::new(
        &instance.id,
        2,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-2",
        "command": WorkflowCommand::enqueue_activity("address_pr_feedback", "feedback-2"),
        "runtime_job_id": "job-2",
        "activity_result": ActivityResult::succeeded(
            "address_pr_feedback",
            "Feedback addressed again."
        ),
    }));

    let first_decision = reduce_runtime_job_completed(&instance, &first_event)
        .expect("event should parse")
        .expect("feedback completion should request local review");
    let second_decision = reduce_runtime_job_completed(&instance, &second_event)
        .expect("event should parse")
        .expect("second feedback completion should request local review");

    assert_eq!(first_decision.next_state, "local_review_gate");
    assert_eq!(second_decision.next_state, "local_review_gate");
    assert_eq!(first_decision.commands.len(), 1);
    assert_eq!(second_decision.commands.len(), 1);
    assert_eq!(
        first_decision.commands[0].activity_name(),
        Some(LOCAL_REVIEW_ACTIVITY)
    );
    assert_eq!(
        second_decision.commands[0].activity_name(),
        Some(LOCAL_REVIEW_ACTIVITY)
    );
    assert_eq!(
        first_decision.commands[0].dedupe_key,
        format!("local-review:{}:after-rework:command-1", instance.id)
    );
    assert_eq!(
        second_decision.commands[0].dedupe_key,
        format!("local-review:{}:after-rework:command-2", instance.id)
    );
    assert_ne!(
        first_decision.commands[0].dedupe_key,
        second_decision.commands[0].dedupe_key
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &first_decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("first post-rework local review decision should validate");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &second_decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("second post-rework local review decision should validate");
}

#[test]
fn local_review_changes_requested_uses_completed_command_dedupe_key() {
    let instance = issue_instance("local_review_gate");
    let first_result = ActivityResult::succeeded(LOCAL_REVIEW_ACTIVITY, "Changes requested.")
        .with_signal(ActivitySignal::new(
            super::super::LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
            }),
        ));
    let second_result =
        ActivityResult::succeeded(LOCAL_REVIEW_ACTIVITY, "Changes requested again.").with_signal(
            ActivitySignal::new(
                super::super::LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                }),
            ),
        );
    let first_event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "local-review-command-1",
        "runtime_job_id": "local-review-job-1",
        "activity_result": first_result,
    }));
    let second_event = WorkflowEvent::new(
        &instance.id,
        2,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "local-review-command-2",
        "runtime_job_id": "local-review-job-2",
        "activity_result": second_result,
    }));

    let first_decision = reduce_runtime_job_completed(&instance, &first_event)
        .expect("event should parse")
        .expect("local review changes should request repair");
    let second_decision = reduce_runtime_job_completed(&instance, &second_event)
        .expect("event should parse")
        .expect("second local review changes should request repair");

    assert_eq!(first_decision.next_state, "addressing_feedback");
    assert_eq!(second_decision.next_state, "addressing_feedback");
    assert_eq!(
        first_decision.commands[0].dedupe_key,
        format!(
            "local-review:{}:77:address:local-review-command-1",
            instance.id
        )
    );
    assert_eq!(
        second_decision.commands[0].dedupe_key,
        format!(
            "local-review:{}:77:address:local-review-command-2",
            instance.id
        )
    );
    assert_ne!(
        first_decision.commands[0].dedupe_key,
        second_decision.commands[0].dedupe_key
    );
}

#[test]
fn local_review_after_rework_replay_keeps_command_dedupe_key() {
    let instance = issue_instance("addressing_feedback");
    let event_payload = json!({
        "command_id": "command-1",
        "command": WorkflowCommand::enqueue_activity("address_pr_feedback", "feedback-1"),
        "runtime_job_id": "job-1",
        "activity_result": ActivityResult::succeeded(
            "address_pr_feedback",
            "Feedback addressed."
        ),
    });
    let first_event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(event_payload.clone());
    let replayed_event = WorkflowEvent::new(
        &instance.id,
        2,
        super::super::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(event_payload);

    let first_decision = reduce_runtime_job_completed(&instance, &first_event)
        .expect("event should parse")
        .expect("feedback completion should request local review");
    let replayed_decision = reduce_runtime_job_completed(&instance, &replayed_event)
        .expect("event should parse")
        .expect("replayed feedback completion should request local review");

    assert_ne!(first_event.id, replayed_event.id);
    assert_eq!(
        first_decision.commands[0].dedupe_key,
        format!("local-review:{}:after-rework:command-1", instance.id)
    );
    assert_eq!(
        first_decision.commands[0].dedupe_key,
        replayed_decision.commands[0].dedupe_key
    );
}
