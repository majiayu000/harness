use super::*;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use harness_workflow::runtime::{
    build_declarative_definition, ActivityArtifact, ActivityResult, RuntimeEvent, RuntimeJob,
    RuntimeKind, WorkflowDefinitionRegistry, WorkflowInstance, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::json;
use std::collections::BTreeMap;

fn runtime_job_with_artifacts(artifacts: Vec<ActivityArtifact>) -> RuntimeJob {
    let result = artifacts.into_iter().fold(
        ActivityResult::succeeded("replan_issue", "Runtime job completed."),
        ActivityResult::with_artifact,
    );
    let mut job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-high",
        json!({}),
    );
    job.output = Some(serde_json::to_value(result).expect("activity result should serialize"));
    job
}

#[test]
fn activity_result_envelope_from_job_returns_latest_valid_envelope() {
    let older = ActivityArtifact::new(
        ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
        json!({
            "schema": ACTIVITY_RESULT_ENVELOPE_SCHEMA,
            "outcome": "accepted",
        }),
    );
    let newer = ActivityArtifact::new(
        ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
        json!({
            "schema": ACTIVITY_RESULT_ENVELOPE_SCHEMA,
            "outcome": "repaired_structured_output",
        }),
    );
    let job = runtime_job_with_artifacts(vec![older, newer]);

    let envelope =
        activity_result_envelope_from_job(&job).expect("valid envelope should be exposed");

    assert_eq!(envelope["outcome"], "repaired_structured_output");
}

#[test]
fn activity_result_envelope_from_job_ignores_missing_or_invalid_envelope() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-high",
        json!({}),
    );
    assert!(activity_result_envelope_from_job(&job).is_none());

    let job = runtime_job_with_artifacts(vec![ActivityArtifact::new(
        ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
        json!({
            "schema": "harness.runtime.activity_result_envelope.v0",
            "outcome": "accepted",
        }),
    )]);
    assert!(activity_result_envelope_from_job(&job).is_none());
}

#[test]
fn runtime_activity_summary_counts_all_loaded_jobs() {
    let accepted = ActivityArtifact::new(
        ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
        json!({
            "schema": ACTIVITY_RESULT_ENVELOPE_SCHEMA,
            "outcome": "accepted",
        }),
    );
    let repaired = ActivityArtifact::new(
        ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
        json!({
            "schema": ACTIVITY_RESULT_ENVELOPE_SCHEMA,
            "outcome": "repaired_structured_output",
        }),
    );
    let mut jobs_by_command = BTreeMap::new();
    jobs_by_command.insert(
        "command-1".to_string(),
        vec![
            runtime_job_with_artifacts(vec![accepted]),
            runtime_job_with_artifacts(vec![repaired]),
            RuntimeJob::pending(
                "command-1",
                RuntimeKind::CodexJsonrpc,
                "codex-high",
                json!({}),
            ),
        ],
    );
    let mut summary = WorkflowRuntimeTreeSummary::default();

    apply_runtime_activity_summary(&mut summary, &jobs_by_command);

    assert_eq!(summary.activity_outcomes["accepted"], 1);
    assert_eq!(summary.activity_outcomes["repaired_structured_output"], 1);
    assert_eq!(summary.jobs_without_activity_envelope, 1);
}

