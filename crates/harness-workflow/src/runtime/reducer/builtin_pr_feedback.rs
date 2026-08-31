use super::support::{
    event_field_string, has_signal, json_value_u64, optional_data_string, result_signal_string,
    result_signal_u64, runtime_blocked_command, runtime_completion_evidence,
};
use super::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::model::{
    ActivityResult, ValidationRecord, WorkflowDecision, WorkflowEvent, WorkflowEvidence,
    WorkflowInstance,
};
use crate::runtime::pr_feedback::{
    build_local_review_completed_decision, build_pr_feedback_decision, next_feedback_repair_round,
    FeedbackRepairLane, FeedbackRepairStop, LocalReviewCompletedInput, LocalReviewOutcome,
    PrFeedbackDecisionInput, PrFeedbackOutcome, LOCAL_REVIEW_ACTIVITY, LOCAL_REVIEW_BLOCKED_SIGNAL,
    LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL, LOCAL_REVIEW_PASSED_SIGNAL, MAX_FEEDBACK_REPAIR_ROUNDS,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY, PR_REPAIR_SNAPSHOT_ARTIFACT,
    SERVER_PR_SNAPSHOT_ARTIFACT,
};
use crate::runtime::WorkflowDefinitionRegistry;
use serde_json::Value;

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
    if outcome == PrFeedbackOutcome::BlockingFeedback {
        if let Some(decision) = feedback_repair_convergence_blocked_decision(
            instance,
            event,
            result,
            result_signal_u64(result, "actionable_blocker_count"),
            FeedbackRepairLane::RemoteFeedback,
        ) {
            return Some(decision);
        }
    }
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

fn feedback_repair_convergence_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    current_blockers: Option<u64>,
    lane: FeedbackRepairLane,
) -> Option<WorkflowDecision> {
    let completed_rounds = instance
        .data
        .get("feedback_repair_round")
        .and_then(json_value_u64)
        .unwrap_or(0);
    if completed_rounds == 0 {
        return None;
    }

    let Some(current_blockers) = current_blockers else {
        if completed_rounds == 0 {
            return None;
        }
        return Some(feedback_repair_blocked_decision(
            instance,
            event,
            result,
            "block_feedback_repair_unmeasured",
            "PR feedback repair progress cannot be measured because the server-owned blocker count is missing; automatic repair is stopped.",
        ));
    };
    let (decision_name, reason) =
        match next_feedback_repair_round(&instance.data, current_blockers, lane) {
        Ok(_) => return None,
        Err(FeedbackRepairStop::RoundLimit { .. }) => (
            "block_feedback_repair_round_limit",
            format!("PR feedback remains actionable after {MAX_FEEDBACK_REPAIR_ROUNDS} repair rounds; operator review is required before more mutations."),
        ),
        Err(FeedbackRepairStop::MissingBaseline { .. }) => (
            "block_feedback_repair_unmeasured",
            "PR feedback repair progress cannot be measured because the prior blocker baseline is missing; automatic repair is stopped."
                .to_string(),
        ),
        Err(FeedbackRepairStop::NoProgress { previous, current }) => (
            "block_feedback_repair_oscillation",
            format!("PR feedback repair did not decrease actionable blockers ({previous} before, {current} now); automatic repair is stopped to prevent oscillation."),
        ),
    };
    Some(feedback_repair_blocked_decision(
        instance,
        event,
        result,
        decision_name,
        &reason,
    ))
}

