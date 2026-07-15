use super::*;
use crate::runtime::store::runtime_job_leases::{
    postgres_timestamp_ceil, RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRejection,
    RuntimeJobLeaseRenewalRequest,
};
use tokio::sync::Barrier;
use uuid::Uuid;

static REMOTE_LEASE_DB_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn remote_lease_store() -> anyhow::Result<WorkflowRuntimeStore> {
    if std::env::var_os("HARNESS_DATABASE_URL").is_none() {
        anyhow::bail!(
            "GH1602 PostgreSQL tests require HARNESS_DATABASE_URL pointing to an isolated disposable database"
        );
    }
    let dir = tempfile::tempdir()?;
    WorkflowRuntimeStore::open(&dir.path().join("remote-host-lease.db")).await
}

async fn enqueue_remote_lease_job(
    store: &WorkflowRuntimeStore,
    key: &str,
) -> anyhow::Result<RuntimeJob> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", format!("issue:{key}")),
    )
    .with_id(format!("remote-lease-{key}"));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("remote_check", format!("remote-{key}"));
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

async fn persist_runtime_job(store: &WorkflowRuntimeStore, job: &RuntimeJob) -> anyhow::Result<()> {
    let status = serde_json::to_value(job.status)?
        .as_str()
        .expect("runtime job status must serialize as a string")
        .to_string();
    let runtime_kind = serde_json::to_value(job.runtime_kind)?
        .as_str()
        .expect("runtime kind must serialize as a string")
        .to_string();
    sqlx::query(
        "UPDATE runtime_jobs
         SET status = $1, runtime_kind = $2, not_before = $3, data = $4::jsonb, updated_at = $5
         WHERE id = $6",
    )
    .bind(status)
    .bind(runtime_kind)
    .bind(job.not_before)
    .bind(serde_json::to_string(job)?)
    .bind(job.updated_at)
    .bind(&job.id)
    .execute(store.pool())
    .await?;
    Ok(())
}

fn renewal<'a>(
    job: &'a RuntimeJob,
    owner: &'a str,
    previous_expires_at: DateTime<Utc>,
    renewal_id: Uuid,
    lease_secs: i64,
    now: DateTime<Utc>,
) -> RuntimeJobLeaseRenewalRequest<'a> {
    RuntimeJobLeaseRenewalRequest {
        runtime_job_id: &job.id,
        owner,
        lease_generation: job.lease_generation,
        previous_expires_at,
        renewal_id,
        lease_secs,
        now,
        max_lease_secs: 3_600,
        owner_active: true,
    }
}

fn renewed_expiry(outcome: RuntimeJobLeaseRenewalOutcome) -> (DateTime<Utc>, bool) {
    match outcome {
        RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_expires_at,
            replayed,
            ..
        } => (lease_expires_at, replayed),
        other => panic!("expected successful renewal, got {other:?}"),
    }
}

fn assert_rejected(
    outcome: RuntimeJobLeaseRenewalOutcome,
    expected: RuntimeJobLeaseRenewalRejection,
) {
    assert_eq!(
        outcome,
        RuntimeJobLeaseRenewalOutcome::LeaseLost { reason: expected }
    );
}

