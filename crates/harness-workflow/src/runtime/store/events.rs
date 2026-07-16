use super::*;

impl WorkflowRuntimeStore {
    pub async fn append_event(
        &self,
        workflow_id: &str,
        event_type: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<WorkflowEvent> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_events:{workflow_id}"))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
            .with_payload(payload);
        let data = to_jsonb_string(&event)?;
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
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn events_for(&self, workflow_id: &str) -> anyhow::Result<Vec<WorkflowEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_events
             WHERE workflow_id = $1
             ORDER BY sequence ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn latest_event_for_type(
        &self,
        workflow_id: &str,
        event_type: &str,
    ) -> anyhow::Result<Option<WorkflowEvent>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_events
             WHERE workflow_id = $1
               AND event_type = $2
             ORDER BY sequence DESC
             LIMIT 1",
        )
        .bind(workflow_id)
        .bind(event_type)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    pub async fn events_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowEvent>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_events
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, sequence ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for (workflow_id, data) in rows {
            by_workflow
                .entry(workflow_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_workflow)
    }
}
