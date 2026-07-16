use super::*;

type RuntimeJobCompactRecordRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
    DateTime<Utc>,
);
fn enum_from_str<T>(value: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(Value::String(value.to_string()))?)
}

impl WorkflowRuntimeStore {
    pub async fn runtime_jobs_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJob>>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT command_id, data::text FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             ORDER BY command_id ASC, created_at ASC",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (command_id, data) in rows {
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_command)
    }

    pub async fn command_sources_for_runtime_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, RuntimeJobCommandSource>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT job.id, command.workflow_id, command.id, command.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE job.id = ANY($1::text[])",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(runtime_job_id, workflow_id, command_id, data)| {
                let command = serde_json::from_str(&data)?;
                Ok((
                    runtime_job_id.clone(),
                    RuntimeJobCommandSource {
                        runtime_job_id,
                        workflow_id,
                        command_id,
                        command,
                    },
                ))
            })
            .collect()
    }

    pub async fn runtime_job_counts_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, usize>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT command_id, COUNT(*)
             FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             GROUP BY command_id",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(command_id, count)| (command_id, count.max(0) as usize))
            .collect())
    }

    pub async fn runtime_job_status_counts_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, usize>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*)
             FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             GROUP BY status
             ORDER BY status ASC",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(status, count)| (status, count.max(0) as usize))
            .collect())
    }

    pub async fn runtime_jobs_for_commands_limited(
        &self,
        command_ids: &[String],
        per_command_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJob>>> {
        if command_ids.is_empty() || per_command_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_command_limit = per_command_limit.clamp(1, 50);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT job.command_id, job.data
             FROM unnest($1::text[]) AS selected(command_id)
             JOIN LATERAL (
                 SELECT command_id, data::text AS data, created_at, (data->>'created_at')::timestamptz AS job_created_at
                 FROM runtime_jobs
                 WHERE command_id = selected.command_id
                 ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
                 LIMIT $2
             ) AS job ON true
             ORDER BY job.command_id ASC, job.created_at ASC, job.job_created_at ASC",
        )
        .bind(command_ids)
        .bind(per_command_limit)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (command_id, data) in rows {
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_command)
    }

    pub async fn compact_runtime_jobs_for_commands_limited(
        &self,
        command_ids: &[String],
        per_command_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJobCompactRecord>>> {
        if command_ids.is_empty() || per_command_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_command_limit = per_command_limit.clamp(1, 50);
        let rows: Vec<RuntimeJobCompactRecordRow> = sqlx::query_as(
            "SELECT job.command_id, job.id, job.runtime_kind, job.runtime_profile,
                    job.status, (job.data->'lease')::text AS lease,
                    COALESCE((job.data->>'lease_generation')::bigint, 0),
                    job.data->>'error',
                    job.data->>'failure_class',
                    (job.data->>'not_before')::timestamptz,
                    job.created_at, job.updated_at
             FROM unnest($1::text[]) AS selected(command_id)
             JOIN LATERAL (
                 SELECT command_id, id, runtime_kind, runtime_profile, status, data,
                        created_at, updated_at,
                        (data->>'created_at')::timestamptz AS job_created_at
                 FROM runtime_jobs
                 WHERE command_id = selected.command_id
                 ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
                 LIMIT $2
             ) AS job ON true
             ORDER BY job.command_id ASC, job.created_at ASC, job.job_created_at ASC",
        )
        .bind(command_ids)
        .bind(per_command_limit)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (
            command_id,
            id,
            runtime_kind,
            runtime_profile,
            status,
            lease_json,
            lease_generation,
            error,
            failure_class,
            not_before,
            created_at,
            updated_at,
        ) in rows
        {
            let lease = lease_json
                .as_deref()
                .filter(|value| *value != "null")
                .map(serde_json::from_str)
                .transpose()?;
            let record = RuntimeJobCompactRecord {
                id,
                command_id: command_id.clone(),
                runtime_kind: enum_from_str(&runtime_kind)?,
                runtime_profile,
                status: enum_from_str(&status)?,
                lease,
                lease_generation: lease_generation.max(0) as u64,
                error,
                failure_class,
                not_before,
                created_at,
                updated_at,
            };
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(record);
        }
        Ok(by_command)
    }

    pub async fn runtime_turns_started_for_workflow(
        &self,
        workflow_id: &str,
        exclude_runtime_job_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_events e
             JOIN runtime_jobs r ON r.id = e.runtime_job_id
             JOIN workflow_commands c ON c.id = r.command_id
             WHERE c.workflow_id = $1
               AND e.event_type = 'RuntimeTurnStarted'
               AND ($2::text IS NULL OR r.id <> $2)",
        )
        .bind(workflow_id)
        .bind(exclude_runtime_job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn runtime_job_count_by_status(
        &self,
        status: RuntimeJobStatus,
    ) -> anyhow::Result<i64> {
        let status = enum_str(&status)?;
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM runtime_jobs WHERE status = $1")
                .bind(&status)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}
