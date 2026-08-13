use super::{
    copy_optional_data_field, optional_string_field, recovery_prompt,
    RecoveryDispatchCommandSource, RecoveryDispatchPlan, RecoveryDispatchTarget,
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryRequest, RECOVERY_CONTEXT_FIELDS,
};
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::{
    candidate_fanout_from_value, WorkflowCommand, WorkflowCommandType, WorkflowInstance,
};
use anyhow::Context;
use serde_json::{json, Value};

pub(super) fn matches(instance: &WorkflowInstance, action: WorkflowRuntimeRecoveryAction) -> bool {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return false;
    }
    match (instance.state.as_str(), action) {
        ("awaiting_dependencies", WorkflowRuntimeRecoveryAction::Unblock) => true,
        ("failed", WorkflowRuntimeRecoveryAction::Retry) => dependency_cycle(&instance.data),
        _ => false,
    }
}

pub(super) fn matches_persisted_state(
    data: &Value,
    action: WorkflowRuntimeRecoveryAction,
    previous_state: &str,
) -> bool {
    matches!(
        (previous_state, action),
        (
            "awaiting_dependencies",
            WorkflowRuntimeRecoveryAction::Unblock
        )
    ) || (previous_state == "failed"
        && action == WorkflowRuntimeRecoveryAction::Retry
        && dependency_cycle(data))
}

fn dependency_cycle(data: &Value) -> bool {
    data.get("dependency_failure_status")
        .and_then(Value::as_str)
        == Some("dependency_cycle")
}

pub(super) fn dispatch_plan(
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
) -> anyhow::Result<RecoveryDispatchPlan> {
    let force_execute = instance
        .data
        .get("force_execute")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (state, activity) = if force_execute {
        ("implementing", "implement_issue")
    } else {
        ("planning", "plan_issue")
    };
    let remote_fact_hash = optional_string_field(&instance.data, "last_remote_fact_hash");
    let dispatch_fact_hash = remote_fact_hash.clone();
    let mut payload = json!({
        "activity": activity,
        "additional_prompt": recovery_prompt::dependency_override(&instance.data, request.reason),
        "dependency_override": {
            "previous_state": instance.state,
            "reason": request.reason,
        },
        "dispatch_gate": {
            "reason": "operator_dependency_override",
            "fact_hash": dispatch_fact_hash,
        },
        "remote_fact_hash": remote_fact_hash,
        "submission_mode": optional_string_field(&instance.data, "submission_mode")
            .unwrap_or_else(|| "immediate".to_string()),
    });
    for field in RECOVERY_CONTEXT_FIELDS {
        copy_optional_data_field(&mut payload, &instance.data, field);
    }
    let candidate_fanout = candidate_fanout_from_value(&instance.data)
        .context("invalid candidate_fanout recovery metadata")?;
    let candidate_fanout = force_execute.then_some(candidate_fanout).flatten();
    Ok(RecoveryDispatchPlan {
        target: RecoveryDispatchTarget {
            state: state.to_string(),
            activity: Some(activity.to_string()),
        },
        command_source: RecoveryDispatchCommandSource::Synthetic {
            command: WorkflowCommand::new(
                WorkflowCommandType::EnqueueActivity,
                "operator-dependency-override-preview",
                payload,
            ),
            candidate_fanout,
        },
    })
}
