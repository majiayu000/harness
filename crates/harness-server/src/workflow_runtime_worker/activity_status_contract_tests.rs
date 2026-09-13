use super::*;
use harness_workflow::runtime::LOCAL_REVIEW_PASSED_SIGNAL;

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
        assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec![format!("signal:{signal_type}")]
        );
    }
}

#[test]
fn declared_local_review_outcome_signals_are_preserved_for_github_issue_pr() {
    for signal_type in [
        LOCAL_REVIEW_PASSED_SIGNAL,
        LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
        LOCAL_REVIEW_BLOCKED_SIGNAL,
    ] {
        let claimed = ActivityResult::succeeded(LOCAL_REVIEW_ACTIVITY, "Review completed.")
            .with_signal(ActivitySignal::new(signal_type, json!({})));

        let (changed, result) =
            enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

        assert!(
            !changed,
            "signal {signal_type} should stay reducer-routable"
        );
        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(status_contract_blockers_from_result(&result).is_empty());
    }
}

#[test]
fn declared_local_review_blocker_outcomes_preserve_matching_blocker_evidence() {
    for signal_type in [
        LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
        LOCAL_REVIEW_BLOCKED_SIGNAL,
    ] {
        let claimed = ActivityResult::succeeded(
            LOCAL_REVIEW_ACTIVITY,
            "Local review found two unresolved review threads and requested changes.",
        )
        .with_signal(ActivitySignal::new(
            signal_type,
            json!({ "pr_number": 1914 }),
        ))
        .with_artifact(ActivityArtifact::new(
            "local_review_findings",
            json!({
                "unresolved_review_threads": [
                    {"path": "AGENTS.md", "line": 61},
                    {"path": "CLAUDE.md", "line": 14}
                ],
                "blockers": [
                    "two unresolved review threads"
                ]
            }),
        ));

        let (changed, result) =
            enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

        assert!(
            !changed,
            "declared {signal_type} should reach the github_issue_pr reducer"
        );
        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(status_contract_blockers_from_result(&result).is_empty());
    }
}

#[test]
fn local_review_passed_with_blocker_evidence_still_downgrades() {
    let claimed = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Local review passed, but two unresolved review threads remain.",
    )
    .with_signal(ActivitySignal::new(
        LOCAL_REVIEW_PASSED_SIGNAL,
        json!({ "pr_number": 1914 }),
    ))
    .with_artifact(ActivityArtifact::new(
        "local_review_findings",
        json!({
            "unresolved_review_threads": [
                {"path": "AGENTS.md", "line": 61},
                {"path": "CLAUDE.md", "line": 14}
            ]
        }),
    ));

    let (changed, result) =
        enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

    assert!(changed);
    assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
    let blockers = status_contract_blockers_from_result(&result);
    assert!(blockers.contains(&"field:unresolved_review_threads".to_string()));
    assert!(!blockers.contains(&"text:unresolved_review_threads".to_string()));
}

#[test]
fn conflicting_declared_local_review_outcomes_still_downgrade() {
    let claimed = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Local review requested changes and could not complete.",
    )
    .with_signal(ActivitySignal::new(
        LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
        json!({ "pr_number": 1914 }),
    ))
    .with_signal(ActivitySignal::new(
        LOCAL_REVIEW_BLOCKED_SIGNAL,
        json!({ "pr_number": 1914 }),
    ))
    .with_artifact(ActivityArtifact::new(
        "local_review_findings",
        json!({
            "unresolved_review_threads": [
                {"path": "AGENTS.md", "line": 61}
            ]
        }),
    ));

    let (changed, result) =
        enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

    assert!(changed);
    assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
    let blockers = status_contract_blockers_from_result(&result);
    assert!(blockers.contains(&format!("signal:{LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL}")));
    assert!(blockers.contains(&format!("signal:{LOCAL_REVIEW_BLOCKED_SIGNAL}")));
    assert!(blockers.contains(&"field:unresolved_review_threads".to_string()));
}