fn feedback_repair_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision_name: &str,
    reason: &str,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "blocked",
        reason,
    )
    .with_command(runtime_blocked_command(
        reason,
        None,
        format!("pr-feedback:{}:convergence-block:{}", instance.id, event.id),
        event,
        result,
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

pub(super) fn pr_feedback_activity_missing_outcome_signal(
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
            "awaiting_feedback",
            "sweep_pr_feedback" | PR_FEEDBACK_INSPECT_ACTIVITY
        ) | (
            PR_FEEDBACK_DEFINITION_ID,
            "inspecting",
            PR_FEEDBACK_INSPECT_ACTIVITY
        )
    ) && pr_feedback_outcome_from_signals(result).is_none()
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
        let ready_snapshot = ready_snapshot_proves_pr_ready(instance, result);
        if structured_decision.is_some_and(structured_ready_decision)
            && !has_signal(result, "PrReadyToMerge")
            && ready_snapshot
        {
            return Some(
                "PR readiness workflow_decision is no longer accepted directly: emit PrReadyToMerge with a current server_pr_snapshot so the parent workflow starts quality_gate before ready_to_merge".to_string(),
            );
        }
        if ready_snapshot {
            return None;
        }
        return Some(
            "PR readiness evidence is missing: ready-to-merge output requires a current server_pr_snapshot with head, checks, mergeability, and zero active unresolved review threads".to_string(),
        );
    }

    None
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
    if outcome == LocalReviewOutcome::ChangesRequested {
        if let Some(decision) = feedback_repair_convergence_blocked_decision(
            instance,
            event,
            result,
            result_signal_u64(result, "actionable_blocker_count"),
            FeedbackRepairLane::LocalReview,
        ) {
            return Some(decision);
        }
    }
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
    let mut decision = build_local_review_completed_decision(
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
    .decision;
    if outcome == LocalReviewOutcome::Blocked {
        let dedupe_key = decision.commands.first()?.dedupe_key.clone();
        decision.commands = vec![runtime_blocked_command(
            result.summary.as_str(),
            None,
            dedupe_key,
            event,
            result,
        )];
    }
    Some(decision.with_evidence(runtime_completion_evidence(event, result)))
}

pub(super) fn pr_feedback_child_decision_from_activity_result(
    registry: &WorkflowDefinitionRegistry,
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

    let mut decision =
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, reason)
            .with_evidence(runtime_completion_evidence(event, result));
    if outcome == PrFeedbackOutcome::ReadyToMerge {
        // GH-1766: `inspecting -> ready_to_merge` requires server_pr_snapshot
        // evidence. The readiness contract above already proved the snapshot;
        // record the evidence class the transition rule demands.
        if ready_snapshot_proves_pr_ready(instance, result) {
            decision = decision.with_evidence(WorkflowEvidence::runtime_observed(
                crate::runtime::completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT,
                "server_github_graphql snapshot proves the PR is ready to merge",
                "server_pr_snapshot",
                Some(event.id.clone()),
            ));
        } else if !crate::runtime::completion_evidence::transition_evidence_enforced_with_registry(
            registry,
            PR_FEEDBACK_DEFINITION_ID,
            "inspecting",
            "ready_to_merge",
            crate::runtime::completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT,
        ) {
            decision = decision.with_evidence(WorkflowEvidence::new(
                crate::runtime::completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT,
                "enforcement_lifted_by_deployment_config",
            ));
        }
    }
    Some(decision.high_confidence())
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
    let mut declared_count = 0;
    let mut declared_outcome = None;
    for signal in &result.signals {
        let outcome = match signal.signal_type.as_str() {
            LOCAL_REVIEW_PASSED_SIGNAL => LocalReviewOutcome::Passed,
            LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL => LocalReviewOutcome::ChangesRequested,
            LOCAL_REVIEW_BLOCKED_SIGNAL => LocalReviewOutcome::Blocked,
            _ => continue,
        };
        declared_count += 1;
        declared_outcome = Some(outcome);
    }

    if declared_count == 1 {
        declared_outcome
    } else {
        None
    }
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
    if result.activity != PR_FEEDBACK_INSPECT_ACTIVITY {
        return false;
    }
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == SERVER_PR_SNAPSHOT_ARTIFACT)
        .any(|artifact| {
            crate::runtime::pr_feedback::server_pr_snapshot_matches_instance(
                instance,
                &artifact.artifact,
            ) && snapshot_has_head_identity(&artifact.artifact)
                && snapshot_has_observation_time(&artifact.artifact)
                && (snapshot_allows_production_readiness(&artifact.artifact)
                    || snapshot_allows_eval_draft_validation(instance, &artifact.artifact))
        })
}

