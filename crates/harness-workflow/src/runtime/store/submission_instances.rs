use super::{WorkflowInstance, WorkflowRuntimeStore};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowSubmissionKindFilter {
    pub include_issue: bool,
    pub include_pr: bool,
    pub include_prompt: bool,
}

impl WorkflowRuntimeStore {
    /// List top-level runtime submissions across built-in and declarative definitions.
    pub async fn list_submission_instances_page(
        &self,
        project_id: Option<&str>,
        cursor_created_at: Option<DateTime<Utc>>,
        cursor_id: Option<&str>,
        kinds: WorkflowSubmissionKindFilter,
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
                   ($4 AND definition_id = 'github_issue_pr'
                       AND data->'data'->>'issue_number' IS NOT NULL)
                   OR ($5 AND definition_id = 'github_issue_pr'
                       AND data->'data'->>'issue_number' IS NULL)
                   OR ($6 AND definition_id <> 'github_issue_pr')
               )
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
             LIMIT $7",
        )
        .bind(project_id)
        .bind(cursor_created_at)
        .bind(cursor_id)
        .bind(kinds.include_issue)
        .bind(kinds.include_pr)
        .bind(kinds.include_prompt)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }
}
