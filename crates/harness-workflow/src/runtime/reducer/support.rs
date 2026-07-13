use crate::runtime::model::{
    ActivityErrorKind, ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::reason_class::{classify_stop, STOP_REASON_INVALID_AGENT_OUTPUT};
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
    .with_command(runtime_blocked_command(
        reason,
        Some(STOP_REASON_INVALID_AGENT_OUTPUT),
        format!("runtime-completion:{}:invalid-output:block", event.id),
        event,
        result,
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

pub(super) fn runtime_blocked_command(
    reason: impl Into<String>,
    stop_reason_code: Option<&str>,
    dedupe_key: impl Into<String>,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowCommand {
    let reason = reason.into();
    WorkflowCommand::new(
        WorkflowCommandType::MarkBlocked,
        dedupe_key,
        runtime_blocked_payload(&reason, stop_reason_code, event, result),
    )
}

pub(super) fn runtime_failed_command(
    reason: impl Into<String>,
    stop_reason_code: Option<&str>,
    dedupe_key: impl Into<String>,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowCommand {
    let reason = reason.into();
    WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        dedupe_key,
        runtime_failed_payload(&reason, stop_reason_code, event, result),
    )
}

/// Structured machine-stable stop reason code emitted by the activity, if
/// any. Unknown or forged codes are harmless: the classifier fails closed.
pub(super) fn result_stop_reason_code(result: &ActivityResult) -> Option<String> {
    result_signal_string(result, "stop_reason_code")
}

pub(super) fn runtime_blocked_payload(
    reason: &str,
    stop_reason_code: Option<&str>,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Value {
    let mut payload = json!({
        "reason": reason,
        "blocked_reason": reason,
        "unblock_hint": blocked_unblock_hint(),
        "last_stop": runtime_stop_metadata("blocked", stop_reason_code, event, result),
    });
    apply_stop_classification(&mut payload, stop_reason_code, result.error_kind);
    payload
}

pub(super) fn runtime_failed_payload(
    reason: &str,
    stop_reason_code: Option<&str>,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Value {
    let mut payload = json!({
        "reason": reason,
        "failure_reason": reason,
        "retry_hint": failed_retry_hint(result.error_kind),
        "last_stop": runtime_stop_metadata("failed", stop_reason_code, event, result),
    });
    if let Some(error_kind) = result.error_kind {
        payload["error_kind"] = json!(error_kind);
    }
    apply_stop_classification(&mut payload, stop_reason_code, result.error_kind);
    payload
}

fn runtime_stop_metadata(
    state: &str,
    stop_reason_code: Option<&str>,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Value {
    let mut metadata = json!({
        "state": state,
        "activity": stop_activity_name(event, result),
        "runtime_job_id": optional_json_string(event_field_string(event, "runtime_job_id")),
        "event_id": event.id,
        "recorded_at": event.created_at,
    });
    if let Some(error_kind) = result.error_kind {
        metadata["error_kind"] = json!(error_kind);
    }
    apply_stop_classification(&mut metadata, stop_reason_code, result.error_kind);
    metadata
}

/// Persist the structured `stop_reason_code` (when present) and the derived
/// `reason_class` (always, B-002: the fail-closed decision is visible in the
/// payload) via the single classifier (B-001). Additive JSON only.
fn apply_stop_classification(
    payload: &mut Value,
    stop_reason_code: Option<&str>,
    error_kind: Option<ActivityErrorKind>,
) {
    if let Some(payload) = payload.as_object_mut() {
        if let Some(code) = stop_reason_code {
            payload.insert("stop_reason_code".to_string(), json!(code));
        }
        payload.insert(
            "reason_class".to_string(),
            json!(classify_stop(stop_reason_code, error_kind).as_str()),
        );
    }
}

fn stop_activity_name(event: &WorkflowEvent, result: &ActivityResult) -> String {
    event_workflow_command(event)
        .as_ref()
        .and_then(WorkflowCommand::activity_name)
        .or_else(|| (!result.activity.trim().is_empty()).then_some(result.activity.as_str()))
        .or_else(|| event_command_type(event))
        .unwrap_or("<unknown>")
        .to_string()
}

fn blocked_unblock_hint() -> &'static str {
    "Resolve the blocked condition, then call the workflow runtime unblock API."
}

fn failed_retry_hint(error_kind: Option<ActivityErrorKind>) -> &'static str {
    match error_kind {
        Some(ActivityErrorKind::Fatal | ActivityErrorKind::Configuration) => {
            "Fix the non-retryable failure before retrying this workflow."
        }
        _ => "Fix the transient condition, then call the workflow runtime retry API.",
    }
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
