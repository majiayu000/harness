use super::{WorkflowInstance, WorkflowRuntimeStore};
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
    /// List top-level runtime submissions across built-in and declarative definitions.
    pub async fn list_submission_instances_page(
        &self,
        project_id: Option<&str>,
        cursor_created_at: Option<DateTime<Utc>>,
        cursor_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.max(1);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE ($1::text IS NULL OR data->'data'->>'project_id' = $1)
               AND parent_workflow_id IS NULL
               AND (
                   definition_id IN ('github_issue_pr', 'prompt_task')
                   OR NULLIF(BTRIM(data->'data'->>'definition_hash'), '') IS NOT NULL
               )
               AND COALESCE(
                   data->'data'->>'submission_id',
                   data->'data'->'task_ids'->>0,
                   data->'data'->>'task_id'
               ) IS NOT NULL
               AND (
                   $2::timestamptz IS NULL
                   OR (data->>'created_at')::timestamptz < $2
                   OR (
                       (data->>'created_at')::timestamptz = $2
                       AND COALESCE(
                           data->'data'->>'submission_id',
                           data->'data'->'task_ids'->>0,
                           data->'data'->>'task_id'
                       ) < COALESCE($3::text, '')
                   )
               )
             ORDER BY (data->>'created_at')::timestamptz DESC,
                      COALESCE(
                          data->'data'->>'submission_id',
                          data->'data'->'task_ids'->>0,
                          data->'data'->>'task_id'
                      ) DESC
             LIMIT $4",
        )
        .bind(project_id)
        .bind(cursor_created_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }
}
