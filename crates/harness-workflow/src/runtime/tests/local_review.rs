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
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("local review changes-requested decision should validate");
}
