use super::*;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, RuntimeEvent, RuntimeJob, RuntimeKind, WorkflowInstance,
    WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::json;

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
    .with_data(json!({
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
    .with_data(json!({
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
    .with_data(json!({
        "failure_reason": "Missing runtime configuration.",
        "error_kind": "configuration",
    }));
    let cancelled = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "cancelled",
        WorkflowSubject::new("issue", "issue:1570"),
    );

    let failed = serde_json::to_value(WorkflowRuntimeTreeProjection::from_workflow(&failed))
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

    let blocked = serde_json::to_value(WorkflowRuntimeTreeProjection::from_workflow(&blocked))
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
