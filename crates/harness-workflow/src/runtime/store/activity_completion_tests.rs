use super::*;
use crate::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST;
use crate::runtime::{
    prepare_runtime_transcript, runtime_transcript_artifact_ref, ActivityArtifact,
    ActivityErrorKind, ActivitySignal, RuntimeTranscriptRead, ValidationRecord, WorkflowSubject,
    QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
    RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
};
use chrono::Duration;
use harness_core::db::resolve_database_url;

fn fixture(
    marker: Value,
) -> (
    RuntimeJob,
    WorkflowCommandRecord,
    WorkflowInstance,
    ActivityResult,
) {
    let now = Utc::now();
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "cancellation_requested": marker }),
    );
    let command = WorkflowCommandRecord {
        id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        decision_id: None,
        status: WorkflowCommandStatus::Dispatched,
        dispatch_owner: None,
        dispatch_lease_expires_at: None,
        dispatch_not_before: None,
        dispatch_attempt_count: 1,
        dispatch_claim_generation: 1,
        dispatch_barrier: None,
        command: WorkflowCommand::enqueue_activity("implement_issue", "command-1"),
        created_at: now,
        updated_at: now,
        attempt_generation: 3,
        superseded_by_command_id: None,
    };
    let mut workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        crate::runtime::WorkflowSubject::new("issue", "issue:1"),
    );
    workflow.version = 7;
    let result = ActivityResult::cancelled("implement_issue", "cleanup complete");
    (job, command, workflow, result)
}

#[test]
fn legacy_cancellation_ack_without_provenance_is_not_stale() {
    let (job, command, workflow, result) = fixture(json!({ "reason": "cancelled" }));

    assert!(!cancellation_ack_is_stale_for_workflow(
        &job, &command, &workflow, &result
    ));
}

#[test]
fn mismatched_or_malformed_cancellation_provenance_is_stale() {
    for marker in [
        json!({ "workflow_version": 6, "command_attempt_generation": 3 }),
        json!({ "workflow_version": 7, "command_attempt_generation": 2 }),
        json!({ "workflow_version": "7", "command_attempt_generation": 3 }),
        json!({ "workflow_version": 7, "command_attempt_generation": null }),
    ] {
        let (job, command, workflow, result) = fixture(marker);
        assert!(cancellation_ack_is_stale_for_workflow(
            &job, &command, &workflow, &result
        ));
    }
}

#[test]
fn child_completion_payload_carries_parent_recovery_identity() {
    let child = WorkflowInstance::new(
        crate::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "feedback_found",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child")
    .with_server_data(json!({
        "started_by_runtime_job_id": "parent-start-child-job",
    }));
    let event = crate::runtime::model::WorkflowEvent::new(
        &child.id,
        1,
        "RuntimeJobCompleted",
        "runtime-worker",
    )
    .with_payload(json!({
        "runtime_job_id": "child-inspection-job",
        "activity_result": {
            "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "status": "succeeded",
            "summary": "Feedback remains.",
            "artifacts": [
                {"artifact_type": "workflow_decision", "artifact": {"workflow_id": child.id}},
                {"artifact_type": "review_evidence", "artifact": {"reviewed": true}}
            ],
            "signals": [],
            "validation": [],
            "error": null,
            "error_kind": null
        }
    }));

    let payload = merge_child_completion_payload(&event, &child);

    assert_eq!(payload["child_workflow_id"], child.id);
    assert_eq!(payload["recovery_activity"], "start_child_workflow");
    assert_eq!(payload["recovery_runtime_job_id"], "parent-start-child-job");
    assert_eq!(payload["runtime_job_id"], "child-inspection-job");
    assert_eq!(
        payload["activity_result"]["artifacts"],
        json!([{"artifact_type": "review_evidence", "artifact": {"reviewed": true}}])
    );
}

const COMPLETION_PARENT_ID: &str = "completion-parent";
const COMPLETION_CHILD_ID: &str = "completion-quality-gate";

async fn claimed_remote_quality_gate_child(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<(RuntimeJob, RuntimeJobCompletionLease<'static>)> {
    let parent = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "quality_gate_pending",
        WorkflowSubject::new("issue", "123"),
    )
    .with_id(COMPLETION_PARENT_ID);
    store.force_upsert_lifecycle_state_for_test(&parent).await?;
    let child = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", "pr:77"),
    )
    .with_id(COMPLETION_CHILD_ID)
    .with_parent(parent.id);
    store.force_upsert_lifecycle_state_for_test(&child).await?;
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "remote-quality-gate");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let pending = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({"activity": QUALITY_GATE_ACTIVITY, "workflow_id": child.id}),
        )
        .await?;
    let owner = "completion-remote-host";
    let job = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            owner,
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .expect("remote quality-gate job should be claimable");
    assert_eq!(job.id, pending.id);
    let expires_at = job.lease.as_ref().expect("claimed job lease").expires_at;
    let proof = store
        .remote_runtime_job_lease_proof(&job.id, owner, job.lease_generation, expires_at)
        .await?
        .expect("remote claim must issue a completion proof");
    let lease =
        RuntimeJobCompletionLease::remote(owner, expires_at, job.lease_generation, Some(proof));
    Ok((job, lease))
}

