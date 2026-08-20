use super::*;
use crate::runtime::store::runtime_job_leases::{
    postgres_timestamp_ceil, postgres_timestamp_floor, RuntimeJobLeaseRenewalOutcome,
    RuntimeJobLeaseRenewalRejection, RuntimeJobLeaseRenewalRequest,
};
use crate::runtime::RuntimeJobCompletionLease;
use uuid::Uuid;

async fn proof_store() -> anyhow::Result<(tempfile::TempDir, WorkflowRuntimeStore)> {
    if resolve_database_url(None).is_err() {
        anyhow::bail!(
            "GH1602 PostgreSQL tests require HARNESS_DATABASE_URL pointing to an isolated disposable database"
        );
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-lease-proof.db")).await?;
    Ok((dir, store))
}

async fn enqueue_proof_job(store: &WorkflowRuntimeStore, key: &str) -> anyhow::Result<RuntimeJob> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", format!("proof:{key}")),
    )
    .with_id(format!("remote-proof-{key}"));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let command = WorkflowCommand::enqueue_activity("remote_check", format!("proof-{key}"));
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({ "activity": "remote_check" }),
        )
        .await
}

async fn downgrade_store_to_v27(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    sqlx::query("DROP TRIGGER IF EXISTS trg_enforce_remote_lease_proof_writer ON runtime_jobs")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP FUNCTION IF EXISTS enforce_remote_lease_proof_writer()")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP TRIGGER IF EXISTS trg_runtime_job_lease_issuance ON runtime_jobs")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP FUNCTION IF EXISTS record_runtime_job_lease_issuance()")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP TABLE IF EXISTS runtime_job_lease_issuances")
        .execute(store.pool())
        .await?;
    sqlx::query("ALTER TABLE runtime_job_lease_renewal_receipts DROP COLUMN legacy_proofless")
        .execute(store.pool())
        .await?;
    sqlx::query("ALTER TABLE runtime_job_completions_dlq DROP COLUMN lease_generation")
        .execute(store.pool())
        .await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 28")
        .execute(store.pool())
        .await?;
    Ok(())
}

fn renewal<'a>(
    job: &'a RuntimeJob,
    owner: &'a str,
    proof: Option<Uuid>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> RuntimeJobLeaseRenewalRequest<'a> {
    RuntimeJobLeaseRenewalRequest {
        runtime_job_id: &job.id,
        owner,
        lease_generation: job.lease_generation,
        lease_proof: proof,
        previous_expires_at: expires_at,
        renewal_id: Uuid::new_v4(),
        lease_secs: 60,
        now,
        max_lease_secs: 3_600,
        owner_active: true,
    }
}

#[tokio::test]
async fn remote_lease_rejects_forged_proof_for_renewal_and_completion() -> anyhow::Result<()> {
    let (_dir, store) = proof_store().await?;
    let now = Utc::now();
    let expires_at = now + Duration::minutes(5);
    let pending = enqueue_proof_job(&store, "forged").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expires_at)
        .await?
        .expect("remote job should be claimed");
    assert_eq!(claimed.id, pending.id);
    let proof = store
        .remote_runtime_job_lease_proof(&claimed.id, "host-a", claimed.lease_generation, expires_at)
        .await?
        .expect("post-migration claims must have a proof");

    let rejected = store
        .renew_remote_host_runtime_job_lease(renewal(
            &claimed,
            "host-a",
            Some(Uuid::new_v4()),
            expires_at,
            now,
        ))
        .await?;
    assert_eq!(
        rejected,
        RuntimeJobLeaseRenewalOutcome::LeaseLost {
            reason: RuntimeJobLeaseRenewalRejection::WrongProof,
        }
    );
    assert!(store
        .complete_runtime_job_if_owned(
            &claimed.id,
            "host-a",
            expires_at,
            &ActivityResult::succeeded("remote_check", "unfenced"),
        )
        .await?
        .is_none());
    assert!(store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &claimed.id,
            RuntimeJobCompletionLease::remote(
                "host-a",
                expires_at,
                claimed.lease_generation,
                Some(Uuid::new_v4()),
            ),
            &ActivityResult::succeeded("remote_check", "forged"),
        )
        .await?
        .is_none());
    assert!(store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &claimed.id,
            RuntimeJobCompletionLease::remote(
                "host-a",
                expires_at,
                claimed.lease_generation,
                Some(proof),
            ),
            &ActivityResult::succeeded("remote_check", "verified"),
        )
        .await?
        .is_some());
    Ok(())
}

