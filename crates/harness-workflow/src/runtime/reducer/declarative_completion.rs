use super::runtime_failure::{
    retry_failed_declarative_activity_decision, runtime_blocked_decision,
};
use super::support::{
    event_workflow_command, invalid_agent_output_blocked_decision, runtime_blocked_command,
    runtime_completion_evidence,
};
use crate::runtime::declarative::{
    workflow_evidence_from_activity_artifacts, DeclarativeWorkflowDefinition,
};
use crate::runtime::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::state_registry::{DeclarativeDefinitionPinError, WorkflowTerminalState};
use harness_core::config::workflow::DeclaredProgressMode;
use serde_json::json;

pub(crate) fn reduce_declarative_completion(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowDecision {
    let policy = definition.policy();
    let Some(source) = policy.states.get(&instance.state) else {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' received a completion in undeclared or terminal state '{}'",
                policy.id, instance.state
            ),
        );
    };
    let Some(expected_activity) = source.activity.as_deref() else {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' state '{}' is not activity-driven",
                policy.id, instance.state
            ),
        );
    };
    if result.activity != expected_activity {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' state '{}' expected activity '{}', but result named '{}'",
                policy.id, instance.state, expected_activity, result.activity
            ),
        );
    }
    let Some(command) = event_workflow_command(event) else {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' completion is missing its enqueue_activity command",
                policy.id
            ),
        );
    };
    if command.command_type != WorkflowCommandType::EnqueueActivity
        || command.activity_name() != Some(expected_activity)
    {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' state '{}' completion command does not enqueue expected activity '{}'",
                policy.id, instance.state, expected_activity
            ),
        );
    }

    let route = match result.status {
        ActivityStatus::Succeeded => success_route(source, result),
        ActivityStatus::Blocked => {
            if let Some(target) = source.on_blocked.as_deref() {
                Some((target, "on_blocked".to_string()))
            } else {
                let decision = runtime_blocked_decision(instance, event, result).with_command(
                    WorkflowCommand::new(
                        WorkflowCommandType::RequestOperatorAttention,
                        format!("{}:blocked:{}:operator", instance.id, event.id),
                        json!({
                            "reason": "declarative activity used the runtime blocked fallback",
                            "activity": result.activity,
                        }),
                    ),
                );
                return with_artifact_evidence(decision, result).unwrap_or_else(|error| {
                    invalid_completion(instance, event, result, error.to_string())
                });
            }
        }
        ActivityStatus::Failed => {
            if let Some(target) = source.on_failure.as_deref() {
                Some((target, "on_failure".to_string()))
            } else if let Some(decision) =
                retry_failed_declarative_activity_decision(instance, event, result)
            {
                return with_artifact_evidence(decision, result).unwrap_or_else(|error| {
                    invalid_completion(instance, event, result, error.to_string())
                });
            } else {
                terminal_for_class(definition, WorkflowTerminalState::Failed)
                    .map(|target| (target, "runtime failed fallback".to_string()))
            }
        }
        ActivityStatus::Cancelled => {
            terminal_for_class(definition, WorkflowTerminalState::Cancelled)
                .map(|target| (target, "runtime cancelled fallback".to_string()))
        }
    };

    let Some((target, route_reason)) = route else {
        return invalid_completion(
            instance,
            event,
            result,
            format!(
                "declarative workflow '{}' state '{}' has no route for {:?} completion",
                policy.id, instance.state, result.status
            ),
        );
    };

    transition_decision(definition, instance, event, result, target, &route_reason)
        .and_then(|decision| with_completion_evidence(decision, event, result))
        .unwrap_or_else(|error| invalid_completion(instance, event, result, error.to_string()))
}

fn success_route<'a>(
    source: &'a harness_core::config::workflow::DeclaredState,
    result: &ActivityResult,
) -> Option<(&'a str, String)> {
    let mapped_signals = source
        .on_signal
        .iter()
        .filter(|(signal_type, _)| {
            result
                .signals
                .iter()
                .any(|signal| signal.signal_type == signal_type.as_str())
        })
        .collect::<Vec<_>>();
    if let Some((signal_type, target)) = mapped_signals.first() {
        let reason = if mapped_signals.len() == 1 {
            format!("on_signal '{}'", signal_type)
        } else {
            format!(
                "on_signal '{}' won lexicographic precedence over [{}]",
                signal_type,
                mapped_signals
                    .iter()
                    .skip(1)
                    .map(|(candidate, _)| candidate.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Some((target.as_str(), reason));
    }
    source
        .on_success
        .as_deref()
        .map(|target| (target, "on_success".to_string()))
}

fn transition_decision(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    target: &str,
    route_reason: &str,
) -> anyhow::Result<WorkflowDecision> {
    let policy = definition.policy();
    let reason = format!(
        "declarative activity '{}' completed with {:?}; {} selected state '{}'",
        result.activity, result.status, route_reason, target
    );
    let command = if let Some(class) = terminal_class(policy, target)? {
        terminal_command(class, instance, event, result, target, &reason)
    } else {
        let state = policy.states.get(target).ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' selected undeclared target state '{}'",
                policy.id,
                target
            )
        })?;
        if let Some(activity) = state.activity.as_deref() {
            WorkflowCommand::enqueue_activity(activity, event_dedupe_key(instance, target, event))
        } else {
            match state.progress {
                Some(DeclaredProgressMode::ExternalWait) => WorkflowCommand::wait(
                    format!("declarative workflow entered external wait state '{target}'"),
                    event_dedupe_key(instance, target, event),
                ),
                Some(DeclaredProgressMode::OperatorGate) => WorkflowCommand::new(
                    WorkflowCommandType::RequestOperatorAttention,
                    event_dedupe_key(instance, target, event),
                    json!({
                        "reason": reason,
                        "activity": result.activity,
                        "target_state": target,
                    }),
                ),
                None => anyhow::bail!(
                    "declarative workflow '{}' target state '{}' has no progress driver",
                    policy.id,
                    target
                ),
            }
        }
    };

    Ok(WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "apply_declarative_transition",
        target,
        reason,
    )
    .with_command(command)
    .high_confidence())
}

