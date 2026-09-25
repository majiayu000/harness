//! Unit tests for eval case evidence collection (split from evidence.rs
//! to keep both files under the size limit).

use super::*;
use crate::runtime::{
    ActivityResult, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
    WorkflowCommandStatus, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::json;

#[test]
fn eval_evidence_missing_quality_gate_and_usage_fails_case() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("workflow-1")
    .with_server_data(json!({
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "eval-task-1",
        "eval": {"eval_run_id": "run-1", "case_id": "owner/repo#42"}
    }));
    let command = command_record(
        "cmd-1",
        "workflow-1",
        WorkflowCommand::enqueue_activity("implement_issue", "impl-1"),
        WorkflowCommandStatus::Completed,
    );
    let mut job = RuntimeJob::pending(
        "cmd-1",
        RuntimeKind::CodexExec,
        RuntimeProfile::new("codex", RuntimeKind::CodexExec).name,
        json!({"activity": "implement_issue"}),
    );
    job.id = "job-1".to_string();
    job.complete(
        &ActivityResult::succeeded("implement_issue", "opened PR").with_artifact(
            crate::runtime::ActivityArtifact::new(
                "pull_request",
                json!({"pr_number": 5, "pr_url": "https://github.com/owner/repo/pull/5"}),
            ),
        ),
    )
    .expect("complete job");

    let evidence = collect_eval_case_evidence_from_records(
        "run-1",
        "owner/repo#42",
        Some(&workflow),
        &[command],
        &[job],
        &BTreeMap::new(),
    );

    assert_eq!(evidence.status, EvalEvidenceStatus::Failed);
    assert!(evidence
        .missing_evidence
        .contains(&"quality_gate".to_string()));
    assert!(evidence.missing_evidence.contains(&"usage".to_string()));
    assert!(evidence
        .missing_evidence
        .contains(&"isolation_policy".to_string()));
    assert_eq!(
        evidence.submission.as_ref().unwrap().runtime_job_ids,
        vec!["job-1".to_string()]
    );
}

#[test]
fn eval_evidence_maps_quality_gate_and_usage_records() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("workflow-1")
    .with_server_data(json!({
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "eval-task-1",
        "eval": {
            "eval_run_id": "run-1",
            "case_id": "owner/repo#42",
            "cleanup": {"status": "cleaned"},
            "isolation": eval_isolation_json()
        }
    }));
    let implementation = command_record(
        "cmd-impl",
        "workflow-1",
        WorkflowCommand::new(
            crate::runtime::WorkflowCommandType::EnqueueActivity,
            "impl-1",
            json!({
                "activity": "implement_issue",
                "eval": {"isolation": eval_isolation_json()}
            }),
        ),
        WorkflowCommandStatus::Completed,
    );
    let quality = command_record(
        "cmd-quality",
        "workflow-1",
        WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-1"),
        WorkflowCommandStatus::Completed,
    );
    let mut implementation_job = RuntimeJob::pending(
        "cmd-impl",
        RuntimeKind::RemoteHost,
        "eval-isolated-runtime-host",
        json!({
            "activity": "implement_issue",
            "isolation": {
                "tier": "container",
                "trust_class": "non_collaborator",
                "reason": "eval command required container isolation tier from policy"
            },
            "runtime_profile": {
                "name": "eval-isolated-runtime-host",
                "kind": "remote_host",
                "sandbox": "workspace-write"
            }
        }),
    );
    implementation_job.id = "job-impl".to_string();
    implementation_job
        .complete(
            &ActivityResult::succeeded("implement_issue", "opened PR").with_artifact(
                crate::runtime::ActivityArtifact::new(
                    "pull_request",
                    json!({"pr_number": 5, "pr_url": "https://github.com/owner/repo/pull/5"}),
                ),
            ),
        )
        .expect("complete implementation job");
    let mut quality_job = RuntimeJob::pending(
        "cmd-quality",
        RuntimeKind::CodexExec,
        "codex",
        json!({"activity": QUALITY_GATE_ACTIVITY}),
    );
    quality_job.id = "job-quality".to_string();
    quality_job
        .complete(
            &ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "validation passed").with_validation(
                ValidationRecord::new("cargo test -p harness-workflow eval_evidence", "passed"),
            ),
        )
        .expect("complete quality job");
    let mut events = BTreeMap::new();
    events.insert(
        "job-impl".to_string(),
        vec![RuntimeEvent::new(
            "job-impl",
            1,
            "UsageRecorded",
            json!({
                "usage": {
                    "agent_invocation_id": "agent-1",
                    "model": "codex-test",
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "total_tokens": 120,
                    "cost_usd_micros": 50
                }
            }),
        )],
    );

    let evidence = collect_eval_case_evidence_from_records(
        "run-1",
        "owner/repo#42",
        Some(&workflow),
        &[implementation, quality],
        &[implementation_job, quality_job],
        &events,
    );

    assert_eq!(evidence.status, EvalEvidenceStatus::Passed);
    assert!(evidence.missing_evidence.is_empty());
    let isolation = evidence.isolation.as_ref().expect("isolation evidence");
    assert_eq!(isolation.required_tier.as_deref(), Some("container"));
    assert_eq!(isolation.selected_tier.as_deref(), Some("container"));
    assert_eq!(isolation.runtime_kind.as_deref(), Some("remote_host"));
    assert_eq!(isolation.backend.as_deref(), Some("container_runtime_host"));
    assert_eq!(
        isolation.image.as_deref(),
        Some("harness-eval-runner:local")
    );
    assert_eq!(isolation.lifecycle.as_deref(), Some("ephemeral"));
    assert_eq!(isolation.cleanup_status.as_deref(), Some("cleaned"));
    assert_eq!(
        evidence.quality_gate.as_ref().unwrap().validation_commands,
        vec!["cargo test -p harness-workflow eval_evidence".to_string()]
    );
    assert_eq!(evidence.usage[0].total_tokens, Some(120));
    assert_eq!(
        evidence.runtime.as_ref().unwrap().terminal_state,
        Some("done".to_string())
    );
}