#[test]
fn runtime_tree_projection_exposes_structured_stop_metadata_and_eligibility() {
    let failed = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("issue", "issue:1567"),
    )
    .with_server_data(json!({
        "failure_reason": "Runtime transport timed out.",
        "error_kind": "timeout",
        "retry_hint": "Fix the transient condition, then call retry.",
        "last_stop": {
            "state": "failed",
            "activity": "implement_issue",
            "runtime_job_id": "job-failed",
        },
    }));
    let blocked = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        WorkflowSubject::new("issue", "issue:1568"),
    )
    .with_server_data(json!({
        "blocked_reason": "Waiting for maintainer approval.",
        "unblock_hint": "Post the approval comment, then call unblock.",
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "runtime_job_id": "job-blocked",
        },
    }));
    let nonretryable = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("issue", "issue:1569"),
    )
    .with_server_data(json!({
        "failure_reason": "Missing runtime configuration.",
        "error_kind": "configuration",
    }));
    let cancelled = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "cancelled",
        WorkflowSubject::new("issue", "issue:1570"),
    );
    let legacy = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("issue", "issue:1571"),
    )
    .with_server_data(json!({
        "previous_error": "Legacy workflow failed before structured metadata shipped.",
    }));

    let failed = serde_json::to_value(
        WorkflowRuntimeTreeProjection::from_workflow_with_stopped_eligibility(
            &failed,
            crate::runtime_projection::RuntimeStoppedActionEligibility {
                can_unblock: false,
                can_retry: true,
            },
        ),
    )
    .expect("failed projection should serialize");
    assert_eq!(failed["failure_reason"], "Runtime transport timed out.");
    assert_eq!(failed["error_kind"], "timeout");
    assert_eq!(
        failed["retry_hint"],
        "Fix the transient condition, then call retry."
    );
    assert_eq!(failed["last_stop"]["runtime_job_id"], "job-failed");
    assert_eq!(failed["can_unblock"], false);
    assert_eq!(failed["can_retry"], true);

    let blocked = serde_json::to_value(
        WorkflowRuntimeTreeProjection::from_workflow_with_stopped_eligibility(
            &blocked,
            crate::runtime_projection::RuntimeStoppedActionEligibility {
                can_unblock: true,
                can_retry: false,
            },
        ),
    )
    .expect("blocked projection should serialize");
    assert_eq!(
        blocked["blocked_reason"],
        "Waiting for maintainer approval."
    );
    assert_eq!(
        blocked["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(blocked["last_stop"]["runtime_job_id"], "job-blocked");
    assert_eq!(blocked["can_unblock"], true);
    assert_eq!(blocked["can_retry"], false);

    let nonretryable =
        serde_json::to_value(WorkflowRuntimeTreeProjection::from_workflow(&nonretryable))
            .expect("nonretryable projection should serialize");
    assert_eq!(nonretryable["error_kind"], "configuration");
    assert_eq!(nonretryable["can_unblock"], false);
    assert_eq!(nonretryable["can_retry"], false);

    let cancelled = serde_json::to_value(WorkflowRuntimeTreeProjection::from_workflow(&cancelled))
        .expect("cancelled projection should serialize");
    assert_eq!(cancelled["can_unblock"], false);
    assert_eq!(cancelled["can_retry"], false);

    let legacy = serde_json::to_value(WorkflowRuntimeTreeProjection::from_workflow(&legacy))
        .expect("legacy projection should serialize");
    assert_eq!(
        legacy["failure_reason"],
        "Legacy workflow failed before structured metadata shipped."
    );
}

#[test]
fn runtime_job_has_in_flight_model_turn_uses_latest_turn_sequence() {
    let mut job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-high",
        json!({}),
    );
    job.claim(
        "worker-1",
        chrono::Utc::now() + chrono::Duration::minutes(5),
    );
    let events = vec![
        RuntimeEvent::new(&job.id, 1, "RuntimeTurnStarted", json!({})),
        RuntimeEvent::new(&job.id, 2, "ActivityResultReady", json!({})),
        RuntimeEvent::new(&job.id, 3, "RuntimeTurnStarted", json!({})),
    ];

    assert!(runtime_job_has_in_flight_model_turn(&job, &events));
}

#[test]
fn runtime_job_has_in_flight_model_turn_ends_after_result_for_latest_turn() {
    let mut job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-high",
        json!({}),
    );
    job.claim(
        "worker-1",
        chrono::Utc::now() + chrono::Duration::minutes(5),
    );
    let events = vec![
        RuntimeEvent::new(&job.id, 1, "RuntimeTurnStarted", json!({})),
        RuntimeEvent::new(&job.id, 2, "ActivityResultReady", json!({})),
        RuntimeEvent::new(&job.id, 3, "RuntimeTurnStarted", json!({})),
        RuntimeEvent::new(&job.id, 4, "ActivityResultReady", json!({})),
    ];

    assert!(!runtime_job_has_in_flight_model_turn(&job, &events));
}

#[test]
fn workflow_summary_projection_preserves_declarative_definition_pins() -> anyhow::Result<()> {
    let definition_id = "runtime_tree_pinned_summary";
    let activities = BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]);
    let old = build_declarative_definition(
        &WorkflowDefinitionPolicy {
            id: definition_id.to_string(),
            initial: "complete".to_string(),
            states: BTreeMap::from([
                (
                    "complete".to_string(),
                    DeclaredState {
                        activity: Some("run".to_string()),
                        on_success: Some("archived".to_string()),
                        on_failure: Some("failed".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("archived".to_string(), "succeeded".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: Vec::new(),
            intake: None,
        },
        &activities,
    )?;
    let current = build_declarative_definition(
        &WorkflowDefinitionPolicy {
            id: definition_id.to_string(),
            initial: "work".to_string(),
            states: BTreeMap::from([
                (
                    "work".to_string(),
                    DeclaredState {
                        activity: Some("run".to_string()),
                        on_success: Some("complete".to_string()),
                        on_failure: Some("failed".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("cancelled".to_string(), "cancelled".to_string()),
                ("complete".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: Vec::new(),
            intake: None,
        },
        &activities,
    )?;
    let mut registry = WorkflowDefinitionRegistry::with_builtins();
    registry.register_declarative_current(current.clone())?;
    registry.register_declarative_historical(old.clone())?;
    let counts = [
        harness_workflow::runtime::store::WorkflowRuntimeStateCount {
            definition_id: definition_id.to_string(),
            definition_version: old.definition_version(),
            definition_hash: Some(old.definition_hash().to_string()),
            state: "complete".to_string(),
            count: 1,
        },
        harness_workflow::runtime::store::WorkflowRuntimeStateCount {
            definition_id: definition_id.to_string(),
            definition_version: current.definition_version(),
            definition_hash: Some(current.definition_hash().to_string()),
            state: "complete".to_string(),
            count: 1,
        },
    ];

    let (statuses, _, _) = workflow_projection_summary_counts(&registry, &counts);

    assert_eq!(statuses.get("done"), Some(&1));
    assert_eq!(statuses.get("waiting"), Some(&1));
    Ok(())
}
