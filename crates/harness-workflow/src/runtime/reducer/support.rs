use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use serde_json::{json, Value};

pub(super) fn invalid_agent_output_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    reason: &str,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "block_invalid_agent_output",
        "blocked",
        reason,
    )
    .with_command(WorkflowCommand::mark_blocked(
        reason,
        format!("runtime-completion:{}:invalid-output:block", event.id),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!("runtime-completion:{}:invalid-output:operator", event.id),
        json!({
            "reason": reason,
            "activity": result.activity,
            "runtime_job_id": event_field_string(event, "runtime_job_id"),
        }),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

pub(super) fn signal_count(result: &ActivityResult, signal_type: &str) -> usize {
    result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == signal_type)
        .count()
}

pub(super) fn has_any_signal(result: &ActivityResult, signal_types: &[&str]) -> bool {
    result
        .signals
        .iter()
        .any(|signal| signal_types.contains(&signal.signal_type.as_str()))
}

pub(super) fn has_signal(result: &ActivityResult, signal_type: &str) -> bool {
    result
        .signals
        .iter()
        .any(|signal| signal.signal_type == signal_type)
}

pub(super) fn result_signal_u64(result: &ActivityResult, field: &str) -> Option<u64> {
    result
        .signals
        .iter()
        .find_map(|signal| signal.signal.get(field).and_then(json_value_u64))
}

pub(super) fn result_signal_string(result: &ActivityResult, field: &str) -> Option<String> {
    result.signals.iter().find_map(|signal| {
        signal
            .signal
            .get(field)
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    })
}

pub(super) fn optional_data_string(instance: &WorkflowInstance, field: &str) -> Option<String> {
    instance
        .data
        .get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn json_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
}

pub(super) fn non_empty_json_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn string_array_field(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(non_empty_json_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(super) fn u64_array_field(value: &Value, field: &str) -> Vec<u64> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(json_value_u64).collect::<Vec<_>>())
        .unwrap_or_default()
}

pub(super) fn array_items<'a>(value: &'a Value, field: &str) -> Vec<&'a Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

pub(super) fn event_field_string(event: &WorkflowEvent, field: &str) -> Option<String> {
    event
        .event
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

pub(super) fn event_command_type(event: &WorkflowEvent) -> Option<&str> {
    event
        .event
        .get("command")
        .and_then(|command| command.get("command_type"))
        .and_then(|value| value.as_str())
}

pub(super) fn event_workflow_command(event: &WorkflowEvent) -> Option<WorkflowCommand> {
    event
        .event
        .get("command")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(super) fn optional_json_string(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

pub(super) fn runtime_completion_evidence(
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowEvidence {
    let command_id = event
        .event
        .get("command_id")
        .and_then(|value| value.as_str())
        .unwrap_or("<unknown>");
    let runtime_job_id = event
        .event
        .get("runtime_job_id")
        .and_then(|value| value.as_str())
        .unwrap_or("<unknown>");
    WorkflowEvidence::new(
        "runtime_completion",
        format!(
            "activity={} status={:?} command_id={} runtime_job_id={} summary={}",
            result.activity, result.status, command_id, runtime_job_id, result.summary
        ),
    )
}
