use super::*;
use crate::runtime::DataProvenance;

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowCancellationCleanupOutcome {
    Cleaned(Box<WorkflowInstance>),
    NoCancellationCommand,
    StaleInstance,
}

impl WorkflowRuntimeStore {
    pub async fn extend_runtime_job_lease_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        current_lease_expires_at: DateTime<Utc>,
        next_lease_expires_at: DateTime<Utc>,
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
            && job.lease.as_ref().is_some_and(|lease| {
                lease.owner == owner && lease.expires_at == current_lease_expires_at
            });
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        job.renew_lease(owner, next_lease_expires_at);
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
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn defer_runtime_job_claim_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        not_before: DateTime<Utc>,
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

        job.status = RuntimeJobStatus::Pending;
        job.lease = None;
        job.not_before = Some(not_before);
        job.updated_at = Utc::now();
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

    pub async fn record_runtime_job_failure_class(
        &self,
        runtime_job_id: &str,
        failure_class: &str,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.failure_class = Some(failure_class.to_string());
        job.updated_at = Utc::now();
        let updated = to_jsonb_string(&job)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET data = $1::jsonb, updated_at = $2
             WHERE id = $3",
        )
        .bind(&updated)
        .bind(job.updated_at)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn defer_ready_runtime_jobs_for_profile(
        &self,
        runtime_profile: &str,
        not_before: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE runtime_profile = $1
               AND status = 'pending'
               AND (not_before IS NULL OR not_before <= CURRENT_TIMESTAMP)
             ORDER BY created_at ASC
             FOR UPDATE",
        )
        .bind(runtime_profile)
        .fetch_all(&mut *tx)
        .await?;
        let now = Utc::now();
        let mut deferred = 0usize;
        for (id, data) in rows {
            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            job.not_before = Some(not_before);
            job.updated_at = now;
            let updated = to_jsonb_string(&job)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET not_before = $1, data = $2::jsonb, updated_at = $3
                 WHERE id = $4",
            )
            .bind(not_before)
            .bind(&updated)
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            deferred += 1;
        }
        tx.commit().await?;
        Ok(deferred)
    }

    pub async fn get_runtime_job(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn runtime_job_matches_running_lease(
        &self,
        expected: &RuntimeJob,
    ) -> anyhow::Result<bool> {
        let Some(expected_lease) = expected.lease.as_ref() else {
            return Ok(false);
        };
        let Some(current) = self.get_runtime_job(&expected.id).await? else {
            return Ok(false);
        };
        let Some(current_lease) = current.lease.as_ref() else {
            return Ok(false);
        };
        Ok(current.status == RuntimeJobStatus::Running
            && current.lease_generation == expected.lease_generation
            && current_lease.owner == expected_lease.owner
            && current_lease.expires_at >= expected_lease.expires_at)
    }

    pub async fn runtime_jobs_for_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Vec<RuntimeJob>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_jobs
             WHERE command_id = $1
             ORDER BY created_at ASC",
        )
        .bind(command_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn cancel_command_and_unfinished_runtime_jobs(
        &self,
        command_id: &str,
        activity: &str,
        summary: &str,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let _command_row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM workflow_commands WHERE id = $1 FOR UPDATE")
                .bind(command_id)
                .fetch_optional(&mut *tx)
                .await?;
        let cancelled =
            cancel_unfinished_runtime_jobs_tx(&mut tx, command_id, activity, summary).await?;
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $2,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL,
                 dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
               AND status IN ($3, $4, $5, $6)",
        )
        .bind(command_id)
        .bind(WorkflowCommandStatus::Cancelled.as_str())
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Dispatched.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(cancelled)
    }

    pub async fn finish_cancellation_cleanup_if_current(
        &self,
        expected: &WorkflowInstance,
        activity: &str,
        summary: &str,
    ) -> anyhow::Result<WorkflowCancellationCleanupOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(mut current) = select_instance_for_update_tx(&mut tx, &expected.id).await? else {
            tx.rollback().await?;
            return Ok(WorkflowCancellationCleanupOutcome::StaleInstance);
        };
        if current.state != expected.state || current.version != expected.version {
            tx.rollback().await?;
            return Ok(WorkflowCancellationCleanupOutcome::StaleInstance);
        }
        let original = current.clone();

        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, status, data::text FROM workflow_commands
             WHERE workflow_id = $1
             ORDER BY id
             FOR UPDATE",
        )
        .bind(&expected.id)
        .fetch_all(&mut *tx)
        .await?;
        let commands = rows
            .into_iter()
            .map(|(id, status, data)| {
                Ok((id, status, serde_json::from_str::<WorkflowCommand>(&data)?))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // The marker authorizing cleanup must belong to the exact accepted
        // decision that placed the instance in its current state (GH-1865).
        // Accepting any historical MarkCancelled row lets a stale generation's
        // cancellation — or a detached marker with no decision behind it, or a
        // superseded attempt — authorize cancelling the current generation's
        // live commands.
        if !self
            .cancellation_marker_is_current_tx(&mut tx, &current)
            .await?
        {
            tx.rollback().await?;
            return Ok(WorkflowCancellationCleanupOutcome::NoCancellationCommand);
        }

        let active_statuses = [
            WorkflowCommandStatus::Pending.as_str(),
            WorkflowCommandStatus::Dispatching.as_str(),
            WorkflowCommandStatus::Deferred.as_str(),
            WorkflowCommandStatus::Dispatched.as_str(),
        ];
        let cancellations = commands
            .iter()
            .filter(|(_, status, _)| active_statuses.contains(&status.as_str()))
            .map(|(command_id, _, _)| RuntimeJobCancellation::new(command_id, activity, summary))
            .collect::<Vec<_>>();
        if !cancellations.is_empty() {
            cancel_unfinished_runtime_jobs_for_commands_tx(&mut tx, &cancellations).await?;
            let command_ids = cancellations
                .iter()
                .map(|cancellation| cancellation.command_id.clone())
                .collect::<Vec<_>>();
            sqlx::query(
                "UPDATE workflow_commands
                 SET status = $2,
                     dispatch_owner = NULL,
                     dispatch_lease_expires_at = NULL,
                     dispatch_not_before = NULL,
                     dispatch_barrier = NULL,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = ANY($1::text[])",
            )
            .bind(&command_ids)
            .bind(WorkflowCommandStatus::Cancelled.as_str())
            .execute(&mut *tx)
            .await?;
        }
        let data_already_cancelled =
            current.data.get("cancelled").and_then(Value::as_bool) == Some(true);
        if cancellations.is_empty() && data_already_cancelled {
            tx.commit().await?;
            return Ok(WorkflowCancellationCleanupOutcome::Cleaned(Box::new(
                current,
            )));
        }
        if !current.data.is_object() {
            current.replace_classified_data(json!({}), DataProvenance::Server);
        }
        // Cancellation is a server decision about the workflow, never agent or
        // remote input, so the marker is server-classified.
        current.set_data_field("cancelled", Value::Bool(true), DataProvenance::Server)?;
        if current.data != original.data {
            current.version = current.version.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow instance `{}` version cannot advance during cancellation cleanup",
                    current.id
                )
            })?;
            commit_same_state_instance_tx(&mut tx, &original, &current).await?;
        }
        tx.commit().await?;
        Ok(WorkflowCancellationCleanupOutcome::Cleaned(Box::new(
            current,
        )))
    }

    /// Prove that the workflow's live cancellation marker was minted by the
    /// exact accepted decision that placed the instance in its current state.
    ///
    /// The latest accepted decision must target the instance's current state
    /// and carry a `MarkCancelled` command, and a live (non-superseded)
    /// command row linking that decision to the marker's dedupe key must
    /// exist. Anything weaker lets an older generation's marker — replayed,
    /// detached, or superseded — speak for the current generation.
    async fn cancellation_marker_is_current_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        current: &WorkflowInstance,
    ) -> anyhow::Result<bool> {
        let latest: Option<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM workflow_decisions
             WHERE workflow_id = $1 AND accepted
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(&current.id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((decision_id, data)) = latest else {
            return Ok(false);
        };
        let record: WorkflowDecisionRecord = serde_json::from_str(&data)?;
        let decision = record.decision;
        if decision.next_state != current.state {
            return Ok(false);
        }
        let marker_keys: Vec<String> = decision
            .commands
            .iter()
            .filter(|command| command.command_type == WorkflowCommandType::MarkCancelled)
            .map(|command| command.dedupe_key.clone())
            .collect();
        if marker_keys.is_empty() {
            return Ok(false);
        }
        let bound: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM workflow_commands
             WHERE workflow_id = $1
               AND decision_id = $2
               AND command_type = $3
               AND dedupe_key = ANY($4::text[])
               AND status <> $5
             LIMIT 1",
        )
        .bind(&current.id)
        .bind(&decision_id)
        .bind(WorkflowCommandType::MarkCancelled.as_str())
        .bind(&marker_keys)
        .bind(WorkflowCommandStatus::Superseded.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        Ok(bound.is_some())
    }
}

