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
            owner,
            lease_expires_at,
            None,
            result,
        )
        .await
    }

    pub async fn commit_runtime_activity_completion_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        lease_generation: Option<u64>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        let mut tx = self.pool.begin().await?;
        let command_id_row: Option<(String,)> =
            sqlx::query_as("SELECT command_id FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((command_id,)) = command_id_row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let command_row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(&command_id)
        .fetch_optional(&mut *tx)
        .await?;
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
            && lease_generation.is_none_or(|generation| generation == job.lease_generation)
            && job.lease.as_ref().is_some_and(|lease| {
                lease.owner == owner
                    && lease.expires_at == lease_expires_at
                    && lease.expires_at > Utc::now()
            });
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
        let Some(command_row) = command_row else {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: None,
                workflow_event: None,
                decision: None,
            }));
        };
        let mut command = workflow_command_record_from_row(command_row)?;
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

        let (workflow_exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM workflow_instances WHERE id = $1)")
                .bind(&command.workflow_id)
                .fetch_one(&mut *tx)
                .await?;
        if !workflow_exists {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: Some(command),
                workflow_event: None,
                decision: None,
            }));
        }

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

        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_events:{}", command.workflow_id))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(&command.workflow_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = WorkflowEvent::new(
            &command.workflow_id,
            next_sequence as u64,
            "RuntimeJobCompleted",
            owner,
        )
        .with_payload(json!({
            "command_id": command.id,
            "command": command.command,
            "runtime_job_id": job.id,
            "runtime_job_status": job.status,
            "active_start_child_workflow_commands": active_start_child_workflow_commands,
            "activity_result": result,
        }));
        let event_data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO workflow_events
                (id, workflow_id, sequence, event_type, source, data)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.workflow_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&event.source)
        .bind(&event_data)
        .execute(&mut *tx)
        .await?;

        let decision_record = runtime_completion::apply_runtime_completion_decision_tx(
            &mut tx,
            &command.workflow_id,
            owner,
            &event,
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
        ActivityStatus::Blocked => WorkflowCommandStatus::Blocked,
        ActivityStatus::Cancelled => WorkflowCommandStatus::Cancelled,
    }
}
