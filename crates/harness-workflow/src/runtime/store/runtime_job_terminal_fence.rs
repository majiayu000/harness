use super::{
    fence_terminal_transition_tx, terminal_state_for_instance_tx,
    workflow_instance_from_persisted_json, WorkflowDefinitionRegistry,
};
use sqlx::{Postgres, Transaction};

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