#[tokio::test]
async fn issued_stale_completion_is_dead_lettered_with_generation() -> anyhow::Result<()> {
    let (_dir, store) = proof_store().await?;
    let expired_at = Utc::now() - Duration::seconds(1);
    enqueue_proof_job(&store, "stale-dlq").await?;
    let first = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expired_at)
        .await?
        .expect("remote job should be claimed");
    let proof = store
        .remote_runtime_job_lease_proof(&first.id, "host-a", first.lease_generation, expired_at)
        .await?
        .expect("post-migration claims must have a proof");
    let reclaimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-b",
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .expect("expired lease should be reclaimed");
    assert!(reclaimed.lease_generation > first.lease_generation);

    let lease = RuntimeJobCompletionLease::remote(
        "host-a",
        expired_at,
        first.lease_generation,
        Some(proof),
    );
    let result = ActivityResult::succeeded("remote_check", "finished on stale lease");
    let inserted = store
        .record_remote_stale_completion_if_issued(&first.id, lease, &result, None)
        .await?;
    assert!(inserted);
    assert!(
        store
            .record_remote_stale_completion_if_issued(&first.id, lease, &result, None)
            .await?,
        "an exact response-loss replay must report the durable dead letter"
    );
    assert!(
        !store
            .record_remote_stale_completion_if_issued(
                &first.id,
                lease,
                &ActivityResult::succeeded("remote_check", "conflicting stale result"),
                None,
            )
            .await?,
        "a different result for the same job must not overwrite the first dead letter"
    );
    let (generation,): (Option<i64>,) = sqlx::query_as(
        "SELECT lease_generation FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&first.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(generation, Some(first.lease_generation as i64));
    let events = store.runtime_events_for(&first.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "LeaseExpiredCompletionRecorded")
            .count(),
        1,
        "an exact replay must not append a duplicate audit event"
    );
    Ok(())
}

#[tokio::test]
async fn revoked_issued_completion_is_dead_lettered_without_restoring_ownership(
) -> anyhow::Result<()> {
    let (_dir, store) = proof_store().await?;
    let expires_at = Utc::now() + Duration::minutes(5);
    enqueue_proof_job(&store, "revoked-dlq").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expires_at)
        .await?
        .expect("remote job should be claimed");
    let proof = store
        .remote_runtime_job_lease_proof(&claimed.id, "host-a", claimed.lease_generation, expires_at)
        .await?
        .expect("post-migration claims must have a proof");
    assert_eq!(
        store
            .revoke_remote_host_runtime_job_leases("host-a", Utc::now())
            .await?,
        1
    );
    assert!(
        store
            .record_remote_stale_completion_if_issued(
                &claimed.id,
                RuntimeJobCompletionLease::remote(
                    "host-a",
                    expires_at,
                    claimed.lease_generation,
                    Some(proof),
                ),
                &ActivityResult::succeeded("remote_check", "finished before revocation"),
                None,
            )
            .await?
    );
    let persisted = store
        .get_runtime_job(&claimed.id)
        .await?
        .expect("revoked job should remain readable");
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    Ok(())
}

