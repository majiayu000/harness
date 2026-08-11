use super::{
    command_attempts::{insert_command_attempt_tx, AttemptReplacement},
    enum_str, runtime_job_for_command_tx, to_jsonb_string, workflow_instance_from_persisted_json,
    ClaimedCommandTerminalOutcome, RuntimeJobEnqueueOutcome, WorkflowRuntimeStore,
};
use crate::runtime::{
    DispatchClaim, RuntimeJob, RuntimeKind, WorkflowCommand, WorkflowCommandStatus,
    WorkflowInstance,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::postgres::PgPool;

impl WorkflowRuntimeStore {
    pub async fn skip_claimed_command_if_owned(
        &self,
        command_id: &str,
        dispatch_claim: DispatchClaim<'_>,
    ) -> anyhow::Result<bool> {
        let generation = i64::try_from(dispatch_claim.generation)
            .map_err(|_| anyhow::anyhow!("dispatch claim generation exceeds PostgreSQL BIGINT"))?;
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $2, dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                 dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status = $3 AND dispatch_owner = $4
               AND dispatch_claim_generation = $5",
        )
        .bind(command_id)
        .bind(WorkflowCommandStatus::Skipped.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(dispatch_claim.owner)
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn finish_claimed_command_for_terminal_workflow(
        &self,
        command_id: &str,
        dispatch_claim: DispatchClaim<'_>,
    ) -> anyhow::Result<ClaimedCommandTerminalOutcome> {
        let generation = i64::try_from(dispatch_claim.generation)
            .map_err(|_| anyhow::anyhow!("dispatch claim generation exceeds PostgreSQL BIGINT"))?;
        let workflow_id: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((workflow_id,)) = workflow_id else {
            anyhow::bail!("workflow command not found: {command_id}");
        };

        let mut tx = self.pool.begin().await?;
        let workflow_data: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
                .bind(&workflow_id)
                .fetch_optional(&mut *tx)
                .await?;
        let workflow = workflow_data
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .transpose()?;
        let command_claim: Option<(String, Option<String>, i64)> = sqlx::query_as(
            "SELECT status, dispatch_owner, dispatch_claim_generation
             FROM workflow_commands WHERE id = $1 FOR UPDATE",
        )
        .bind(command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status, owner, current_generation)) = command_claim else {
            anyhow::bail!("workflow command not found: {command_id}");
        };
        if status != WorkflowCommandStatus::Dispatching.as_str()
            || owner.as_deref() != Some(dispatch_claim.owner)
            || current_generation != generation
        {
            tx.rollback().await?;
            return Ok(ClaimedCommandTerminalOutcome::StaleClaim);
        }
        let Some(workflow) = workflow.filter(WorkflowInstance::is_terminal) else {
            tx.rollback().await?;
            return Ok(ClaimedCommandTerminalOutcome::NotTerminal);
        };
        let terminal_status = if workflow.state == "cancelled" {
            WorkflowCommandStatus::Cancelled
        } else {
            WorkflowCommandStatus::Skipped
        };
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $2, dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                 dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status = $3 AND dispatch_owner = $4
               AND dispatch_claim_generation = $5",
        )
        .bind(command_id)
        .bind(terminal_status.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(dispatch_claim.owner)
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(ClaimedCommandTerminalOutcome::StaleClaim);
        }
        let workflow_state = workflow.state;
        tx.commit().await?;
        Ok(ClaimedCommandTerminalOutcome::WorkflowTerminal {
            status: terminal_status,
            workflow_state,
        })
    }
}

pub(super) async fn insert_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
) -> anyhow::Result<String> {
    let id = insert_command_attempt_tx(
        tx,
        workflow_id,
        decision_id,
        command,
        status,
        AttemptReplacement::PendingIntent,
    )
    .await?;
    super::artifacts::reconcile_runtime_transcript_dependencies_tx(tx, workflow_id).await?;
    Ok(id)
}

