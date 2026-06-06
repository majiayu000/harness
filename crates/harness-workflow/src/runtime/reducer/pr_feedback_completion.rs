use super::support::{
    event_field_string, has_signal, json_value_u64, optional_data_string, result_signal_string,
    result_signal_u64, runtime_completion_evidence,
};
use super::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::model::{
    ActivityResult, ValidationRecord, WorkflowDecision, WorkflowEvent, WorkflowInstance,
};
use crate::runtime::pr_feedback::{
    build_local_review_completed_decision, build_pr_feedback_decision, LocalReviewCompletedInput,
    LocalReviewOutcome, PrFeedbackDecisionInput, PrFeedbackOutcome, LOCAL_REVIEW_ACTIVITY,
    LOCAL_REVIEW_BLOCKED_SIGNAL, LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL, LOCAL_REVIEW_PASSED_SIGNAL,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY, PR_REPAIR_SNAPSHOT_ARTIFACT,
};
use serde_json::Value;

const PR_FEEDBACK_SNAPSHOT_ARTIFACT: &str = "pr_feedback_snapshot";

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

pub(super) fn pr_feedback_success_contract_error(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    structured_decision: Option<&WorkflowDecision>,
) -> Option<String> {
    if !github_issue_pr_feedback_activity_matches(instance, result) {
        return None;
    }

    if result.activity == "address_pr_feedback" && instance.state == "addressing_feedback" {
        if repair_snapshot_proves_action(instance, result) {
            return None;
        }
        return Some(
            "PR repair evidence is missing: address_pr_feedback succeeded without a pr_repair_snapshot proving pushed changes, review-thread action, or an explicit no-code-change reason plus validation".to_string(),
        );
    }

    if readiness_claimed(result, structured_decision) {
        if pr_feedback_outcome_from_signals(result) == Some(PrFeedbackOutcome::BlockingFeedback) {
            return None;
        }
        if ready_snapshot_proves_pr_ready(instance, result) {
            return None;
        }
        return Some(
            "PR readiness evidence is missing: ready-to-merge output requires a current pr_repair_snapshot with head, checks, mergeability, and zero active unresolved review threads".to_string(),
        );
    }

    None
}

