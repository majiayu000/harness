mod github_issue_completion;
mod plan_issue_completion;
mod pr_feedback_completion;
mod quality_gate_completion;
mod runtime_failure;
mod support;

use self::github_issue_completion::{
    bind_pr_from_activity_result, closed_issue_evidence_from_activity_result,
    closed_issue_evidence_from_activity_result_value, closed_issue_evidence_from_value,
    github_issue_closed_decision, issue_implementation_missing_result_decision,
    merged_pr_from_activity_result, scope_too_large_decision,
};
use self::plan_issue_completion::issue_plan_decision_from_activity_result;
use self::pr_feedback_completion::{
    local_review_decision_from_activity_result,
    pr_feedback_blocking_signal_overrides_structured_ready,
    pr_feedback_child_decision_from_activity_result, pr_feedback_success_contract_error,
    pr_feedback_sweep_decision_from_activity_result,
};
use self::quality_gate_completion::{
    parent_quality_gate_pass_decision, quality_gate_activity_matches,
    quality_gate_success_contract_error, quality_gate_success_decision,
};
use self::runtime_failure::{
    retry_failed_activity_decision, runtime_blocked_decision, runtime_cancelled_decision,
    runtime_failed_decision,
};
use self::support::{
    event_command_type, event_field_string, invalid_agent_output_blocked_decision,
    runtime_completion_evidence,
};
use super::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowInstance,
};
use super::pr_feedback::PR_FEEDBACK_DEFINITION_ID;
use super::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
use super::quality_gate::QUALITY_GATE_DEFINITION_ID;
use super::validator::{DecisionValidator, ValidationContext};
use serde_json::{json, Value};

pub const RUNTIME_JOB_COMPLETED_EVENT: &str = "RuntimeJobCompleted";
pub const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
pub const ISSUE_CLOSED_SIGNAL: &str = "IssueClosed";
pub const ISSUE_ALREADY_RESOLVED_SIGNAL: &str = "IssueAlreadyResolved";
pub const ISSUE_STATE_ARTIFACT: &str = "issue_state";
pub const SCOPE_TOO_LARGE_SIGNAL: &str = "SCOPE_TOO_LARGE";

pub fn activity_result_has_closed_issue_evidence(result: &ActivityResult) -> bool {
    closed_issue_evidence_from_activity_result(result).is_some()
}

pub fn activity_result_value_has_closed_issue_evidence(value: &Value) -> bool {
    closed_issue_evidence_from_activity_result_value(value).is_some()
}

