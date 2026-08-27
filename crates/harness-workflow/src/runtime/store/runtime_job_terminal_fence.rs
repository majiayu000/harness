use super::{
    fence_terminal_transition_tx, terminal_state_for_instance_tx,
    workflow_instance_from_persisted_json, RuntimeJob, WorkflowCommandRecord,
    WorkflowDefinitionRegistry, WorkflowInstance, WorkflowRuntimeStore,
};
use crate::runtime::command_record::{
    from_row as workflow_command_record_from_row, WorkflowCommandRecordRow,
};
use sqlx::{Postgres, Transaction};

/// A read-only authorization snapshot whose rows stay locked until the
/// caller finishes or releases the external server-owned mutation.
pub struct RuntimeJobMutationFence {
    tx: Transaction<'static, Postgres>,
    pub workflow: WorkflowInstance,
    pub command: WorkflowCommandRecord,
    pub job: RuntimeJob,
}

impl RuntimeJobMutationFence {
    pub async fn release(self) -> anyhow::Result<()> {
        self.tx.rollback().await?;
        Ok(())
    }
}

impl WorkflowRuntimeStore {
    /// Lock a runtime job's authorization chain in canonical parent-to-child
    /// order. Holding the returned fence prevents workflow transitions,
    /// command supersession, and lease revocation from committing while a
    /// server-owned external mutation is in flight.
    pub async fn fence_runtime_job_mutation(
        &self,
        runtime_job_id: &str,
        command_id: &str,
    ) -> anyhow::Result<RuntimeJobMutationFence> {
        let workflow_id: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&self.pool)
                .await?;
        let (workflow_id,) = workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflow command not found: {command_id}"))?;

        let mut tx = self.pool.begin().await?;
        let workflow_data: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
                .bind(&workflow_id)
                .fetch_optional(&mut *tx)
                .await?;
        let workflow = workflow_data
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("workflow not found: {workflow_id}"))?;

        let command_row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at,
                    attempt_generation, superseded_by_command_id
             FROM workflow_commands
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let command = command_row
            .map(workflow_command_record_from_row)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("workflow command not found: {command_id}"))?;

        let job_data: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let job = job_data
            .map(|(data,)| serde_json::from_str(&data))
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("runtime job not found: {runtime_job_id}"))?;

        Ok(RuntimeJobMutationFence {
            tx,
            workflow,
            command,
            job,
        })
    }
}

/// Lock the workflow that owns a runtime job and repair a legacy terminal row
/// before the caller locks or mutates the job.
pub(super) async fn fence_terminal_runtime_job_workflow_tx(
    tx: &mut Transaction<'_, Postgres>,
    definition_registry: &WorkflowDefinitionRegistry,
    runtime_job_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT workflow.data::text
         FROM runtime_jobs AS job
         JOIN workflow_commands AS command ON command.id = job.command_id
         JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
         WHERE job.id = $1
         FOR UPDATE OF workflow",
    )
    .bind(runtime_job_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((workflow_data,)) = row else {
        return Ok(false);
    };
    let workflow = workflow_instance_from_persisted_json(&workflow_data)?;
    if terminal_state_for_instance_tx(tx, definition_registry, &workflow)
        .await?
        .is_some()
    {
        fence_terminal_transition_tx(tx, definition_registry, &workflow).await?;
    }
    Ok(true)
}
