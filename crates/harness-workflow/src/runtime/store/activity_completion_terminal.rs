use super::*;

pub(super) fn command_status_is_active(status: WorkflowCommandStatus) -> bool {
    matches!(
        status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Dispatching
            | WorkflowCommandStatus::Deferred
            | WorkflowCommandStatus::Dispatched
    )
}

pub(super) fn cancellation_ack_matches(job: &RuntimeJob, result: &ActivityResult) -> bool {
    job.input.get("cancellation_requested").is_some()
        && job.runtime_kind == RuntimeKind::RemoteHost
        && (job.input.get("eval").is_some() || job.input.pointer("/command/eval").is_some())
        && result.status == ActivityStatus::Cancelled
        && result.artifacts.iter().any(|artifact| {
            artifact.artifact_type
                == crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                && artifact.artifact.get("status").and_then(Value::as_str) == Some("cleaned")
                && artifact
                    .artifact
                    .get("evidence_source")
                    .and_then(Value::as_str)
                    == Some("runtime_host_cancellation_ack")
        })
}

pub(super) async fn lock_workflow_commands_for_terminal_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "SELECT id FROM workflow_commands
         WHERE workflow_id = $1
         ORDER BY id
         FOR UPDATE",
    )
    .bind(workflow_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn lock_workflow_runtime_jobs_for_terminal_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "SELECT job.id FROM runtime_jobs AS job
         JOIN workflow_commands AS command ON command.id = job.command_id
         WHERE command.workflow_id = $1
         ORDER BY job.id
         FOR UPDATE OF job",
    )
    .bind(workflow_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(())
}
