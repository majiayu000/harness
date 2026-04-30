use super::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use serde_json::json;

pub const RUNTIME_JOB_COMPLETED_EVENT: &str = "RuntimeJobCompleted";
pub const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";

pub fn reduce_runtime_job_completed(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecision>> {
    if event.event_type != RUNTIME_JOB_COMPLETED_EVENT {
        return Ok(None);
    }

    let result: ActivityResult =
        serde_json::from_value(event.event.get("activity_result").cloned().ok_or_else(|| {
            anyhow::anyhow!("RuntimeJobCompleted event missing activity_result")
        })?)?;

    let decision = match result.status {
        ActivityStatus::Succeeded => reduce_success(instance, event, &result),
        ActivityStatus::Blocked => Some(runtime_blocked_decision(instance, event, &result)),
        ActivityStatus::Failed => Some(runtime_failed_decision(instance, event, &result)),
        ActivityStatus::Cancelled => Some(runtime_cancelled_decision(instance, event, &result)),
    };
    Ok(decision)
}

fn reduce_success(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    let (next_state, decision, reason) = match (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) {
        (GITHUB_ISSUE_PR_DEFINITION_ID, "replanning", "replan_issue") => (
            "implementing",
            "resume_implementation_after_replan",
            "replan activity completed; implementation can continue",
        ),
        (GITHUB_ISSUE_PR_DEFINITION_ID, "addressing_feedback", "address_pr_feedback") => (
            "awaiting_feedback",
            "await_feedback_after_rework",
            "PR feedback rework activity completed; wait for fresh feedback",
        ),
        _ => return None,
    };

    Some(
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, reason)
            .with_evidence(runtime_completion_evidence(event, result))
            .high_confidence(),
    )
}

fn runtime_blocked_decision(
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
    .with_command(WorkflowCommand::mark_blocked(
        &reason,
        format!("runtime-completion:{}:blocked", event.id),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

fn runtime_failed_decision(
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
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("runtime-completion:{}:failed", event.id),
        json!({ "reason": reason }),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

fn runtime_cancelled_decision(
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

fn runtime_completion_evidence(event: &WorkflowEvent, result: &ActivityResult) -> WorkflowEvidence {
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
