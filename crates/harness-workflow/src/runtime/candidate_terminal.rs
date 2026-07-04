use super::candidate_selection::{CandidateEvidence, CandidateOutcome};
use super::model::{
    ActivityErrorKind, ActivityResult, ActivityStatus, WorkflowCommand, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use serde_json::Value;

const IMPLEMENT_ISSUE_ACTIVITY: &str = "implement_issue";
const DEFERRED_SUBMISSION_MODE: &str = "deferred";

pub(super) fn deferred_candidate_terminal_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> Option<anyhow::Result<WorkflowDecision>> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        IMPLEMENT_ISSUE_ACTIVITY,
    ) {
        return None;
    }
    if command
        .command
        .get("submission_mode")
        .and_then(Value::as_str)
        != Some(DEFERRED_SUBMISSION_MODE)
    {
        return None;
    }
    if !matches!(
        result.status,
        ActivityStatus::Failed | ActivityStatus::Cancelled
    ) {
        return None;
    }
    Some(deferred_candidate_terminal_decision_inner(
        instance, event, result, command,
    ))
}

fn deferred_candidate_terminal_decision_inner(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> anyhow::Result<WorkflowDecision> {
    let candidate = DeferredCandidateMetadata::from_command(command)?;
    let evidence = terminal_candidate_evidence(&candidate, event, result);
    let outcome_label = candidate_outcome_label(evidence.outcome);
    let reason = format!(
        "deferred candidate {} reached terminal outcome `{outcome_label}`; recording evidence for candidate selection",
        candidate.candidate_id
    );

    Ok(WorkflowDecision::new(
        &instance.id,
        &instance.state,
        format!("record_deferred_candidate_{outcome_label}"),
        &instance.state,
        reason,
    )
    .with_evidence(WorkflowEvidence::new(
        "candidate_terminal",
        candidate_terminal_summary(&candidate, &evidence, result),
    ))
    .high_confidence())
}

fn terminal_candidate_evidence(
    candidate: &DeferredCandidateMetadata,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> CandidateEvidence {
    let mut evidence = match candidate_outcome(result) {
        CandidateOutcome::Failed => CandidateEvidence::failed(candidate.candidate_id.clone()),
        CandidateOutcome::Stalled => CandidateEvidence::stalled(candidate.candidate_id.clone()),
        CandidateOutcome::Cancelled => CandidateEvidence::cancelled(candidate.candidate_id.clone()),
        CandidateOutcome::Succeeded => CandidateEvidence::succeeded(candidate.candidate_id.clone()),
    }
    .with_completed_at_epoch_ms(event.created_at.timestamp_millis());
    if let Some(runtime_job_id) = event
        .event
        .get("runtime_job_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        evidence = evidence.with_runtime_job_id(runtime_job_id);
    }
    evidence
}

fn candidate_outcome(result: &ActivityResult) -> CandidateOutcome {
    match result.status {
        ActivityStatus::Failed if result.error_kind == Some(ActivityErrorKind::Timeout) => {
            CandidateOutcome::Stalled
        }
        ActivityStatus::Failed => CandidateOutcome::Failed,
        ActivityStatus::Cancelled => CandidateOutcome::Cancelled,
        ActivityStatus::Blocked => CandidateOutcome::Failed,
        ActivityStatus::Succeeded => CandidateOutcome::Succeeded,
    }
}

fn candidate_terminal_summary(
    candidate: &DeferredCandidateMetadata,
    evidence: &CandidateEvidence,
    result: &ActivityResult,
) -> String {
    let runtime_job_id = evidence.runtime_job_id.as_deref().unwrap_or("<unknown>");
    let error_kind = result
        .error_kind
        .map(|kind| {
            serde_json::to_value(kind)
                .unwrap_or(Value::Null)
                .to_string()
        })
        .unwrap_or_else(|| "null".to_string());
    format!(
        "candidate_group_id={} candidate_id={} outcome={} runtime_job_id={} error_kind={} terminal=true",
        candidate.candidate_group_id,
        candidate.candidate_id,
        candidate_outcome_label(evidence.outcome),
        runtime_job_id,
        error_kind
    )
}

fn candidate_outcome_label(outcome: CandidateOutcome) -> &'static str {
    match outcome {
        CandidateOutcome::Succeeded => "succeeded",
        CandidateOutcome::Failed => "failed",
        CandidateOutcome::Stalled => "stalled",
        CandidateOutcome::Cancelled => "cancelled",
    }
}

struct DeferredCandidateMetadata {
    candidate_group_id: String,
    candidate_id: String,
}

impl DeferredCandidateMetadata {
    fn from_command(command: &WorkflowCommand) -> anyhow::Result<Self> {
        let candidate = command.command.get("candidate").ok_or_else(|| {
            anyhow::anyhow!("deferred implement_issue command is missing candidate metadata")
        })?;
        let candidate_group_id =
            required_candidate_metadata_string(candidate, "candidate_group_id")?;
        let candidate_id = required_candidate_metadata_string(candidate, "candidate_id")?;
        Ok(Self {
            candidate_group_id,
            candidate_id,
        })
    }
}

fn required_candidate_metadata_string(value: &Value, field: &str) -> anyhow::Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("candidate metadata is missing {field}"))
}
