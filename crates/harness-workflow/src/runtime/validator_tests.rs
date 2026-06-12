use super::*;
use crate::runtime::{WorkflowEvidence, WorkflowSubject};
use serde_json::json;

fn issue_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", "123"),
    )
}

fn blocked_done_pr_merge_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "reconcile_pr_merged",
        "done",
        "reconciled: PR merged externally",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "runtime-reconcile:workflow-1:done:77",
        json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77"
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "github_pr",
        "repo=owner/repo issue=123 pr=77 url=https://github.com/owner/repo/pull/77",
    ))
}

fn blocked_done_slug_pr_merge_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "reconcile_pr_merged",
        "done",
        "reconciled: PR merged externally",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "runtime-reconcile:workflow-1:done:77",
        json!({
            "pr_number": 77,
            "repo": "owner/repo"
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "github_pr",
        "repo=owner/repo issue=123 pr=77 url=<unknown>",
    ))
}

#[test]
fn github_issue_pr_validator_allows_blocked_done_for_pr_merge_reconciliation() {
    let instance = issue_instance("blocked");
    let decision = blocked_done_pr_merge_decision(&instance);

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("reconciliation", Utc::now()),
        )
        .expect("PR-merge reconciliation should finish a blocked issue workflow");
}

#[test]
fn github_issue_pr_validator_allows_slug_only_blocked_done_reconciliation() {
    let instance = issue_instance("blocked");
    let decision = blocked_done_slug_pr_merge_decision(&instance);

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("reconciliation", Utc::now()),
        )
        .expect("PR-merge reconciliation can use repo plus pr_number without pr_url");
}

#[test]
fn github_issue_pr_validator_does_not_advertise_reconciliation_only_blocked_done() {
    let validator = DecisionValidator::github_issue_pr();

    assert!(!validator
        .transition_rules_from("blocked")
        .any(|rule| rule.to_state == "done"));
}

#[test]
fn github_issue_pr_validator_rejects_non_reconciliation_blocked_done() {
    let instance = issue_instance("blocked");
    let decision = blocked_done_pr_merge_decision(&instance);

    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("controller-1", Utc::now()),
        )
        .expect_err("non-reconciliation actors must not finish blocked issue workflows");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::MissingTerminalEvidence
    );
}

#[test]
fn github_issue_pr_validator_rejects_unevidenced_blocked_done() {
    let instance = issue_instance("blocked");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "agent_reported_done",
        "done",
        "The agent reported completion without external merge evidence.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "issue-123-done",
        json!({ "reason": "done" }),
    ));

    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("reconciliation", Utc::now()),
        )
        .expect_err("blocked -> done requires merged PR evidence");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::MissingTerminalEvidence
    );
}
