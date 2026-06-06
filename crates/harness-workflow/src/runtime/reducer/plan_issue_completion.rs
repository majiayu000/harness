use super::support::{
    event_field_string, invalid_agent_output_blocked_decision, non_empty_json_string,
    runtime_completion_evidence,
};
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::plan_issue::{
    ISSUE_PLAN_ACTIVITY, ISSUE_PLAN_ARTIFACT, ISSUE_PLAN_READY_SIGNAL,
};
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use serde_json::{json, Value};

pub(super) fn issue_plan_decision_from_activity_result(
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
        "planning",
        ISSUE_PLAN_ACTIVITY,
    ) {
        return None;
    }

    let Some(issue_plan) = issue_plan_payload(result) else {
        let reason = "plan_issue succeeded without an issue_plan artifact or IssuePlanReady signal";
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, reason,
        ));
    };

    let plan_summary =
        issue_plan_summary(&issue_plan).unwrap_or_else(|| result.summary.trim().to_string());
    let completion_command_id =
        event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());

    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "start_implementation_after_issue_plan",
            "implementing",
            "issue planning activity produced a structured plan",
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            format!(
                "issue-plan:{}:implement:{completion_command_id}",
                instance.id
            ),
            json!({
                "activity": "implement_issue",
                "issue_plan": issue_plan,
                "issue_plan_summary": plan_summary,
            }),
        ))
        .with_evidence(WorkflowEvidence::new("issue_plan", plan_summary))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn issue_plan_payload(result: &ActivityResult) -> Option<Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == ISSUE_PLAN_ARTIFACT)
        .map(|artifact| artifact.artifact.clone())
        .or_else(|| {
            result
                .signals
                .iter()
                .find(|signal| signal.signal_type == ISSUE_PLAN_READY_SIGNAL)
                .map(|signal| signal.signal.clone())
        })
}

fn issue_plan_summary(issue_plan: &Value) -> Option<String> {
    ["summary", "plan_summary", "title"]
        .iter()
        .find_map(|field| issue_plan.get(field).and_then(non_empty_json_string))
}