#[tokio::test]
async fn runtime_store_remote_host_lease_ttl_receipts_and_fences() -> anyhow::Result<()> {
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = remote_lease_store().await?;
    let now = Utc::now()
        .with_nanosecond(123_456_789)
        .expect("fixed nanosecond component must be valid");
    let original_expiry = now + Duration::seconds(60);
    let pending = enqueue_remote_lease_job(&store, "ttl-receipts").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", original_expiry)
        .await?
        .expect("remote job should be claimed");
    assert_eq!(claimed.id, pending.id);

    assert!(store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-b",
            now + Duration::seconds(90),
        )
        .await?
        .is_none());

    let renewal_id = Uuid::new_v4();
    let (short_target_expiry, replayed) = renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                original_expiry,
                renewal_id,
                1,
                now,
            ))
            .await?,
    );
    assert!(short_target_expiry >= original_expiry);
    assert_eq!(short_target_expiry.nanosecond() % 1_000, 0);
    assert!(!replayed);

    let (replayed_expiry, replayed) = renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                original_expiry,
                renewal_id,
                1,
                now,
            ))
            .await?,
    );
    assert_eq!(replayed_expiry, short_target_expiry);
    assert_eq!(
        serde_json::to_string(&replayed_expiry)?,
        serde_json::to_string(&short_target_expiry)?
    );
    assert!(replayed);

    let same_microsecond_expiry = original_expiry
        .with_nanosecond(original_expiry.nanosecond() / 1_000 * 1_000 + 1)
        .expect("same-microsecond timestamp must be valid");
    let (precision_replay_expiry, replayed) = renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                same_microsecond_expiry,
                renewal_id,
                1,
                now,
            ))
            .await?,
    );
    assert_eq!(precision_replay_expiry, short_target_expiry);
    assert!(replayed);

    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                original_expiry,
                renewal_id,
                2,
                now,
            ))
            .await?,
        RuntimeJobLeaseRenewalRejection::RenewalIdConflict,
    );

    let extended_id = Uuid::new_v4();
    let (extended_expiry, _) = renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                short_target_expiry,
                extended_id,
                120,
                now,
            ))
            .await?,
    );
    assert_eq!(
        Some(extended_expiry),
        postgres_timestamp_ceil(now + Duration::seconds(120))
    );

    let mut wrong_generation =
        renewal(&claimed, "host-a", extended_expiry, Uuid::new_v4(), 60, now);
    wrong_generation.lease_generation += 1;
    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(wrong_generation)
            .await?,
        RuntimeJobLeaseRenewalRejection::WrongGeneration,
    );
    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-b",
                extended_expiry,
                Uuid::new_v4(),
                60,
                now,
            ))
            .await?,
        RuntimeJobLeaseRenewalRejection::WrongOwner,
    );
    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                original_expiry,
                Uuid::new_v4(),
                60,
                now,
            ))
            .await?,
        RuntimeJobLeaseRenewalRejection::StaleExpiry,
    );

    let events = store.runtime_events_for(&claimed.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRenewed")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRenewalRejected")
            .count(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_remote_host_lease_reclaim_cleans_receipts_and_fences_generation(
) -> anyhow::Result<()> {
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = remote_lease_store().await?;
    let now = Utc::now();
    let expiry = now + Duration::seconds(60);
    let pending = enqueue_remote_lease_job(&store, "same-host-reclaim").await?;
    let first = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expiry)
        .await?
        .expect("first claim should succeed");
    let renewal_id = Uuid::new_v4();
    renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &first, "host-a", expiry, renewal_id, 120, now,
            ))
            .await?,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM runtime_job_lease_renewal_receipts WHERE runtime_job_id = $1",
        )
        .bind(&pending.id)
        .fetch_one(store.pool())
        .await?,
        1
    );

    assert_eq!(
        store
            .revoke_remote_host_runtime_job_leases("host-a", now)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM runtime_job_lease_renewal_receipts WHERE runtime_job_id = $1",
        )
        .bind(&pending.id)
        .fetch_one(store.pool())
        .await?,
        0
    );

    let second_expiry = now + Duration::seconds(180);
    let second = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", second_expiry)
        .await?
        .expect("same host should reclaim revoked job");
    assert_eq!(second.lease_generation, first.lease_generation + 1);

    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &first,
                "host-a",
                expiry,
                Uuid::new_v4(),
                60,
                now,
            ))
            .await?,
        RuntimeJobLeaseRenewalRejection::WrongGeneration,
    );
    let stale_completion = store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &first.id,
            "host-a",
            expiry,
            Some(first.lease_generation),
            &ActivityResult::succeeded("remote_check", "done"),
        )
        .await?;
    assert!(stale_completion.is_none());

    let events = store.runtime_events_for(&first.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRevoked")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_remote_host_lease_concurrent_renew_and_complete_have_one_winner(
) -> anyhow::Result<()> {
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = remote_lease_store().await?;
    let store = Arc::new(store);
    let now = Utc::now();
    let expiry = now + Duration::seconds(60);
    let pending = enqueue_remote_lease_job(&store, "renew-complete-race").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expiry)
        .await?
        .expect("job should be claimed");
    assert_eq!(claimed.id, pending.id);

    let barrier = Arc::new(Barrier::new(3));
    let renew_store = store.clone();
    let renew_barrier = barrier.clone();
    let renew_job = claimed.clone();
    let renew = tokio::spawn(async move {
        renew_barrier.wait().await;
        renew_store
            .renew_remote_host_runtime_job_lease(renewal(
                &renew_job,
                "host-a",
                expiry,
                Uuid::new_v4(),
                120,
                now,
            ))
            .await
    });
    let complete_store = store.clone();
    let complete_barrier = barrier.clone();
    let complete_job = claimed.clone();
    let complete = tokio::spawn(async move {
        complete_barrier.wait().await;
        complete_store
            .commit_runtime_activity_completion_if_owned_with_generation(
                &complete_job.id,
                "host-a",
                expiry,
                Some(complete_job.lease_generation),
                &ActivityResult::succeeded("remote_check", "done"),
            )
            .await
    });
    barrier.wait().await;

    let renewed = matches!(renew.await??, RuntimeJobLeaseRenewalOutcome::Renewed { .. });
    let completed = complete.await??.is_some();
    assert_ne!(renewed, completed, "exactly one fenced operation must win");

    let persisted = store
        .get_runtime_job(&claimed.id)
        .await?
        .expect("job should remain readable");
    if renewed {
        assert_eq!(persisted.status, RuntimeJobStatus::Running);
        assert_eq!(
            persisted.lease.map(|lease| lease.expires_at),
            postgres_timestamp_ceil(now + Duration::seconds(120))
        );
    } else {
        assert_eq!(persisted.status, RuntimeJobStatus::Succeeded);
        assert!(persisted.lease.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn runtime_store_remote_host_lease_concurrent_renew_and_reclaim_have_one_mutation_winner(
) -> anyhow::Result<()> {
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = Arc::new(remote_lease_store().await?);
    let now = Utc::now();
    let expired_at = now - Duration::seconds(1);
    enqueue_remote_lease_job(&store, "renew-reclaim-race").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expired_at)
        .await?
        .expect("job should be initially claimed");

    let barrier = Arc::new(Barrier::new(3));
    let renew_store = store.clone();
    let renew_barrier = barrier.clone();
    let renew_job = claimed.clone();
    let renew = tokio::spawn(async move {
        renew_barrier.wait().await;
        renew_store
            .renew_remote_host_runtime_job_lease(renewal(
                &renew_job,
                "host-a",
                expired_at,
                Uuid::new_v4(),
                60,
                now,
            ))
            .await
    });
    let reclaim_store = store.clone();
    let reclaim_barrier = barrier.clone();
    let reclaim = tokio::spawn(async move {
        reclaim_barrier.wait().await;
        reclaim_store
            .claim_next_runtime_job_for_runtime_kind(
                RuntimeKind::RemoteHost,
                "host-b",
                now + Duration::seconds(60),
            )
            .await
    });
    barrier.wait().await;

    let renewal_outcome = renew.await??;
    assert!(matches!(
        renewal_outcome,
        RuntimeJobLeaseRenewalOutcome::LeaseLost {
            reason: RuntimeJobLeaseRenewalRejection::Expired
                | RuntimeJobLeaseRenewalRejection::WrongGeneration
                | RuntimeJobLeaseRenewalRejection::WrongOwner
        }
    ));
    let reclaimed = reclaim.await??.expect("expired job must be reclaimed");
    assert_eq!(reclaimed.lease_generation, claimed.lease_generation + 1);
    assert_eq!(
        reclaimed.lease.as_ref().map(|lease| lease.owner.as_str()),
        Some("host-b")
    );

    let persisted = store
        .get_runtime_job(&claimed.id)
        .await?
        .expect("reclaimed job must remain readable");
    assert_eq!(persisted.lease_generation, reclaimed.lease_generation);
    assert_eq!(persisted.lease, reclaimed.lease);
    let events = store.runtime_events_for(&claimed.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobReclaimed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "RuntimeJobLeaseRenewalRejected")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_remote_host_lease_expired_receipt_is_cleaned_before_rejection(
) -> anyhow::Result<()> {
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = remote_lease_store().await?;
    let now = Utc::now();
    let expiry = now + Duration::seconds(1);
    enqueue_remote_lease_job(&store, "receipt-expiry").await?;
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expiry)
        .await?
        .expect("job should be claimed");
    let renewal_id = Uuid::new_v4();
    renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed, "host-a", expiry, renewal_id, 1, now,
            ))
            .await?,
    );

    let normalized_expiry = postgres_timestamp_ceil(expiry)
        .expect("ordinary test timestamp must normalize to PostgreSQL precision");
    assert_rejected(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                expiry,
                renewal_id,
                1,
                normalized_expiry,
            ))
            .await?,
        RuntimeJobLeaseRenewalRejection::Expired,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM runtime_job_lease_renewal_receipts WHERE runtime_job_id = $1",
        )
        .bind(&claimed.id)
        .fetch_one(store.pool())
        .await?,
        0
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum FenceCase {
    Draining,
    Expired,
    Revoked,
    WrongRuntimeKind,
    WrongState,
    InvalidZeroDuration,
    InvalidOversizedDuration,
    OverHorizon,
}

