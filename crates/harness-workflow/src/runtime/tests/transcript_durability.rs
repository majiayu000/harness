use super::*;
use crate::runtime::{
    prepare_runtime_transcript, runtime_transcript_artifact_ref, runtime_transcript_checksum,
    RuntimeTranscriptRead, RUNTIME_TRANSCRIPT_ARTIFACT, RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
};

async fn claimed_transcript_job(
    store: &WorkflowRuntimeStore,
    workflow: &WorkflowInstance,
) -> anyhow::Result<(RuntimeJob, DateTime<Utc>)> {
    store.upsert_instance(workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "transcript-command");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({
                "workflow_id": workflow.id,
                "activity": "implement_issue",
                "command": {"activity": "implement_issue"},
            }),
        )
        .await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let job = store
        .claim_next_runtime_job("transcript-test", lease_expires_at)
        .await?
        .expect("runtime job should be claimable");
    Ok((job, lease_expires_at))
}

fn result_with_transcript(content: &str) -> ActivityResult {
    ActivityResult::succeeded("implement_issue", "opened pull request")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 1704,
                "pr_url": "https://github.com/example/repo/pull/1704",
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
            json!({
                "content": content,
                "content_format": "harness.turn.v1+json",
                "turn_id": "turn-1",
            }),
        ))
}

struct TranscriptResultExecutor;

#[async_trait]
impl RuntimeJobExecutor for TranscriptResultExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        result_with_transcript("exact")
    }
}

#[tokio::test]
async fn transcript_completion_is_atomic_restart_safe_and_pinned_while_active() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("workflow_runtime.db");
    let store = WorkflowRuntimeStore::open(&path).await?;
    let workflow = issue_instance("implementing").with_id("transcript-atomic-workflow");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &workflow).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("exact"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);

    let completion = store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("owned completion should commit");

    assert_eq!(completion.runtime_job.status, RuntimeJobStatus::Succeeded);
    let output = completion.runtime_job.output.expect("job output");
    assert!(output["artifacts"]
        .as_array()
        .expect("artifact array")
        .iter()
        .any(|artifact| artifact["artifact_type"] == RUNTIME_TRANSCRIPT_ARTIFACT));
    assert!(!output.to_string().contains("runtime_transcript_source"));
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(_)
    ));

    drop(store);
    let reopened = WorkflowRuntimeStore::open(&path).await?;
    assert!(matches!(
        reopened.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(_)
    ));
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&workflow.id)
        .bind(Utc::now() - Duration::days(45))
        .execute(reopened.pool())
        .await?;
    let summary = reopened
        .prune_terminal_runtime_history(Utc::now() - Duration::days(30), 100)
        .await?;
    assert!(
        summary.is_empty(),
        "active workflow must pin its transcript"
    );
    assert!(matches!(
        reopened.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(_)
    ));
    Ok(())
}

#[tokio::test]
async fn transcript_integrity_loss_can_be_reconstructed_from_provider_export() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("transcript-reconstruct-workflow");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &workflow).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("original"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("completion should commit");

    sqlx::query(
        "UPDATE workflow_artifacts
         SET data = jsonb_set(data, '{content}', to_jsonb('corrupt'::text))
         WHERE id = $1",
    )
    .bind(&artifact_ref)
    .execute(store.pool())
    .await?;
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::InvalidMetadata { .. }
            | RuntimeTranscriptRead::ChecksumMismatch { .. }
    ));

    let expected = runtime_transcript_checksum("provider export");
    let reconstructed = store
        .reconstruct_runtime_transcript(
            &workflow.id,
            &job.id,
            "provider export",
            Some(&expected),
            "operator",
        )
        .await?;
    assert!(reconstructed.reconstructed);
    assert_eq!(reconstructed.reconstructed_by.as_deref(), Some("operator"));
    match store.read_runtime_transcript(&artifact_ref).await? {
        RuntimeTranscriptRead::Verified(record) => {
            assert_eq!(record.content, "provider export");
            assert_eq!(record.reference.checksum, expected);
        }
        other => panic!("reconstructed transcript must verify, got {other:?}"),
    }
    let error = store
        .reconstruct_runtime_transcript(
            &workflow.id,
            &job.id,
            "different provider export",
            None,
            "operator",
        )
        .await
        .expect_err("verified transcript must not be overwritten with different content");
    assert!(error
        .to_string()
        .contains("verified transcript already exists"));
    Ok(())
}

