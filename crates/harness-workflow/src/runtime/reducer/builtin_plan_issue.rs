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
use crate::runtime::submission::append_candidate_commands;
use crate::runtime::{candidate_fanout_from_value, SubmissionMode, CHANGE_SCOPE_REVIEW_ACTIVITY};
use serde_json::{json, Value};

pub(super) fn issue_plan_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    let is_plan_activity = matches!(
        (
            instance.definition_id.as_str(),
            instance.state.as_str(),
            result.activity.as_str(),
        ),
        (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "planning",
            ISSUE_PLAN_ACTIVITY
        ) | (GITHUB_ISSUE_PR_DEFINITION_ID, "replanning", "replan_issue")
    );
    if !is_plan_activity {
        return None;
    }

    let Some(issue_plan) = issue_plan_payload(result) else {
        if legacy_replan_completion_requires_retry(instance, event, result) {
            let command_id =
                event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
            return Some(
                WorkflowDecision::new(
                    &instance.id,
                    &instance.state,
                    "retry_replan_with_structured_contract",
                    "replanning",
                    "legacy replan completion used the retired workflow_decision contract; rerun once with the structured issue-plan contract",
                )
                .with_command(WorkflowCommand::new(
                    WorkflowCommandType::EnqueueActivity,
                    format!("legacy-replan-contract:{}:{command_id}", instance.id),
                    json!({
                        "activity": "replan_issue",
                        "structured_issue_plan_contract": true,
                    }),
                ))
                .with_evidence(runtime_completion_evidence(event, result))
                .high_confidence(),
            );
        }
        let reason =
            "plan_issue succeeded without a valid issue_plan artifact or IssuePlanReady signal";
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, reason,
        ));
    };

    let plan_summary =
        issue_plan_summary(&issue_plan).unwrap_or_else(|| result.summary.trim().to_string());
    let completion_command_id =
        event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
    let submission_mode = submission_mode_from_event(event);
    if instance.definition_version == 1 {
        let candidate_fanout = match candidate_fanout_from_value(&instance.data) {
            Ok(candidate_fanout) => candidate_fanout,
            Err(error) => {
                let reason = format!(
                    "runtime issue workflow has invalid candidate_fanout metadata: {error}"
                );
                return Some(invalid_agent_output_blocked_decision(
                    instance, event, result, &reason,
                ));
            }
        };
        let command = WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            format!(
                "issue-plan:{}:implement:{completion_command_id}",
                instance.id
            ),
            json!({
                "activity": "implement_issue",
                "issue_plan": issue_plan,
                "issue_plan_summary": plan_summary,
                "submission_mode": submission_mode.as_str(),
            }),
        );
        let decision = WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "start_implementation_after_issue_plan",
            "implementing",
            "issue planning activity produced a structured plan",
        )
        .with_evidence(WorkflowEvidence::new("issue_plan", plan_summary))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence();
        return Some(append_candidate_commands(
            decision,
            command,
            candidate_fanout.as_ref(),
        ));
    }
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "issue-plan:{}:classify-scope:{completion_command_id}",
            instance.id
        ),
        json!({
            "activity": CHANGE_SCOPE_REVIEW_ACTIVITY,
            "scope_facts": {
                "issue_plan": issue_plan,
                "issue_plan_summary": plan_summary,
            },
            "classifier_continuations": {
                "implementing": {
                    "activity": "implement_issue",
                    "apply_candidate_fanout": true,
                    "issue_plan": issue_plan,
                    "issue_plan_summary": plan_summary,
                    "submission_mode": submission_mode.as_str(),
                }
            }
        }),
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "review_issue_plan_scope",
        "plan_scope_review",
        "issue planning activity produced a structured plan for independent scope review",
    )
    .with_evidence(WorkflowEvidence::new("issue_plan", plan_summary))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence();
    Some(decision.with_command(command))
}

fn legacy_replan_completion_requires_retry(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> bool {
    instance.definition_version != 1
        && instance.state == "replanning"
        && result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "workflow_decision")
        && event
            .event
            .pointer("/command/command/structured_issue_plan_contract")
            .and_then(Value::as_bool)
            != Some(true)
}

fn submission_mode_from_event(event: &WorkflowEvent) -> SubmissionMode {
    event
        .event
        .pointer("/command/command/submission_mode")
        .and_then(Value::as_str)
        .and_then(SubmissionMode::from_wire_value)
        .unwrap_or_default()
}

fn issue_plan_payload(result: &ActivityResult) -> Option<Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == ISSUE_PLAN_ARTIFACT)
        .and_then(|artifact| valid_issue_plan_payload(&artifact.artifact))
        .or_else(|| {
            result
                .signals
                .iter()
                .find(|signal| signal.signal_type == ISSUE_PLAN_READY_SIGNAL)
                .and_then(|signal| valid_issue_plan_payload(&signal.signal))
        })
}

fn valid_issue_plan_payload(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    if object.is_empty()
        || issue_plan_summary(value).is_none()
        || !non_empty_string_field(value, "task_class")
        || !non_empty_string_array(value, "target_files")
        || !non_empty_string_array(value, "validation_plan")
        || !array_field_exists(value, "blockers")
    {
        return None;
    }
    Some(value.clone())
}

fn non_empty_string_field(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn non_empty_string_array(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_array)
        .is_some_and(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|text| !text.trim().is_empty()))
        })
}

fn array_field_exists(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_array).is_some()
}

fn issue_plan_summary(issue_plan: &Value) -> Option<String> {
    ["summary", "plan_summary", "title"]
        .iter()
        .find_map(|field| issue_plan.get(field).and_then(non_empty_json_string))
}
