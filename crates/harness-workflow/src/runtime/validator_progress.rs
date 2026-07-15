use super::model::{WorkflowDecision, WorkflowInstance};
use super::state_registry::{self, WorkflowProgressMode};
use super::validator::{WorkflowDecisionRejection, WorkflowDecisionRejectionKind};

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
