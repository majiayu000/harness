use super::*;
use crate::runtime::store::runtime_job_leases::{
    postgres_timestamp_floor, RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRequest,
};
use crate::runtime::ActivityArtifact;
use crate::runtime::RuntimeJobCompletionLease;
use uuid::Uuid;

#[tokio::test]
async fn terminal_cleanup_ack_preserves_inactive_command_status() -> anyhow::Result<()> {
    if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
    let mut workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:legacy-inactive-eval"),
    )
    .with_id("legacy-inactive-eval")
    .with_server_data(json!({
        "eval": {"eval_run_id": "run-legacy", "case_id": "case-legacy"},
    }));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "legacy-inactive-eval-command",
        json!({
            "activity": "implement_issue",
            "eval": {"eval_run_id": "run-legacy", "case_id": "case-legacy"},
        }),
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::RemoteHost,
            "eval-host",
            json!({"activity": "implement_issue", "command": command.command}),
        )
        .await?;
    let expired_at = postgres_timestamp_floor(Utc::now() - Duration::seconds(1));
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "legacy-host", expired_at)
        .await?
        .expect("legacy eval should be claimed before terminalization");
    let initial_proof = store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-host",
            claimed.lease_generation,
            expired_at,
        )
        .await?
        .expect("legacy eval claim should carry a proof");
    store
        .mark_command_status(&command_id, WorkflowCommandStatus::Failed)
        .await?;
    workflow.state = "done".to_string();
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;

    assert!(store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "late-host",
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .is_none());
    let requested = store
        .get_runtime_job(&job.id)
        .await?
        .expect("legacy eval should remain readable");
    assert!(requested.input.get("cancellation_requested").is_some());

    let now = Utc::now();
    let reserved = store
        .reserve_cancelled_remote_host_runtime_job_completion(RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &claimed.id,
            owner: "legacy-host",
            lease_generation: claimed.lease_generation,
            lease_proof: Some(initial_proof),
            previous_expires_at: expired_at,
            renewal_id: Uuid::new_v4(),
            lease_secs: 60,
            now,
            max_lease_secs: 3_600,
            owner_active: true,
        })
        .await?;
    let RuntimeJobLeaseRenewalOutcome::Renewed {
        lease_expires_at,
        lease_generation,
        ..
    } = reserved
    else {
        anyhow::bail!("legacy cancellation acknowledgement reservation was rejected")
    };
    let reserved_proof = store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-host",
            lease_generation,
            lease_expires_at,
        )
        .await?
        .expect("reserved cancellation acknowledgement should carry a proof");
    let result = ActivityResult::cancelled("implement_issue", "legacy host cleaned").with_artifact(
        ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({"status": "cleaned", "evidence_source": "runtime_host_cancellation_ack"}),
        ),
    );
    assert!(store
        .commit_cancelled_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &claimed.id,
            RuntimeJobCompletionLease::remote(
                "legacy-host",
                lease_expires_at,
                lease_generation,
                Some(reserved_proof),
            ),
            &result,
            None,
        )
        .await?
        .is_some());
    assert_eq!(
        store
            .get_runtime_job(&job.id)
            .await?
            .expect("acknowledged legacy eval should remain readable")
            .status,
        RuntimeJobStatus::Cancelled
    );
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("inactive command should remain readable")
            .status,
        WorkflowCommandStatus::Failed
    );
    Ok(())
}