fn passing_quality_gate_result() -> ActivityResult {
    ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
        .with_signal(ActivitySignal::new(
            QUALITY_PASSED_SIGNAL,
            json!({"validation": "passed"}),
        ))
        .with_validation(ValidationRecord::new("cargo check", "passed"))
        .with_artifact(ActivityArtifact::new(
            ARTIFACT_SERVER_VALIDATION_DIGEST,
            json!({"commands": [
                {"command": "cargo check", "exit_code": 0, "output_sha256": "d0"}
            ]}),
        ))
}

#[tokio::test]
async fn remote_quality_gate_completion_commits_parent_outcome() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let outcomes = [
        (passing_quality_gate_result(), "passed", "ready_to_merge"),
        (
            ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation was claimed.")
                .with_signal(ActivitySignal::new(QUALITY_PASSED_SIGNAL, json!({}))),
            "blocked",
            "blocked",
        ),
        (
            ActivityResult::failed(
                QUALITY_GATE_ACTIVITY,
                "Validation failed.",
                "command failed",
            )
            .with_error_kind(ActivityErrorKind::Fatal),
            "failed",
            "failed",
        ),
        (
            ActivityResult::cancelled(QUALITY_GATE_ACTIVITY, "Validation cancelled."),
            "cancelled",
            "cancelled",
        ),
    ];
    for (result, child_state, parent_state) in outcomes {
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("remote-child.db")).await?;
        let (job, lease) = claimed_remote_quality_gate_child(&store).await?;

        // This is the shared store API used by the runtime-host HTTP handler;
        // no local RuntimeWorker runs a post-completion propagation step.
        let completion = store
            .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
                &job.id, lease, &result, None,
            )
            .await?
            .expect("owned remote completion should commit");

        assert_eq!(completion.runtime_job.id, job.id);
        assert_eq!(
            store
                .get_instance(COMPLETION_CHILD_ID)
                .await?
                .unwrap()
                .state,
            child_state
        );
        assert_eq!(
            store
                .get_instance(COMPLETION_PARENT_ID)
                .await?
                .unwrap()
                .state,
            parent_state
        );
        let events = store.events_for(COMPLETION_PARENT_ID).await?;
        let parent_events = events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobCompleted")
            .collect::<Vec<_>>();
        assert_eq!(parent_events.len(), 1);
        assert_eq!(
            parent_events[0].event["child_workflow_id"],
            COMPLETION_CHILD_ID
        );
        assert_eq!(parent_events[0].event["runtime_job_id"], job.id);
    }
    Ok(())
}

#[tokio::test]
async fn remote_child_completion_replay_does_not_duplicate_parent_event() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-child-replay.db")).await?;
    let (job, lease) = claimed_remote_quality_gate_child(&store).await?;
    let result = passing_quality_gate_result();

    let (first, second) = tokio::join!(
        store.commit_runtime_activity_completion_if_owned_with_generation(&job.id, lease, &result),
        store.commit_runtime_activity_completion_if_owned_with_generation(&job.id, lease, &result),
    );
    assert_eq!(
        usize::from(first?.is_some()) + usize::from(second?.is_some()),
        1
    );
    let parent = store.get_instance(COMPLETION_PARENT_ID).await?.unwrap();
    assert_eq!(parent.state, "ready_to_merge");
    let parent_events = store.events_for(COMPLETION_PARENT_ID).await?;
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobCompleted")
            .count(),
        1
    );

    assert!(store
        .commit_runtime_activity_completion_if_owned_with_generation(&job.id, lease, &result)
        .await?
        .is_none());
    assert_eq!(
        store.get_instance(COMPLETION_PARENT_ID).await?.unwrap(),
        parent
    );
    assert_eq!(store.events_for(COMPLETION_PARENT_ID).await?, parent_events);
    Ok(())
}