pub fn value_has_closed_issue_evidence(value: &Value) -> bool {
    closed_issue_evidence_from_value(value, ISSUE_STATE_ARTIFACT).is_some()
}

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
        ActivityStatus::Blocked => github_issue_closed_decision(instance, event, &result)
            .or_else(|| scope_too_large_decision(instance, event, &result))
            .or_else(|| Some(runtime_blocked_decision(instance, event, &result))),
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
    let structured_decision = workflow_decision_from_activity_result(event, result);
    if let Some(decision) = github_issue_closed_decision(instance, event, result) {
        return Some(decision);
    }
    if let Some(decision) = issue_plan_decision_from_activity_result(instance, event, result) {
        return Some(decision);
    }
    if let Some(decision) = scope_too_large_decision(instance, event, result) {
        return Some(decision);
    }
    if let Some(reason) =
        pr_feedback_success_contract_error(instance, result, structured_decision.as_ref())
    {
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, &reason,
        ));
    }
    let pr_feedback_blocker_overrides_structured_ready =
        pr_feedback_blocking_signal_overrides_structured_ready(
            instance,
            result,
            structured_decision.as_ref(),
        );
    if let Some(decision) = structured_decision
        .as_ref()
        .filter(|_| !pr_feedback_blocker_overrides_structured_ready)
        .filter(|decision| structured_decision_validates(instance, event, result, decision))
        .cloned()
    {
        return Some(decision);
    }

    if let Some(decision) = parent_quality_gate_pass_decision(instance, event, result) {
        return Some(decision);
    }

    if quality_gate_activity_matches(instance, result) {
        if let Some(decision) = structured_decision.as_ref() {
            let reason = if let Some(contract_reason) = quality_gate_success_contract_error(result)
            {
                format!(
                    "runtime activity `{}` emitted workflow_decision `{}` for workflow `{}` in state `{}`, but {contract_reason}",
                    result.activity, decision.decision, instance.definition_id, instance.state
                )
            } else {
                format!(
                    "runtime activity `{}` emitted workflow_decision `{}` for workflow `{}` in state `{}`, but the decision to `{}` did not validate",
                    result.activity,
                    decision.decision,
                    instance.definition_id,
                    instance.state,
                    decision.next_state
                )
            };
            return Some(invalid_agent_output_blocked_decision(
                instance, event, result, &reason,
            ));
        }
        return quality_gate_success_decision(instance, event, result);
    }

    if prompt_task_activity_matches(instance, result) {
        if let Some(reason) = prompt_task_success_contract_error(result) {
            return Some(invalid_agent_output_blocked_decision(
                instance, event, result, reason,
            ));
        }
    }

    if let Some(decision) = bind_pr_from_activity_result(instance, event, result) {
        return Some(decision);
    }

    if let Some(decision) = merged_pr_from_activity_result(instance, event, result) {
        return Some(decision);
    }

    if let Some(decision) = pr_feedback_sweep_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) = local_review_decision_from_activity_result(instance, event, result) {
        return Some(decision);
    }
    if instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "local_review_gate"
        && result.activity == super::pr_feedback::LOCAL_REVIEW_ACTIVITY
    {
        let reason = "run_local_review succeeded without a LocalReviewPassed, LocalReviewChangesRequested, or LocalReviewBlocked signal";
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, reason,
        ));
    }

    if let Some(decision) = pr_feedback_child_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if stale_success_completion(instance, result) {
        return None;
    }

    if let Some(decision) = structured_decision.as_ref() {
        let reason = format!(
            "runtime activity `{}` emitted workflow_decision `{}` for workflow `{}` in state `{}`, but the decision to `{}` did not validate and no domain fallback was available",
            result.activity,
            decision.decision,
            instance.definition_id,
            instance.state,
            decision.next_state
        );
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, &reason,
        ));
    }

    if let Some(decision) = issue_implementation_missing_result_decision(instance, event, result) {
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
            "local_review_gate",
            "run_local_review_after_rework",
            "PR feedback rework activity completed; run local review before remote feedback",
        ),
        (QUALITY_GATE_DEFINITION_ID, "checking", super::quality_gate::QUALITY_GATE_ACTIVITY) => (
            "passed",
            "quality_passed",
            "quality gate activity completed successfully",
        ),
        (PROMPT_TASK_DEFINITION_ID, "implementing", PROMPT_TASK_IMPLEMENT_ACTIVITY) => (
            "done",
            "finish_prompt_task",
            "prompt implementation activity completed successfully",
        ),
        _ if known_success_without_decision(instance, event, result) => return None,
        _ => {
            let reason = format!(
                "runtime activity `{}` succeeded for workflow `{}` in state `{}`, but no reducer fallback was available",
                result.activity, instance.definition_id, instance.state
            );
            return Some(invalid_agent_output_blocked_decision(
                instance, event, result, &reason,
            ));
        }
    };

    let mut workflow_decision =
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, reason)
            .with_evidence(runtime_completion_evidence(event, result));
    if instance.definition_id == PROMPT_TASK_DEFINITION_ID
        && instance.state == "implementing"
        && result.activity == PROMPT_TASK_IMPLEMENT_ACTIVITY
        && next_state == "done"
    {
        workflow_decision = workflow_decision.with_command(WorkflowCommand::new(
            WorkflowCommandType::MarkDone,
            format!("prompt-task:{}:done", instance.id),
            json!({
                "activity": result.activity,
                "workflow_id": instance.id,
            }),
        ));
    }
    if instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "addressing_feedback"
        && result.activity == "address_pr_feedback"
        && next_state == "local_review_gate"
    {
        let completion_command_id =
            event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
        workflow_decision = workflow_decision.with_command(WorkflowCommand::enqueue_activity(
            super::pr_feedback::LOCAL_REVIEW_ACTIVITY,
            format!(
                "local-review:{}:after-rework:{completion_command_id}",
                instance.id
            ),
        ));
    }

    Some(workflow_decision.high_confidence())
}

