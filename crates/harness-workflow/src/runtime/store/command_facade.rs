use super::*;

/// Point-in-time dispatch pool state used by the starvation probe (GH-1895):
/// distinguishes "no work exists" from "work exists but is gated".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DispatchPoolSnapshot {
    pub pending_commands: u64,
    pub deferred_commands: u64,
    pub dispatched_commands: u64,
    pub gated_workflows: u64,
}

impl DispatchPoolSnapshot {
    /// True when undispatchable work is sitting in the pool: deferred
    /// commands behind a dispatch barrier, or workflows parked in a gated
    /// state. This is the starvation case; an all-zero snapshot is idle.
    pub fn has_gated_work(&self) -> bool {
        self.deferred_commands > 0 || self.gated_workflows > 0
    }
}

impl WorkflowRuntimeStore {
    /// Count commands by dispatch status plus workflows parked in gated
    /// states (`blocked`, `awaiting_feedback`, `awaiting_dependencies` —
    /// the last one is how the 08-01 mutual-dependency starvation presented
    /// while nothing was dispatching, GH-1885).
    pub async fn dispatch_pool_snapshot(&self) -> anyhow::Result<DispatchPoolSnapshot> {
        let (pending, deferred, dispatched): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*) FILTER (WHERE status = 'pending'),
                    COUNT(*) FILTER (WHERE status = 'deferred'),
                    COUNT(*) FILTER (WHERE status = 'dispatched')
             FROM workflow_commands",
        )
        .fetch_one(&self.pool)
        .await?;
        let (gated_workflows,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workflow_instances
             WHERE state IN ('blocked', 'awaiting_feedback', 'awaiting_dependencies')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(DispatchPoolSnapshot {
            pending_commands: pending.max(0) as u64,
            deferred_commands: deferred.max(0) as u64,
            dispatched_commands: dispatched.max(0) as u64,
            gated_workflows: gated_workflows.max(0) as u64,
        })
    }

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
        let mut tx = self.pool.begin().await?;
        let workflow = select_instance_for_update_tx(&mut tx, workflow_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow instance not found: {workflow_id}"))?;
        if workflow.is_terminal() {
            anyhow::bail!(
                "cannot enqueue command for terminal workflow {workflow_id} ({})",
                workflow.state
            );
        }
        let command_id =
            command_store::insert_tx(&mut tx, workflow_id, decision_id, command, status).await?;
        tx.commit().await?;
        Ok(command_id)
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at,
                    attempt_generation, superseded_by_command_id
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
                    dispatch_barrier::text, data::text, created_at, updated_at,
                    attempt_generation, superseded_by_command_id
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
                    command.created_at, command.updated_at,
                    command.attempt_generation, command.superseded_by_command_id
             FROM unnest($1::text[]) AS selected(workflow_id)
             JOIN LATERAL (
                 SELECT id, workflow_id, decision_id, status, dispatch_owner,
                        dispatch_lease_expires_at, dispatch_not_before,
                        dispatch_attempt_count, dispatch_claim_generation,
                        dispatch_barrier::text AS dispatch_barrier,
                        data::text AS data, created_at, updated_at,
                        attempt_generation, superseded_by_command_id
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
                    dispatch_barrier::text, data::text, created_at, updated_at,
                    attempt_generation, superseded_by_command_id
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
                    command.data::text, command.created_at, command.updated_at,
                    command.attempt_generation, command.superseded_by_command_id
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

    /// The single definition of "claimable now" (B-001): a pending command, a
    /// dispatching command whose lease expired, or a deferred command whose
    /// backoff elapsed with an intact barrier. `claim_pending_commands` and
    /// `peek_claimable_commands` must not drift apart, so both render this
    /// fragment with their own bind indices.
    fn claimable_command_predicate(pending: u8, dispatching: u8, deferred: u8) -> String {
        format!(
            "command.status = ${pending}
                    OR (
                        command.status = ${dispatching}
                        AND COALESCE(command.dispatch_lease_expires_at, '-infinity'::timestamptz)
                            <= CURRENT_TIMESTAMP
                    )
                    OR (
                        command.status = ${deferred}
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
                    )"
        )
    }

    /// Claimable commands without claiming them — the throttle band needs to
    /// know whether other work could run instead (GH-1770 §4.1).
    pub async fn peek_claimable_commands(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(&format!(
            "SELECT command.id, command.workflow_id, command.decision_id, command.status,
                    command.dispatch_owner, command.dispatch_lease_expires_at,
                    command.dispatch_not_before, command.dispatch_attempt_count,
                    command.dispatch_claim_generation, command.dispatch_barrier::text,
                    command.data::text, command.created_at, command.updated_at,
                    command.attempt_generation, command.superseded_by_command_id
             FROM workflow_commands AS command
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE {}
             ORDER BY command.created_at ASC
             LIMIT $4",
            Self::claimable_command_predicate(1, 2, 3)
        ))
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
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
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(&format!(
            "WITH candidates AS (
                 SELECT command.id
                 FROM workflow_commands AS command
                 JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
                 WHERE {}
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
                       command.data::text, command.created_at, command.updated_at,
                       command.attempt_generation, command.superseded_by_command_id",
            Self::claimable_command_predicate(3, 4, 5)
        ))
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

    /// Move a command's dispatch status.
    ///
    /// A superseded attempt is history and cannot be moved: reviving it would
    /// put two live rows on one dedupe key and let a replaced attempt dispatch
    /// (GH-1865).
    pub async fn mark_command_status(
        &self,
        command_id: &str,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<()> {
        let updated = sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL,
                 dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2 AND status <> $3",
        )
        .bind(status.as_str())
        .bind(command_id)
        .bind(WorkflowCommandStatus::Superseded.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        if updated {
            return Ok(());
        }
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT status FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&self.pool)
                .await?;
        match existing {
            Some((current,)) if current == WorkflowCommandStatus::Superseded.as_str() => {
                anyhow::bail!(
                    "workflow command `{command_id}` was superseded by a newer attempt and cannot be moved to `{status}`"
                )
            }
            _ => Ok(()),
        }
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