#[tokio::test]
async fn v27_running_lease_allows_one_proofless_rotation() -> anyhow::Result<()> {
    let (dir, store) = proof_store().await?;
    downgrade_store_to_v27(&store).await?;

    let now = Utc::now();
    let legacy_expires_at = now + Duration::hours(2);
    enqueue_proof_job(&store, "legacy-v27").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "legacy-host",
            legacy_expires_at,
        )
        .await?
        .expect("legacy remote job should be claimed");
    drop(store);

    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-lease-proof.db")).await?;
    assert!(store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-host",
            claimed.lease_generation,
            legacy_expires_at,
        )
        .await?
        .is_none());
    let renewed = store
        .renew_remote_host_runtime_job_lease(renewal(
            &claimed,
            "legacy-host",
            None,
            legacy_expires_at,
            now,
        ))
        .await?;
    let RuntimeJobLeaseRenewalOutcome::Renewed {
        lease_expires_at, ..
    } = renewed
    else {
        anyhow::bail!("legacy proofless lease should rotate successfully");
    };
    let proof = store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-host",
            claimed.lease_generation,
            lease_expires_at,
        )
        .await?
        .expect("legacy renewal must rotate to a proof-bearing issuance");
    let rejected = store
        .renew_remote_host_runtime_job_lease(renewal(
            &claimed,
            "legacy-host",
            None,
            lease_expires_at,
            now,
        ))
        .await?;
    assert!(matches!(
        rejected,
        RuntimeJobLeaseRenewalOutcome::LeaseLost {
            reason: RuntimeJobLeaseRenewalRejection::WrongProof,
        }
    ));
    assert!(matches!(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "legacy-host",
                Some(proof),
                lease_expires_at,
                now + Duration::minutes(61),
            ))
            .await?,
        RuntimeJobLeaseRenewalOutcome::Renewed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn v27_renewal_response_loss_replays_after_v28_migration() -> anyhow::Result<()> {
    let (dir, store) = proof_store().await?;
    downgrade_store_to_v27(&store).await?;

    let now = Utc::now();
    let previous_expires_at = now + Duration::minutes(5);
    enqueue_proof_job(&store, "legacy-renewal-replay").await?;
    let mut claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "legacy-host",
            previous_expires_at,
        )
        .await?
        .expect("legacy remote job should be claimed");
    let renewal_id = Uuid::new_v4();
    let renewed_expires_at = postgres_timestamp_ceil(now + Duration::minutes(10))
        .ok_or_else(|| anyhow::anyhow!("legacy renewal expiry should normalize"))?;

    // Simulate the v27 renewal SQL committing before its response is lost.
    // v27 has no issuance table or proof fields to write.
    claimed.renew_lease("legacy-host", renewed_expires_at);
    sqlx::query(
        "UPDATE runtime_jobs
         SET data = $1::jsonb, updated_at = $2
         WHERE id = $3",
    )
    .bind(serde_json::to_string(&claimed)?)
    .bind(now)
    .bind(&claimed.id)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO runtime_job_lease_renewal_receipts
            (runtime_job_id, renewal_id, owner, lease_generation,
             previous_expires_at, renewed_expires_at, lease_secs, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&claimed.id)
    .bind(renewal_id)
    .bind("legacy-host")
    .bind(i64::try_from(claimed.lease_generation)?)
    .bind(postgres_timestamp_floor(previous_expires_at))
    .bind(renewed_expires_at)
    .bind(300_i64)
    .bind(now)
    .execute(store.pool())
    .await?;
    drop(store);

    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-lease-proof.db")).await?;
    let replayed = store
        .renew_remote_host_runtime_job_lease(RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &claimed.id,
            owner: "legacy-host",
            lease_generation: claimed.lease_generation,
            lease_proof: None,
            previous_expires_at,
            renewal_id,
            lease_secs: 300,
            now,
            max_lease_secs: 3_600,
            owner_active: true,
        })
        .await?;
    assert!(matches!(
        replayed,
        RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_expires_at,
            replayed: true,
            ..
        } if lease_expires_at == postgres_timestamp_floor(renewed_expires_at)
    ));
    assert!(store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-host",
            claimed.lease_generation,
            renewed_expires_at,
        )
        .await?
        .is_some());
    Ok(())
}