fn known_success_without_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> bool {
    if event_command_type(event) == Some(WorkflowCommandType::StartChildWorkflow.as_str()) {
        return true;
    }

    if stale_success_completion(instance, result) {
        return true;
    }

    (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) == (
        PR_FEEDBACK_DEFINITION_ID,
        "inspecting",
        super::pr_feedback::PR_FEEDBACK_INSPECT_ACTIVITY,
    )
}

fn stale_success_completion(instance: &WorkflowInstance, result: &ActivityResult) -> bool {
    if instance.is_terminal() {
        return true;
    }

    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return false;
    }

    match result.activity.as_str() {
        "sweep_pr_feedback" | super::pr_feedback::PR_FEEDBACK_INSPECT_ACTIVITY => matches!(
            instance.state.as_str(),
            "addressing_feedback"
                | "local_review_gate"
                | "quality_gate_pending"
                | "ready_to_merge"
                | "blocked"
        ),
        super::pr_feedback::LOCAL_REVIEW_ACTIVITY => matches!(
            instance.state.as_str(),
            "awaiting_feedback"
                | "addressing_feedback"
                | "quality_gate_pending"
                | "ready_to_merge"
                | "blocked"
        ),
        _ => false,
    }
}

fn workflow_decision_from_activity_result(
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

fn structured_decision_validates(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision: &WorkflowDecision,
) -> bool {
    if instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "implementing"
        && result.activity == "implement_issue"
        && decision.next_state == "done"
        && closed_issue_evidence_from_activity_result(result).is_none()
    {
        return false;
    }
    if quality_gate_activity_matches(instance, result)
        && decision.next_state == "passed"
        && quality_gate_success_contract_error(result).is_some()
    {
        return false;
    }
    if prompt_task_activity_matches(instance, result)
        && decision.next_state == "done"
        && prompt_task_success_contract_error(result).is_some()
    {
        return false;
    }

    let validator = match instance.definition_id.as_str() {
        GITHUB_ISSUE_PR_DEFINITION_ID => DecisionValidator::github_issue_pr(),
        QUALITY_GATE_DEFINITION_ID => DecisionValidator::quality_gate(),
        PR_FEEDBACK_DEFINITION_ID => DecisionValidator::pr_feedback(),
        PROMPT_TASK_DEFINITION_ID => DecisionValidator::prompt_task(),
        _ => return true,
    };
    validator
        .validate(
            instance,
            decision,
            &ValidationContext::new(event.source.as_str(), event.created_at),
        )
        .is_ok()
}

fn prompt_task_activity_matches(instance: &WorkflowInstance, result: &ActivityResult) -> bool {
    (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) == (
        PROMPT_TASK_DEFINITION_ID,
        "implementing",
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
    )
}

fn prompt_task_success_contract_error(result: &ActivityResult) -> Option<&'static str> {
    if prompt_task_has_validation_evidence(result) {
        None
    } else {
        Some("implement_prompt succeeded without validation evidence")
    }
}

fn prompt_task_has_validation_evidence(result: &ActivityResult) -> bool {
    result
        .validation
        .iter()
        .any(|record| !record.command.trim().is_empty())
        || result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "validation_report")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{
        ActivityResult, WorkflowCommandType, WorkflowEvent, WorkflowInstance, WorkflowSubject,
    };
    use crate::runtime::validator::{DecisionValidator, ValidationContext};
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn prompt_task_success_without_validation_evidence_blocks() -> anyhow::Result<()> {
        let instance = WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-123"),
        );
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        );
        let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
            .with_payload(json!({
                "command_id": "command-1",
                "runtime_job_id": "job-1",
                "activity_result": result,
            }));

        let decision = reduce_runtime_job_completed(&instance, &event)?.ok_or_else(|| {
            anyhow::anyhow!("prompt success without validation evidence should block")
        })?;

        assert_eq!(decision.decision, "block_invalid_agent_output");
        assert_eq!(decision.next_state, "blocked");
        assert!(decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
        assert!(decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention));
        DecisionValidator::prompt_task().validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )?;
        Ok(())
    }
}
