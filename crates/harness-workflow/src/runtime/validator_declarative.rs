use super::{
    DecisionValidator, TransitionRule, ValidationContext, WorkflowDecisionRejection,
    WorkflowDecisionRejectionKind,
};
use crate::runtime::model::{WorkflowCommandType, WorkflowDecision, WorkflowInstance};
use crate::runtime::state_registry::declarative_workflow_definition_for_instance;

pub(super) fn validate_decision(
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if decision.observed_state != "blocked" || decision.next_state == "blocked" {
        return Ok(());
    }
    if context.actor == "workflow_runtime_operator_action"
        && decision.decision == "operator_runtime_unblock"
    {
        return Ok(());
    }
    Err(WorkflowDecisionRejection::new(
        WorkflowDecisionRejectionKind::TransitionNotAllowed,
        "declarative recovery transitions require workflow runtime operator action context",
    ))
}

pub(super) fn validate_retry(
    validator: &DecisionValidator,
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<bool, WorkflowDecisionRejection> {
    if decision.decision != "retry_failed_runtime_activity"
        || decision.observed_state != decision.next_state
    {
        return Ok(false);
    }
    let Some(definition) = declarative_workflow_definition_for_instance(instance) else {
        return Ok(false);
    };
    let expected_activity = definition
        .policy()
        .states
        .get(&instance.state)
        .and_then(|state| state.activity.as_deref())
        .ok_or_else(|| {
            WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::InvalidDecisionContract,
                "declarative activity retry requires the pinned current state to declare an activity",
            )
        })?;
    if decision.commands.len() != 1
        || decision.commands[0].command_type != WorkflowCommandType::EnqueueActivity
        || decision.commands[0].activity_name() != Some(expected_activity)
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::InvalidDecisionContract,
            format!(
                "declarative activity retry for state '{}' must enqueue only activity '{}'",
                instance.state, expected_activity
            ),
        ));
    }
    let rule = TransitionRule::new(
        instance.state.as_str(),
        instance.state.as_str(),
        [WorkflowCommandType::EnqueueActivity],
    );
    validator.validate_commands(&rule, decision, context)?;
    crate::runtime::validator_progress::validate_target_progress_contract(instance, decision)?;
    Ok(true)
}

pub(super) fn validate_definition_version_missing(
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    let command_types = decision
        .commands
        .iter()
        .map(|command| command.command_type)
        .collect::<Vec<_>>();
    if decision.decision != "definition_version_missing"
        || decision.next_state != "blocked"
        || command_types
            != [
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::RequestOperatorAttention,
            ]
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::InvalidDecisionContract,
            "missing declarative definitions may only emit definition_version_missing with MarkBlocked then RequestOperatorAttention",
        ));
    }
    Ok(())
}
