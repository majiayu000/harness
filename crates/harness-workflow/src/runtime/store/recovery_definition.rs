use super::{
    WorkflowInstance, WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome,
    WorkflowRuntimeRecoveryRequest,
};
use crate::runtime::pr_feedback::PR_FEEDBACK_DEFINITION_ID;
use crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID;
use crate::runtime::quality_gate::QUALITY_GATE_DEFINITION_ID;
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::state_registry::{
    DeclarativeDefinitionPinError, DeclarativeDefinitionResolution, WorkflowDefinitionRegistry,
};

pub(super) fn custom_declarative_definition(
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
) -> Option<
    Result<
        std::sync::Arc<crate::runtime::declarative::DeclarativeWorkflowDefinition>,
        DeclarativeDefinitionPinError,
    >,
> {
    if is_builtin_definition_id(&instance.definition_id) {
        return None;
    }
    match registry.resolve_declarative_definition(instance) {
        DeclarativeDefinitionResolution::PinError(error) => Some(Err(error)),
        DeclarativeDefinitionResolution::Resolved(definition) => Some(Ok(definition)),
        DeclarativeDefinitionResolution::NotDeclarative => None,
    }
}

pub(super) fn is_builtin_definition_id(definition_id: &str) -> bool {
    matches!(
        definition_id,
        GITHUB_ISSUE_PR_DEFINITION_ID
            | PROMPT_TASK_DEFINITION_ID
            | QUALITY_GATE_DEFINITION_ID
            | PR_FEEDBACK_DEFINITION_ID
    )
}

#[rustfmt::skip]
pub(super) fn declarative_recovery_rejection(instance: &WorkflowInstance, request: &WorkflowRuntimeRecoveryRequest<'_>, definition: &crate::runtime::declarative::DeclarativeWorkflowDefinition) -> Option<WorkflowRuntimeRecoveryOutcome> {
    if request.actor != "operator" { return Some(WorkflowRuntimeRecoveryOutcome::OperatorRequired { workflow: instance.clone() }); }
    if request.action != WorkflowRuntimeRecoveryAction::Unblock || instance.state != "blocked" { return Some(WorkflowRuntimeRecoveryOutcome::WrongState { workflow: instance.clone() }); }
    if request.target_state.is_none() && definition.policy().recovery_targets.len() != 1 { return Some(WorkflowRuntimeRecoveryOutcome::TargetRequired { workflow: instance.clone() }); }
    request.target_state.filter(|target| !definition.policy().recovery_targets.iter().any(|allowed| allowed == target)).map(|target_state| WorkflowRuntimeRecoveryOutcome::TargetNotAllowed { workflow: instance.clone(), target_state: target_state.to_string() })
}
