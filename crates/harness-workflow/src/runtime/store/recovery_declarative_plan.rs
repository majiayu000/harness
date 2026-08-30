//! Recovery dispatch planning for declarative workflow definitions, split
//! from `recovery.rs` to keep that module within size limits.

use super::{
    RecoveryDispatchCommandSource, RecoveryDispatchPlan, RecoveryDispatchTarget,
    WorkflowRuntimeRecoveryRequest,
};
use crate::runtime::declarative::DeclarativeWorkflowDefinition;
use crate::runtime::declarative_agent_contract::declarative_enqueue_activity_command;
use crate::runtime::model::{WorkflowCommand, WorkflowCommandType, WorkflowInstance};
use crate::runtime::state_registry::WorkflowProgressMode;
use serde_json::json;

pub(super) fn declarative_recovery_dispatch_plan(
    request: &WorkflowRuntimeRecoveryRequest<'_>,
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
) -> anyhow::Result<Result<RecoveryDispatchPlan, Option<String>>> {
    let target = request
        .target_state
        .unwrap_or_else(|| definition.policy().recovery_targets[0].as_str());
    let state = &definition.policy().states[target];
    let target = RecoveryDispatchTarget {
        state: target.to_string(),
        activity: state.activity.clone(),
    };
    let command = if let Some(activity) = state.activity.as_deref() {
        // The pinned-command path keeps an agent contract, its prompt, and the
        // definition hash in the payload; the dedupe key is assigned when the
        // recovery decision is committed.
        let mut command =
            declarative_enqueue_activity_command(definition, instance, activity, String::new())?;
        if let Some(payload) = command.command.as_object_mut() {
            payload.insert("reason".to_string(), json!(request.reason));
            payload.insert("recovery_target".to_string(), json!(target.state));
        }
        command
    } else {
        let command_type = match definition
            .registered()
            .states
            .iter()
            .find(|candidate| candidate.key.state.as_ref() == target.state)
            .and_then(|candidate| candidate.progress_mode)
        {
            Some(WorkflowProgressMode::ExternalWait) => WorkflowCommandType::Wait,
            Some(WorkflowProgressMode::OperatorGate) => {
                WorkflowCommandType::RequestOperatorAttention
            }
            _ => return Ok(Err(None)),
        };
        WorkflowCommand::new(
            command_type,
            String::new(),
            json!({
                "reason": request.reason,
                "recovery_target": target.state,
                "activity": target.activity,
            }),
        )
    };
    Ok(Ok(RecoveryDispatchPlan {
        target,
        command_source: RecoveryDispatchCommandSource::DeclarativeProgress(command),
    }))
}