fn snapshot_allows_production_readiness(snapshot: &Value) -> bool {
    snapshot_check_state_allows_ready(snapshot)
        && snapshot_merge_state_allows_ready(snapshot)
        && snapshot_review_state_allows_ready(snapshot)
        && snapshot_draft_state_allows_ready(snapshot)
        && snapshot_review_threads_allow_ready(snapshot)
        && snapshot_review_threads_are_complete(snapshot)
}

fn snapshot_allows_eval_draft_validation(instance: &WorkflowInstance, snapshot: &Value) -> bool {
    crate::runtime::eval::server_owned_eval_metadata(instance).is_some()
        && string_field(snapshot, &["head_oid", "head_sha", "headOid", "headSha"])
            .is_some_and(valid_git_oid)
        && string_field_matches(snapshot, &["state"], &["OPEN"])
        && matches!(
            field_bool(snapshot, &["is_draft", "isDraft", "draft"]),
            Some(true)
        )
        && expected_base_ref_matches(instance, snapshot)
}

fn valid_git_oid(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn expected_base_ref_matches(instance: &WorkflowInstance, snapshot: &Value) -> bool {
    let Some(expected) = optional_data_string(instance, "expected_base_ref") else {
        return true;
    };
    string_field(snapshot, &["base_ref", "baseRefName"])
        .is_some_and(|observed| observed == expected)
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
        .or_else(|| pr_number_from_subject(instance))
        .or_else(|| result_signal_u64(result, "pr_number"))
}

fn pr_number_from_subject(instance: &WorkflowInstance) -> Option<u64> {
    instance
        .subject
        .subject_key
        .strip_prefix("pr:")
        .and_then(|value| value.parse::<u64>().ok())
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
    let thread_array_fields = &[
        "active_unresolved_review_threads",
        "activeUnresolvedReviewThreads",
        "unresolved_review_threads",
        "unresolvedReviewThreads",
    ];
    if non_empty_array(snapshot, thread_array_fields) {
        return false;
    }

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

    empty_array(snapshot, thread_array_fields)
}

fn snapshot_review_threads_are_complete(snapshot: &Value) -> bool {
    field_bool(
        snapshot,
        &["review_threads_complete", "reviewThreadsComplete"],
    ) == Some(true)
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
    let validation_fields = &["validation", "validation_records", "validationRecords"];
    let validation_command_fields = &["validation_commands", "validationCommands"];
    if result
        .validation
        .iter()
        .any(validation_record_reports_failure)
        || validation_array_has_failure(snapshot, validation_fields)
        || validation_array_has_failure(snapshot, validation_command_fields)
    {
        return false;
    }

    result
        .validation
        .iter()
        .any(validation_record_allows_success)
        || validation_array_has_success(snapshot, validation_fields)
        || validation_array_has_success(snapshot, validation_command_fields)
}

fn validation_record_allows_success(record: &ValidationRecord) -> bool {
    !record.command.trim().is_empty() && validation_status_allows_success(&record.status)
}

fn validation_record_reports_failure(record: &ValidationRecord) -> bool {
    !record.command.trim().is_empty() && !validation_status_allows_success(&record.status)
}

fn validation_array_has_success(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(validation_value_allows_success))
    })
}

fn validation_array_has_failure(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        field_value(value, field)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(validation_value_reports_failure))
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

fn validation_value_reports_failure(value: &Value) -> bool {
    let has_command = non_empty_string(value, &["command", "cmd", "name"])
        || non_empty_array(value, &["commands"]);
    let has_failure_status = string_field(value, &["status", "outcome", "result"])
        .is_some_and(|status| !validation_status_allows_success(status))
        || matches!(
            field_bool(value, &["passed", "success", "succeeded"]),
            Some(false)
        );
    has_command && has_failure_status
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
