use super::activity_completion_terminal::{
    cancellation_ack_matches, command_status_is_active,
    lock_workflow_commands_for_terminal_fence_tx, lock_workflow_runtime_jobs_for_terminal_fence_tx,
};
use super::*;

#[derive(Clone, Copy)]
enum RuntimeCompletionKind {
    Normal,
    CancellationAck,
}

#[derive(Clone, Copy)]
struct RuntimeActivityCompletionRequest<'a> {
    runtime_job_id: &'a str,
    lease: RuntimeJobCompletionLease<'a>,
    result: &'a ActivityResult,
    transcript: Option<&'a PendingRuntimeTranscript>,
    completion_kind: RuntimeCompletionKind,
}

impl WorkflowRuntimeStore {
    pub async fn complete_runtime_job_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let workflow_id_row: Option<(String,)> = sqlx::query_as(
            "SELECT command.workflow_id
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE job.id = $1",
        )
        .bind(runtime_job_id)
        .fetch_optional(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        let locked_workflow = if let Some((workflow_id,)) = workflow_id_row.as_ref() {
            transaction_helpers::select_instance_for_update_tx(&mut tx, workflow_id).await?
        } else {
            None
        };
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let workflow_is_terminal = if let Some(workflow) = locked_workflow.as_ref() {
            definitions::terminal_state_for_instance_tx(
                &mut tx,
                &self.definition_registry,
                workflow,
            )
            .await?
            .is_some()
        } else {
            false
        };
        let is_current_lease = !workflow_is_terminal
            && job.input.get("cancellation_requested").is_none()
            && job.status == RuntimeJobStatus::Running
            && job.runtime_kind != RuntimeKind::RemoteHost
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
        self.commit_runtime_activity_completion_retrying(RuntimeActivityCompletionRequest {
            runtime_job_id,
            lease: RuntimeJobCompletionLease::local(owner, lease_expires_at),
            result,
            transcript,
            completion_kind: RuntimeCompletionKind::Normal,
        })
        .await
    }

    pub async fn commit_runtime_activity_completion_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(RuntimeActivityCompletionRequest {
            runtime_job_id,
            lease,
            result,
            transcript: None,
            completion_kind: RuntimeCompletionKind::Normal,
        })
        .await
    }

    pub async fn commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(RuntimeActivityCompletionRequest {
            runtime_job_id,
            lease,
            result,
            transcript,
            completion_kind: RuntimeCompletionKind::Normal,
        })
        .await
    }

    /// Commit the explicit cleanup acknowledgement for a cancelled remote eval.
    ///
    /// Once the terminal fence requests cancellation, only a cancelled result
    /// carrying the runtime-host cleanup proof may consume the preserved lease.
    pub async fn commit_cancelled_runtime_activity_completion_with_transcript_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        lease: RuntimeJobCompletionLease<'_>,
        result: &ActivityResult,
        transcript: Option<&PendingRuntimeTranscript>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        self.commit_runtime_activity_completion_retrying(RuntimeActivityCompletionRequest {
            runtime_job_id,
            lease,
            result,
            transcript,
            completion_kind: RuntimeCompletionKind::CancellationAck,
        })
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
        request: RuntimeActivityCompletionRequest<'_>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        lock_order::retry_on_transaction_abort("commit_runtime_activity_completion", || {
            self.commit_runtime_activity_completion_inner(request)
        })
        .await
    }

    async fn commit_runtime_activity_completion_inner(
        &self,
        request: RuntimeActivityCompletionRequest<'_>,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        let RuntimeActivityCompletionRequest {
            runtime_job_id,
            lease,
            result,
            transcript,
            completion_kind,
        } = request;
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
        if let (Some(_), Some((workflow_id,))) =
            (locked_workflow.as_ref(), workflow_id_row.as_ref())
        {
            lock_workflow_commands_for_terminal_fence_tx(&mut tx, workflow_id).await?;
            lock_workflow_runtime_jobs_for_terminal_fence_tx(&mut tx, workflow_id).await?;
        }
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
        let workflow_is_terminal = if let Some(workflow) = locked_workflow.as_ref() {
            definitions::terminal_state_for_instance_tx(
                &mut tx,
                &self.definition_registry,
                workflow,
            )
            .await?
            .is_some()
        } else {
            false
        };
        let completion_kind_matches = match completion_kind {
            RuntimeCompletionKind::Normal => {
                !workflow_is_terminal && job.input.get("cancellation_requested").is_none()
            }
            RuntimeCompletionKind::CancellationAck => cancellation_ack_matches(&job, result),
        };
        let mut is_current_lease = completion_kind_matches
            && job.status == RuntimeJobStatus::Running
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
        let stale_cancellation_ack = match (command.as_ref(), locked_workflow.as_ref()) {
            (Some(command), Some(workflow)) => {
                cancellation_ack_is_stale_for_workflow(&job, command, workflow, result)
            }
            _ => false,
        };
        if let Some(transcript) = transcript.filter(|_| !stale_cancellation_ack) {
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
        if job.runtime_kind == RuntimeKind::RemoteHost {
            runtime_job_leases::mark_remote_lease_proof_v1_tx(&mut tx).await?;
        }
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
        runtime_job_leases::append_runtime_event_tx(
            &mut tx,
            runtime_job_id,
            "ActivityResultReady",
            serde_json::to_value(result)?,
        )
        .await?;
        if stale_cancellation_ack {
            runtime_job_leases::append_runtime_event_tx(
                &mut tx,
                runtime_job_id,
                "StaleCancellationAcknowledgementRecorded",
                json!({
                    "owner": lease.owner,
                    "command_id": command.as_ref().map(|command| &command.id),
                    "workflow_id": command.as_ref().map(|command| &command.workflow_id),
                    "applied_to_workflow": false,
                }),
            )
            .await?;
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command,
                workflow_event: None,
                decision: None,
            }));
        }
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
        let preserve_inactive_command =
            matches!(completion_kind, RuntimeCompletionKind::CancellationAck)
                && !command_status_is_active(command.status);
        if !preserve_inactive_command {
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
        }

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
            &self.definition_registry,
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

fn cancellation_ack_is_stale_for_workflow(
    job: &RuntimeJob,
    command: &WorkflowCommandRecord,
    workflow: &WorkflowInstance,
    result: &ActivityResult,
) -> bool {
    if result.status != ActivityStatus::Cancelled {
        return false;
    }
    let Some(marker) = job.input.get("cancellation_requested") else {
        return false;
    };
    let mismatches = |key, expected| {
        marker
            .get(key)
            .is_some_and(|value| value.as_u64() != Some(expected))
    };
    mismatches("workflow_version", workflow.version)
        || mismatches(
            "command_attempt_generation",
            u64::from(command.attempt_generation),
        )
        || command.status != WorkflowCommandStatus::Dispatched
        || command.superseded_by_command_id.is_some()
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

#[cfg(test)]
#[path = "activity_completion_tests.rs"]
mod tests;
