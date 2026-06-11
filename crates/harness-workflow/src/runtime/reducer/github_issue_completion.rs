use super::support::{event_field_string, non_empty_json_string, runtime_completion_evidence};
use super::{
    GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL,
    ISSUE_STATE_ARTIFACT,
};
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use serde_json::{json, Value};

pub(super) fn issue_implementation_missing_result_decision(
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

    let reason = "implement_issue succeeded without a pull_request artifact, closed-issue evidence, or another validated terminal signal";
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "block_missing_implementation_result",
            "blocked",
            reason,
        )
        .with_command(WorkflowCommand::mark_blocked(
            reason,
            format!(
                "runtime-completion:{}:missing-implementation:block",
                event.id
            ),
        ))
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::RequestOperatorAttention,
            format!(
                "runtime-completion:{}:missing-implementation:operator",
                event.id
            ),
            json!({
                "reason": reason,
                "activity": result.activity,
                "runtime_job_id": event_field_string(event, "runtime_job_id"),
            }),
        ))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

pub(super) fn github_issue_closed_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || !github_issue_state_can_finish_closed(instance.state.as_str())
    {
        return None;
    }

    let closed_issue = closed_issue_evidence_from_activity_result(result)?;
    let reason = format!(
        "{} reported structured evidence that the GitHub issue is already closed",
        result.activity
    );
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "finish_closed_issue",
            "done",
            &reason,
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::MarkDone,
            format!("runtime-completion:{}:closed-issue:done", event.id),
            json!({
                "reason": reason,
                "activity": result.activity,
                "runtime_job_id": event_field_string(event, "runtime_job_id"),
                "closed_issue_evidence": closed_issue.payload,
            }),
        ))
        .with_evidence(WorkflowEvidence::new("closed_issue", closed_issue.summary))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn github_issue_state_can_finish_closed(state: &str) -> bool {
    matches!(
        state,
        "implementing"
            | "pr_open"
            | "awaiting_feedback"
            | "addressing_feedback"
            | "quality_gate_pending"
            | "ready_to_merge"
    )
}

pub(super) fn bind_pr_from_activity_result(
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

#[derive(Debug, Clone)]
pub(super) struct ClosedIssueEvidence {
    summary: String,
    payload: Value,
}

pub(super) fn closed_issue_evidence_from_activity_result(
    result: &ActivityResult,
) -> Option<ClosedIssueEvidence> {
    result
        .signals
        .iter()
        .find_map(|signal| match signal.signal_type.as_str() {
            ISSUE_CLOSED_SIGNAL | ISSUE_ALREADY_RESOLVED_SIGNAL => {
                closed_issue_evidence_from_value(&signal.signal, &signal.signal_type)
            }
            _ => None,
        })
        .or_else(|| {
            result
                .artifacts
                .iter()
                .filter(|artifact| artifact.artifact_type == ISSUE_STATE_ARTIFACT)
                .find_map(|artifact| {
                    if issue_state_is_closed(&artifact.artifact) {
                        closed_issue_evidence_from_value(&artifact.artifact, ISSUE_STATE_ARTIFACT)
                    } else {
                        None
                    }
                })
        })
}

pub(super) fn closed_issue_evidence_from_activity_result_value(
    value: &Value,
) -> Option<ClosedIssueEvidence> {
    value
        .get("signals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|signal| {
            let signal_type = signal.get("signal_type").and_then(non_empty_json_string)?;
            match signal_type.as_str() {
                ISSUE_CLOSED_SIGNAL | ISSUE_ALREADY_RESOLVED_SIGNAL => signal
                    .get("signal")
                    .and_then(|payload| closed_issue_evidence_from_value(payload, &signal_type)),
                _ => None,
            }
        })
        .or_else(|| {
            value
                .get("artifacts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|artifact| {
                    artifact.get("artifact_type").and_then(Value::as_str)
                        == Some(ISSUE_STATE_ARTIFACT)
                })
                .find_map(|artifact| {
                    artifact
                        .get("artifact")
                        .filter(|payload| issue_state_is_closed(payload))
                        .and_then(|payload| {
                            closed_issue_evidence_from_value(payload, ISSUE_STATE_ARTIFACT)
                        })
                })
        })
}

pub(super) fn closed_issue_evidence_from_value(
    value: &Value,
    source: &str,
) -> Option<ClosedIssueEvidence> {
    let issue_number = value.get("issue_number").and_then(Value::as_u64);
    let issue_url = value
        .get("issue_url")
        .or_else(|| value.get("html_url"))
        .or_else(|| value.get("url"))
        .and_then(non_empty_json_string);
    let state = value.get("state").and_then(non_empty_json_string);
    let closed = issue_state_is_closed(value);

    if !closed || (issue_number.is_none() && issue_url.is_none()) {
        return None;
    }

    let mut facts = vec![format!("source={source}")];
    if let Some(issue_number) = issue_number {
        facts.push(format!("issue_number={issue_number}"));
    }
    if let Some(issue_url) = issue_url.clone() {
        facts.push(format!("issue_url={issue_url}"));
    }
    if let Some(state) = state.clone() {
        facts.push(format!("state={state}"));
    }
    if closed {
        facts.push("closed=true".to_string());
    }

    Some(ClosedIssueEvidence {
        summary: facts.join(" "),
        payload: json!({
            "source": source,
            "issue_number": issue_number,
            "issue_url": issue_url,
            "state": state,
            "closed": closed,
        }),
    })
}

fn issue_state_is_closed(value: &Value) -> bool {
    value
        .get("closed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| {
                state.trim().eq_ignore_ascii_case("closed")
                    || state.trim().eq_ignore_ascii_case("resolved")
            })
        || value
            .get("is_closed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}
