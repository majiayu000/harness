use super::*;

impl WorkflowRuntimeStore {
    pub async fn enqueue_command(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
    ) -> anyhow::Result<String> {
        self.enqueue_command_with_status(
            workflow_id,
            decision_id,
            command,
            WorkflowCommandStatus::Pending,
        )
        .await
    }

    pub async fn enqueue_command_with_status(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<String> {
        command_store::insert(&self.pool, workflow_id, decision_id, command, status).await
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = $1
                 ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn commands_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowCommandRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for row in rows {
            let record = workflow_command_record_from_row(row)?;
            by_workflow
                .entry(record.workflow_id.clone())
                .or_insert_with(Vec::new)
                .push(record);
        }
        Ok(by_workflow)
    }

    pub async fn commands_for_workflows_limited(
        &self,
        workflow_ids: &[String],
        per_workflow_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowCommandRecord>>> {
        if workflow_ids.is_empty() || per_workflow_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_workflow_limit = per_workflow_limit.clamp(1, 50);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT command.id, command.workflow_id, command.decision_id, command.status,
                    command.dispatch_owner, command.dispatch_lease_expires_at,
                    command.dispatch_not_before, command.dispatch_attempt_count,
                    command.dispatch_claim_generation, command.dispatch_barrier,
                    command.data,
                    command.created_at, command.updated_at
             FROM unnest($1::text[]) AS selected(workflow_id)
             JOIN LATERAL (
                 SELECT id, workflow_id, decision_id, status, dispatch_owner,
                        dispatch_lease_expires_at, dispatch_not_before,
                        dispatch_attempt_count, dispatch_claim_generation,
                        dispatch_barrier::text AS dispatch_barrier,
                        data::text AS data, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = selected.workflow_id
                 ORDER BY created_at DESC
                 LIMIT $2
             ) AS command ON true
             ORDER BY command.workflow_id ASC, command.created_at ASC",
        )
        .bind(workflow_ids)
        .bind(per_workflow_limit)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for row in rows {
            let record = workflow_command_record_from_row(row)?;
            by_workflow
                .entry(record.workflow_id.clone())
                .or_insert_with(Vec::new)
                .push(record);
        }
        Ok(by_workflow)
    }

    pub async fn get_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Option<WorkflowCommandRecord>> {
        let row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1",
        )
        .bind(command_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(workflow_command_record_from_row).transpose()
    }

    pub async fn pending_commands(&self, limit: i64) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT command.id, command.workflow_id, command.decision_id, command.status,
                    command.dispatch_owner, command.dispatch_lease_expires_at,
                    command.dispatch_not_before, command.dispatch_attempt_count,
                    command.dispatch_claim_generation, command.dispatch_barrier::text,
                    command.data::text, command.created_at, command.updated_at
             FROM workflow_commands AS command
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE command.status = 'pending'
             ORDER BY command.created_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn claim_pending_commands(
        &self,
        owner: &str,
        expires_at: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let mut tx = self.pool.begin().await?;
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT command.id
                 FROM workflow_commands AS command
                 JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE command.status = $3
                    OR (
                        command.status = $4
                        AND COALESCE(command.dispatch_lease_expires_at, '-infinity'::timestamptz)
                            <= CURRENT_TIMESTAMP
                    )
                    OR (
                        command.status = $5
                        AND command.dispatch_not_before <= CURRENT_TIMESTAMP
                        AND command.dispatch_owner IS NULL
                        AND command.dispatch_lease_expires_at IS NULL
                        AND command.dispatch_attempt_count > 0
                        AND command.dispatch_claim_generation > 0
                        AND command.dispatch_barrier IS NOT NULL
                        AND jsonb_typeof(command.dispatch_barrier) = 'object'
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'reason'), '') IS NOT NULL
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'project_id'), '') IS NOT NULL
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'dispatch_owner'), '') IS NOT NULL
                        AND command.dispatch_barrier->>'command_id' = command.id
                        AND command.dispatch_barrier->>'workflow_id' = command.workflow_id
                        AND (command.dispatch_barrier->>'attempt')::BIGINT
                            = command.dispatch_attempt_count
                        AND (command.dispatch_barrier->>'claim_generation')::BIGINT
                            = command.dispatch_claim_generation
                        AND (command.dispatch_barrier->>'next_dispatch_at')::TIMESTAMPTZ
                            = command.dispatch_not_before
                    )
                 ORDER BY command.created_at ASC
                 LIMIT $6
                 FOR UPDATE OF command SKIP LOCKED
             )
             UPDATE workflow_commands AS command
             SET status = $4,
                 dispatch_owner = $1,
                 dispatch_lease_expires_at = $2,
                 dispatch_claim_generation = command.dispatch_claim_generation + 1,
                 updated_at = CURRENT_TIMESTAMP
             FROM candidates
             WHERE command.id = candidates.id
             RETURNING command.id, command.workflow_id, command.decision_id, command.status,
                       command.dispatch_owner, command.dispatch_lease_expires_at,
                       command.dispatch_not_before, command.dispatch_attempt_count,
                       command.dispatch_claim_generation, command.dispatch_barrier::text,
                       command.data::text, command.created_at, command.updated_at",
        )
        .bind(owner)
        .bind(expires_at)
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let records: anyhow::Result<Vec<_>> = rows
            .into_iter()
            .map(workflow_command_record_from_row)
            .collect();
        let mut records = match records {
            Ok(records) => records,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        tx.commit().await?;
        records.sort_by_key(|record| record.created_at);
        Ok(records)
    }

    pub async fn mark_command_status(
        &self,
        command_id: &str,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL,
                 dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(status.as_str())
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_pending_command_status(
        &self,
        command_id: &str,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2 AND status = $3",
        )
        .bind(status.as_str())
        .bind(command_id)
        .bind(WorkflowCommandStatus::Pending.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}