pub(super) struct RuntimeJobCancellation {
    command_id: String,
    activity: String,
    summary: String,
}

impl RuntimeJobCancellation {
    pub(super) fn new(
        command_id: impl Into<String>,
        activity: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            activity: activity.into(),
            summary: summary.into(),
        }
    }
}

pub(super) async fn cancel_unfinished_runtime_jobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
    activity: &str,
    summary: &str,
) -> anyhow::Result<usize> {
    cancel_unfinished_runtime_jobs_for_commands_tx(
        tx,
        &[RuntimeJobCancellation::new(command_id, activity, summary)],
    )
    .await
}

pub(super) async fn cancel_unfinished_runtime_jobs_for_commands_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cancellations: &[RuntimeJobCancellation],
) -> anyhow::Result<usize> {
    if cancellations.is_empty() {
        return Ok(0);
    }
    let command_ids = cancellations
        .iter()
        .map(|cancellation| cancellation.command_id.clone())
        .collect::<Vec<_>>();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, command_id, data::text FROM runtime_jobs
         WHERE command_id = ANY($1::text[]) AND status IN ('pending', 'running')
         ORDER BY id
         FOR UPDATE",
    )
    .bind(&command_ids)
    .fetch_all(&mut **tx)
    .await?;
    for (id, command_id, data) in &rows {
        let cancellation = cancellations
            .iter()
            .find(|cancellation| cancellation.command_id == *command_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "runtime job `{id}` was locked without a cancellation for command \
                     `{command_id}`"
                )
            })?;
        let mut job: RuntimeJob = serde_json::from_str(data)?;
        job.complete(&ActivityResult::cancelled(
            &cancellation.activity,
            &cancellation.summary,
        ))?;
        sqlx::query(
            "UPDATE runtime_jobs SET status = $1, not_before = $2, data = $3::jsonb,
                updated_at = CURRENT_TIMESTAMP WHERE id = $4",
        )
        .bind(enum_str(&job.status)?)
        .bind(job.not_before)
        .bind(to_jsonb_string(&job)?)
        .bind(id)
        .execute(&mut **tx)
        .await?;
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(tx, id, job.lease_generation)
            .await?;
    }
    Ok(rows.len())
}