#[tokio::test]
async fn duplicate_reconstruction_preserves_verified_transcript_provenance() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("transcript-reconstruct-idempotent");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &workflow).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("original"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("completion should commit");

    let before: (String, DateTime<Utc>) =
        sqlx::query_as("SELECT data::text, created_at FROM workflow_artifacts WHERE id = $1")
            .bind(&artifact_ref)
            .fetch_one(store.pool())
            .await?;
    let expected = runtime_transcript_checksum("original");

    let returned = store
        .reconstruct_runtime_transcript(
            &workflow.id,
            &job.id,
            "original",
            Some(&expected),
            "different-operator",
        )
        .await?;

    assert!(!returned.reconstructed);
    assert_eq!(returned.reconstructed_by, None);
    assert_ne!(returned.source, json!({"kind": "provider_reexport"}));
    let after: (String, DateTime<Utc>) =
        sqlx::query_as("SELECT data::text, created_at FROM workflow_artifacts WHERE id = $1")
            .bind(&artifact_ref)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(after, before, "duplicate reconstruction must be a no-op");
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(record) if record == returned
    ));
    Ok(())
}

#[tokio::test]
async fn transcript_persistence_failure_rolls_back_runtime_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("transcript-rollback-workflow");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &workflow).await?;
    let (result, mut pending) = prepare_runtime_transcript(&job, result_with_transcript("exact"))?;
    pending
        .as_mut()
        .expect("pending transcript")
        .record
        .workflow_id = "different-workflow".to_string();

    let error = store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await
        .expect_err("mismatched transcript must reject completion");
    assert!(error.to_string().contains("workflow mismatch"));
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should remain");
    assert_eq!(persisted.status, RuntimeJobStatus::Running);
    assert!(persisted.output.is_none());
    assert!(matches!(
        store
            .read_runtime_transcript(&runtime_transcript_artifact_ref(&job.id))
            .await?,
        RuntimeTranscriptRead::Missing
    ));

    let worker_workflow =
        issue_instance("implementing").with_id("transcript-worker-rollback-workflow");
    store.upsert_instance(&worker_workflow).await?;
    let worker_command =
        WorkflowCommand::enqueue_activity("implement_issue", "transcript-worker-rollback-command");
    let worker_command_id = store
        .enqueue_command(&worker_workflow.id, None, &worker_command)
        .await?;
    let worker_job = store
        .enqueue_runtime_job(
            &worker_command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({
                "workflow_id": "different-workflow",
                "activity": "implement_issue",
            }),
        )
        .await?;
    let worker_error = RuntimeWorker::new(&store, "transcript-worker-test")
        .run_once(&TranscriptResultExecutor)
        .await
        .expect_err("worker completion must reject a mismatched transcript");
    assert!(worker_error.to_string().contains("workflow mismatch"));
    let events = store.runtime_events_for(&worker_job.id).await?;
    assert!(
        events
            .iter()
            .all(|event| event.event_type != "ActivityResultReady"),
        "a rolled-back completion must not expose ActivityResultReady"
    );
    Ok(())
}

#[tokio::test]
async fn transcript_retention_waits_for_every_dependent_workflow_to_finish() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let producer = issue_instance("implementing").with_id("transcript-pin-producer");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &producer).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("shared"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("producer completion should commit");
    let mut terminal_producer = store
        .get_instance(&producer.id)
        .await?
        .expect("producer workflow");
    terminal_producer.state = "done".to_string();
    store.upsert_instance(&terminal_producer).await?;

    let dependent = issue_instance("implementing").with_id("transcript-pin-dependent");
    store.upsert_instance(&dependent).await?;
    let dependent_command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "dependent-replay",
        json!({
            "activity": "exact_replay",
            "exact_replay": {
                "transcript_artifact_ref": artifact_ref,
            },
        }),
    );
    let dependent_command_id = store
        .enqueue_command(&dependent.id, None, &dependent_command)
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&producer.id)
        .bind(Utc::now() - Duration::days(45))
        .execute(store.pool())
        .await?;

    let summary = store
        .prune_terminal_runtime_history(Utc::now() - Duration::days(30), 100)
        .await?;
    assert!(
        summary.is_empty(),
        "active dependent must pin producer history"
    );
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Verified(_)
    ));

    store
        .enqueue_runtime_job(
            &dependent_command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({
                "workflow_id": dependent.id,
                "activity": "exact_replay",
                "command": dependent_command.command,
            }),
        )
        .await?;

    let mut terminal_dependent = dependent;
    terminal_dependent.state = "done".to_string();
    store.upsert_instance(&terminal_dependent).await?;
    let summary = store
        .prune_terminal_runtime_history(Utc::now() - Duration::days(30), 100)
        .await?;
    assert_eq!(summary.workflow_instances_deleted, 1);
    assert!(store.get_instance(&producer.id).await?.is_none());
    assert!(matches!(
        store.read_runtime_transcript(&artifact_ref).await?,
        RuntimeTranscriptRead::Missing
    ));
    Ok(())
}

