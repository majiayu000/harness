use super::builtin_github_issue::{
    bind_pr_from_activity_result, closed_issue_evidence_from_activity_result,
    github_issue_closed_decision, issue_implementation_missing_result_decision,
    merged_pr_from_activity_result, scope_too_large_decision,
};
use super::builtin_plan_issue::issue_plan_decision_from_activity_result;
use super::builtin_pr_feedback::{
    local_review_decision_from_activity_result,
    pr_feedback_blocking_signal_overrides_structured_ready,
    pr_feedback_child_decision_from_activity_result, pr_feedback_success_contract_error,
    pr_feedback_sweep_decision_from_activity_result,
};
use super::builtin_prompt_task::prompt_task_success_decision;
use super::builtin_quality_gate::{
    parent_quality_gate_pass_decision, quality_gate_activity_matches,
    quality_gate_success_contract_error, quality_gate_success_decision,
};
use super::runtime_failure::{
    retry_failed_activity_decision, runtime_blocked_decision, runtime_cancelled_decision,
    runtime_failed_decision,
};
use super::support::{
    event_command_type, event_field_string, event_workflow_command,
    invalid_agent_output_blocked_decision, runtime_completion_evidence,
};
use super::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::candidate_promotion::{
    build_candidate_promotion_decision, candidate_promotion_failure_decision,
    candidate_promotion_success_decision, candidate_selection_record_from_activity_result,
    deferred_candidate_result_decision,
};
use crate::runtime::candidate_terminal::deferred_candidate_terminal_decision;
use crate::runtime::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowInstance,
};
use crate::runtime::pr_feedback::{
    LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use crate::runtime::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
use crate::runtime::quality_gate::{QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID};
use crate::runtime::state_registry::decision_validator_for_definition;
use crate::runtime::validator::ValidationContext;
use serde_json::json;

pub(super) fn reduce_builtin_completion(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<anyhow::Result<Option<WorkflowDecision>>> {
    if !matches!(
        instance.definition_id.as_str(),
        GITHUB_ISSUE_PR_DEFINITION_ID
            | PROMPT_TASK_DEFINITION_ID
            | QUALITY_GATE_DEFINITION_ID
            | PR_FEEDBACK_DEFINITION_ID
    ) {
        return None;
    }
    let decision = match result.status {
        ActivityStatus::Succeeded => reduce_success(instance, event, result),
        ActivityStatus::Blocked | ActivityStatus::SucceededWithBlockers => {
            github_issue_closed_decision(instance, event, result)
                .or_else(|| scope_too_large_decision(instance, event, result))
                .or_else(|| Some(runtime_blocked_decision(instance, event, result)))
        }
        ActivityStatus::Failed => {
            if let Some(command) = event_workflow_command(event) {
                if let Some(decision) =
                    deferred_candidate_terminal_decision(instance, event, result, &command)
                {
                    return Some(decision.map(Some));
                }
                if let Some(decision) =
                    candidate_promotion_failure_decision(instance, event, result, &command)
                {
                    return Some(decision.map(Some));
                }
            }
            Some(
                retry_failed_activity_decision(instance, event, result)
                    .unwrap_or_else(|| runtime_failed_decision(instance, event, result)),
            )
        }
        ActivityStatus::Cancelled => {
            if let Some(command) = event_workflow_command(event) {
                if let Some(decision) =
                    deferred_candidate_terminal_decision(instance, event, result, &command)
                {
                    return Some(decision.map(Some));
                }
            }
            Some(runtime_cancelled_decision(instance, event, result))
        }
    };
    Some(Ok(decision))
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
    if let Some(selection) = candidate_selection_record_from_activity_result(result) {
        return Some(
            match selection.and_then(|selection| {
                build_candidate_promotion_decision(instance, event, result, selection, Vec::new())
                    .map_err(Into::into)
            }) {
                Ok(decision) => decision,
                Err(error) => invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    &format!("candidate selection could not be promoted: {error}"),
                ),
            },
        );
    }
    if let Some(command) = event_workflow_command(event) {
        if let Some(decision) =
            candidate_promotion_success_decision(instance, event, result, &command)
        {
            return Some(decision.unwrap_or_else(|error| {
                invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    &format!("candidate promotion result could not be applied: {error}"),
                )
            }));
        }
        if let Some(decision) =
            deferred_candidate_result_decision(instance, event, result, &command)
        {
            return Some(decision.unwrap_or_else(|error| {
                invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    &format!("deferred candidate result is invalid: {error}"),
                )
            }));
        }
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
        return prompt_task_success_decision(instance, event, result);
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
        && result.activity == LOCAL_REVIEW_ACTIVITY
    {
        let reason = "run_local_review succeeded without exactly one LocalReviewPassed, LocalReviewChangesRequested, or LocalReviewBlocked signal";
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
        (QUALITY_GATE_DEFINITION_ID, "checking", QUALITY_GATE_ACTIVITY) => (
            "passed",
            "quality_passed",
            "quality gate activity completed successfully",
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
    if instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "replanning"
        && result.activity == "replan_issue"
        && next_state == "implementing"
    {
        let completion_command_id =
            event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
        workflow_decision = workflow_decision.with_command(WorkflowCommand::enqueue_activity(
            "implement_issue",
            format!(
                "issue-replan:{}:implement:{completion_command_id}",
                instance.id
            ),
        ));
    }
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
            LOCAL_REVIEW_ACTIVITY,
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
        PR_FEEDBACK_INSPECT_ACTIVITY,
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
        "sweep_pr_feedback" | PR_FEEDBACK_INSPECT_ACTIVITY => matches!(
            instance.state.as_str(),
            "addressing_feedback"
                | "local_review_gate"
                | "quality_gate_pending"
                | "ready_to_merge"
                | "blocked"
        ),
        LOCAL_REVIEW_ACTIVITY => matches!(
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
        .map(|decision| {
            // GH-1766: the decision body is agent-authored, so it may not
            // assert server-owned evidence classes. Drop any it claims and
            // re-mint only the classes the server itself proved from its own
            // reserved artifacts.
            let mut decision = strip_server_owned_evidence(decision);
            for evidence in server_owned_evidence_for_result(result) {
                decision = decision.with_evidence(evidence);
            }
            decision.with_evidence(runtime_completion_evidence(event, result))
        })
}

/// Evidence classes that only the server may assert.
const SERVER_OWNED_EVIDENCE_KINDS: [&str; 3] = [
    crate::runtime::completion_evidence::EVIDENCE_VERIFIED_PR_BINDING,
    crate::runtime::completion_evidence::EVIDENCE_SERVER_VALIDATION_DIGEST,
    crate::runtime::completion_evidence::EVIDENCE_GITHUB_TERMINAL,
];

fn strip_server_owned_evidence(mut decision: WorkflowDecision) -> WorkflowDecision {
    decision
        .evidence
        .retain(|evidence| !SERVER_OWNED_EVIDENCE_KINDS.contains(&evidence.kind.as_str()));
    decision
}

/// Server-owned evidence the runtime can vouch for, derived from the
/// server-authored artifacts on this result.
fn server_owned_evidence_for_result(
    result: &ActivityResult,
) -> Vec<crate::runtime::model::WorkflowEvidence> {
    use crate::runtime::completion_evidence::{
        server_validation_digest_passed, verified_pr_binding_artifact,
        EVIDENCE_SERVER_VALIDATION_DIGEST, EVIDENCE_VERIFIED_PR_BINDING,
    };
    use crate::runtime::model::WorkflowEvidence;

    let mut evidence = Vec::new();
    if let Some(verified) = verified_pr_binding_artifact(result) {
        evidence.push(WorkflowEvidence::new(
            EVIDENCE_VERIFIED_PR_BINDING,
            verified.to_string(),
        ));
    }
    if server_validation_digest_passed(result) {
        evidence.push(WorkflowEvidence::new(
            EVIDENCE_SERVER_VALIDATION_DIGEST,
            "server validation digest recorded all commands exiting zero",
        ));
    }
    evidence
}

fn structured_decision_validates(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision: &WorkflowDecision,
) -> bool {
    if prompt_task_activity_matches(instance, result) {
        // Prompt-task outcomes are derived from the persisted policy and activity evidence.
        // Agent-provided decisions must not override or bootstrap that runtime-owned state.
        return false;
    }
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
    let Some(validator) = decision_validator_for_definition(&instance.definition_id) else {
        return true;
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
