use harness_workflow::runtime::reducer::prompt_validation_report_has_nonzero_exit;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivitySignal, ActivityStatus, PROMPT_TASK_DEFINITION_ID,
    PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
use serde_json::{json, Value};

/// Reconciles `succeeded`-claimed activity results that simultaneously report
/// blockers (GH-1897). The blocker vocabulary below is the explicit contract:
/// every entry forces the `succeeded_with_blockers` reconciliation, which
/// downgrades the effective status to `blocked` and records the evidence in
/// an `activity_status_contract` artifact.
///
/// Per-signal dispositions are declared here, not implied by parsing. Today
/// every recognized blocker blocks; if a class ever warrants a different
/// disposition (e.g. auto-remediable merge states), it moves out of these
/// tables into its own path rather than growing a special case inside the
/// parser.
const BLOCKING_SIGNAL_TYPES: &[&str] = &[
    "ChangesRequested",
    "ChecksFailed",
    "LocalReviewChangesRequested",
    "LocalReviewBlocked",
    "QualityBlocked",
    "QualityFailed",
];

/// Structured artifact fields whose non-empty/non-zero value reports a blocker.
const BLOCKING_COUNT_FIELDS: &[&str] = &[
    "open_review_threads",
    "unresolved_review_threads",
    "pending_checks",
    "failing_checks",
    "failed_checks",
    "requested_changes",
    "blocking_reviews",
    "mergeability_blockers",
    "blockers",
];

/// `merge_state_status` values that report a blocker.
const BLOCKING_MERGE_STATES: &[&str] = &["blocked", "dirty", "unknown", "unstable", "behind"];

/// The reconciled outcome name recorded in the contract artifact.
const RECONCILED_OUTCOME: &str = "succeeded_with_blockers";

pub(super) fn enforce_activity_status_contract(
    workflow_definition: Option<&str>,
    mut result: ActivityResult,
) -> (bool, ActivityResult) {
    if result.status != ActivityStatus::Succeeded {
        return (false, result);
    }

    let blockers = activity_status_contract_blockers(workflow_definition, &result);
    if blockers.is_empty() {
        return (false, result);
    }

    let claimed_summary = result.summary.clone();
    let reason = format!(
        "activity result claimed succeeded while reporting blockers: {}",
        blockers.join("; ")
    );

    result.status = ActivityStatus::Blocked;
    result.summary = format!("Activity blocked by status contract. {reason}");
    result.error = Some(reason);
    result.artifacts.push(ActivityArtifact::new(
        "activity_status_contract",
        json!({
            "schema": "harness.runtime.activity_status_contract.v1",
            "claimed_status": "succeeded",
            "effective_status": "blocked",
            "reconciled_outcome": RECONCILED_OUTCOME,
            "claimed_summary": claimed_summary,
            "blocker_signals": blockers,
        }),
    ));
    result.signals.push(ActivitySignal::new(
        "ActivityStatusContractDowngraded",
        json!({
            "claimed_status": "succeeded",
            "effective_status": "blocked",
            "reconciled_outcome": RECONCILED_OUTCOME,
        }),
    ));

    (true, result)
}

pub(super) fn status_contract_blockers_from_result(result: &ActivityResult) -> Vec<String> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "activity_status_contract")
        .and_then(|artifact| artifact.artifact.get("blocker_signals"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn activity_status_contract_blockers(
    workflow_definition: Option<&str>,
    result: &ActivityResult,
) -> Vec<String> {
    let mut blockers = Vec::new();

    for signal in &result.signals {
        if BLOCKING_SIGNAL_TYPES.contains(&signal.signal_type.as_str()) {
            push_unique(&mut blockers, format!("signal:{}", signal.signal_type));
        }
    }

    for artifact in &result.artifacts {
        collect_structured_blockers(&artifact.artifact, &mut blockers);
    }

    let mut summary_blockers = Vec::new();
    collect_textual_blockers(&result.summary, &mut summary_blockers);
    if workflow_definition == Some(PROMPT_TASK_DEFINITION_ID)
        && result.activity == PROMPT_TASK_IMPLEMENT_ACTIVITY
        && prompt_validation_report_has_nonzero_exit(result)
    {
        summary_blockers.retain(|blocker| blocker != "text:failing_checks");
    }
    for blocker in summary_blockers {
        push_unique(&mut blockers, blocker);
    }
    if let Some(error) = result.error.as_deref() {
        collect_textual_blockers(error, &mut blockers);
    }

    blockers
}

fn collect_structured_blockers(value: &Value, blockers: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let normalized_key = key.to_ascii_lowercase();
                if BLOCKING_COUNT_FIELDS.contains(&normalized_key.as_str()) {
                    if json_value_reports_blocker(value) {
                        push_unique(blockers, format!("field:{normalized_key}"));
                    }
                } else if normalized_key == "review_decision" {
                    if json_string_equals(value, "changes_requested") {
                        push_unique(blockers, "field:review_decision_changes_requested");
                    }
                } else if normalized_key == "merge_state_status" {
                    if json_string_is_one_of(value, BLOCKING_MERGE_STATES) {
                        push_unique(blockers, "field:merge_state_status_blocked");
                    }
                } else if normalized_key == "mergeable" && value.as_bool() == Some(false) {
                    push_unique(blockers, "field:mergeable_false");
                }
                collect_structured_blockers(value, blockers);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_structured_blockers(value, blockers);
            }
        }
        _ => {}
    }
}