#[tokio::test]
async fn missing_transcript_dependency_keeps_producer_reconstructable() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let producer = issue_instance("implementing").with_id("missing-pin-producer");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &producer).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("original"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("producer completion should commit");
    let mut terminal_producer = producer.clone();
    terminal_producer.state = "done".to_string();
    store.upsert_instance(&terminal_producer).await?;

    let dependent = issue_instance("failed")
        .with_id("missing-pin-dependent")
        .with_data(json!({
            "stop_reason_code": "runtime_transcript_lost",
            "last_stop": {"stop_reason_code": "runtime_transcript_lost"},
        }));
    store.upsert_instance(&dependent).await?;
    let replay = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "missing-dependent-replay",
        json!({
            "activity": "exact_replay",
            "exact_replay": {"transcript_artifact_ref": artifact_ref},
        }),
    );
    store.enqueue_command(&dependent.id, None, &replay).await?;
    sqlx::query("DELETE FROM workflow_artifacts WHERE id = $1")
        .bind(&artifact_ref)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&producer.id)
        .bind(Utc::now() - Duration::days(45))
        .execute(store.pool())
        .await?;

    let summary = store
        .prune_terminal_runtime_history(Utc::now() - Duration::days(30), 100)
        .await?;
    assert!(summary.is_empty(), "missing artifact must remain pinned");
    assert!(store.get_instance(&producer.id).await?.is_some());
    assert!(store.get_runtime_job(&job.id).await?.is_some());
    let reconstructed = store
        .reconstruct_runtime_transcript(
            &producer.id,
            &job.id,
            "provider re-export",
            None,
            "operator",
        )
        .await?;
    assert_eq!(reconstructed.content, "provider re-export");
    Ok(())
}

#[tokio::test]
async fn lost_transcript_consumer_and_producer_remain_pinned_until_recovery() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let producer = issue_instance("implementing").with_id("lost-family-producer");
    let (job, lease_expires_at) = claimed_transcript_job(&store, &producer).await?;
    let (result, pending) = prepare_runtime_transcript(&job, result_with_transcript("original"))?;
    let artifact_ref = runtime_transcript_artifact_ref(&job.id);
    store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "transcript-test",
            lease_expires_at,
            &result,
            pending.as_ref(),
        )
        .await?
        .expect("producer completion should commit");
    let mut terminal_producer = producer.clone();
    terminal_producer.state = "done".to_string();
    store.upsert_instance(&terminal_producer).await?;

    let consumer = issue_instance("failed")
        .with_id("lost-family-consumer")
        .with_data(json!({
            "stop_reason_code": "runtime_transcript_lost",
            "last_stop": {"stop_reason_code": "runtime_transcript_lost"},
        }));
    store.upsert_instance(&consumer).await?;
    let replay = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "lost-family-replay",
        json!({
            "activity": "exact_replay",
            "exact_replay": {"transcript_artifact_ref": artifact_ref},
        }),
    );
    store.enqueue_command(&consumer.id, None, &replay).await?;
    sqlx::query("DELETE FROM workflow_artifacts WHERE id = $1")
        .bind(&artifact_ref)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = ANY($1::text[])")
        .bind(vec![producer.id.as_str(), consumer.id.as_str()])
        .bind(Utc::now() - Duration::days(45))
        .execute(store.pool())
        .await?;

    for _ in 0..2 {
        let summary = store
            .prune_terminal_runtime_history(Utc::now() - Duration::days(30), 100)
            .await?;
        assert!(
            summary.is_empty(),
            "the lost consumer family and its producer must stay pinned across prune passes"
        );
    }
    assert!(store.get_instance(&producer.id).await?.is_some());
    assert!(store.get_instance(&consumer.id).await?.is_some());
    assert!(store.get_runtime_job(&job.id).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn transcript_dependencies_follow_the_persisted_dedupe_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("transcript-dependency-reconcile");
    store.upsert_instance(&workflow).await?;
    let replay = |artifact_ref: &str| {
        WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "shared-replay-dedupe",
            json!({
                "activity": "exact_replay",
                "exact_replay": {"transcript_artifact_ref": artifact_ref},
            }),
        )
    };

    let command_id = store
        .enqueue_command(&workflow.id, None, &replay("runtime-transcript:old"))
        .await?;
    store
        .enqueue_command(&workflow.id, None, &replay("runtime-transcript:current"))
        .await?;
    let mut dependencies: Vec<(String,)> = sqlx::query_as(
        "SELECT artifact_ref FROM workflow_artifact_dependencies
         WHERE workflow_id = $1 ORDER BY artifact_ref",
    )
    .bind(&workflow.id)
    .fetch_all(store.pool())
    .await?;
    assert_eq!(
        dependencies,
        vec![("runtime-transcript:current".to_string(),)]
    );

    store
        .mark_command_status(&command_id, WorkflowCommandStatus::Dispatched)
        .await?;
    store
        .enqueue_command(&workflow.id, None, &replay("runtime-transcript:ignored"))
        .await?;
    dependencies = sqlx::query_as(
        "SELECT artifact_ref FROM workflow_artifact_dependencies
         WHERE workflow_id = $1 ORDER BY artifact_ref",
    )
    .bind(&workflow.id)
    .fetch_all(store.pool())
    .await?;
    assert_eq!(
        dependencies,
        vec![("runtime-transcript:current".to_string(),)]
    );
    Ok(())
}
