use super::*;
use crate::runtime::store::runtime_job_leases::{
    RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRejection, RuntimeJobLeaseRenewalRequest,
};
use crate::runtime::{ActivityArtifact, RuntimeJobClaimDeferOutcome};
use uuid::Uuid;

#[tokio::test]
async fn running_remote_eval_waits_for_host_cleanup_acknowledgement() -> anyhow::Result<()> {
    if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("eval-cancellation-ack")
    .with_server_data(json!({
        "eval": {"eval_run_id": "run-1", "case_id": "case-1"},
    }));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "eval-cancellation-command",
        json!({
            "activity": "implement_issue",
            "eval": {"eval_run_id": "run-1", "case_id": "case-1"},
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
    let expires_at = Utc::now() - Duration::seconds(1);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-1", expires_at)
        .await?
        .ok_or_else(|| anyhow::anyhow!("eval job should be claimed"))?;

    store
        .cancel_command_and_unfinished_runtime_jobs(
            &command_id,
            "implement_issue",
            "operator cancelled",
        )
        .await?;
    let requested = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("eval job should remain readable"))?;
    assert_eq!(requested.status, RuntimeJobStatus::Running);
    assert!(requested.input.get("cancellation_requested").is_some());
    assert!(matches!(
        store
            .defer_runtime_job_claim_if_owned(
                &job.id,
                "host-1",
                expires_at,
                Utc::now() + Duration::minutes(1),
            )
            .await?,
        RuntimeJobClaimDeferOutcome::CancellationRequested(_)
    ));
    assert_eq!(
        store
            .revoke_remote_host_runtime_job_leases("host-1", Utc::now())
            .await?,
        0
    );
    let preserved = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cancelled eval lease should remain acknowledged-owned"))?;
    assert_eq!(preserved.status, RuntimeJobStatus::Running);
    assert!(preserved.lease.is_some());
    assert!(store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-2",
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .is_none());

    let stale_job_only_completion = store
        .complete_runtime_job_if_owned(
            &claimed.id,
            "host-1",
            expires_at,
            &ActivityResult::succeeded("implement_issue", "stale job-only success"),
        )
        .await?;
    assert!(
        stale_job_only_completion.is_none(),
        "legacy job-only completion must honor the cancellation fence"
    );

    let stale_completion = store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &claimed.id,
            "host-1",
            expires_at,
            Some(claimed.lease_generation),
            &ActivityResult::succeeded("implement_issue", "stale success after cancellation"),
        )
        .await?;
    assert!(
        stale_completion.is_none(),
        "ordinary completion must not cross a requested cancellation fence"
    );

    let now = Utc::now();
    let renewal = |renewal_id| RuntimeJobLeaseRenewalRequest {
        runtime_job_id: &claimed.id,
        owner: "host-1",
        lease_generation: claimed.lease_generation,
        previous_expires_at: expires_at,
        renewal_id,
        lease_secs: 60,
        now,
        max_lease_secs: 3_600,
        owner_active: true,
    };
    assert!(matches!(
        store
            .renew_remote_host_runtime_job_lease(renewal(Uuid::new_v4()))
            .await?,
        RuntimeJobLeaseRenewalOutcome::LeaseLost {
            reason: RuntimeJobLeaseRenewalRejection::CancellationRequested,
        }
    ));
    let reserved = store
        .reserve_cancelled_remote_host_runtime_job_completion(renewal(Uuid::new_v4()))
        .await?;
    let RuntimeJobLeaseRenewalOutcome::Renewed {
        lease_expires_at,
        lease_generation,
        ..
    } = reserved
    else {
        anyhow::bail!("cancellation cleanup acknowledgement reservation was rejected")
    };

    let result = ActivityResult::cancelled("implement_issue", "host stopped and cleaned")
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({"status": "cleaned", "evidence_source": "runtime_host_cancellation_ack"}),
        ));
    let completion = store
        .commit_cancelled_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &claimed.id,
            "host-1",
            lease_expires_at,
            lease_generation,
            &result,
            None,
        )
        .await?;
    assert!(completion.is_some());
    let acknowledged = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("acknowledged eval job should remain readable"))?;
    assert_eq!(acknowledged.status, RuntimeJobStatus::Cancelled);
    Ok(())
}

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
    let expired_at = Utc::now() - Duration::seconds(1);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "legacy-host", expired_at)
        .await?
        .expect("legacy eval should be claimed before terminalization");
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
    let result = ActivityResult::cancelled("implement_issue", "legacy host cleaned").with_artifact(
        ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({"status": "cleaned", "evidence_source": "runtime_host_cancellation_ack"}),
        ),
    );
    assert!(store
        .commit_cancelled_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &claimed.id,
            "legacy-host",
            lease_expires_at,
            lease_generation,
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
