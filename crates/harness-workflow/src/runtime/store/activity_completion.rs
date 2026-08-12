use super::*;

impl WorkflowRuntimeStore {
    pub async fn complete_runtime_job_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let is_current_lease = job.status == RuntimeJobStatus::Running
            && job
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner == owner && lease.expires_at == lease_expires_at);
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        job.complete(result)?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(
            &mut tx,
            runtime_job_id,
            job.lease_generation,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn commit_runtime_activity_completion_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_if_owned_with_generation(
            runtime_job_id,
            RuntimeJobCompletionLease::local(owner, lease_expires_at),
            result,
        )
        .await
    }

    pub async fn commit_runtime_activity_completion_with_transcript_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(
            runtime_job_id,
            RuntimeJobCompletionLease::local(owner, lease_expires_at),
            result,
            transcript,
        )
        .await
    }

    pub async fn commit_runtime_activity_completion_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(runtime_job_id, lease, result, None)
            .await
    }

    pub async fn commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(runtime_job_id, lease, result, transcript)
            .await
    }

    /// Runs the completion transaction, re-running it when PostgreSQL aborts
    /// it to break a lock cycle. Retrying is safe: an aborted transaction
    /// applied nothing, so the attempt starts from the same committed state.
    ///
    /// The lock order fix below makes this abort rare rather than routine —
    /// this exists so a residual conflict costs a retry instead of failing the
    /// runtime job.
    async fn commit_runtime_activity_completion_retrying(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        lock_order::retry_on_transaction_abort("commit_runtime_activity_completion", || {
            self.commit_runtime_activity_completion_inner(runtime_job_id, lease, result, transcript)
        })
        .await
    }

    async fn commit_runtime_activity_completion_inner(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        // Resolve the keys of the rows this transaction will lock BEFORE it
        // opens, with plain reads that take no row locks. `command_id` and
        // `workflow_id` are immutable for the life of a job/command, so an
        // unlocked read of them cannot go stale in a way that matters — and it
        // lets the transaction below take its locks parent-first. See
        // `lock_order` for the canonical order.
        let command_id_row: Option<(String,)> =
            sqlx::query_as("SELECT command_id FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((command_id,)) = command_id_row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let workflow_id_row: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(&command_id)
                .fetch_optional(&self.pool)
                .await?;

        let mut tx = self.pool.begin().await?;
        // Lock order 1/3: the workflow instance. `apply_runtime_completion_decision_tx`
        // needs this lock at the end of the transaction; taking it here instead
        // keeps this path from inverting the order used by command dispatch.
        // A missing instance is fine — the no-workflow path below owns that
        // case, and locking an absent row is a no-op.
        let locked_workflow = if let Some((workflow_id,)) = workflow_id_row.as_ref() {
            transaction_helpers::select_instance_for_update_tx(&mut tx, workflow_id).await?
        } else {
            None
        };
        // Lock order 2/3: the command.
        let command_row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at,
                    attempt_generation, superseded_by_command_id
             FROM workflow_commands
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(&command_id)
        .fetch_optional(&mut *tx)
        .await?;
        // Lock order 3/3: the runtime job.
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let mut is_current_lease = job.status == RuntimeJobStatus::Running
            && lease
                .generation
                .is_none_or(|generation| generation == job.lease_generation)
            && job.lease.as_ref().is_some_and(|current| {
                current.owner == lease.owner
                    && runtime_job_leases::postgres_timestamp_floor(current.expires_at)
                        == runtime_job_leases::postgres_timestamp_floor(lease.expires_at)
                    && current.expires_at > Utc::now()
            });
        if is_current_lease && job.runtime_kind == RuntimeKind::RemoteHost {
            is_current_lease = match lease.generation {
                Some(generation) => {
                    runtime_job_leases::remote_runtime_job_lease_proof_matches_tx(
                        &mut tx,
                        runtime_job_id,
                        lease.owner,
                        generation,
                        lease.expires_at,
                        lease.proof,
                    )
                    .await?
                }
                None => false,
            };
        }
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        let mut command = command_row
            .map(workflow_command_record_from_row)
            .transpose()?;
        if let Some(transcript) = transcript {
            let command = command.as_ref().ok_or_else(|| {
                anyhow::anyhow!("runtime transcript cannot be persisted without its command")
            })?;
            artifacts::insert_runtime_transcript_tx(
                &mut tx,
                &command.workflow_id,
                runtime_job_id,
                transcript,
            )
            .await?;
        }

        job.complete(result)?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(
            &mut tx,
            runtime_job_id,
            job.lease_generation,
        )
        .await?;
        let Some(mut command) = command.take() else {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: None,
                workflow_event: None,
                decision: None,
            }));
        };
        let command_status = command_status_for_activity(result.status);
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(command_status.as_str())
        .bind(&command.id)
        .execute(&mut *tx)
        .await?;
        command.status = command_status;
        command.dispatch_owner = None;
        command.dispatch_lease_expires_at = None;

        let Some(locked_workflow) = locked_workflow else {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: Some(command),
                workflow_event: None,
                decision: None,
            }));
        };

        let active_start_child_workflow_commands =
            if command.command.command_type == WorkflowCommandType::StartChildWorkflow {
                let command_type = enum_str(&WorkflowCommandType::StartChildWorkflow)?;
                let (count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM workflow_commands
                     WHERE workflow_id = $1
                       AND id <> $2
                       AND command_type = $3
                       AND status IN ($4, $5, $6, $7)",
                )
                .bind(&command.workflow_id)
                .bind(&command.id)
                .bind(&command_type)
                .bind(WorkflowCommandStatus::Pending.as_str())
                .bind(WorkflowCommandStatus::Dispatching.as_str())
                .bind(WorkflowCommandStatus::Dispatched.as_str())
                .bind(WorkflowCommandStatus::Deferred.as_str())
                .fetch_one(&mut *tx)
                .await?;
                count as usize
            } else {
                0
            };

        let event = transaction_helpers::insert_event_tx(
            &mut tx,
            &command.workflow_id,
            "RuntimeJobCompleted",
            lease.owner,
            json!({
                "command_id": command.id,
                "command": command.command,
                "runtime_job_id": job.id,
                "runtime_job_status": job.status,
                "active_start_child_workflow_commands": active_start_child_workflow_commands,
                "activity_result": result,
            }),
        )
        .await?;

        let decision_record = runtime_completion::apply_runtime_completion_decision_tx(
            &mut tx,
            &command.workflow_id,
            lease.owner,
            &event,
            &self.budget_policy,
        )
        .await?;

        super::evidence::record_runtime_completion_evidence_tx(
            &mut tx,
            &locked_workflow,
            &command,
            &job,
            &event,
            result,
            decision_record.as_ref(),
        )
        .await?;

        tx.commit().await?;
        if let Some(decision) = decision_record.as_ref() {
            self.record_terminal_repo_memory_for_completion(&event, decision)
                .await;
        }
        Ok(Some(RuntimeActivityCompletion {
            runtime_job: job,
            command: Some(command),
            workflow_event: Some(event),
            decision: decision_record,
        }))
    }
}

