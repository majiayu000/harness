use super::{ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind};
use crate::runtime::model::{WorkflowCommand, WorkflowCommandType, WorkflowDecision};

pub(super) fn validate_decision(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    validate_operator_recovery_transition(decision, context)?;
    if is_reconciliation_only_pr_merge_done_transition(decision) {
        validate_reconciliation_only_pr_merge_done(decision, context)?;
    }
    Ok(())
}

fn validate_operator_recovery_transition(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if !is_operator_recovery_transition(decision) {
        return Ok(());
    }
    if context.actor == "workflow_runtime_operator_action"
        && matches!(
            decision.decision.as_str(),
            "operator_runtime_unblock" | "operator_runtime_retry"
        )
    {
        return Ok(());
    }
    Err(WorkflowDecisionRejection::new(
        WorkflowDecisionRejectionKind::TransitionNotAllowed,
        "stopped-state recovery transitions require workflow runtime operator action context",
    ))
}

fn is_operator_recovery_transition(decision: &WorkflowDecision) -> bool {
    matches!(
        (
            decision.observed_state.as_str(),
            decision.next_state.as_str()
        ),
        (
            "blocked",
            "implementing"
                | "replanning"
                | "local_review_gate"
                | "awaiting_feedback"
                | "addressing_feedback"
                | "merging"
        ) | (
            "failed",
            "replanning"
                | "local_review_gate"
                | "awaiting_feedback"
                | "addressing_feedback"
                | "merging"
        )
    )
}

pub(super) fn validate_reconciliation_only_pr_merge_done(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if context.actor != "reconciliation" || decision.decision != "reconcile_pr_merged" {
        return Err(missing_terminal_evidence(
            "issue workflows in reconciliation-only states can only be marked done by PR-merge reconciliation",
        ));
    }

    if !decision.commands.iter().any(is_pr_merge_mark_done_command) {
        return Err(missing_terminal_evidence(
            "issue PR-merge reconciliation requires pr_number plus pr_url or repo evidence",
        ));
    }

    if !decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "github_pr")
    {
        return Err(missing_terminal_evidence(
            "issue PR-merge reconciliation requires github_pr evidence",
        ));
    }

    Ok(())
}

pub(super) fn is_reconciliation_only_pr_merge_done_transition(decision: &WorkflowDecision) -> bool {
    matches!(
        decision.observed_state.as_str(),
        "blocked" | "local_review_gate"
    ) && decision.next_state == "done"
}

fn is_pr_merge_mark_done_command(command: &WorkflowCommand) -> bool {
    command.command_type == WorkflowCommandType::MarkDone
        && command
            .command
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && (has_non_empty_command_string(command, "pr_url")
            || has_non_empty_command_string(command, "repo"))
}

fn has_non_empty_command_string(command: &WorkflowCommand, field: &str) -> bool {
    command
        .command
        .get(field)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn missing_terminal_evidence(message: impl Into<String>) -> WorkflowDecisionRejection {
    WorkflowDecisionRejection::new(
        WorkflowDecisionRejectionKind::MissingTerminalEvidence,
        message,
    )
}
