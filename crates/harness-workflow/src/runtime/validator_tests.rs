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

fn blocked_done_issue_completed_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        instance.id.clone(),
        "blocked",
        "reconcile_issue_completed",
        "done",
        "reconciled: issue completed externally",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "runtime-reconcile:workflow-1:done:issue-123",
        json!({
            "issue_number": 123,
            "repo": "owner/repo"
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "github_issue",
        "repo=owner/repo issue=123",
    ))
}

fn local_review_gate_done_pr_merge_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        instance.id.clone(),
        "local_review_gate",
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
fn github_issue_pr_validator_allows_local_review_gate_done_for_pr_merge_reconciliation() {
    let instance = issue_instance("local_review_gate");
    let decision = local_review_gate_done_pr_merge_decision(&instance);

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("reconciliation", Utc::now()),
        )
        .expect("PR-merge reconciliation should finish a local review gate workflow");
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
fn github_issue_pr_validator_allows_completed_issue_done_reconciliation() {
    let instance = issue_instance("blocked");
    let decision = blocked_done_issue_completed_decision(&instance);

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("reconciliation", Utc::now()),
        )
        .expect("completed issue reconciliation should finish a blocked issue workflow");
}

#[test]
fn github_issue_pr_validator_does_not_advertise_reconciliation_only_local_review_gate_done() {
    let validator = DecisionValidator::github_issue_pr();

    assert!(!validator
        .transition_rules_from("local_review_gate")
        .any(|rule| rule.to_state == "done"));
}

#[test]
fn github_issue_pr_validator_does_not_advertise_reconciliation_only_blocked_done() {
    let validator = DecisionValidator::github_issue_pr();

    assert!(!validator
        .transition_rules_from("blocked")
        .any(|rule| rule.to_state == "done"));
}

#[test]
fn github_issue_pr_validator_rejects_non_reconciliation_local_review_gate_done() {
    let instance = issue_instance("local_review_gate");
    let decision = local_review_gate_done_pr_merge_decision(&instance);

    let err = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("controller-1", Utc::now()),
        )
        .expect_err("non-reconciliation actors must not finish local review gate workflows");

    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::MissingTerminalEvidence
    );
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
fn github_issue_pr_validator_rejects_unevidenced_local_review_gate_done() {
    let instance = issue_instance("local_review_gate");
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "local_review_gate",
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
        .expect_err("local_review_gate -> done requires merged PR evidence");

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

#[test]
fn require_evidence_attaches_classes_to_an_allowed_transition() {
    let allowlist = TransitionAllowlist::default()
        .allow("implementing", "done", [WorkflowCommandType::MarkDone])
        .require_evidence("implementing", "done", ["prompt_completion_evidence"]);

    let rule = allowlist
        .rule_for("implementing", "done")
        .expect("transition must exist");
    assert!(rule
        .required_evidence
        .contains("prompt_completion_evidence"));
}

#[test]
fn allow_alone_leaves_a_transition_evidence_free() {
    let allowlist = TransitionAllowlist::default().allow(
        "implementing",
        "done",
        [WorkflowCommandType::MarkDone],
    );

    let rule = allowlist
        .rule_for("implementing", "done")
        .expect("transition must exist");
    assert!(rule.required_evidence.is_empty());
}

#[test]
#[should_panic(expected = "cannot require evidence for unallowed transition")]
fn require_evidence_rejects_a_transition_that_was_never_allowed() {
    let _ = TransitionAllowlist::default().require_evidence(
        "implementing",
        "done",
        ["prompt_completion_evidence"],
    );
}

#[test]
fn without_required_evidence_strips_requirements_but_keeps_commands() {
    let allowlist = TransitionAllowlist::default()
        .allow("implementing", "done", [WorkflowCommandType::MarkDone])
        .require_evidence("implementing", "done", ["prompt_completion_evidence"])
        .without_required_evidence();

    let rule = allowlist
        .rule_for("implementing", "done")
        .expect("transition must exist");
    assert!(rule.required_evidence.is_empty());
    assert!(rule
        .allowed_commands
        .contains(&WorkflowCommandType::MarkDone));
}

#[test]
fn declared_evidence_gates_a_decision_and_enforcement_can_be_lifted() {
    let instance = WorkflowInstance::new(
        "prompt_task",
        1,
        "implementing",
        WorkflowSubject::new("prompt", "task-1"),
    );
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "agent_reported_done",
        "done",
        "agent reported completion",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "task-1-done",
        json!({ "reason": "done" }),
    ));

    let enforcing = DecisionValidator::new(
        TransitionAllowlist::default()
            .allow("implementing", "done", [WorkflowCommandType::MarkDone])
            .require_evidence("implementing", "done", ["prompt_completion_evidence"]),
    );
    let err = enforcing
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime", Utc::now()),
        )
        .expect_err("decision without the declared evidence must be rejected");
    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::MissingRequiredEvidence
    );

    let evidenced = decision.clone().with_evidence(WorkflowEvidence::new(
        "prompt_completion_evidence",
        "validation report attached",
    ));
    enforcing
        .validate(
            &instance,
            &evidenced,
            &ValidationContext::new("runtime", Utc::now()),
        )
        .expect("decision carrying the declared evidence must be accepted");

    let lifted = DecisionValidator::new(
        TransitionAllowlist::default()
            .allow("implementing", "done", [WorkflowCommandType::MarkDone])
            .require_evidence("implementing", "done", ["prompt_completion_evidence"])
            .without_required_evidence(),
    );
    lifted
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime", Utc::now()),
        )
        .expect("kill switch must restore claim-trusting behavior");
}

#[test]
fn prompt_task_done_requires_completion_evidence_even_if_the_reducer_is_bypassed() {
    let instance = WorkflowInstance::new(
        "prompt_task",
        1,
        "implementing",
        WorkflowSubject::new("prompt", "task-1"),
    );
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "finish_prompt_task",
        "done",
        "prompt implementation activity completed successfully",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "prompt-task-1-done",
        json!({ "reason": "done" }),
    ));

    let err = DecisionValidator::prompt_task()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("implementing -> done without completion evidence must be rejected");
    assert_eq!(
        err.kind,
        WorkflowDecisionRejectionKind::MissingRequiredEvidence
    );

    let evidenced = decision.with_evidence(WorkflowEvidence::new(
        "prompt_completion_evidence",
        "validation_report: 1 command(s) reported, 0 non-zero exit(s)",
    ));
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &evidenced,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("the declared evidence unlocks the transition");
}