#[tokio::test]
async fn parent_completion_failure_rolls_back_child_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-child-atomic.db")).await?;
    let (job, lease) = claimed_remote_quality_gate_child(&store).await?;
    let child_before = store.get_instance(COMPLETION_CHILD_ID).await?.unwrap();
    let parent_before = store.get_instance(COMPLETION_PARENT_ID).await?.unwrap();
    let command_before = store.get_command(&job.command_id).await?.unwrap();
    let parent_command_id = store
        .enqueue_command(
            COMPLETION_PARENT_ID,
            None,
            &WorkflowCommand::enqueue_activity("merge_pr", "pending-parent-work"),
        )
        .await?;
    let pending_parent_job = store
        .enqueue_runtime_job(
            &parent_command_id,
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({"activity": "merge_pr"}),
        )
        .await?;
    let parent_command_before = store.get_command(&parent_command_id).await?.unwrap();
    let (result, transcript) = prepare_runtime_transcript(
        &job,
        ActivityResult::failed(
            QUALITY_GATE_ACTIVITY,
            "Validation failed.",
            "command failed",
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
            json!({"content": "failed validation transcript"}),
        )),
    )?;

    // Fail after the parent event, decision and terminal fence were written.
    // A separate child transaction would already have committed at this point.
    sqlx::query(
        "CREATE FUNCTION harness_test_fail_parent_completion() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.id = 'completion-parent' AND NEW.state = 'failed' THEN
                 RAISE EXCEPTION 'injected parent completion failure';
             END IF;
             RETURN NEW;
         END;
         $$",
    )
    .execute(store.pool())
    .await?;
    sqlx::query(
        "CREATE TRIGGER harness_test_fail_parent_completion
         BEFORE UPDATE ON workflow_instances
         FOR EACH ROW EXECUTE FUNCTION harness_test_fail_parent_completion()",
    )
    .execute(store.pool())
    .await?;

    let error = store
        .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &job.id,
            lease,
            &result,
            transcript.as_ref(),
        )
        .await
        .expect_err("parent persistence failure must reject the entire completion");
    assert!(format!("{error:#}").contains("injected parent completion failure"));
    assert_eq!(store.get_runtime_job(&job.id).await?.unwrap(), job);
    assert_eq!(
        store.get_instance(COMPLETION_CHILD_ID).await?.unwrap(),
        child_before
    );
    assert_eq!(
        store.get_instance(COMPLETION_PARENT_ID).await?.unwrap(),
        parent_before
    );
    assert_eq!(
        store.get_command(&job.command_id).await?.unwrap(),
        command_before
    );
    assert_eq!(
        store.get_command(&parent_command_id).await?.unwrap(),
        parent_command_before
    );
    assert_eq!(
        store
            .get_runtime_job(&pending_parent_job.id)
            .await?
            .unwrap(),
        pending_parent_job
    );
    for workflow_id in [COMPLETION_CHILD_ID, COMPLETION_PARENT_ID] {
        assert!(!store
            .events_for(workflow_id)
            .await?
            .iter()
            .any(|event| { event.event_type == "RuntimeJobCompleted" }));
    }
    assert!(!store
        .runtime_events_for(&job.id)
        .await?
        .iter()
        .any(|event| { event.event_type == "ActivityResultReady" }));
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    assert_eq!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Missing
    );

    sqlx::query("DROP TRIGGER harness_test_fail_parent_completion ON workflow_instances")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP FUNCTION harness_test_fail_parent_completion()")
        .execute(store.pool())
        .await?;
    store
        .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &job.id,
            lease,
            &result,
            transcript.as_ref(),
        )
        .await?
        .expect("same lease and proof must still commit after rollback");
    assert_eq!(
        store
            .get_instance(COMPLETION_CHILD_ID)
            .await?
            .unwrap()
            .state,
        "failed"
    );
    assert_eq!(
        store
            .get_instance(COMPLETION_PARENT_ID)
            .await?
            .unwrap()
            .state,
        "failed"
    );
    assert_eq!(
        store
            .get_runtime_job(&pending_parent_job.id)
            .await?
            .unwrap()
            .status,
        RuntimeJobStatus::Cancelled
    );
    assert_eq!(
        store.get_command(&parent_command_id).await?.unwrap().status,
        WorkflowCommandStatus::Cancelled
    );
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(_)
    ));
    Ok(())
}