fn command_status_for_activity(status: ActivityStatus) -> WorkflowCommandStatus {
    match status {
        ActivityStatus::Succeeded => WorkflowCommandStatus::Completed,
        ActivityStatus::Failed => WorkflowCommandStatus::Failed,
        ActivityStatus::Blocked | ActivityStatus::SucceededWithBlockers => {
            WorkflowCommandStatus::Blocked
        }
        ActivityStatus::Cancelled => WorkflowCommandStatus::Cancelled,
    }
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
        if insert_lease_expired_completion_tx(
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
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_remote_stale_completion_if_issued(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<bool> {
        let result_json = serde_json::to_value(result)?;
        let transcript_json = transcript
            .map(|pending| serde_json::to_value(&pending.record))
            .transpose()?;
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
        let Some(lease_generation) = lease.generation else {
            tx.commit().await?;
            return Ok(false);
        };
        let same_expired_generation = current.lease_generation == lease_generation
            && current.status == RuntimeJobStatus::Running
            && current
                .lease
                .as_ref()
                .is_some_and(|lease| lease.expires_at <= Utc::now());
        let reclaimed_generation = current.lease_generation > lease_generation;
        if current.runtime_kind != RuntimeKind::RemoteHost
            || (!same_expired_generation && !reclaimed_generation)
            || !runtime_job_leases::remote_runtime_job_lease_proof_matches_tx(
                &mut tx,
                runtime_job_id,
                lease.owner,
                lease_generation,
                lease.expires_at,
                lease.proof,
            )
            .await?
        {
            tx.commit().await?;
            return Ok(false);
        }
        let inserted = insert_lease_expired_completion_tx(
            &mut tx,
            runtime_job_id,
            lease.owner,
            lease_generation,
            lease.expires_at,
            &result_json,
            transcript_json.as_ref(),
        )
        .await?;
        if inserted {
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
        Ok(true)
    }
}

async fn insert_lease_expired_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    runtime_job_id: &str,
    owner: &str,
    lease_generation: u64,
    lease_expires_at: DateTime<Utc>,
    result_json: &serde_json::Value,
    transcript_json: Option<&serde_json::Value>,
) -> anyhow::Result<bool> {
    // The DLQ holds at most one record per job (id = runtime_job_id):
    // reconciliation sees exactly one pending decision per job.
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
    Ok(inserted == 1)
}
