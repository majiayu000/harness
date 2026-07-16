use super::*;

impl WorkflowRuntimeStore {
    pub async fn record_decision(&self, record: &WorkflowDecisionRecord) -> anyhow::Result<()> {
        let data = to_jsonb_string(record)?;
        sqlx::query(
            "INSERT INTO workflow_decisions
                (id, workflow_id, event_id, accepted, data, rejection_reason)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)
             ON CONFLICT (id) DO UPDATE SET
                accepted = EXCLUDED.accepted,
                data = EXCLUDED.data,
                rejection_reason = EXCLUDED.rejection_reason",
        )
        .bind(&record.id)
        .bind(&record.workflow_id)
        .bind(&record.event_id)
        .bind(record.accepted)
        .bind(&data)
        .bind(&record.rejection_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn decisions_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowDecisionRecord>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_decisions
             WHERE workflow_id = $1
             ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn decisions_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowDecisionRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_decisions
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
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

    pub async fn detail_counts_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, WorkflowRuntimeDetailCounts>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT selected.workflow_id,
                    COALESCE(events.event_count, 0),
                    COALESCE(decisions.decision_count, 0),
                    COALESCE(decisions.rejected_decision_count, 0),
                    COALESCE(commands.command_count, 0),
                    COALESCE(jobs.runtime_job_count, 0)
             FROM unnest($1::text[]) AS selected(workflow_id)
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS event_count
                 FROM workflow_events
                 WHERE workflow_id = selected.workflow_id
             ) AS events ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS decision_count,
                        COUNT(*) FILTER (WHERE accepted = false) AS rejected_decision_count
                 FROM workflow_decisions
                 WHERE workflow_id = selected.workflow_id
             ) AS decisions ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS command_count
                 FROM workflow_commands
                 WHERE workflow_id = selected.workflow_id
             ) AS commands ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(runtime_jobs.id) AS runtime_job_count
                 FROM workflow_commands
                 JOIN runtime_jobs ON runtime_jobs.command_id = workflow_commands.id
                 WHERE workflow_commands.workflow_id = selected.workflow_id
             ) AS jobs ON true",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    workflow_id,
                    event_count,
                    decision_count,
                    rejected_decision_count,
                    command_count,
                    runtime_job_count,
                )| {
                    (
                        workflow_id,
                        WorkflowRuntimeDetailCounts {
                            event_count: event_count.max(0) as usize,
                            decision_count: decision_count.max(0) as usize,
                            rejected_decision_count: rejected_decision_count.max(0) as usize,
                            command_count: command_count.max(0) as usize,
                            runtime_job_count: runtime_job_count.max(0) as usize,
                        },
                    )
                },
            )
            .collect())
    }

    pub async fn rejected_decisions_for_workflows_limited(
        &self,
        workflow_ids: &[String],
        per_workflow_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowDecisionRecord>>> {
        if workflow_ids.is_empty() || per_workflow_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_workflow_limit = per_workflow_limit.clamp(1, 20);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT decision.workflow_id, decision.data
             FROM unnest($1::text[]) AS selected(workflow_id)
             JOIN LATERAL (
                 SELECT workflow_id, data::text AS data, created_at
                 FROM workflow_decisions
                 WHERE workflow_id = selected.workflow_id
                   AND accepted = false
                 ORDER BY created_at DESC
                 LIMIT $2
             ) AS decision ON true
             ORDER BY decision.workflow_id ASC, decision.created_at DESC",
        )
        .bind(workflow_ids)
        .bind(per_workflow_limit)
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
