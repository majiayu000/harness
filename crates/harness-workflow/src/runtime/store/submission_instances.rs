use super::instance_helpers::terminal_task_status_rows;
use super::{WorkflowInstance, WorkflowRuntimeStore};
use crate::runtime::declarative_pinning::DECLARATIVE_DEFINITION_METADATA_KIND;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSubmissionFilter {
    pub project_id: Option<String>,
    pub source: Option<String>,
    pub repo: Option<String>,
    pub include_issue: bool,
    pub include_pr: bool,
    pub include_prompt: bool,
    pub active_only: bool,
    pub task_statuses: Vec<String>,
}

impl WorkflowRuntimeStore {
    /// List top-level runtime submissions across built-in and declarative definitions.
    pub async fn list_submission_instances_page(
        &self,
        cursor_created_at: Option<DateTime<Utc>>,
        cursor_id: Option<&str>,
        filter: &WorkflowSubmissionFilter,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.max(1);
        let (terminal_definition_ids, terminal_states, terminal_task_statuses) =
            terminal_task_status_rows();
        let rows: Vec<(String,)> = sqlx::query_as(
            "WITH terminal_states(definition_id, state, task_status) AS (
                 SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
             ),
             persisted_terminal_states AS (
                 SELECT definitions.id AS definition_id,
                        definitions.version AS definition_version,
                        definitions.data->>'definition_hash' AS definition_hash,
                        terminal.state,
                        CASE terminal.class
                            WHEN 'succeeded' THEN 'done'
                            WHEN 'failed' THEN 'failed'
                            WHEN 'cancelled' THEN 'cancelled'
                        END AS task_status
                 FROM workflow_definitions AS definitions
                 CROSS JOIN LATERAL jsonb_each_text(
                     CASE
                         WHEN jsonb_typeof(
                             definitions.data->'metadata'->'policy'->'terminal'
                         ) = 'object'
                         THEN definitions.data->'metadata'->'policy'->'terminal'
                         ELSE '{}'::jsonb
                     END
                 ) AS terminal(state, class)
                 WHERE definitions.data->'metadata'->>'kind' = $15
             ),
             submissions AS (
                 SELECT workflow_instances.*,
                        COALESCE(
                            terminal.task_status,
                            persisted_terminal.task_status,
                            CASE workflow_instances.state
                                WHEN 'awaiting_dependencies' THEN 'awaiting_deps'
                                WHEN 'scheduled' THEN 'pending'
                                WHEN 'discovered' THEN 'pending'
                                WHEN 'planning' THEN 'planning'
                                WHEN 'checking' THEN 'implementing'
                                WHEN 'dispatching' THEN 'implementing'
                                WHEN 'implementing' THEN 'implementing'
                                WHEN 'inspecting' THEN 'implementing'
                                WHEN 'planning_batch' THEN 'implementing'
                                WHEN 'reconciling' THEN 'implementing'
                                WHEN 'replanning' THEN 'implementing'
                                WHEN 'scanning' THEN 'implementing'
                                WHEN 'addressing_feedback' THEN 'implementing'
                                ELSE 'waiting'
                            END
                        ) AS task_status
                 FROM workflow_instances
                 LEFT JOIN terminal_states AS terminal
                   ON terminal.definition_id = workflow_instances.definition_id
                  AND terminal.state = workflow_instances.state
                 LEFT JOIN persisted_terminal_states AS persisted_terminal
                   ON persisted_terminal.definition_id = workflow_instances.definition_id
                  AND persisted_terminal.definition_version =
                      (workflow_instances.data->>'definition_version')::bigint
                  AND persisted_terminal.definition_hash =
                      workflow_instances.data->'data'->>'definition_hash'
                  AND persisted_terminal.state = workflow_instances.state
             )
             SELECT data::text FROM submissions
             WHERE ($4::text IS NULL OR data->'data'->>'project_id' = $4)
               AND ($12::text IS NULL OR data->'data'->>'source' = $12)
               AND ($13::text IS NULL OR data->'data'->>'repo' = $13)
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
                   ($7 AND definition_id = 'github_issue_pr'
                       AND data->'data'->>'issue_number' IS NOT NULL)
                   OR ($8 AND definition_id = 'github_issue_pr'
                       AND data->'data'->>'issue_number' IS NULL)
                   OR ($9 AND definition_id <> 'github_issue_pr')
               )
               AND (NOT $10 OR task_status NOT IN ('done', 'failed', 'cancelled'))
               AND (cardinality($11::text[]) = 0 OR task_status = ANY($11::text[]))
               AND (
                   $5::timestamptz IS NULL
                   OR (data->>'created_at')::timestamptz < $5
                   OR (
                       (data->>'created_at')::timestamptz = $5
                       AND COALESCE(
                           data->'data'->>'submission_id',
                           data->'data'->'task_ids'->>0,
                           data->'data'->>'task_id'
                       ) < COALESCE($6::text, '')
                   )
               )
             ORDER BY (data->>'created_at')::timestamptz DESC,
                      COALESCE(
                          data->'data'->>'submission_id',
                          data->'data'->'task_ids'->>0,
                          data->'data'->>'task_id'
                      ) DESC
             LIMIT $14",
        )
        .bind(&terminal_definition_ids)
        .bind(&terminal_states)
        .bind(&terminal_task_statuses)
        .bind(filter.project_id.as_deref())
        .bind(cursor_created_at)
        .bind(cursor_id)
        .bind(filter.include_issue)
        .bind(filter.include_pr)
        .bind(filter.include_prompt)
        .bind(filter.active_only)
        .bind(&filter.task_statuses)
        .bind(filter.source.as_deref())
        .bind(filter.repo.as_deref())
        .bind(limit)
        .bind(DECLARATIVE_DEFINITION_METADATA_KIND)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }
}
