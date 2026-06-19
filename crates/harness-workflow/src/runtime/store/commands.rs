use super::{enum_str, to_jsonb_string};
use crate::runtime::{WorkflowCommand, WorkflowCommandStatus};
use sqlx::postgres::PgPool;
use uuid::Uuid;

pub(super) async fn insert(
    pool: &PgPool,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
) -> anyhow::Result<String> {
    let data = to_jsonb_string(command)?;
    let command_type = enum_str(&command.command_type)?;
    let (id,): (String,) = sqlx::query_as(insert_sql())
        .bind(Uuid::new_v4().to_string())
        .bind(workflow_id)
        .bind(decision_id)
        .bind(&command_type)
        .bind(&command.dedupe_key)
        .bind(status.as_str())
        .bind(&data)
        .bind(WorkflowCommandStatus::Pending.as_str())
        .fetch_one(pool)
        .await?;
    Ok(id)
}

pub(super) async fn insert_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
) -> anyhow::Result<String> {
    let data = to_jsonb_string(command)?;
    let command_type = enum_str(&command.command_type)?;
    let (id,): (String,) = sqlx::query_as(insert_sql())
        .bind(Uuid::new_v4().to_string())
        .bind(workflow_id)
        .bind(decision_id)
        .bind(&command_type)
        .bind(&command.dedupe_key)
        .bind(status.as_str())
        .bind(&data)
        .bind(WorkflowCommandStatus::Pending.as_str())
        .fetch_one(&mut **tx)
        .await?;
    Ok(id)
}

fn insert_sql() -> &'static str {
    "INSERT INTO workflow_commands
        (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
     VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)
     ON CONFLICT (workflow_id, dedupe_key) DO UPDATE SET
        decision_id = CASE
            WHEN workflow_commands.status = $8 THEN EXCLUDED.decision_id
            ELSE workflow_commands.decision_id
        END,
        command_type = CASE
            WHEN workflow_commands.status = $8 THEN EXCLUDED.command_type
            ELSE workflow_commands.command_type
        END,
        data = CASE
            WHEN workflow_commands.status = $8 THEN EXCLUDED.data
            ELSE workflow_commands.data
        END,
        status = CASE
            WHEN workflow_commands.status = $8 THEN EXCLUDED.status
            ELSE workflow_commands.status
        END,
        updated_at = CASE
            WHEN workflow_commands.status = $8
                 AND (
                     workflow_commands.status <> EXCLUDED.status
                     OR workflow_commands.decision_id IS DISTINCT FROM EXCLUDED.decision_id
                     OR workflow_commands.command_type <> EXCLUDED.command_type
                     OR workflow_commands.data <> EXCLUDED.data
                 )
            THEN CURRENT_TIMESTAMP
            ELSE workflow_commands.updated_at
        END
     RETURNING id"
}
