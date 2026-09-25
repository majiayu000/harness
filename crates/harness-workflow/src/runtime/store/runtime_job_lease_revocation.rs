use super::{
    fence_terminal_transition_tx,
    runtime_job_leases::{
        append_runtime_event_tx, delete_runtime_job_lease_receipts_tx,
        mark_remote_lease_proof_v1_tx,
    },
    terminal_state_for_instance_tx, to_jsonb_string, workflow_instance_from_persisted_json,
    WorkflowRuntimeStore,
};
use crate::runtime::{RuntimeJob, RuntimeJobStatus};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

impl WorkflowRuntimeStore {
    pub async fn revoke_remote_host_runtime_job_leases(
        &self,
        owner: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let workflows: Vec<(String,)> = sqlx::query_as(
            "SELECT workflow.data::text
             FROM workflow_instances AS workflow
             WHERE workflow.id IN (
                 SELECT command.workflow_id
                 FROM runtime_jobs AS job
                 JOIN workflow_commands AS command ON command.id = job.command_id
                 WHERE job.status = 'running'
                   AND job.runtime_kind = 'remote_host'
                   AND job.data #>> '{lease,owner}' = $1
             )
             ORDER BY workflow.id
             FOR UPDATE OF workflow",
        )
        .bind(owner)
        .fetch_all(&mut *tx)
        .await?;
        for (workflow_data,) in workflows {
            let workflow = workflow_instance_from_persisted_json(&workflow_data)?;
            if terminal_state_for_instance_tx(&mut tx, &self.definition_registry, &workflow)
                .await?
                .is_some()
            {
                fence_terminal_transition_tx(&mut tx, &self.definition_registry, &workflow).await?;
            }
        }

        let rows: Vec<(String, String, i64, i32)> = sqlx::query_as(
            "SELECT job.id, job.data::text, workflow.version, command.attempt_generation
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE job.status = 'running'
               AND job.runtime_kind = 'remote_host'
               AND job.data #>> '{lease,owner}' = $1
             ORDER BY job.id
             FOR UPDATE OF job",
        )
        .bind(owner)
        .fetch_all(&mut *tx)
        .await?;
        let mut revoked = 0;
        if !rows.is_empty() {
            mark_remote_lease_proof_v1_tx(&mut tx).await?;
        }
        for (runtime_job_id, data, workflow_version, command_attempt_generation) in &rows {
            let mut job: RuntimeJob = serde_json::from_str(data)?;
            if job.input.get("cancellation_requested").is_some() {
                continue;
            }
            if job.is_eval_job() {
                let activity = job
                    .input
                    .get("activity")
                    .and_then(Value::as_str)
                    .unwrap_or("remote_eval");
                job.input["cancellation_requested"] = json!({
                    "reason": "runtime host deregistered",
                    "activity": activity,
                    "requested_at": now,
                    "workflow_version": workflow_version,
                    "command_attempt_generation": command_attempt_generation,
                });
                job.updated_at = now;
                sqlx::query(
                    "UPDATE runtime_jobs SET data = $1::jsonb, updated_at = $2 WHERE id = $3",
                )
                .bind(to_jsonb_string(&job)?)
                .bind(now)
                .bind(runtime_job_id)
                .execute(&mut *tx)
                .await?;
                continue;
            }

            let previous_expires_at = job.lease.as_ref().map(|lease| lease.expires_at);
            job.status = RuntimeJobStatus::Pending;
            job.lease = None;
            job.not_before = None;
            job.updated_at = now;
            let updated = to_jsonb_string(&job)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET status = 'pending', not_before = NULL, data = $1::jsonb, updated_at = $2
                 WHERE id = $3",
            )
            .bind(&updated)
            .bind(now)
            .bind(runtime_job_id)
            .execute(&mut *tx)
            .await?;
            delete_runtime_job_lease_receipts_tx(&mut tx, runtime_job_id, job.lease_generation)
                .await?;
            append_runtime_event_tx(
                &mut tx,
                runtime_job_id,
                "RuntimeJobLeaseRevoked",
                json!({
                    "owner": owner,
                    "lease_generation": job.lease_generation,
                    "previous_expires_at": previous_expires_at,
                    "reason": "host_deregistered",
                    "source": "runtime_host_deregister",
                }),
            )
            .await?;
            revoked += 1;
        }
        tx.commit().await?;
        Ok(revoked)
    }
}
