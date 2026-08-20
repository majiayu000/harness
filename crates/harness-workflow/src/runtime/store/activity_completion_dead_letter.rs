use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteStaleCompletionOutcome {
    CancellationRequested,
    DeadLettered,
    Rejected,
}

impl WorkflowRuntimeStore {
    /// Durably record a completed activity result that the worker can no
    /// longer commit because its job lease expired or was reclaimed
    /// (GH-1878). The payload — result plus optional transcript — lands in
    /// `runtime_job_completions_dlq` instead of being dropped, so no finished
    /// agent work vanishes silently; reconciliation can decide later whether
    /// the result is still applicable or the job must re-run.
    pub async fn record_lease_expired_completion(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_generation: u64,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
        transcript: Option<&crate::runtime::transcript::PendingRuntimeTranscript>,
    ) -> anyhow::Result<()> {
        const MAX_ATTEMPTS: usize = 3;

        let mut last_error = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self
                .record_lease_expired_completion_once(
                    runtime_job_id,
                    owner,
                    lease_generation,
                    lease_expires_at,
                    result,
                    transcript,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64))
                            .await;
                    }
                }
            }
        }
        match last_error {
            Some(error) => Err(error).context(format!(
                "failed to record lease-expired completion after {MAX_ATTEMPTS} attempts"
            )),
            None => anyhow::bail!("dead-letter retry loop did not execute"),
        }
    }

    async fn record_lease_expired_completion_once(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_generation: u64,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
        transcript: Option<&crate::runtime::transcript::PendingRuntimeTranscript>,
    ) -> anyhow::Result<()> {
        let result_json = serde_json::to_value(result)?;
        let transcript_json = transcript
            .map(|pending| serde_json::to_value(&pending.record))
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        match insert_lease_expired_completion_tx(
            &mut tx,
            runtime_job_id,
            owner,
            lease_generation,
            lease_expires_at,
            &result_json,
            transcript_json.as_ref(),
        )
        .await?
        {
            LeaseExpiredCompletionInsertOutcome::Inserted => {
                runtime_job_leases::append_runtime_event_tx(
                    &mut tx,
                    runtime_job_id,
                    "LeaseExpiredCompletionRecorded",
                    serde_json::json!({
                        "owner": owner,
                        "lease_generation": lease_generation,
                        "lease_expires_at": lease_expires_at,
                        "applied": false,
                    }),
                )
                .await?;
            }
            LeaseExpiredCompletionInsertOutcome::Replayed => {}
            LeaseExpiredCompletionInsertOutcome::Conflict => {
                anyhow::bail!(
                    "conflicting lease-expired completion already exists for runtime job {runtime_job_id}"
                );
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_remote_stale_completion_if_issued(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<RemoteStaleCompletionOutcome> {
        let lease_generation = lease
            .generation
            .ok_or_else(|| anyhow::anyhow!("remote stale completion requires a generation"))?;
        let result_json = serde_json::to_value(result)?;
        let transcript_json = transcript
            .map(|pending| serde_json::to_value(&pending.record))
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        let workflow: Option<(String,)> = sqlx::query_as(
            "SELECT workflow.id
             FROM workflow_instances AS workflow
             JOIN workflow_commands AS command ON command.workflow_id = workflow.id
             JOIN runtime_jobs AS job ON job.command_id = command.id
             WHERE job.id = $1
             FOR UPDATE OF workflow",
        )
        .bind(runtime_job_id)
        .fetch_optional(&mut *tx)
        .await?;
        if workflow.is_none() {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        }
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let current: RuntimeJob = serde_json::from_str(&data)?;
        if current_remote_eval_cancellation_matches_lease_tx(
            &mut tx,
            runtime_job_id,
            &current,
            lease,
        )
        .await?
        {
            tx.commit().await?;
            return Ok(RemoteStaleCompletionOutcome::CancellationRequested);
        }
        if !remote_stale_completion_is_issued_tx(
            &mut tx,
            runtime_job_id,
            &current,
            lease,
            Utc::now(),
        )
        .await?
        {
            tx.commit().await?;
            return Ok(RemoteStaleCompletionOutcome::Rejected);
        }
        let insert_outcome = insert_lease_expired_completion_tx(
            &mut tx,
            runtime_job_id,
            lease.owner,
            lease_generation,
            lease.expires_at,
            &result_json,
            transcript_json.as_ref(),
        )
        .await?;
        if insert_outcome == LeaseExpiredCompletionInsertOutcome::Inserted {
            runtime_job_leases::append_runtime_event_tx(
                &mut tx,
                runtime_job_id,
                "LeaseExpiredCompletionRecorded",
                serde_json::json!({
                    "owner": lease.owner,
                    "lease_generation": lease_generation,
                    "lease_expires_at": lease.expires_at,
                    "applied": false,
                    "source": "runtime_host",
                }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(
            if insert_outcome == LeaseExpiredCompletionInsertOutcome::Conflict {
                RemoteStaleCompletionOutcome::Rejected
            } else {
                RemoteStaleCompletionOutcome::DeadLettered
            },
        )
    }

    pub async fn remote_stale_completion_is_issued(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let current: RuntimeJob = serde_json::from_str(&data)?;
        let issued = remote_stale_completion_is_issued_tx(
            &mut tx,
            runtime_job_id,
            &current,
            lease,
            Utc::now(),
        )
        .await?;
        tx.commit().await?;
        Ok(issued)
    }
}

async fn current_remote_eval_cancellation_matches_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    runtime_job_id: &str,
    current: &RuntimeJob,
    lease: RuntimeJobCompletionLease<'_>,
) -> anyhow::Result<bool> {
    let Some(lease_generation) = lease.generation else {
        return Ok(false);
    };
    let matches_current_owner = current.lease_generation == lease_generation
        && current
            .lease
            .as_ref()
            .is_some_and(|current_lease| current_lease.owner == lease.owner);
    if current.status != RuntimeJobStatus::Running
        || current.runtime_kind != RuntimeKind::RemoteHost
        || current.input.get("cancellation_requested").is_none()
        || (current.input.get("eval").is_none() && current.input.pointer("/command/eval").is_none())
        || !matches_current_owner
    {
        return Ok(false);
    }
    runtime_job_leases::remote_runtime_job_lease_proof_matches_tx(
        tx,
        runtime_job_id,
        lease.owner,
        lease_generation,
        lease.expires_at,
        lease.proof,
    )
    .await
}

async fn remote_stale_completion_is_issued_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    runtime_job_id: &str,
    current: &RuntimeJob,
    lease: RuntimeJobCompletionLease<'_>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(lease_generation) = lease.generation else {
        return Ok(false);
    };
    let same_expired_generation = current.lease_generation == lease_generation
        && current.status == RuntimeJobStatus::Running
        && current
            .lease
            .as_ref()
            .is_some_and(|current_lease| current_lease.expires_at <= now);
    let revoked_generation = current.lease_generation == lease_generation
        && current.status == RuntimeJobStatus::Pending
        && current.lease.is_none();
    let reclaimed_generation = current.lease_generation > lease_generation;
    if current.runtime_kind != RuntimeKind::RemoteHost
        || (!same_expired_generation && !revoked_generation && !reclaimed_generation)
    {
        return Ok(false);
    }
    runtime_job_leases::remote_runtime_job_lease_proof_matches_tx(
        tx,
        runtime_job_id,
        lease.owner,
        lease_generation,
        lease.expires_at,
        lease.proof,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseExpiredCompletionInsertOutcome {
    Inserted,
    Replayed,
    Conflict,
}

async fn insert_lease_expired_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    runtime_job_id: &str,
    owner: &str,
    lease_generation: u64,
    lease_expires_at: DateTime<Utc>,
    result_json: &serde_json::Value,
    transcript_json: Option<&serde_json::Value>,
) -> anyhow::Result<LeaseExpiredCompletionInsertOutcome> {
    let lease_generation = i64::try_from(lease_generation)
        .map_err(|_| anyhow::anyhow!("runtime job lease generation exceeds BIGINT"))?;
    let inserted = sqlx::query(
        "INSERT INTO runtime_job_completions_dlq
            (id, runtime_job_id, owner, lease_generation, lease_expires_at, result, transcript)
         VALUES ($1, $1, $2, $3, $4, $5::jsonb, $6::jsonb)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(runtime_job_id)
    .bind(owner)
    .bind(lease_generation)
    .bind(lease_expires_at)
    .bind(result_json)
    .bind(transcript_json)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted > 0 {
        return Ok(LeaseExpiredCompletionInsertOutcome::Inserted);
    }
    let replayed: Option<(bool,)> = sqlx::query_as(
        "SELECT owner = $2
                AND lease_generation IS NOT DISTINCT FROM $3
                AND lease_expires_at = $4
                AND result = $5::jsonb
                AND transcript IS NOT DISTINCT FROM $6::jsonb
         FROM runtime_job_completions_dlq
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(runtime_job_id)
    .bind(owner)
    .bind(lease_generation)
    .bind(lease_expires_at)
    .bind(result_json)
    .bind(transcript_json)
    .fetch_optional(&mut **tx)
    .await?;
    match replayed {
        Some((true,)) => Ok(LeaseExpiredCompletionInsertOutcome::Replayed),
        Some((false,)) => Ok(LeaseExpiredCompletionInsertOutcome::Conflict),
        None => anyhow::bail!(
            "runtime completion dead-letter conflict disappeared for runtime job {runtime_job_id}"
        ),
    }
}