#[test]
fn eval_isolation_fails_without_cleanup_evidence() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("workflow-1")
    .with_server_data(json!({
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "eval-task-1",
        "eval": {
            "eval_run_id": "run-1",
            "case_id": "owner/repo#42",
            "isolation": eval_isolation_json()
        }
    }));
    let implementation = command_record(
        "cmd-impl",
        "workflow-1",
        WorkflowCommand::new(
            crate::runtime::WorkflowCommandType::EnqueueActivity,
            "impl-1",
            json!({
                "activity": "implement_issue",
                "eval": {"isolation": eval_isolation_json()}
            }),
        ),
        WorkflowCommandStatus::Completed,
    );
    let mut implementation_job = RuntimeJob::pending(
        "cmd-impl",
        RuntimeKind::RemoteHost,
        "eval-isolated-runtime-host",
        json!({
            "activity": "implement_issue",
            "isolation": {"tier": "container"},
            "runtime_profile": {
                "name": "eval-isolated-runtime-host",
                "kind": "remote_host",
                "sandbox": "workspace-write"
            }
        }),
    );
    implementation_job.id = "job-impl".to_string();
    implementation_job
        .complete(
            &ActivityResult::succeeded("implement_issue", "opened PR").with_artifact(
                crate::runtime::ActivityArtifact::new(
                    "pull_request",
                    json!({"pr_number": 5, "pr_url": "https://github.com/owner/repo/pull/5"}),
                ),
            ),
        )
        .expect("complete implementation job");

    let evidence = collect_eval_case_evidence_from_records(
        "run-1",
        "owner/repo#42",
        Some(&workflow),
        &[implementation],
        &[implementation_job],
        &BTreeMap::new(),
    );

    assert_eq!(evidence.status, EvalEvidenceStatus::Failed);
    assert!(evidence
        .missing_evidence
        .contains(&"isolation_cleanup".to_string()));
}

#[test]
fn eval_usage_invalid_cost_keeps_cost_confidence_unknown() {
    let snapshot = usage_snapshot_from_event(
        Some("workflow-1"),
        "job-1",
        &json!({
            "usage": {
                "input_tokens": 10,
                "cost_usd_micros": "not-a-number"
            }
        }),
    );

    assert_eq!(snapshot.cost_usd_micros, None);
    assert_eq!(snapshot.cost_confidence, Confidence::Unknown);
}

fn command_record(
    id: &str,
    workflow_id: &str,
    command: WorkflowCommand,
    status: WorkflowCommandStatus,
) -> WorkflowCommandRecord {
    WorkflowCommandRecord {
        id: id.to_string(),
        workflow_id: workflow_id.to_string(),
        decision_id: None,
        status,
        dispatch_owner: None,
        dispatch_lease_expires_at: None,
        dispatch_not_before: None,
        dispatch_attempt_count: 0,
        dispatch_claim_generation: 0,
        dispatch_barrier: None,
        command,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        attempt_generation: 1,
        superseded_by_command_id: None,
    }
}

fn eval_isolation_json() -> serde_json::Value {
    json!({
        "tier": "container",
        "runtime_kind": "remote_host",
        "runtime_profile": "eval-isolated-runtime-host",
        "sandbox": "workspace-write",
        "backend": "container_runtime_host",
        "image": "harness-eval-runner:local",
        "lifecycle": "ephemeral",
        "cleanup_required": true
    })
}

#[test]
fn malformed_activity_output_is_skipped_and_reported_missing() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("workflow-1")
    .with_server_data(json!({
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "eval-task-1",
        "eval": {"eval_run_id": "run-1", "case_id": "owner/repo#42"}
    }));
    let command = command_record(
        "cmd-1",
        "workflow-1",
        WorkflowCommand::enqueue_activity("implement_issue", "impl-1"),
        WorkflowCommandStatus::Completed,
    );
    // The quality-gate job's output is a bare string, which cannot
    // deserialize into `ActivityResult`. The evidence builder must treat
    // the step as missing rather than misreporting it as succeeded.
    let mut job = RuntimeJob::pending(
        "cmd-1",
        RuntimeKind::CodexExec,
        RuntimeProfile::new("codex", RuntimeKind::CodexExec).name,
        json!({"activity": "implement_issue"}),
    );
    job.id = "job-1".to_string();
    job.output = Some(json!("not-an-activity-result"));

    assert!(activity_result_from_job(&job).is_none());

    let evidence = collect_eval_case_evidence_from_records(
        "run-1",
        "owner/repo#42",
        Some(&workflow),
        &[command],
        &[job],
        &BTreeMap::new(),
    );
    assert!(
        evidence
            .missing_evidence
            .contains(&"quality_gate".to_string()),
        "malformed output must surface as missing evidence: {:?}",
        evidence.missing_evidence
    );
}