pub(super) fn pr_feedback_blocking_signal_overrides_structured_ready(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    structured_decision: Option<&WorkflowDecision>,
) -> bool {
    github_issue_pr_feedback_activity_matches(instance, result)
        && pr_feedback_outcome_from_signals(result) == Some(PrFeedbackOutcome::BlockingFeedback)
        && structured_decision.is_some_and(structured_ready_decision)
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

fn github_issue_pr_feedback_activity_matches(
    instance: &WorkflowInstance,
    result: &ActivityResult,
) -> bool {
    matches!(
        (
            instance.definition_id.as_str(),
            instance.state.as_str(),
            result.activity.as_str()
        ),
        (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "addressing_feedback",
            "address_pr_feedback"
        ) | (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_feedback",
            "sweep_pr_feedback"
        ) | (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_feedback",
            PR_FEEDBACK_INSPECT_ACTIVITY
        ) | (
            PR_FEEDBACK_DEFINITION_ID,
            "inspecting",
            PR_FEEDBACK_INSPECT_ACTIVITY
        )
    )
}

fn repair_snapshot_proves_action(instance: &WorkflowInstance, result: &ActivityResult) -> bool {
    result.artifacts.iter().any(|artifact| {
        artifact.artifact_type == PR_REPAIR_SNAPSHOT_ARTIFACT
            && snapshot_has_pr_identity(instance, result, &artifact.artifact)
            && snapshot_has_head_identity(&artifact.artifact)
            && snapshot_has_observation_time(&artifact.artifact)
            && snapshot_has_repair_action(&artifact.artifact)
            && snapshot_has_validation_evidence(result, &artifact.artifact)
    })
}

fn readiness_claimed(
    result: &ActivityResult,
    structured_decision: Option<&WorkflowDecision>,
) -> bool {
    has_signal(result, "PrReadyToMerge")
        || structured_decision.is_some_and(structured_ready_decision)
}

fn structured_ready_decision(decision: &WorkflowDecision) -> bool {
    decision.next_state == "ready_to_merge" || decision.decision == "mark_ready_to_merge"
}

fn ready_snapshot_proves_pr_ready(instance: &WorkflowInstance, result: &ActivityResult) -> bool {
    result
        .artifacts
        .iter()
        .filter(|artifact| {
            matches!(
                artifact.artifact_type.as_str(),
                PR_REPAIR_SNAPSHOT_ARTIFACT | PR_FEEDBACK_SNAPSHOT_ARTIFACT
            )
        })
        .any(|artifact| {
            snapshot_has_pr_identity(instance, result, &artifact.artifact)
                && snapshot_has_head_identity(&artifact.artifact)
                && snapshot_has_observation_time(&artifact.artifact)
                && snapshot_check_state_allows_ready(&artifact.artifact)
                && snapshot_merge_state_allows_ready(&artifact.artifact)
                && snapshot_review_state_allows_ready(&artifact.artifact)
                && snapshot_draft_state_allows_ready(&artifact.artifact)
                && snapshot_review_threads_allow_ready(&artifact.artifact)
        })
}

fn snapshot_has_pr_identity(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    snapshot: &Value,
) -> bool {
    let Some(snapshot_number) = field_u64(snapshot, &["pr_number", "prNumber"]) else {
        return false;
    };
    let Some(snapshot_url) = string_field(snapshot, &["pr_url", "prUrl", "url"]) else {
        return false;
    };

    if let Some(expected_number) = expected_pr_number(instance, result) {
        if snapshot_number != expected_number {
            return false;
        }
    }
    if let Some(expected_url) = expected_pr_url(instance, result) {
        if normalize_pr_url(snapshot_url) != normalize_pr_url(expected_url.as_str()) {
            return false;
        }
    }

    true
}

fn expected_pr_number(instance: &WorkflowInstance, result: &ActivityResult) -> Option<u64> {
    instance
        .data
        .get("pr_number")
        .and_then(json_value_u64)
        .or_else(|| result_signal_u64(result, "pr_number"))
}

fn expected_pr_url(instance: &WorkflowInstance, result: &ActivityResult) -> Option<String> {
    optional_data_string(instance, "pr_url").or_else(|| result_signal_string(result, "pr_url"))
}

fn normalize_pr_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

fn snapshot_has_head_identity(snapshot: &Value) -> bool {
    non_empty_string(snapshot, &["head_oid", "head_sha", "headOid", "headSha"])
}

fn snapshot_has_observation_time(snapshot: &Value) -> bool {
    non_empty_string(snapshot, &["observed_at", "observedAt"])
}

fn snapshot_check_state_allows_ready(snapshot: &Value) -> bool {
    string_field_matches(
        snapshot,
        &[
            "status_check_rollup_state",
            "statusCheckRollupState",
            "statusCheckRollup.state",
            "check_state",
            "checkState",
        ],
        &["SUCCESS", "PASSING", "PASSED"],
    )
}

fn snapshot_merge_state_allows_ready(snapshot: &Value) -> bool {
    string_field_matches(
        snapshot,
        &[
            "merge_state_status",
            "mergeStateStatus",
            "merge_state",
            "mergeState",
        ],
        &["CLEAN"],
    )
}

fn snapshot_review_state_allows_ready(snapshot: &Value) -> bool {
    string_field_matches(
        snapshot,
        &["review_decision", "reviewDecision"],
        &["APPROVED"],
    )
}

fn snapshot_draft_state_allows_ready(snapshot: &Value) -> bool {
    matches!(
        field_bool(snapshot, &["is_draft", "isDraft", "draft"]),
        Some(false)
    )
}

fn snapshot_review_threads_allow_ready(snapshot: &Value) -> bool {
    if let Some(count) = field_u64(
        snapshot,
        &[
            "active_unresolved_review_threads_count",
            "activeUnresolvedReviewThreadsCount",
            "active_unresolved_review_thread_count",
            "unresolved_review_threads_count",
            "unresolvedReviewThreadsCount",
            "unresolved_threads",
            "unresolvedThreads",
        ],
    ) {
        return count == 0;
    }

    empty_array(
        snapshot,
        &[
            "active_unresolved_review_threads",
            "activeUnresolvedReviewThreads",
            "unresolved_review_threads",
            "unresolvedReviewThreads",
        ],
    )
}

fn snapshot_has_repair_action(snapshot: &Value) -> bool {
    non_empty_string(
        snapshot,
        &[
            "pushed_head_sha",
            "pushedHeadSha",
            "pushed_head_oid",
            "pushedHeadOid",
            "action_taken",
            "actionTaken",
            "no_code_change_reason",
            "noCodeChangeReason",
        ],
    ) || non_empty_array(
        snapshot,
        &[
            "changed_files",
            "changedFiles",
            "review_thread_actions",
            "reviewThreadActions",
            "resolved_review_thread_ids",
            "resolvedReviewThreadIds",
        ],
    )
}

fn snapshot_has_validation_evidence(result: &ActivityResult, snapshot: &Value) -> bool {
    result
        .validation
        .iter()
        .any(validation_record_allows_success)
        || validation_array_has_success(
            snapshot,
            &["validation", "validation_records", "validationRecords"],
        )
        || validation_array_has_success(snapshot, &["validation_commands", "validationCommands"])
}

fn validation_record_allows_success(record: &ValidationRecord) -> bool {
    !record.command.trim().is_empty() && validation_status_allows_success(&record.status)
}

fn validation_array_has_success(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(validation_value_allows_success))
    })
}

fn validation_value_allows_success(value: &Value) -> bool {
    let has_command = non_empty_string(value, &["command", "cmd", "name"])
        || non_empty_array(value, &["commands"]);
    let has_success_status = string_field(value, &["status", "outcome", "result"])
        .is_some_and(validation_status_allows_success)
        || matches!(
            field_bool(value, &["passed", "success", "succeeded"]),
            Some(true)
        );
    has_command && has_success_status
}

fn validation_status_allows_success(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "passed" | "pass" | "success" | "succeeded" | "ok"
    )
}

fn non_empty_array(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    })
}

fn empty_array(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    })
}

fn non_empty_string(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
    })
}

fn string_field<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields.iter().find_map(|field| {
        field_value(value, field)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
    })
}

fn string_field_matches(value: &Value, fields: &[&str], expected: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_str)
            .is_some_and(|text| expected.iter().any(|item| text.eq_ignore_ascii_case(item)))
    })
}

fn field_bool(value: &Value, fields: &[&str]) -> Option<bool> {
    fields
        .iter()
        .find_map(|field| field_value(value, field).and_then(Value::as_bool))
}

fn field_u64(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| field_value(value, field).and_then(json_value_u64))
}

fn field_value<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    field
        .split('.')
        .try_fold(value, |current, part| current.get(part))
}
