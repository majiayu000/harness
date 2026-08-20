use super::*;
use crate::runtime::store::runtime_job_leases::{
    postgres_timestamp_floor, RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRejection,
    RuntimeJobLeaseRenewalRequest,
};
use crate::runtime::RuntimeJobCompletionLease;
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
    let expires_at =
        postgres_timestamp_floor(Utc::now() - Duration::seconds(1)) + Duration::nanoseconds(999);
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

    let now = Utc::now();
    let lease_proof = store
        .remote_runtime_job_lease_proof(&claimed.id, "host-1", claimed.lease_generation, expires_at)
        .await?;
    let renewal = |renewal_id| RuntimeJobLeaseRenewalRequest {
        runtime_job_id: &claimed.id,
        owner: "host-1",
        lease_generation: claimed.lease_generation,
        lease_proof,
        previous_expires_at: expires_at,
        renewal_id,
        lease_secs: 60,
        now,
        max_lease_secs: 3_600,
        owner_active: false,
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
    let lease_proof = store
        .remote_runtime_job_lease_proof(&claimed.id, "host-1", lease_generation, lease_expires_at)
        .await?;

    let result = ActivityResult::cancelled("implement_issue", "host stopped and cleaned")
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({"status": "cleaned", "evidence_source": "runtime_host_cancellation_ack"}),
        ));
    let completion = store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &claimed.id,
            RuntimeJobCompletionLease::remote(
                "host-1",
                lease_expires_at,
                lease_generation,
                lease_proof,
            ),
            &result,
        )
        .await?;
    let completion = completion.expect("cleanup acknowledgement should complete the runtime job");
    assert!(completion.decision.is_none());
    assert!(completion.workflow_event.is_none());
    let unchanged = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should remain readable before its cancellation transition");
    assert_eq!(unchanged.version, workflow.version);
    assert_eq!(unchanged.state, workflow.state);
    let acknowledged = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("acknowledged eval job should remain readable"))?;
    assert_eq!(acknowledged.status, RuntimeJobStatus::Cancelled);
    Ok(())
}

#[tokio::test]
async fn late_cancellation_ack_does_not_decide_reopened_workflow() -> anyhow::Result<()> {
    if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:late-cancel-ack"),
    )
    .with_id("late-cancellation-ack")
    .with_server_data(json!({
        "eval": {"eval_run_id": "run-late", "case_id": "case-late"},
    }));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "late-cancellation-command",
        json!({
            "activity": "implement_issue",
            "eval": {"eval_run_id": "run-late", "case_id": "case-late"},
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
    let expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-1", expires_at)
        .await?
        .expect("eval job should be claimed");
    let proof = store
        .remote_runtime_job_lease_proof(&claimed.id, "host-1", claimed.lease_generation, expires_at)
        .await?
        .expect("claimed lease should carry a proof");
    store
        .cancel_command_and_unfinished_runtime_jobs(
            &command_id,
            "implement_issue",
            "operator cancelled",
        )
        .await?;

    let mut reopened = workflow.clone();
    reopened.version = workflow.version + 1;
    reopened.state = "implementing".to_string();
    reopened.updated_at = Utc::now();
    store
        .force_upsert_lifecycle_state_for_test(&reopened)
        .await?;
    let replacement_id = Uuid::new_v4().to_string();
    let mut replacement = command.clone();
    replacement.dedupe_key = command.dedupe_key.clone();
    let mut tx = store.pool().begin().await?;
    sqlx::query(
        "UPDATE workflow_commands
         SET status = 'superseded', superseded_by_command_id = $2
         WHERE id = $1",
    )
    .bind(&command_id)
    .bind(&replacement_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO workflow_commands
            (id, workflow_id, command_type, dedupe_key, status, data,
             attempt_generation)
         VALUES ($1, $2, 'enqueue_activity', $3, 'pending', $4::jsonb, 2)",
    )
    .bind(&replacement_id)
    .bind(&workflow.id)
    .bind(&replacement.dedupe_key)
    .bind(serde_json::to_string(&replacement)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let result = ActivityResult::cancelled("implement_issue", "host stopped and cleaned")
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({"status": "cleaned", "evidence_source": "runtime_host_cancellation_ack"}),
        ));
    let completion = store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &job.id,
            RuntimeJobCompletionLease::remote(
                "host-1",
                expires_at,
                claimed.lease_generation,
                Some(proof),
            ),
            &result,
        )
        .await?
        .expect("cleanup acknowledgement should finalize the old runtime job");
    assert!(completion.decision.is_none());
    assert!(completion.workflow_event.is_none());
    let old_command = store
        .get_command(&command_id)
        .await?
        .expect("superseded command should remain readable");
    assert_eq!(old_command.status, WorkflowCommandStatus::Superseded);
    assert_eq!(
        old_command.superseded_by_command_id.as_deref(),
        Some(replacement_id.as_str())
    );
    assert_eq!(
        store
            .get_command(&replacement_id)
            .await?
            .expect("replacement command should remain readable")
            .status,
        WorkflowCommandStatus::Pending
    );
    let runtime_events = store.runtime_events_for(&job.id).await?;
    assert!(runtime_events
        .iter()
        .any(|event| event.event_type == "StaleCancellationAcknowledgementRecorded"));
    let current = store
        .get_instance(&workflow.id)
        .await?
        .expect("reopened workflow should remain readable");
    assert_eq!(current.version, reopened.version);
    assert_eq!(current.state, reopened.state);
    Ok(())
}