fn json_value_reports_blocker(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64().is_some_and(|count| count > 0),
        Value::String(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !matches!(
                normalized.as_str(),
                "" | "0" | "false" | "none" | "no" | "clean" | "[]"
            )
        }
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        Value::Null => false,
    }
}

fn json_string_equals(value: &Value, expected: &str) -> bool {
    value
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn json_string_is_one_of(value: &Value, expected: &[&str]) -> bool {
    value.as_str().is_some_and(|value| {
        expected
            .iter()
            .any(|expected| value.eq_ignore_ascii_case(expected))
    })
}

fn collect_textual_blockers(text: &str, blockers: &mut Vec<String>) {
    let normalized = text.to_ascii_lowercase();
    let patterns = [
        (
            "text:pending_ci",
            &["pending ci", "pending check", "pending checks"][..],
        ),
        (
            "text:failing_checks",
            &[
                "failing check",
                "failing checks",
                "failed check",
                "failed checks",
            ],
        ),
        (
            "text:requested_changes",
            &["requested changes", "changes requested"],
        ),
        (
            "text:unresolved_review_threads",
            &[
                "open review thread",
                "open review threads",
                "unresolved review thread",
                "unresolved review threads",
            ],
        ),
        (
            "text:not_merge_ready",
            &["not merge-ready", "not merge ready", "not ready to merge"],
        ),
        (
            "text:review_quota_blocker",
            &["quota/credit-limit", "credit-limit notice", "quota notice"],
        ),
    ];

    for (label, needles) in patterns {
        if needles
            .iter()
            .any(|needle| contains_affirmative_blocker(&normalized, needle))
        {
            push_unique(blockers, label);
        }
    }
}

fn contains_affirmative_blocker(normalized_text: &str, needle: &str) -> bool {
    normalized_text.contains(needle)
        && !normalized_text.contains(&format!("no {needle}"))
        && !normalized_text.contains(&format!("without {needle}"))
}

fn push_unique(blockers: &mut Vec<String>, blocker: impl Into<String>) {
    let blocker = blocker.into();
    if !blockers.contains(&blocker) {
        blockers.push(blocker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_result_with_report(exit_code: Value) -> ActivityResult {
        ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Validation reported failed checks.",
        )
        .with_artifact(ActivityArtifact::new(
            "validation_report",
            json!([{
                "command": "cargo test",
                "exit_code": exit_code,
            }]),
        ))
    }

    #[test]
    fn every_blocking_signal_type_downgrades_claimed_success() {
        for signal_type in BLOCKING_SIGNAL_TYPES {
            let claimed = ActivityResult::succeeded("run_local_review", "All good.")
                .with_signal(ActivitySignal::new(*signal_type, json!({})));

            let (changed, result) = enforce_activity_status_contract(None, claimed);

            assert!(changed, "signal {signal_type} must downgrade");
            assert_eq!(result.status, ActivityStatus::Blocked);
            assert_eq!(
                status_contract_blockers_from_result(&result),
                vec![format!("signal:{signal_type}")]
            );
        }
    }

    #[test]
    fn every_blocking_count_field_downgrades_claimed_success() {
        for field in BLOCKING_COUNT_FIELDS {
            let claimed = ActivityResult::succeeded("run_local_review", "All good.").with_artifact(
                ActivityArtifact::new("review_summary", json!({ *field: 2 })),
            );

            let (changed, result) = enforce_activity_status_contract(None, claimed);

            assert!(changed, "field {field} must downgrade");
            assert_eq!(result.status, ActivityStatus::Blocked);
            assert_eq!(
                status_contract_blockers_from_result(&result),
                vec![format!("field:{field}")]
            );
        }
    }

    #[test]
    fn every_blocking_merge_state_downgrades_claimed_success() {
        for merge_state in BLOCKING_MERGE_STATES {
            let claimed = ActivityResult::succeeded("inspect_pr", "PR inspected.").with_artifact(
                ActivityArtifact::new("pr_state", json!({ "merge_state_status": *merge_state })),
            );

            let (changed, result) = enforce_activity_status_contract(None, claimed);

            assert!(changed, "merge state {merge_state} must downgrade");
            assert_eq!(result.status, ActivityStatus::Blocked);
            assert_eq!(
                status_contract_blockers_from_result(&result),
                vec!["field:merge_state_status_blocked"]
            );
        }

        let clean = ActivityResult::succeeded("inspect_pr", "PR inspected.").with_artifact(
            ActivityArtifact::new("pr_state", json!({ "merge_state_status": "clean" })),
        );
        let (changed, result) = enforce_activity_status_contract(None, clean);
        assert!(!changed);
        assert_eq!(result.status, ActivityStatus::Succeeded);
    }

    #[test]
    fn review_decision_and_mergeable_false_downgrade_claimed_success() {
        let changes_requested = ActivityResult::succeeded("inspect_pr", "PR inspected.")
            .with_artifact(ActivityArtifact::new(
                "pr_state",
                json!({ "review_decision": "CHANGES_REQUESTED" }),
            ));
        let (changed, result) = enforce_activity_status_contract(None, changes_requested);
        assert!(changed);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec!["field:review_decision_changes_requested"]
        );

        let unmergeable = ActivityResult::succeeded("inspect_pr", "PR inspected.").with_artifact(
            ActivityArtifact::new("pr_state", json!({ "mergeable": false })),
        );
        let (changed, result) = enforce_activity_status_contract(None, unmergeable);
        assert!(changed);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec!["field:mergeable_false"]
        );
    }

    #[test]
    fn reconciliation_records_explicit_outcome_evidence() {
        let claimed = ActivityResult::succeeded("run_local_review", "Review done.").with_signal(
            ActivitySignal::new("LocalReviewChangesRequested", json!({})),
        );

        let (changed, result) = enforce_activity_status_contract(None, claimed);

        assert!(changed);
        let artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "activity_status_contract")
            .expect("contract artifact");
        assert_eq!(
            artifact.artifact.get("reconciled_outcome"),
            Some(&json!(RECONCILED_OUTCOME))
        );
        assert_eq!(
            artifact.artifact.get("claimed_summary"),
            Some(&json!("Review done."))
        );
        let downgrade_signal = result
            .signals
            .iter()
            .find(|signal| signal.signal_type == "ActivityStatusContractDowngraded")
            .expect("downgrade signal");
        assert_eq!(
            downgrade_signal.signal.get("reconciled_outcome"),
            Some(&json!(RECONCILED_OUTCOME))
        );
    }

    #[test]
    fn negated_textual_blockers_do_not_downgrade() {
        let claimed = ActivityResult::succeeded(
            "run_local_review",
            "Merged cleanly with no failing checks and no unresolved review threads.",
        );

        let (changed, result) = enforce_activity_status_contract(None, claimed);

        assert!(!changed);
        assert_eq!(result.status, ActivityStatus::Succeeded);
    }

    #[test]
    fn non_succeeded_results_are_left_untouched() {
        let blocked = ActivityResult {
            status: ActivityStatus::Blocked,
            ..ActivityResult::succeeded("run_local_review", "Blocked upstream.")
        }
        .with_signal(ActivitySignal::new(
            "LocalReviewChangesRequested",
            json!({}),
        ));

        let (changed, result) = enforce_activity_status_contract(None, blocked);

        assert!(!changed);
        assert_eq!(result.status, ActivityStatus::Blocked);
        assert!(status_contract_blockers_from_result(&result).is_empty());
    }

    #[test]
    fn prompt_failed_checks_without_nonzero_report_remain_blocking() {
        let (changed, result) = enforce_activity_status_contract(
            Some(PROMPT_TASK_DEFINITION_ID),
            prompt_result_with_report(json!(0)),
        );

        assert!(changed);
        assert_eq!(result.status, ActivityStatus::Blocked);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec!["text:failing_checks"]
        );
    }

    #[test]
    fn prompt_nonzero_report_does_not_hide_explicit_checks_failed_signal() {
        let claimed = prompt_result_with_report(json!(101)).with_signal(ActivitySignal::new(
            "ChecksFailed",
            json!({ "check": "cargo test" }),
        ));

        let (changed, result) =
            enforce_activity_status_contract(Some(PROMPT_TASK_DEFINITION_ID), claimed);

        assert!(changed);
        assert_eq!(result.status, ActivityStatus::Blocked);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec!["signal:ChecksFailed"]
        );
    }

    #[test]
    fn missing_or_custom_workflow_keeps_prompt_named_failed_checks_blocking() {
        for workflow_definition in [None, Some("custom_prompt_workflow")] {
            let (changed, result) = enforce_activity_status_contract(
                workflow_definition,
                prompt_result_with_report(json!(101)),
            );

            assert!(changed);
            assert_eq!(result.status, ActivityStatus::Blocked);
            assert_eq!(
                status_contract_blockers_from_result(&result),
                vec!["text:failing_checks"]
            );
        }
    }
}