#[test]
fn duplicate_declared_local_review_outcomes_downgrade() {
    for signal_type in [
        LOCAL_REVIEW_PASSED_SIGNAL,
        LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
        LOCAL_REVIEW_BLOCKED_SIGNAL,
    ] {
        let claimed = ActivityResult::succeeded(LOCAL_REVIEW_ACTIVITY, "Review completed.")
            .with_signal(ActivitySignal::new(signal_type, json!({})))
            .with_signal(ActivitySignal::new(signal_type, json!({})));

        let (changed, result) =
            enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

        assert!(
            changed,
            "duplicate {signal_type} declarations must fail closed"
        );
        assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec![format!("signal:{signal_type}")]
        );
    }
}

#[test]
fn local_review_blocker_evidence_still_downgrades_without_declared_outcome() {
    let claimed = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Local review found two unresolved review threads.",
    )
    .with_artifact(ActivityArtifact::new(
        "local_review_findings",
        json!({
            "unresolved_review_threads": [
                {"path": "AGENTS.md", "line": 61},
                {"path": "CLAUDE.md", "line": 14}
            ]
        }),
    ));

    let (changed, result) =
        enforce_activity_status_contract(Some(GITHUB_ISSUE_PR_DEFINITION_ID), claimed);

    assert!(changed);
    assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
    let blockers = status_contract_blockers_from_result(&result);
    assert!(blockers.contains(&"field:unresolved_review_threads".to_string()));
    assert!(blockers.contains(&"text:unresolved_review_threads".to_string()));
}

#[test]
fn local_review_blocker_like_signals_still_downgrade_outside_declared_context() {
    for (workflow_definition, activity) in [
        (Some(GITHUB_ISSUE_PR_DEFINITION_ID), "inspect_pr_feedback"),
        (Some("custom_workflow"), LOCAL_REVIEW_ACTIVITY),
        (None, LOCAL_REVIEW_ACTIVITY),
    ] {
        for signal_type in [
            LOCAL_REVIEW_PASSED_SIGNAL,
            LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
            LOCAL_REVIEW_BLOCKED_SIGNAL,
        ] {
            let claimed = ActivityResult::succeeded(activity, "Review completed.")
                .with_signal(ActivitySignal::new(signal_type, json!({})));

            let (changed, result) = enforce_activity_status_contract(workflow_definition, claimed);

            assert!(
                changed,
                "signal {signal_type} must downgrade for workflow={workflow_definition:?} activity={activity}"
            );
            assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
            assert_eq!(
                status_contract_blockers_from_result(&result),
                vec![format!("signal:{signal_type}")]
            );
        }
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
        assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
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
        assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
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
    let changes_requested = ActivityResult::succeeded("inspect_pr", "PR inspected.").with_artifact(
        ActivityArtifact::new(
            "pr_state",
            json!({ "review_decision": "CHANGES_REQUESTED" }),
        ),
    );
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
    assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
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
    assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
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
        assert_eq!(result.status, ActivityStatus::SucceededWithBlockers);
        assert_eq!(
            status_contract_blockers_from_result(&result),
            vec!["text:failing_checks"]
        );
    }
}

#[test]
fn remote_readiness_wait_does_not_hide_actionable_failures() {
    let mut blockers = Vec::new();
    collect_structured_blockers(
        &json!({"pending_checks":2,"merge_state_status":"BLOCKED"}),
        &mut blockers,
        true,
    );
    assert!(blockers.is_empty());
    collect_structured_blockers(
        &json!({"failed_checks":1,"merge_state_status":"DIRTY","unresolved_review_threads":1}),
        &mut blockers,
        true,
    );
    assert!(blockers.contains(&"field:failed_checks".to_string()));
    assert!(blockers.contains(&"field:merge_state_status_blocked".to_string()));
    assert!(blockers.contains(&"field:unresolved_review_threads".to_string()));
}
