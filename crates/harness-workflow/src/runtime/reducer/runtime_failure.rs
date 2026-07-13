use super::support::{
    event_command_type, event_field_string, event_workflow_command, optional_json_string,
    result_stop_reason_code, runtime_blocked_command, runtime_completion_evidence,
    runtime_failed_command,
};
use super::{
    GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID, PR_FEEDBACK_DEFINITION_ID,
    QUALITY_GATE_DEFINITION_ID,
};
use crate::runtime::model::{
    ActivityErrorKind, ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowInstance,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

pub(super) fn runtime_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowDecision {
    let reason = runtime_failure_reason(result, "Runtime activity was blocked.");
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "block_after_runtime_activity",
        "blocked",
        &reason,
    )
    .with_command(runtime_blocked_command(
        &reason,
        result_stop_reason_code(result).as_deref(),
        format!("runtime-completion:{}:blocked", event.id),
        event,
        result,
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

pub(super) fn runtime_failed_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowDecision {
    let reason = runtime_failure_reason(result, "Runtime activity failed.");
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "fail_after_runtime_activity",
        "failed",
        &reason,
    )
    .with_command(runtime_failed_command(
        &reason,
        result_stop_reason_code(result).as_deref(),
        format!("runtime-completion:{}:failed", event.id),
        event,
        result,
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

pub(super) fn retry_failed_activity_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if !is_retryable_error_kind(result.error_kind) {
        return None;
    }
    if !supports_same_state_activity_retry(&instance.definition_id, &instance.state) {
        return None;
    }
    let retry_attempt = failed_activity_retry_attempt(event);
    let activity = retry_activity_name(event, result)?;
    let retry_limit = failed_activity_retry_limit(instance, &activity)?;
    if retry_attempt >= retry_limit {
        return None;
    }
    let next_attempt = retry_attempt + 1;
    let reason = runtime_failure_reason(result, "Runtime activity failed.");
    let retry_schedule =
        failed_activity_retry_schedule(instance, &activity, next_attempt, event.created_at);
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "retry_failed_runtime_activity",
            &instance.state,
            format!(
                "Runtime activity `{activity}` failed; retrying attempt {next_attempt} of {retry_limit}. Last error: {reason}"
            ),
        )
        .with_command(retry_command(
            event,
            &activity,
            result.error_kind,
            next_attempt,
            retry_limit,
            &reason,
            retry_schedule,
        ))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn is_retryable_error_kind(error_kind: Option<ActivityErrorKind>) -> bool {
    !matches!(
        error_kind,
        Some(ActivityErrorKind::Fatal | ActivityErrorKind::Configuration)
    )
}

pub(super) fn runtime_cancelled_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowDecision {
    let reason = runtime_failure_reason(result, "Runtime activity was cancelled.");
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "cancel_after_runtime_activity",
        "cancelled",
        &reason,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!("runtime-completion:{}:cancelled", event.id),
        json!({ "reason": reason }),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

fn runtime_failure_reason(result: &ActivityResult, fallback: &str) -> String {
    result
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!result.summary.trim().is_empty()).then_some(result.summary.trim()))
        .unwrap_or(fallback)
        .to_string()
}

fn failed_activity_retry_limit(instance: &WorkflowInstance, activity: &str) -> Option<u64> {
    if let Some(limit) = retry_policy_u64(instance, activity, "max_failed_activity_retries") {
        return (limit > 0).then_some(limit);
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct RetrySchedule {
    delay_secs: u64,
    not_before: DateTime<Utc>,
}

fn failed_activity_retry_schedule(
    instance: &WorkflowInstance,
    activity: &str,
    next_attempt: u64,
    base_time: DateTime<Utc>,
) -> Option<RetrySchedule> {
    let base_delay = retry_policy_u64(instance, activity, "retry_delay_secs")?;
    if base_delay == 0 {
        return None;
    }
    let multiplier = 1_u64
        .checked_shl(next_attempt.saturating_sub(1).min(63) as u32)
        .unwrap_or(u64::MAX);
    let mut delay_secs = base_delay.saturating_mul(multiplier);
    if let Some(max_delay) = retry_policy_u64(instance, activity, "max_retry_delay_secs")
        .filter(|max_delay| *max_delay > 0)
    {
        delay_secs = delay_secs.min(max_delay);
    }
    const MAX_SAFE_RETRY_DELAY_SECS: u64 = 30 * 24 * 60 * 60;
    delay_secs = delay_secs.min(MAX_SAFE_RETRY_DELAY_SECS);
    Some(RetrySchedule {
        delay_secs,
        not_before: base_time + Duration::seconds(delay_secs as i64),
    })
}

fn retry_policy_u64(instance: &WorkflowInstance, activity: &str, field: &str) -> Option<u64> {
    let policy = instance.data.get("runtime_retry_policy")?;
    policy
        .get("activity_retries")
        .and_then(|activities| activities.get(activity))
        .and_then(|activity_policy| activity_policy.get(field))
        .and_then(Value::as_u64)
        .or_else(|| policy.get(field).and_then(Value::as_u64))
}

fn failed_activity_retry_attempt(event: &WorkflowEvent) -> u64 {
    event
        .event
        .get("command")
        .and_then(|command| command.get("command"))
        .and_then(|command| command.get("retry_attempt"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

fn retry_activity_name(event: &WorkflowEvent, result: &ActivityResult) -> Option<String> {
    event
        .event
        .get("command")
        .and_then(|command| command.get("command"))
        .and_then(|command| command.get("activity"))
        .and_then(|value| value.as_str())
        .filter(|activity| !activity.trim().is_empty())
        .or_else(|| event_command_type(event))
        .or_else(|| (!result.activity.trim().is_empty()).then_some(result.activity.as_str()))
        .map(str::to_string)
}

fn retry_command(
    event: &WorkflowEvent,
    activity: &str,
    error_kind: Option<ActivityErrorKind>,
    next_attempt: u64,
    retry_limit: u64,
    reason: &str,
    retry_schedule: Option<RetrySchedule>,
) -> WorkflowCommand {
    let previous = event_workflow_command(event);
    let command_type = previous
        .as_ref()
        .map(|command| command.command_type)
        .unwrap_or(WorkflowCommandType::EnqueueActivity);
    let mut command_payload = previous
        .map(|command| command.command)
        .unwrap_or_else(|| json!({ "activity": activity }));
    if let Some(object) = command_payload.as_object_mut() {
        if command_type == WorkflowCommandType::EnqueueActivity && !object.contains_key("activity")
        {
            object.insert("activity".to_string(), json!(activity));
        }
        object.insert("retry_attempt".to_string(), json!(next_attempt));
        object.insert(
            "max_failed_activity_retries".to_string(),
            json!(retry_limit),
        );
        object.insert(
            "previous_command_id".to_string(),
            optional_json_string(event_field_string(event, "command_id")),
        );
        object.insert(
            "previous_runtime_job_id".to_string(),
            optional_json_string(event_field_string(event, "runtime_job_id")),
        );
        object.insert("previous_error".to_string(), json!(reason));
        if let Some(error_kind) = error_kind {
            object.insert("previous_error_kind".to_string(), json!(error_kind));
        }
        if let Some(schedule) = retry_schedule {
            object.insert("retry_delay_secs".to_string(), json!(schedule.delay_secs));
            object.insert(
                "retry_not_before".to_string(),
                json!(schedule.not_before.to_rfc3339()),
            );
        }
    } else {
        command_payload = json!({
            "activity": activity,
            "retry_attempt": next_attempt,
            "max_failed_activity_retries": retry_limit,
            "previous_command_id": event_field_string(event, "command_id"),
            "previous_runtime_job_id": event_field_string(event, "runtime_job_id"),
            "previous_error": reason,
        });
        if let Some(error_kind) = error_kind {
            if let Some(object) = command_payload.as_object_mut() {
                object.insert("previous_error_kind".to_string(), json!(error_kind));
            }
        }
        if let Some(schedule) = retry_schedule {
            if let Some(object) = command_payload.as_object_mut() {
                object.insert("retry_delay_secs".to_string(), json!(schedule.delay_secs));
                object.insert(
                    "retry_not_before".to_string(),
                    json!(schedule.not_before.to_rfc3339()),
                );
            }
        }
    }
    WorkflowCommand::new(
        command_type,
        format!(
            "runtime-completion:{}:retry:{}:{}",
            event.id, activity, next_attempt
        ),
        command_payload,
    )
}

fn supports_same_state_activity_retry(definition_id: &str, state: &str) -> bool {
    matches!(
        (definition_id, state),
        (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "planning"
                | "implementing"
                | "local_review_gate"
                | "awaiting_feedback"
                | "addressing_feedback"
        ) | (PR_FEEDBACK_DEFINITION_ID, "inspecting")
            | (PROMPT_TASK_DEFINITION_ID, "implementing")
            | (QUALITY_GATE_DEFINITION_ID, "checking")
    )
}
