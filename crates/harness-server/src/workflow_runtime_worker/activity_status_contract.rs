use harness_workflow::runtime::{ActivityArtifact, ActivityResult, ActivitySignal, ActivityStatus};
use serde_json::{json, Value};

pub(super) fn enforce_activity_status_contract(
    mut result: ActivityResult,
) -> (bool, ActivityResult) {
    if result.status != ActivityStatus::Succeeded {
        return (false, result);
    }

    let blockers = activity_status_contract_blockers(&result);
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
            "claimed_summary": claimed_summary,
            "blocker_signals": blockers,
        }),
    ));
    result.signals.push(ActivitySignal::new(
        "ActivityStatusContractDowngraded",
        json!({
            "claimed_status": "succeeded",
            "effective_status": "blocked",
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

fn activity_status_contract_blockers(result: &ActivityResult) -> Vec<String> {
    let mut blockers = Vec::new();

    for signal in &result.signals {
        match signal.signal_type.as_str() {
            "ChangesRequested"
            | "ChecksFailed"
            | "LocalReviewChangesRequested"
            | "LocalReviewBlocked"
            | "QualityBlocked"
            | "QualityFailed" => {
                push_unique(&mut blockers, format!("signal:{}", signal.signal_type));
            }
            _ => {}
        }
    }

    for artifact in &result.artifacts {
        collect_structured_blockers(&artifact.artifact, &mut blockers);
    }

    collect_textual_blockers(&result.summary, &mut blockers);
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
                match normalized_key.as_str() {
                    "open_review_threads"
                    | "unresolved_review_threads"
                    | "pending_checks"
                    | "failing_checks"
                    | "failed_checks"
                    | "requested_changes"
                    | "blocking_reviews"
                    | "mergeability_blockers"
                    | "blockers"
                        if json_value_reports_blocker(value) =>
                    {
                        push_unique(blockers, format!("field:{normalized_key}"));
                    }
                    "review_decision" if json_string_equals(value, "changes_requested") => {
                        push_unique(blockers, "field:review_decision_changes_requested");
                    }
                    "merge_state_status"
                        if json_string_is_one_of(
                            value,
                            &["blocked", "dirty", "unknown", "unstable", "behind"],
                        ) =>
                    {
                        push_unique(blockers, "field:merge_state_status_blocked");
                    }
                    "mergeable" if value.as_bool() == Some(false) => {
                        push_unique(blockers, "field:mergeable_false");
                    }
                    _ => {}
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