pub(super) async fn insert_or_reactivate_cancelled_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
) -> anyhow::Result<String> {
    insert_command_attempt_tx(
        tx,
        workflow_id,
        decision_id,
        command,
        status,
        AttemptReplacement::ReactivateCancelled,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn enqueue_runtime_job_for_command(
    pool: &PgPool,
    command_id: &str,
    dispatch_claim: Option<DispatchClaim<'_>>,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: Value,
    not_before: Option<DateTime<Utc>>,
) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
    let generation = dispatch_claim
        .map(|claim| i64::try_from(claim.generation))
        .transpose()
        .map_err(|_| anyhow::anyhow!("dispatch claim generation exceeds PostgreSQL BIGINT"))?;
    let workflow_id: Option<(String,)> =
        sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
            .bind(command_id)
            .fetch_optional(pool)
            .await?;
    let Some((workflow_id,)) = workflow_id else {
        anyhow::bail!("workflow command not found: {command_id}");
    };
    let mut tx = pool.begin().await?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
            .bind(&workflow_id)
            .fetch_optional(&mut *tx)
            .await?;
    let workflow = row
        .map(|(data,)| workflow_instance_from_persisted_json(&data))
        .transpose()?;
    let command_row: Option<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT status, dispatch_owner, dispatch_claim_generation
         FROM workflow_commands WHERE id = $1 FOR UPDATE",
    )
    .bind(command_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((command_status, current_owner, current_generation)) = command_row else {
        anyhow::bail!("workflow command not found: {command_id}");
    };
    let command_status = WorkflowCommandStatus::try_from(command_status.as_str())?;
    let existing = runtime_job_for_command_tx(&mut tx, command_id).await?;

    if let Some(claim) = dispatch_claim {
        if command_status == WorkflowCommandStatus::Dispatched
            && current_generation == generation.expect("claimed generation is present")
        {
            if let Some(existing) = existing
                .as_ref()
                .filter(|job| exact_dispatch_claim(job, claim.owner, claim.generation))
            {
                tx.rollback().await?;
                return Ok(RuntimeJobEnqueueOutcome::AlreadyExists(existing.clone()));
            }
        }
        if command_status != WorkflowCommandStatus::Dispatching
            || current_owner.as_deref() != Some(claim.owner)
            || current_generation != generation.expect("claimed generation is present")
        {
            tx.rollback().await?;
            return Ok(RuntimeJobEnqueueOutcome::StaleClaim);
        }
        if let Some(workflow) = workflow.as_ref().filter(|workflow| workflow.is_terminal()) {
            let terminal_status = if workflow.state == "cancelled" {
                WorkflowCommandStatus::Cancelled
            } else {
                WorkflowCommandStatus::Skipped
            };
            sqlx::query(
                "UPDATE workflow_commands SET status = $2, dispatch_owner = NULL,
                    dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                    dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1 AND status = $3 AND dispatch_owner = $4
                   AND dispatch_claim_generation = $5",
            )
            .bind(command_id)
            .bind(terminal_status.as_str())
            .bind(WorkflowCommandStatus::Dispatching.as_str())
            .bind(claim.owner)
            .bind(generation.expect("claimed generation is present"))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(RuntimeJobEnqueueOutcome::WorkflowTerminal {
                status: terminal_status,
            });
        }
        if existing.as_ref().is_some_and(|job| {
            matches!(
                job.status,
                crate::runtime::RuntimeJobStatus::Pending
                    | crate::runtime::RuntimeJobStatus::Running
            )
        }) {
            tx.rollback().await?;
            return Ok(RuntimeJobEnqueueOutcome::StaleClaim);
        }
    } else if let Some(workflow) = workflow.as_ref().filter(|workflow| workflow.is_terminal()) {
        let terminal_status = if workflow.state == "cancelled" {
            WorkflowCommandStatus::Cancelled
        } else {
            WorkflowCommandStatus::Skipped
        };
        sqlx::query(
            "UPDATE workflow_commands SET status = $2, dispatch_owner = NULL,
                dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status IN ($3, $4, $5, $6)",
        )
        .bind(command_id)
        .bind(terminal_status.as_str())
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Dispatched.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(RuntimeJobEnqueueOutcome::WorkflowTerminal {
            status: terminal_status,
        });
    } else if command_status != WorkflowCommandStatus::Pending {
        tx.rollback().await?;
        return Ok(match existing {
            Some(runtime_job) => RuntimeJobEnqueueOutcome::AlreadyExists(runtime_job),
            None => RuntimeJobEnqueueOutcome::CommandNotPending {
                status: command_status,
            },
        });
    } else if let Some(existing) = existing.filter(|job| {
        matches!(
            job.status,
            crate::runtime::RuntimeJobStatus::Pending | crate::runtime::RuntimeJobStatus::Running
        )
    }) {
        tx.rollback().await?;
        return Ok(RuntimeJobEnqueueOutcome::AlreadyExists(existing));
    }

    let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
    job.not_before = not_before;
    if let Some(claim) = dispatch_claim {
        let object = job
            .input
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("claimed runtime job input must be a JSON object"))?;
        object.insert(
            "_dispatch_claim".to_string(),
            json!({ "owner": claim.owner, "generation": claim.generation }),
        );
    }
    let data = to_jsonb_string(&job)?;
    let status = enum_str(&job.status)?;
    let runtime_kind = enum_str(&job.runtime_kind)?;
    sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind(&job.id)
    .bind(&job.command_id)
    .bind(&runtime_kind)
    .bind(&job.runtime_profile)
    .bind(&status)
    .bind(job.not_before)
    .bind(&data)
    .execute(&mut *tx)
    .await?;
    super::artifacts::reconcile_runtime_transcript_dependencies_tx(&mut tx, &workflow_id).await?;
    let mut update = sqlx::query(
        "UPDATE workflow_commands
         SET status = $2, dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
             dispatch_not_before = NULL, dispatch_barrier = NULL,
             updated_at = CURRENT_TIMESTAMP
         WHERE id = $1",
    )
    .bind(command_id)
    .bind(WorkflowCommandStatus::Dispatched.as_str());
    if let Some(claim) = dispatch_claim {
        update = sqlx::query(
            "UPDATE workflow_commands
             SET status = $2, dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL, dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status = $3 AND dispatch_owner = $4
               AND dispatch_claim_generation = $5",
        )
        .bind(command_id)
        .bind(WorkflowCommandStatus::Dispatched.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(claim.owner)
        .bind(generation.expect("claimed generation is present"));
    }
    let result = update.execute(&mut *tx).await?;
    if result.rows_affected() != 1 {
        anyhow::bail!("workflow command fence changed during runtime job enqueue");
    }
    tx.commit().await?;
    Ok(RuntimeJobEnqueueOutcome::Enqueued(job))
}

fn exact_dispatch_claim(job: &RuntimeJob, owner: &str, generation: u64) -> bool {
    job.input.get("_dispatch_claim").is_some_and(|claim| {
        claim.get("owner").and_then(Value::as_str) == Some(owner)
            && claim.get("generation").and_then(Value::as_u64) == Some(generation)
    })
}
