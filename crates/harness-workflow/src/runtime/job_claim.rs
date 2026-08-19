use super::store::runtime_job_leases::{
    append_runtime_event_tx, delete_all_runtime_job_lease_receipts_tx,
};
use super::store::{
    enum_str, fence_terminal_transition_tx, terminal_state_for_instance_tx, to_jsonb_string,
};
use super::{RuntimeJob, RuntimeKind, WorkflowRuntimeStore};
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
    pub async fn claim_next_runtime_job_for_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(
            Some(runtime_kind),
            None,
            owner,
            expires_at,
            true,
            true,
        )
        .await
    }

    pub async fn claim_next_remote_host_runtime_job(
        &self,
        owner: &str,
        expires_at: DateTime<Utc>,
        supports_eval_resource_limits: bool,
        supports_trusted_eval_verifier: bool,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(
            Some(RuntimeKind::RemoteHost),
            None,
            owner,
            expires_at,
            supports_eval_resource_limits,
            supports_trusted_eval_verifier,
        )
        .await
    }

    pub async fn claim_next_runtime_job_excluding_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(
            None,
            Some(runtime_kind),
            owner,
            expires_at,
            true,
            true,
        )
        .await
    }

    pub(in crate::runtime) async fn claim_next_runtime_job_matching(
        &self,
        only_runtime_kind: Option<RuntimeKind>,
        excluded_runtime_kind: Option<RuntimeKind>,
        owner: &str,
        expires_at: DateTime<Utc>,
        supports_eval_resource_limits: bool,
        supports_trusted_eval_verifier: bool,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let records_remote_host_audit = only_runtime_kind == Some(RuntimeKind::RemoteHost);
        let only_runtime_kind = only_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        let excluded_runtime_kind = excluded_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        loop {
            let mut tx = self.pool.begin().await?;
            // Claimers participate in the terminal-transition lock order by
            // locking the workflow before its runtime job. Locking only the
            // job would let a pre-fence legacy row cross terminalization.
            let candidate: Option<(String, String)> = sqlx::query_as(
                "SELECT job.id, workflow.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE (
                 (
                     job.status = 'pending'
                     AND (job.not_before IS NULL OR job.not_before <= CURRENT_TIMESTAMP)
                 ) OR (
                     job.status = 'running'
                     AND job.data ? 'lease'
                     AND (job.data->'lease' ? 'expires_at')
                     AND (job.data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
                 )
             )
             AND job.data #> '{input,cancellation_requested}' IS NULL
             AND ($1::text IS NULL OR job.runtime_kind = $1)
             AND ($2::text IS NULL OR job.runtime_kind <> $2)
             AND (
                 $3::boolean
                 OR (
                     job.data #> '{input,eval}' IS NULL
                     AND job.data #> '{input,command,eval}' IS NULL
                 )
             )
             AND (
                 $4::boolean
                 OR NOT (
                     COALESCE(
                         (job.data #> '{input,eval,required_runtime_host_capabilities}')
                             ? 'trusted_eval_verifier_v1',
                         false
                     )
                     OR COALESCE(
                         (job.data #> '{input,command,eval,required_runtime_host_capabilities}')
                             ? 'trusted_eval_verifier_v1',
                         false
                     )
                 )
             )
             ORDER BY
                 CASE
                     WHEN COALESCE(job.data #>> '{input,activity}', '') IN (
                         'implement_issue',
                         'implement_prompt',
                         'inspect_pr_feedback',
                         'address_pr_feedback'
                     ) THEN 0
                     ELSE 1
                 END ASC,
                 job.created_at ASC
             LIMIT 1
             FOR UPDATE OF workflow SKIP LOCKED",
            )
            .bind(only_runtime_kind.as_deref())
            .bind(excluded_runtime_kind.as_deref())
            .bind(supports_eval_resource_limits)
            .bind(supports_trusted_eval_verifier)
            .fetch_optional(&mut *tx)
            .await?;

            let Some((id, workflow_data)) = candidate else {
                tx.commit().await?;
                return Ok(None);
            };
            let workflow = super::store::workflow_instance_from_persisted_json(&workflow_data)?;
            if terminal_state_for_instance_tx(&mut tx, &self.definition_registry, &workflow)
                .await?
                .is_some()
            {
                // Old databases can contain unfinished jobs created before
                // terminal admission was enforced. Repair the invariant and
                // continue looking for work in a fresh transaction so locks
                // never span multiple workflows.
                fence_terminal_transition_tx(&mut tx, &self.definition_registry, &workflow).await?;
                tx.commit().await?;
                continue;
            }

            // The workflow lock serializes all current claim paths. Recheck
            // eligibility while taking the child-row lock because another
            // legacy writer could still have changed the candidate.
            let row: Option<(String,)> = sqlx::query_as(
                "SELECT data::text FROM runtime_jobs
                 WHERE id = $1
                   AND (
                       (
                           status = 'pending'
                           AND (not_before IS NULL OR not_before <= CURRENT_TIMESTAMP)
                       ) OR (
                           status = 'running'
                           AND data ? 'lease'
                           AND (data->'lease' ? 'expires_at')
                           AND (data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
                       )
                   )
                   AND data #> '{input,cancellation_requested}' IS NULL
                 FOR UPDATE",
            )
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((data,)) = row else {
                tx.commit().await?;
                continue;
            };

            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            let reclaimed = job.status == super::RuntimeJobStatus::Running;
            job.claim(owner, expires_at);
            let updated = to_jsonb_string(&job)?;
            let status = enum_str(&job.status)?;
            sqlx::query(
                "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
            )
            .bind(&status)
            .bind(job.not_before)
            .bind(&updated)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            if records_remote_host_audit {
                delete_all_runtime_job_lease_receipts_tx(&mut tx, &id).await?;
                append_runtime_event_tx(
                    &mut tx,
                    &id,
                    if reclaimed {
                        "RuntimeJobReclaimed"
                    } else {
                        "RuntimeJobClaimed"
                    },
                    serde_json::json!({
                        "owner": owner,
                        "lease_generation": job.lease_generation,
                        "lease_expires_at": expires_at,
                        "claim_api": "runtime_host",
                    }),
                )
                .await?;
            }
            tx.commit().await?;
            return Ok(Some(job));
        }
    }
}
