use super::model::{
    ActivityErrorKind, ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType,
    WorkflowDecision, WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use super::pr_feedback::{build_pr_feedback_decision, PrFeedbackDecisionInput, PrFeedbackOutcome};
use super::quality_gate::{QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID};
use super::repo_backlog::{REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY};
use super::validator::{DecisionValidator, ValidationContext};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

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
        ActivityStatus::Failed => Some(
            retry_failed_activity_decision(instance, event, &result)
                .unwrap_or_else(|| runtime_failed_decision(instance, event, &result)),
        ),
        ActivityStatus::Cancelled => Some(runtime_cancelled_decision(instance, event, &result)),
    };
    Ok(decision)
}

fn reduce_success(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    let structured_decision = workflow_decision_from_activity_result(instance, event, result);
    if let Some(decision) = structured_decision
        .as_ref()
        .filter(|decision| structured_decision_validates(instance, event, decision))
        .cloned()
    {
        return Some(decision);
    }

    if let Some(decision) = bind_pr_from_activity_result(instance, event, result) {
        return Some(decision);
    }

    if let Some(decision) = pr_feedback_sweep_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) = repo_backlog_poll_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if repo_backlog_child_dispatch_still_active(instance, event) {
        return None;
    }

    if let Some(decision) = structured_decision {
        return Some(decision);
    }

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
        (REPO_BACKLOG_DEFINITION_ID, "dispatching", _)
            if event_command_type(event) == Some("start_child_workflow") =>
        {
            (
                "idle",
                "finish_issue_workflow_dispatch",
                "repo backlog child workflow dispatch completed",
            )
        }
        (REPO_BACKLOG_DEFINITION_ID, "reconciling", "mark_bound_issue_done") => (
            "idle",
            "finish_bound_issue_reconciliation",
            "bound issue reconciliation activity completed",
        ),
        (REPO_BACKLOG_DEFINITION_ID, "reconciling", "recover_issue_workflow") => (
            "idle",
            "finish_issue_workflow_recovery",
            "issue workflow recovery activity completed",
        ),
        (REPO_BACKLOG_DEFINITION_ID, "scanning", REPO_BACKLOG_POLL_ACTIVITY) => (
            "idle",
            "finish_repo_backlog_scan",
            "repo backlog scan completed without new child workflow commands",
        ),
        (QUALITY_GATE_DEFINITION_ID, "checking", QUALITY_GATE_ACTIVITY) => (
            "passed",
            "quality_passed",
            "quality gate activity completed successfully",
        ),
        _ => return None,
    };

    Some(
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, reason)
            .with_evidence(runtime_completion_evidence(event, result))
            .high_confidence(),
    )
}

fn workflow_decision_from_activity_result(
    _instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "workflow_decision")
        .find_map(|artifact| {
            serde_json::from_value::<WorkflowDecision>(artifact.artifact.clone()).ok()
        })
        .map(|decision| decision.with_evidence(runtime_completion_evidence(event, result)))
}

fn repo_backlog_poll_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        REPO_BACKLOG_DEFINITION_ID,
        "scanning",
        REPO_BACKLOG_POLL_ACTIVITY,
    ) {
        return None;
    }

    let parent_repo = optional_data_string(instance, "repo");
    let discovered = result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == "IssueDiscovered")
        .filter_map(|signal| {
            let issue_number = signal.signal.get("issue_number").and_then(json_value_u64)?;
            let repo = signal
                .signal
                .get("repo")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| parent_repo.clone());
            let issue_url = signal
                .signal
                .get("issue_url")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned);
            let title = signal
                .signal
                .get("title")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned);
            let labels = signal
                .signal
                .get("labels")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some((issue_number, repo, issue_url, title, labels))
        })
        .collect::<Vec<_>>();

    if discovered.is_empty() {
        return None;
    }

    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "start_issue_workflows_from_scan",
        "dispatching",
        format!(
            "repo backlog scan discovered {} open issue(s) without runtime workflows",
            discovered.len()
        ),
    )
    .with_evidence(runtime_completion_evidence(event, result));

    for (issue_number, repo, issue_url, title, labels) in discovered {
        let repo_key = repo.as_deref().unwrap_or("<none>");
        decision = decision
            .with_command(WorkflowCommand::new(
                WorkflowCommandType::StartChildWorkflow,
                format!("repo-backlog-scan:{repo_key}:issue:{issue_number}:start"),
                json!({
                    "definition_id": "github_issue_pr",
                    "subject_key": format!("issue:{issue_number}"),
                    "repo": repo,
                    "issue_number": issue_number,
                    "issue_url": issue_url.clone(),
                    "title": title,
                    "labels": labels,
                    "source": "github",
                    "external_id": issue_number.to_string(),
                    "auto_submit": true,
                }),
            ))
            .with_evidence(WorkflowEvidence::new(
                "github_issue",
                match issue_url {
                    Some(url) => format!("repo={repo_key} issue={issue_number} url={url}"),
                    None => format!("repo={repo_key} issue={issue_number}"),
                },
            ));
    }

    Some(decision.high_confidence())
}

