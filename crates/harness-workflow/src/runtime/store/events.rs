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
        let event = insert_event_tx(&mut tx, workflow_id, event_type, source, payload).await?;
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