fn terminal_class(
    policy: &harness_core::config::workflow::WorkflowDefinitionPolicy,
    state: &str,
) -> anyhow::Result<Option<WorkflowTerminalState>> {
    policy
        .terminal
        .get(state)
        .map(|class| match class.as_str() {
            "succeeded" => Ok(WorkflowTerminalState::Succeeded),
            "failed" => Ok(WorkflowTerminalState::Failed),
            "cancelled" => Ok(WorkflowTerminalState::Cancelled),
            unknown => anyhow::bail!(
                "declarative workflow '{}' terminal state '{}' has unknown class '{}'",
                policy.id,
                state,
                unknown
            ),
        })
        .transpose()
}

fn terminal_for_class(
    definition: &DeclarativeWorkflowDefinition,
    expected: WorkflowTerminalState,
) -> Option<&str> {
    definition
        .policy()
        .terminal
        .iter()
        .find_map(|(state, class)| {
            let matches = matches!(
                (expected, class.as_str()),
                (WorkflowTerminalState::Succeeded, "succeeded")
                    | (WorkflowTerminalState::Failed, "failed")
                    | (WorkflowTerminalState::Cancelled, "cancelled")
            );
            matches.then_some(state.as_str())
        })
}

fn terminal_command(
    class: WorkflowTerminalState,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    target: &str,
    reason: &str,
) -> WorkflowCommand {
    let command_type = match class {
        WorkflowTerminalState::Succeeded => WorkflowCommandType::MarkDone,
        WorkflowTerminalState::Failed => WorkflowCommandType::MarkFailed,
        WorkflowTerminalState::Cancelled => WorkflowCommandType::MarkCancelled,
    };
    WorkflowCommand::new(
        command_type,
        event_dedupe_key(instance, target, event),
        json!({
            "reason": reason,
            "activity": result.activity,
            "target_state": target,
        }),
    )
}

fn event_dedupe_key(instance: &WorkflowInstance, target: &str, event: &WorkflowEvent) -> String {
    format!("{}:{}:{}", instance.id, target, event.id)
}

fn with_completion_evidence(
    mut decision: WorkflowDecision,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> anyhow::Result<WorkflowDecision> {
    decision = decision.with_evidence(runtime_completion_evidence(event, result));
    with_artifact_evidence(decision, result)
}

fn with_artifact_evidence(
    mut decision: WorkflowDecision,
    result: &ActivityResult,
) -> anyhow::Result<WorkflowDecision> {
    for evidence in workflow_evidence_from_activity_artifacts(&result.artifacts)? {
        decision = decision.with_evidence(evidence);
    }
    Ok(decision)
}

fn invalid_completion(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    reason: impl AsRef<str>,
) -> WorkflowDecision {
    invalid_agent_output_blocked_decision(instance, event, result, reason.as_ref())
}

pub(super) fn definition_pin_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    error: DeclarativeDefinitionPinError,
) -> WorkflowDecision {
    let error_code = match error {
        DeclarativeDefinitionPinError::MissingVersion => "missing_version",
        DeclarativeDefinitionPinError::MissingHash => "missing_hash",
        DeclarativeDefinitionPinError::InvalidHash => "invalid_hash",
        DeclarativeDefinitionPinError::HashMismatch => "hash_mismatch",
    };
    let reason = format!(
        "declarative workflow definition '{}@{}' could not resolve its pinned content: {}",
        instance.definition_id, instance.definition_version, error_code
    );
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "definition_version_missing",
        "blocked",
        &reason,
    )
    .with_command(runtime_blocked_command(
        &reason,
        None,
        format!("{}:blocked:{}:pin-error", instance.id, event.id),
        event,
        result,
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!("{}:blocked:{}:pin-error:operator", instance.id, event.id),
        json!({
            "reason": reason,
            "definition_id": instance.definition_id,
            "definition_version": instance.definition_version,
            "pin_error": error_code,
        }),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .with_evidence(WorkflowEvidence::new(
        "definition_pin_error",
        format!(
            "definition={} version={} error={}",
            instance.definition_id, instance.definition_version, error_code
        ),
    ))
    .high_confidence()
}