fn structured_decision_validates(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    decision: &WorkflowDecision,
) -> bool {
    let validator = match instance.definition_id.as_str() {
        GITHUB_ISSUE_PR_DEFINITION_ID => DecisionValidator::github_issue_pr(),
        QUALITY_GATE_DEFINITION_ID => DecisionValidator::quality_gate(),
        REPO_BACKLOG_DEFINITION_ID => DecisionValidator::repo_backlog(),
        _ => return true,
    };
    validator
        .validate(
            instance,
            decision,
            &ValidationContext::new(event.source.as_str(), Utc::now()),
        )
        .is_ok()
}

fn repo_backlog_child_dispatch_still_active(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
) -> bool {
    instance.definition_id == REPO_BACKLOG_DEFINITION_ID
        && instance.state == "dispatching"
        && event_command_type(event) == Some("start_child_workflow")
        && event
            .event
            .get("active_start_child_workflow_commands")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            > 0
}

fn bind_pr_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        "implement_issue",
    ) {
        return None;
    }
    let (pr_number, pr_url) = pull_request_artifact(result)?;
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "bind_pr",
            "pr_open",
            "implementation activity returned a structured pull request artifact",
        )
        .with_command(WorkflowCommand::bind_pr(
            pr_number,
            pr_url.clone(),
            format!("runtime-completion:{}:bind-pr:{pr_number}", event.id),
        ))
        .with_evidence(WorkflowEvidence::new("pull_request", pr_url))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn pull_request_artifact(result: &ActivityResult) -> Option<(u64, String)> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "pull_request")
        .find_map(|artifact| {
            let pr_number = artifact.artifact.get("pr_number")?.as_u64()?;
            let pr_url = artifact
                .artifact
                .get("pr_url")?
                .as_str()
                .filter(|value| !value.trim().is_empty())?
                .to_string();
            Some((pr_number, pr_url))
        })
}

fn pr_feedback_sweep_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || result.activity != "sweep_pr_feedback"
    {
        return None;
    }
    if !matches!(
        instance.state.as_str(),
        "pr_open" | "awaiting_feedback" | "addressing_feedback"
    ) {
        return None;
    }
    let outcome = pr_feedback_outcome_from_signals(result)?;
    let pr_number = result_signal_u64(result, "pr_number").or_else(|| {
        instance
            .data
            .get("pr_number")
            .and_then(|value| value.as_u64())
    })?;
    let pr_url =
        result_signal_string(result, "pr_url").or_else(|| optional_data_string(instance, "pr_url"));
    let task_id = event_field_string(event, "runtime_job_id")
        .or_else(|| optional_data_string(instance, "task_id"))
        .unwrap_or_else(|| event.id.clone());
    Some(
        build_pr_feedback_decision(
            instance,
            PrFeedbackDecisionInput {
                task_id: &task_id,
                pr_number,
                pr_url: pr_url.as_deref(),
                outcome,
                summary: result.summary.as_str(),
            },
        )
        .decision
        .with_evidence(runtime_completion_evidence(event, result)),
    )
}

fn pr_feedback_outcome_from_signals(result: &ActivityResult) -> Option<PrFeedbackOutcome> {
    if has_signal(result, "FeedbackFound")
        || has_signal(result, "ChangesRequested")
        || has_signal(result, "ChecksFailed")
    {
        return Some(PrFeedbackOutcome::BlockingFeedback);
    }
    if has_signal(result, "PrReadyToMerge") {
        return Some(PrFeedbackOutcome::ReadyToMerge);
    }
    if has_signal(result, "NoFeedbackFound") {
        return Some(PrFeedbackOutcome::NoActionableFeedback);
    }
    None
}

fn has_signal(result: &ActivityResult, signal_type: &str) -> bool {
    result
        .signals
        .iter()
        .any(|signal| signal.signal_type == signal_type)
}

fn result_signal_u64(result: &ActivityResult, field: &str) -> Option<u64> {
    result
        .signals
        .iter()
        .find_map(|signal| signal.signal.get(field).and_then(json_value_u64))
}

fn result_signal_string(result: &ActivityResult, field: &str) -> Option<String> {
    result.signals.iter().find_map(|signal| {
        signal
            .signal
            .get(field)
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    })
}

fn optional_data_string(instance: &WorkflowInstance, field: &str) -> Option<String> {
    instance
        .data
        .get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn json_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
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
    let mut command_payload = json!({ "reason": reason.as_str() });
    if let Some(error_kind) = result.error_kind {
        if let Some(object) = command_payload.as_object_mut() {
            object.insert("error_kind".to_string(), json!(error_kind));
        }
    }
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
        command_payload,
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

fn retry_failed_activity_decision(
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
    let retry_schedule = failed_activity_retry_schedule(instance, &activity, next_attempt);
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
        not_before: Utc::now() + Duration::seconds(delay_secs as i64),
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
            "implementing" | "awaiting_feedback" | "addressing_feedback"
        ) | (REPO_BACKLOG_DEFINITION_ID, "dispatching" | "reconciling")
            | (QUALITY_GATE_DEFINITION_ID, "checking")
    )
}

fn event_field_string(event: &WorkflowEvent, field: &str) -> Option<String> {
    event
        .event
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn event_command_type(event: &WorkflowEvent) -> Option<&str> {
    event
        .event
        .get("command")
        .and_then(|command| command.get("command_type"))
        .and_then(|value| value.as_str())
}

fn event_workflow_command(event: &WorkflowEvent) -> Option<WorkflowCommand> {
    event
        .event
        .get("command")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn optional_json_string(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
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
