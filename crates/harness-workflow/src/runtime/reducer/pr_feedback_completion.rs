use super::support::{
    event_field_string, has_signal, optional_data_string, result_signal_string, result_signal_u64,
    runtime_completion_evidence,
};
use super::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::model::{ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowInstance};
use crate::runtime::pr_feedback::{
    build_local_review_completed_decision, build_pr_feedback_decision, LocalReviewCompletedInput,
    LocalReviewOutcome, PrFeedbackDecisionInput, PrFeedbackOutcome, LOCAL_REVIEW_ACTIVITY,
    LOCAL_REVIEW_BLOCKED_SIGNAL, LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL, LOCAL_REVIEW_PASSED_SIGNAL,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};

pub(super) fn pr_feedback_sweep_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || !matches!(
            result.activity.as_str(),
            "sweep_pr_feedback" | PR_FEEDBACK_INSPECT_ACTIVITY
        )
    {
        return None;
    }
    if instance.state != "awaiting_feedback" {
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

pub(super) fn local_review_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || instance.state != "local_review_gate"
        || result.activity != LOCAL_REVIEW_ACTIVITY
    {
        return None;
    }
    let outcome = local_review_outcome_from_signals(result)?;
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
    let repair_dedupe_source =
        event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
    let repair_dedupe_key = format!(
        "local-review:{}:{}:address:{}",
        instance.id, pr_number, repair_dedupe_source
    );
    Some(
        build_local_review_completed_decision(
            instance,
            LocalReviewCompletedInput {
                task_id: &task_id,
                pr_number,
                pr_url: pr_url.as_deref(),
                repair_dedupe_key: &repair_dedupe_key,
                outcome,
                summary: result.summary.as_str(),
            },
        )
        .decision
        .with_evidence(runtime_completion_evidence(event, result)),
    )
}

pub(super) fn pr_feedback_child_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        PR_FEEDBACK_DEFINITION_ID,
        "inspecting",
        PR_FEEDBACK_INSPECT_ACTIVITY,
    ) {
        return None;
    }

    let outcome = pr_feedback_outcome_from_signals(result)?;
    let (next_state, decision, reason) = match outcome {
        PrFeedbackOutcome::BlockingFeedback => (
            "feedback_found",
            "record_feedback_found",
            "PR feedback inspection found actionable feedback.",
        ),
        PrFeedbackOutcome::NoActionableFeedback => (
            "no_actionable_feedback",
            "record_no_actionable_feedback",
            "PR feedback inspection found no actionable feedback.",
        ),
        PrFeedbackOutcome::ReadyToMerge => (
            "ready_to_merge",
            "record_ready_to_merge",
            "PR feedback inspection found the PR ready to merge.",
        ),
    };

    Some(
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, reason)
            .with_evidence(runtime_completion_evidence(event, result))
            .high_confidence(),
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

fn local_review_outcome_from_signals(result: &ActivityResult) -> Option<LocalReviewOutcome> {
    if has_signal(result, LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL) {
        return Some(LocalReviewOutcome::ChangesRequested);
    }
    if has_signal(result, LOCAL_REVIEW_BLOCKED_SIGNAL) {
        return Some(LocalReviewOutcome::Blocked);
    }
    if has_signal(result, LOCAL_REVIEW_PASSED_SIGNAL) {
        return Some(LocalReviewOutcome::Passed);
    }
    None
}
