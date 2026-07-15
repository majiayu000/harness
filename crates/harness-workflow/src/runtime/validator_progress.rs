use super::model::{WorkflowDecision, WorkflowInstance};
use super::state_registry::{self, WorkflowProgressMode};
use super::validator::{
    TransitionRule, ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
use std::collections::BTreeSet;

pub(super) fn validate_declarative_transition_metadata(
    rule: &TransitionRule,
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if rule.operator_recovery_only
        && (decision.observed_state != "blocked"
            || decision.decision != "operator_runtime_unblock"
            || context.actor != "workflow_runtime_operator_action")
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::OperatorRecoveryDenied,
            "declarative recovery transitions require an operator unblock from blocked state",
        ));
    }
    if let Some(required_command) = rule.required_command {
        if !decision
            .commands
            .iter()
            .any(|command| command.command_type == required_command)
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::RequiredCommandMissing,
                format!(
                    "declarative transition '{}' -> '{}' requires command {:?}",
                    decision.observed_state, decision.next_state, required_command
                ),
            ));
        }
    }
    let is_activity_retry = decision.decision == "retry_failed_runtime_activity"
        && decision.observed_state == decision.next_state
        && decision.commands.len() == 1
        && decision.commands[0].command_type == super::model::WorkflowCommandType::EnqueueActivity;
    if is_activity_retry {
        return Ok(());
    }
    let evidence_kinds = decision
        .evidence
        .iter()
        .map(|evidence| evidence.kind.as_str())
        .collect::<BTreeSet<_>>();
    let missing = rule
        .required_evidence
        .iter()
        .filter(|required| !evidence_kinds.contains(required.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::MissingRequiredEvidence,
            format!(
                "transition '{}' -> '{}' is missing required evidence: {}",
                decision.observed_state,
                decision.next_state,
                missing.join(", ")
            ),
        ));
    }
    Ok(())
}

pub(super) fn validate_target_progress_contract(
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    if state_registry::workflow_state_definition_for_instance(instance, &decision.next_state)
        .is_none()
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            format!(
                "target state '{}.{}' has no registered progress contract",
                instance.definition_id, decision.next_state
            ),
        ));
    }
    let progress_mode =
        state_registry::workflow_state_definition_for_instance(instance, &decision.next_state)
            .and_then(|state| state.progress_mode);
    let has_driver = decision
        .commands
        .iter()
        .any(super::model::WorkflowCommand::requires_runtime_job);
    if progress_mode == Some(WorkflowProgressMode::CommandDriven) && !has_driver {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::ProgressDriverMissing,
            format!(
                "command-driven target state '{}.{}' requires an allowlisted runtime-job-producing command in the same decision",
                instance.definition_id, decision.next_state
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "validator_evidence_tests.rs"]
mod tests;
