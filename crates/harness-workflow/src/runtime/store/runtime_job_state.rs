use super::*;

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
}

pub(super) async fn cancel_unfinished_runtime_jobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
    activity: &str,
    summary: &str,
) -> anyhow::Result<usize> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, data::text FROM runtime_jobs
         WHERE command_id = $1 AND status IN ('pending', 'running') FOR UPDATE",
    )
    .bind(command_id)
    .fetch_all(&mut **tx)
    .await?;
    for (id, data) in &rows {
        let mut job: RuntimeJob = serde_json::from_str(data)?;
        job.complete(&ActivityResult::cancelled(activity, summary))?;
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
