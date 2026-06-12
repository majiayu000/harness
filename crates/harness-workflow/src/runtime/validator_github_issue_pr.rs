use super::{ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind};
use crate::runtime::model::{WorkflowCommand, WorkflowCommandType, WorkflowDecision};

pub(super) fn validate_decision(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if is_blocked_done_transition(decision) {
        validate_blocked_done_reconciliation(decision, context)?;
    }
    Ok(())
}

pub(super) fn is_blocked_done_transition(decision: &WorkflowDecision) -> bool {
    decision.observed_state == "blocked" && decision.next_state == "done"
}

pub(super) fn validate_blocked_done_reconciliation(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if context.actor != "reconciliation" || decision.decision != "reconcile_pr_merged" {
        return Err(missing_terminal_evidence(
            "blocked issue workflows can only be marked done by PR-merge reconciliation",
        ));
    }

    if !decision.commands.iter().any(is_pr_merge_mark_done_command) {
        return Err(missing_terminal_evidence(
            "blocked issue PR-merge reconciliation requires pr_number plus pr_url or repo evidence",
        ));
    }

    if !decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "github_pr")
    {
        return Err(missing_terminal_evidence(
            "blocked issue PR-merge reconciliation requires github_pr evidence",
        ));
    }

    Ok(())
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
