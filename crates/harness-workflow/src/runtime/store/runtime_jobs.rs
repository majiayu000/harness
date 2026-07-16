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
