use super::*;

type RuntimeEventSummaryRow = (
    String,
    i64,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
impl WorkflowRuntimeStore {
    pub async fn record_runtime_event(
        &self,
        runtime_job_id: &str,
        event_type: &str,
        payload: Value,
    ) -> anyhow::Result<RuntimeEvent> {
        let mut tx = self.pool.begin().await?;
        let event = runtime_job_leases::append_runtime_event_tx(
            &mut tx,
            runtime_job_id,
            event_type,
            payload,
        )
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn runtime_events_for(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Vec<RuntimeEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_events
             WHERE runtime_job_id = $1
             ORDER BY sequence ASC",
        )
        .bind(runtime_job_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn runtime_events_for_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeEvent>>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT runtime_job_id, data::text FROM runtime_events
             WHERE runtime_job_id = ANY($1::text[])
             ORDER BY runtime_job_id ASC, sequence ASC",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_job = BTreeMap::new();
        for (runtime_job_id, data) in rows {
            by_job
                .entry(runtime_job_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_job)
    }

    pub async fn runtime_event_summaries_for_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, RuntimeEventSummary>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<RuntimeEventSummaryRow> = sqlx::query_as(
            "SELECT selected.runtime_job_id,
                    COALESCE(counts.runtime_event_count, 0),
                    latest.event_type,
                    latest.created_at,
                    prompt.prompt_packet_digest,
                    counts.latest_turn_sequence,
                    counts.latest_activity_result_sequence
             FROM unnest($1::text[]) AS selected(runtime_job_id)
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS runtime_event_count,
                        MAX(sequence) FILTER (WHERE event_type = 'RuntimeTurnStarted')
                            AS latest_turn_sequence,
                        MAX(sequence) FILTER (WHERE event_type = 'ActivityResultReady')
                            AS latest_activity_result_sequence
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
             ) AS counts ON true
             LEFT JOIN LATERAL (
                 SELECT event_type, created_at
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
                 ORDER BY sequence DESC
                 LIMIT 1
             ) AS latest ON true
             LEFT JOIN LATERAL (
                 SELECT data #>> '{event,prompt_packet_digest}' AS prompt_packet_digest
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
                   AND event_type = 'RuntimePromptPrepared'
                   AND data #>> '{event,prompt_packet_digest}' IS NOT NULL
                 ORDER BY sequence DESC
                 LIMIT 1
             ) AS prompt ON true",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    runtime_job_id,
                    runtime_event_count,
                    latest_runtime_event_type,
                    latest_runtime_event_at,
                    prompt_packet_digest,
                    latest_turn_sequence,
                    latest_activity_result_sequence,
                )| {
                    (
                        runtime_job_id.clone(),
                        RuntimeEventSummary {
                            runtime_job_id,
                            runtime_event_count: runtime_event_count.max(0) as usize,
                            latest_runtime_event_type,
                            prompt_packet_digest,
                            latest_runtime_event_at,
                            latest_turn_sequence: latest_turn_sequence
                                .and_then(|sequence| u64::try_from(sequence).ok()),
                            latest_activity_result_sequence: latest_activity_result_sequence
                                .and_then(|sequence| u64::try_from(sequence).ok()),
                        },
                    )
                },
            )
            .collect())
    }
}

impl WorkflowRuntimeStore {
    pub async fn enqueue_runtime_job(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
    ) -> anyhow::Result<RuntimeJob> {
        self.enqueue_runtime_job_with_not_before(
            command_id,
            runtime_kind,
            runtime_profile,
            input,
            None,
        )
        .await
    }

    pub async fn enqueue_runtime_job_with_not_before(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJob> {
        let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        job.not_before = not_before;
        let data = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        let runtime_kind = enum_str(&job.runtime_kind)?;
        let mut tx = self.pool.begin().await?;
        let workflow_id: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((workflow_id,)) = workflow_id else {
            anyhow::bail!("workflow command not found: {command_id}");
        };
        if let Some(command) = job.input.get("command") {
            super::artifacts::pin_runtime_transcript_dependency_tx(&mut tx, &workflow_id, command)
                .await?;
        }
        sqlx::query(
            "INSERT INTO runtime_jobs
                (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(job.not_before)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(job)
    }

    pub async fn enqueue_runtime_job_for_pending_command(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        command_store::enqueue_runtime_job_for_command(
            &self.pool,
            command_id,
            None,
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    pub async fn enqueue_runtime_job_for_claimed_command(
        &self,
        command_id: &str,
        dispatch_claim: super::super::DispatchClaim<'_>,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        command_store::enqueue_runtime_job_for_command(
            &self.pool,
            command_id,
            Some(dispatch_claim),
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    pub async fn claim_next_runtime_job(
        &self,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT job.id, job.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE (
                 job.status = 'pending'
                 AND (job.not_before IS NULL OR job.not_before <= CURRENT_TIMESTAMP)
             ) OR (
                 job.status = 'running'
                 AND job.data ? 'lease'
                 AND (job.data->'lease' ? 'expires_at')
                 AND (job.data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
             )
             ORDER BY
                 CASE
                     WHEN COALESCE(job.data #>> '{input,activity}', '') IN (
                         'implement_issue',
                         'implement_prompt',
                         'inspect_pr_feedback',
                         'address_pr_feedback'
                     ) THEN 0
                     ELSE 1
                 END ASC,
                 job.created_at ASC
             LIMIT 1
             FOR UPDATE OF job SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some((id, data)) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.claim(owner, expires_at);
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
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }
}
