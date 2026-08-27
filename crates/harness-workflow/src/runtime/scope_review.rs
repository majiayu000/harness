use super::completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT;
use super::{
    ActivityResult, ActivityStatus, RuntimeJob, WorkflowCommand, WorkflowCommandType,
    WorkflowDecision, WorkflowDefinitionRegistry, WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::{json, Value};

pub const CHANGE_SCOPE_REVIEW_ACTIVITY: &str = "classify_change_scope";
pub const PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD: &str =
    "pinned_change_scope_classifier_policy";

pub(crate) fn has_server_classifier_assessment(result: &ActivityResult) -> bool {
    validated_server_classifier_assessment(result).is_some()
}

pub(crate) fn validated_server_classifier_assessment(result: &ActivityResult) -> Option<&Value> {
    let assessments = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == ARTIFACT_CLASSIFIER_ASSESSMENT)
        .collect::<Vec<_>>();
    let [artifact] = assessments.as_slice() else {
        return None;
    };
    let assessment = &artifact.artifact;
    if assessment.get("schema").and_then(Value::as_str)
        != Some("harness.runtime.classifier_assessment.v1")
    {
        return None;
    }
    let attestation = assessment.get("attestation")?;
    for field in ["runtime_job_id", "runtime_profile"] {
        if !nonempty_string(attestation.get(field)) {
            return None;
        }
    }
    match result.status {
        ActivityStatus::Succeeded => {
            if !nonempty_string(assessment.get("verdict"))
                || !nonempty_string(assessment.get("rationale"))
                || !nonempty_string(attestation.get("requested_model"))
                || !nonempty_string(attestation.get("model"))
                || !nonempty_string(attestation.get("prompt_packet_digest"))
                || !nonempty_string(attestation.get("policy_sha256"))
            {
                return None;
            }
            let evidence_refs = assessment.get("evidence_refs")?.as_array()?;
            if evidence_refs
                .iter()
                .any(|value| !nonempty_string(Some(value)))
            {
                return None;
            }
            let [signal] = result.signals.as_slice() else {
                return None;
            };
            if signal.signal_type != assessment.get("verdict")?.as_str()?
                || signal.signal != *assessment
            {
                return None;
            }
        }
        _ => {
            if !nonempty_string(assessment.get("outcome")) || !result.signals.is_empty() {
                return None;
            }
        }
    }
    Some(assessment)
}

fn nonempty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

pub(crate) fn runtime_job_requires_local_server(
    registry: &WorkflowDefinitionRegistry,
    workflow: &WorkflowInstance,
    job: &RuntimeJob,
) -> Result<bool, super::DeclarativeDefinitionPinError> {
    let Some(activity) = job.input.get("activity").and_then(Value::as_str) else {
        return Ok(false);
    };
    if workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID && activity == "merge_pr" {
        return Ok(true);
    }
    registry.instance_has_classifier_activity(workflow, activity)
}

pub(crate) fn enqueue_pr_scope_review(
    dedupe_key: impl Into<String>,
    pr_number: u64,
    pr_url: &str,
    issue_plan: Value,
) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        dedupe_key,
        json!({
            "activity": CHANGE_SCOPE_REVIEW_ACTIVITY,
            "scope_facts": {
                "issue_plan": issue_plan,
                "pull_request": {
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                }
            }
        }),
    )
}

pub(crate) fn enqueue_candidate_pr_scope_review(
    event_id: &str,
    pr_number: u64,
    pr_url: &str,
) -> WorkflowCommand {
    enqueue_pr_scope_review(
        format!("candidate-promotion:{event_id}:classify-pr-scope:{pr_number}"),
        pr_number,
        pr_url,
        Value::Null,
    )
}

pub(crate) fn finish_candidate_pr_promotion(
    workflow: &WorkflowInstance,
    mut decision: WorkflowDecision,
    event_id: &str,
    pr_number: u64,
    pr_url: &str,
) -> WorkflowDecision {
    if workflow.definition_version == 1 {
        decision.next_state = "pr_open".to_string();
        return decision;
    }
    decision.with_command(enqueue_candidate_pr_scope_review(
        event_id, pr_number, pr_url,
    ))
}