#[tokio::test]
async fn v28_migration_rejects_continuing_v27_lease_writers() -> anyhow::Result<()> {
    let (dir, store) = proof_store().await?;
    downgrade_store_to_v27(&store).await?;
    let pending = enqueue_proof_job(&store, "mixed-version-barrier").await?;
    drop(store);

    let store = WorkflowRuntimeStore::open(&dir.path().join("remote-lease-proof.db")).await?;
    let mut legacy_claim = pending.clone();
    legacy_claim.claim("legacy-host", Utc::now() + Duration::minutes(5));
    let old_writer_error = sqlx::query(
        "UPDATE runtime_jobs
         SET status = 'running', data = $1::jsonb, updated_at = CURRENT_TIMESTAMP
         WHERE id = $2",
    )
    .bind(serde_json::to_string(&legacy_claim)?)
    .bind(&pending.id)
    .execute(store.pool())
    .await
    .expect_err("v27 claim writer must fail closed after v28 migration");
    assert_eq!(
        old_writer_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55000"))
    );

    let expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "proof-aware-host",
            expires_at,
        )
        .await?
        .expect("v28 claim should pass the database rollout barrier");
    let proof = store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "proof-aware-host",
            claimed.lease_generation,
            expires_at,
        )
        .await?
        .expect("v28 claim should issue a proof");
    let mut legacy_completion = claimed.clone();
    legacy_completion.complete(&ActivityResult::succeeded("remote_check", "legacy"))?;
    let old_completion_error = sqlx::query(
        "UPDATE runtime_jobs
         SET status = 'succeeded', data = $1::jsonb, updated_at = CURRENT_TIMESTAMP
         WHERE id = $2",
    )
    .bind(serde_json::to_string(&legacy_completion)?)
    .bind(&claimed.id)
    .execute(store.pool())
    .await
    .expect_err("v27 completion writer must fail closed after v28 migration");
    assert_eq!(
        old_completion_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55000"))
    );
    assert!(store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &claimed.id,
            RuntimeJobCompletionLease::remote(
                "proof-aware-host",
                expires_at,
                claimed.lease_generation,
                Some(proof),
            ),
            &ActivityResult::succeeded("remote_check", "proof-aware"),
        )
        .await?
        .is_some());
    Ok(())
}

#[tokio::test]
async fn v28_cancels_running_non_eval_remote_job_through_writer_barrier() -> anyhow::Result<()> {
    let (_dir, store) = proof_store().await?;
    let pending = enqueue_proof_job(&store, "non-eval-cancellation").await?;
    store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .expect("remote job should be claimed");
    assert_eq!(
        store
            .cancel_command_and_unfinished_runtime_jobs(
                &pending.command_id,
                "remote_check",
                "operator cancelled",
            )
            .await?,
        1
    );
    let cancelled = store
        .get_runtime_job(&pending.id)
        .await?
        .expect("cancelled remote job should remain readable");
    assert_eq!(cancelled.status, RuntimeJobStatus::Cancelled);
    Ok(())
}

#[tokio::test]
async fn v28_migration_locks_runtime_jobs_before_renewal_receipts() -> anyhow::Result<()> {
    let (dir, store) = proof_store().await?;
    downgrade_store_to_v27(&store).await?;
    let job = enqueue_proof_job(&store, "migration-lock-order").await?;
    let path = dir.path().join("remote-lease-proof.db");

    let mut old_claim = store.pool().begin().await?;
    let (old_claim_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *old_claim)
        .await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .execute(&mut *old_claim)
        .await?;

    let migration = tokio::spawn(async move { WorkflowRuntimeStore::open(&path).await });
    let mut migration_waits_for_runtime_jobs = false;
    for _ in 0..100 {
        migration_waits_for_runtime_jobs = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                 FROM pg_stat_activity
                 WHERE $1 = ANY(pg_blocking_pids(pid))
                   AND query LIKE '%LOCK TABLE runtime_jobs IN ACCESS EXCLUSIVE MODE%'
             )",
        )
        .bind(old_claim_backend)
        .fetch_one(store.pool())
        .await?;
        if migration_waits_for_runtime_jobs {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        migration_waits_for_runtime_jobs,
        "v28 migration did not reach the runtime_jobs lock barrier"
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sqlx::query("DELETE FROM runtime_job_lease_renewal_receipts WHERE runtime_job_id = $1")
            .bind(&job.id)
            .execute(&mut *old_claim),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "v28 migration locked renewal receipts before the old claim released runtime_jobs"
        )
    })??;
    old_claim.commit().await?;

    let migration_result = tokio::time::timeout(std::time::Duration::from_secs(10), migration)
        .await
        .map_err(|_| {
            anyhow::anyhow!("v28 migration did not finish after the old claim committed")
        })?;
    let reopened = migration_result??;
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
             WHERE table_name = 'runtime_job_lease_renewal_receipts'
               AND column_name = 'legacy_proofless'
         )",
        )
        .fetch_one(reopened.pool())
        .await?
    );
    Ok(())
}