impl FenceCase {
    fn name(self) -> &'static str {
        match self {
            Self::Draining => "draining",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::WrongRuntimeKind => "wrong-runtime-kind",
            Self::WrongState => "wrong-state",
            Self::InvalidZeroDuration => "invalid-zero-duration",
            Self::InvalidOversizedDuration => "invalid-oversized-duration",
            Self::OverHorizon => "over-horizon",
        }
    }

    fn rejection(self) -> RuntimeJobLeaseRenewalRejection {
        match self {
            Self::Draining => RuntimeJobLeaseRenewalRejection::HostDraining,
            Self::Expired => RuntimeJobLeaseRenewalRejection::Expired,
            Self::Revoked => RuntimeJobLeaseRenewalRejection::Revoked,
            Self::WrongRuntimeKind => RuntimeJobLeaseRenewalRejection::WrongRuntimeKind,
            Self::WrongState => RuntimeJobLeaseRenewalRejection::WrongState,
            Self::InvalidZeroDuration | Self::InvalidOversizedDuration => {
                RuntimeJobLeaseRenewalRejection::InvalidDuration
            }
            Self::OverHorizon => RuntimeJobLeaseRenewalRejection::InvariantViolation,
        }
    }
}

#[tokio::test]
async fn runtime_store_remote_host_lease_full_state_and_horizon_fence_matrix() -> anyhow::Result<()>
{
    let _db_guard = REMOTE_LEASE_DB_TEST_LOCK.lock().await;
    let store = remote_lease_store().await?;
    let now = Utc::now();
    let cases = [
        FenceCase::Draining,
        FenceCase::Expired,
        FenceCase::Revoked,
        FenceCase::WrongRuntimeKind,
        FenceCase::WrongState,
        FenceCase::InvalidZeroDuration,
        FenceCase::InvalidOversizedDuration,
        FenceCase::OverHorizon,
    ];

    for case in cases {
        let initial_expiry = match case {
            FenceCase::Expired => now - Duration::seconds(1),
            FenceCase::OverHorizon => now + Duration::seconds(3_601),
            _ => now + Duration::seconds(60),
        };
        enqueue_remote_lease_job(&store, case.name()).await?;
        let mut claimed = store
            .claim_next_runtime_job_for_runtime_kind(
                RuntimeKind::RemoteHost,
                "host-a",
                initial_expiry,
            )
            .await?
            .expect("matrix job should be claimed");
        match case {
            FenceCase::Revoked => claimed.lease = None,
            FenceCase::WrongRuntimeKind => claimed.runtime_kind = RuntimeKind::CodexJsonrpc,
            FenceCase::WrongState => claimed.status = RuntimeJobStatus::Succeeded,
            _ => {}
        }
        if matches!(
            case,
            FenceCase::Revoked | FenceCase::WrongRuntimeKind | FenceCase::WrongState
        ) {
            persist_runtime_job(&store, &claimed).await?;
        }

        let lease_secs = match case {
            FenceCase::InvalidZeroDuration => 0,
            FenceCase::InvalidOversizedDuration => 3_601,
            _ => 60,
        };
        let mut request = renewal(
            &claimed,
            "host-a",
            initial_expiry,
            Uuid::new_v4(),
            lease_secs,
            now,
        );
        request.owner_active = !matches!(case, FenceCase::Draining);
        assert_rejected(
            store.renew_remote_host_runtime_job_lease(request).await?,
            case.rejection(),
        );

        let persisted = store
            .get_runtime_job(&claimed.id)
            .await?
            .expect("rejected matrix job must remain readable");
        assert_eq!(persisted.status, claimed.status);
        assert_eq!(persisted.runtime_kind, claimed.runtime_kind);
        assert_eq!(persisted.lease, claimed.lease);
        let events = store.runtime_events_for(&claimed.id).await?;
        let rejection = events
            .last()
            .expect("known-job rejection must append an event");
        assert_eq!(rejection.event_type, "RuntimeJobLeaseRenewalRejected");
        assert_eq!(rejection.event["reason"], case.rejection().as_str());
    }

    enqueue_remote_lease_job(&store, "maximum-ttl").await?;
    let expiry = now + Duration::seconds(60);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expiry)
        .await?
        .expect("maximum-TTL job should be claimed");
    let (maximum_expiry, _) = renewed_expiry(
        store
            .renew_remote_host_runtime_job_lease(renewal(
                &claimed,
                "host-a",
                expiry,
                Uuid::new_v4(),
                3_600,
                now,
            ))
            .await?,
    );
    assert_eq!(
        Some(maximum_expiry),
        postgres_timestamp_ceil(now + Duration::seconds(3_600))
    );
    Ok(())
}
