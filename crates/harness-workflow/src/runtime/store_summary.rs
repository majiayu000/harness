use std::collections::BTreeMap;

use super::store::{WorkflowRuntimeStore, WorkflowRuntimeSummaryCounts};

impl WorkflowRuntimeStore {
    pub async fn runtime_summary_counts_for_instances(
        &self,
        project_id: Option<&str>,
        activity_envelope_artifact_type: &str,
        activity_envelope_schema: &str,
    ) -> anyhow::Result<WorkflowRuntimeSummaryCounts> {
        let command_statuses = command_statuses_for_instances(self, project_id).await?;
        let total_commands = command_statuses.values().sum();
        let runtime_job_statuses = runtime_job_statuses_for_instances(self, project_id).await?;
        let total_runtime_jobs = runtime_job_statuses.values().sum();
        let running_job_lease_statuses =
            running_job_lease_statuses_for_instances(self, project_id).await?;
        let (activity_outcomes, jobs_without_activity_envelope) = activity_outcomes_for_instances(
            self,
            project_id,
            activity_envelope_artifact_type,
            activity_envelope_schema,
        )
        .await?;

        Ok(WorkflowRuntimeSummaryCounts {
            total_commands,
            total_runtime_jobs,
            command_statuses,
            runtime_job_statuses,
            running_job_lease_statuses,
            activity_outcomes,
            jobs_without_activity_envelope,
        })
    }
}

async fn command_statuses_for_instances(
    store: &WorkflowRuntimeStore,
    project_id: Option<&str>,
) -> anyhow::Result<BTreeMap<String, usize>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT command.status, COUNT(*)
         FROM workflow_commands command
         JOIN workflow_instances workflow ON workflow.id = command.workflow_id
         WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
         GROUP BY command.status
         ORDER BY command.status ASC",
    )
    .bind(project_id)
    .fetch_all(store.pool())
    .await?;
    Ok(count_rows_to_map(rows))
}

async fn runtime_job_statuses_for_instances(
    store: &WorkflowRuntimeStore,
    project_id: Option<&str>,
) -> anyhow::Result<BTreeMap<String, usize>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT job.status, COUNT(*)
         FROM runtime_jobs job
         JOIN workflow_commands command ON command.id = job.command_id
         JOIN workflow_instances workflow ON workflow.id = command.workflow_id
         WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
         GROUP BY job.status
         ORDER BY job.status ASC",
    )
    .bind(project_id)
    .fetch_all(store.pool())
    .await?;
    Ok(count_rows_to_map(rows))
}

async fn running_job_lease_statuses_for_instances(
    store: &WorkflowRuntimeStore,
    project_id: Option<&str>,
) -> anyhow::Result<BTreeMap<String, usize>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT
             CASE
                 WHEN job.data->'lease'->>'expires_at' IS NULL
                 THEN 'missing_lease'
                 WHEN (job.data->'lease'->>'expires_at')::timestamptz > CURRENT_TIMESTAMP
                 THEN 'active_leased'
                 ELSE 'expired_lease'
             END AS lease_state,
             COUNT(*)
         FROM runtime_jobs job
         JOIN workflow_commands command ON command.id = job.command_id
         JOIN workflow_instances workflow ON workflow.id = command.workflow_id
         WHERE job.status = 'running'
           AND ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
         GROUP BY lease_state
         ORDER BY lease_state ASC",
    )
    .bind(project_id)
    .fetch_all(store.pool())
    .await?;
    Ok(count_rows_to_map(rows))
}

async fn activity_outcomes_for_instances(
    store: &WorkflowRuntimeStore,
    project_id: Option<&str>,
    activity_envelope_artifact_type: &str,
    activity_envelope_schema: &str,
) -> anyhow::Result<(BTreeMap<String, usize>, usize)> {
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT envelope.payload->>'outcome' AS outcome, COUNT(*)
         FROM runtime_jobs job
         JOIN workflow_commands command ON command.id = job.command_id
         JOIN workflow_instances workflow ON workflow.id = command.workflow_id
         LEFT JOIN LATERAL (
             SELECT artifact.value->'artifact' AS payload
             FROM jsonb_array_elements(
                 CASE
                     WHEN jsonb_typeof(job.data->'output'->'artifacts') = 'array'
                     THEN job.data->'output'->'artifacts'
                     ELSE '[]'::jsonb
                 END
             ) WITH ORDINALITY AS artifact(value, ordinal)
             WHERE artifact.value->>'artifact_type' = $2
               AND artifact.value->'artifact'->>'schema' = $3
             ORDER BY artifact.ordinal DESC
             LIMIT 1
         ) envelope ON true
         WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
         GROUP BY envelope.payload->>'outcome'
         ORDER BY envelope.payload->>'outcome' ASC NULLS FIRST",
    )
    .bind(project_id)
    .bind(activity_envelope_artifact_type)
    .bind(activity_envelope_schema)
    .fetch_all(store.pool())
    .await?;
    let mut activity_outcomes = BTreeMap::new();
    let mut jobs_without_activity_envelope = 0;
    for (outcome, count) in rows {
        match outcome {
            Some(outcome) => {
                activity_outcomes.insert(outcome, count.max(0) as usize);
            }
            None => {
                jobs_without_activity_envelope = count.max(0) as usize;
            }
        }
    }
    Ok((activity_outcomes, jobs_without_activity_envelope))
}

fn count_rows_to_map(rows: Vec<(String, i64)>) -> BTreeMap<String, usize> {
    rows.into_iter()
        .map(|(status, count)| (status, count.max(0) as usize))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    use super::*;
    use crate::runtime::{
        RuntimeJobStatus, RuntimeKind, WorkflowCommand, WorkflowInstance, WorkflowSubject,
    };

    #[tokio::test]
    async fn runtime_summary_splits_running_job_lease_states() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let workflow = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1170"),
        )
        .with_id("issue-1170")
        .with_data(json!({
            "project_id": "/project-a",
            "repo": "owner/repo",
            "issue_number": 1170,
        }));
        store.upsert_instance(&workflow).await?;

        for activity in ["implement_issue", "inspect_pr_feedback"] {
            let command = WorkflowCommand::enqueue_activity(activity, activity);
            let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
            store
                .enqueue_runtime_job(
                    &command_id,
                    RuntimeKind::CodexJsonrpc,
                    "codex-high",
                    json!({ "workflow_id": workflow.id, "activity": activity }),
                )
                .await?;
        }

        store
            .claim_next_runtime_job("active-worker", Utc::now() + Duration::minutes(5))
            .await?
            .ok_or_else(|| anyhow::anyhow!("active runtime job should be claimed"))?;
        store
            .claim_next_runtime_job("expired-worker", Utc::now() - Duration::minutes(5))
            .await?
            .ok_or_else(|| anyhow::anyhow!("expired runtime job should be claimed"))?;

        let summary = store
            .runtime_summary_counts_for_instances(
                Some("/project-a"),
                "activity_result_envelope",
                "harness.runtime.activity_result_envelope.v1",
            )
            .await?;

        assert_eq!(summary.total_runtime_jobs, 2);
        assert_eq!(summary.runtime_job_statuses["running"], 2);
        assert_eq!(summary.running_job_lease_statuses["active_leased"], 1);
        assert_eq!(summary.running_job_lease_statuses["expired_lease"], 1);
        assert_eq!(
            store
                .runtime_job_count_by_status(RuntimeJobStatus::Running)
                .await?,
            2
        );
        Ok(())
    }
}
