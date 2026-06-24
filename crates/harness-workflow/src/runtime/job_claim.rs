use super::store::{enum_str, to_jsonb_string};
use super::{RuntimeJob, RuntimeKind, WorkflowRuntimeStore};
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
    pub async fn claim_next_runtime_job_for_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(Some(runtime_kind), None, owner, expires_at)
            .await
    }

    pub async fn claim_next_runtime_job_excluding_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(None, Some(runtime_kind), owner, expires_at)
            .await
    }

    async fn claim_next_runtime_job_matching(
        &self,
        only_runtime_kind: Option<RuntimeKind>,
        excluded_runtime_kind: Option<RuntimeKind>,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let only_runtime_kind = only_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        let excluded_runtime_kind = excluded_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT job.id, job.data::text
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
             AND ($1::text IS NULL OR job.runtime_kind = $1)
             AND ($2::text IS NULL OR job.runtime_kind <> $2)
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
             FOR UPDATE OF job SKIP LOCKED",
        )
        .bind(only_runtime_kind.as_deref())
        .bind(excluded_runtime_kind.as_deref())
        .fetch_optional(&mut *tx)
        .await?;

        let Some((id, data)) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let mut job: RuntimeJob = serde_json::from_str(&data)?;
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
        tx.commit().await?;
        Ok(Some(job))
    }
}
