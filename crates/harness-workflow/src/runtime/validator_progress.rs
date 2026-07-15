use super::model::{WorkflowDecision, WorkflowInstance};
use super::state_registry::{self, WorkflowProgressMode};
use super::validator::{WorkflowDecisionRejection, WorkflowDecisionRejectionKind};

pub(super) fn validate_target_progress_contract(
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    if !state_registry::workflow_state_exists(&instance.definition_id, &decision.next_state) {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            format!(
                "target state '{}.{}' has no registered progress contract",
                instance.definition_id, decision.next_state
            ),
        ));
    }
    let progress_mode =
        state_registry::workflow_state_progress_mode(&instance.definition_id, &decision.next_state);
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
