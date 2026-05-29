mod github_issue_completion;
mod pr_feedback_completion;
mod quality_gate_completion;
mod repo_backlog_candidates;
mod repo_backlog_completion;
mod runtime_failure;
mod support;

use self::github_issue_completion::{
    bind_pr_from_activity_result, closed_issue_evidence_from_activity_result,
    closed_issue_evidence_from_activity_result_value, closed_issue_evidence_from_value,
    github_issue_closed_decision, issue_implementation_missing_result_decision,
};
use self::pr_feedback_completion::{
    pr_feedback_child_decision_from_activity_result,
    pr_feedback_sweep_decision_from_activity_result,
};
use self::quality_gate_completion::{
    quality_gate_activity_matches, quality_gate_success_contract_error,
    quality_gate_success_decision,
};
use self::repo_backlog_completion::{
    repo_backlog_child_dispatch_still_active, repo_backlog_invalid_success_decision,
    repo_backlog_legacy_scan_dispatch_decision_from_activity_result,
    repo_backlog_poll_decision_from_activity_result,
    repo_backlog_sprint_plan_decision_from_activity_result,
};
use self::runtime_failure::{
    retry_failed_activity_decision, runtime_blocked_decision, runtime_cancelled_decision,
    runtime_failed_decision,
};
use self::support::{
    event_command_type, invalid_agent_output_blocked_decision, runtime_completion_evidence,
};
use super::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowInstance,
};
use super::pr_feedback::PR_FEEDBACK_DEFINITION_ID;
use super::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
use super::quality_gate::QUALITY_GATE_DEFINITION_ID;
use super::repo_backlog::{
    REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
};
use super::validator::{DecisionValidator, ValidationContext};
use chrono::Utc;
use serde_json::{json, Value};

pub const RUNTIME_JOB_COMPLETED_EVENT: &str = "RuntimeJobCompleted";
pub const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
pub const ISSUE_CLOSED_SIGNAL: &str = "IssueClosed";
pub const ISSUE_ALREADY_RESOLVED_SIGNAL: &str = "IssueAlreadyResolved";
pub const ISSUE_STATE_ARTIFACT: &str = "issue_state";

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
    if let Some(decision) = structured_decision
        .as_ref()
        .filter(|decision| structured_decision_validates(instance, event, result, decision))
        .cloned()
    {
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

    if let Some(decision) = github_issue_closed_decision(instance, event, result) {
        return Some(decision);
    }

    if let Some(decision) = pr_feedback_sweep_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) = pr_feedback_child_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) =
        repo_backlog_invalid_success_decision(instance, event, result, structured_decision.as_ref())
    {
        return Some(decision);
    }

    if let Some(decision) = repo_backlog_poll_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) =
        repo_backlog_sprint_plan_decision_from_activity_result(instance, event, result)
    {
        return Some(decision);
    }

    if let Some(decision) = repo_backlog_legacy_scan_dispatch_decision_from_activity_result(
        instance,
        event,
        result,
        structured_decision.as_ref(),
    ) {
        return Some(decision);
    }

    if repo_backlog_child_dispatch_still_active(instance, event) {
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
        (REPO_BACKLOG_DEFINITION_ID, "planning_batch", REPO_BACKLOG_SPRINT_PLAN_ACTIVITY) => (
            "idle",
            "finish_repo_sprint_plan",
            "repo sprint planning completed without selected issue workflow commands",
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
        _ => return None,
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

    Some(workflow_decision.high_confidence())
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
