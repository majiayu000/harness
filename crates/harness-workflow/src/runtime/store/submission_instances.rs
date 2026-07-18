use super::instance_helpers::terminal_task_status_rows;
use super::{
    WorkflowInstance, WorkflowRuntimeStore, WorkflowSubmissionHourlyDone,
    WorkflowSubmissionMetrics, WorkflowSubmissionProjectMetrics,
};
use crate::runtime::declarative_pinning::DECLARATIVE_DEFINITION_METADATA_KIND;
use chrono::{DateTime, Utc};

#[derive(Debug, sqlx::FromRow)]
struct SubmissionMetricRow {
    row_kind: String,
    project_id: String,
    done: i64,
    failed: i64,
    stalled: i64,
    latest_pr_url: Option<String>,
    latest_pr_updated_at: Option<DateTime<Utc>>,
    hour: Option<String>,
    hourly_done: i64,
}

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

    /// Aggregate durable top-level submission metrics without materializing
    /// every workflow instance on dashboard polling paths.
    pub async fn submission_metrics_since(
        &self,
        since: DateTime<Utc>,
    ) -> anyhow::Result<WorkflowSubmissionMetrics> {
        let (terminal_definition_ids, terminal_states, terminal_task_statuses) =
            terminal_task_status_rows();
        let stalled_reason = "round_budget_exhausted";
        let stalled_pattern = format!("%\"reason\":\"{stalled_reason}\"%");
        let rows = sqlx::query_as::<_, SubmissionMetricRow>(
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
                 WHERE definitions.data->'metadata'->>'kind' = $4
             ),
             submissions AS (
                 SELECT COALESCE(workflow_instances.data->'data'->>'project_id', '')
                            AS project_id,
                        workflow_instances.data->'data'->>'pr_url' AS pr_url,
                        workflow_instances.updated_at,
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
                        ) AS task_status,
                        (
                            COALESCE(
                                workflow_instances.data->'data'->>'stop_reason_code', ''
                            ) = $6
                            OR COALESCE(
                                workflow_instances.data->'data'->'last_stop'->>'stop_reason_code',
                                ''
                            ) = $6
                            OR COALESCE(
                                workflow_instances.data->'data'->>'failure_reason', ''
                            ) LIKE $7
                        ) AS is_stalled
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
                 WHERE workflow_instances.parent_workflow_id IS NULL
                   AND (
                       workflow_instances.definition_id IN ('github_issue_pr', 'prompt_task')
                       OR NULLIF(BTRIM(
                           workflow_instances.data->'data'->>'definition_hash'
                       ), '') IS NOT NULL
                   )
                   AND COALESCE(
                       workflow_instances.data->'data'->>'submission_id',
                       workflow_instances.data->'data'->'task_ids'->>0,
                       workflow_instances.data->'data'->>'task_id'
                   ) IS NOT NULL
             ),
             project_counts AS (
                 SELECT project_id,
                        COUNT(*) FILTER (WHERE task_status = 'done') AS done,
                        COUNT(*) FILTER (WHERE task_status = 'failed') AS failed,
                        COUNT(*) FILTER (
                            WHERE task_status = 'failed' AND is_stalled
                        ) AS stalled,
                        (ARRAY_AGG(pr_url ORDER BY updated_at DESC)
                            FILTER (
                                WHERE task_status = 'done' AND pr_url IS NOT NULL
                            ))[1] AS latest_pr_url,
                        MAX(updated_at) FILTER (
                            WHERE task_status = 'done' AND pr_url IS NOT NULL
                        ) AS latest_pr_updated_at
                 FROM submissions
                 GROUP BY project_id
             ),
             hourly_counts AS (
                 SELECT project_id,
                        TO_CHAR(
                            DATE_TRUNC('hour', updated_at AT TIME ZONE 'UTC'),
                            'YYYY-MM-DD\"T\"HH24:00:00\"Z\"'
                        ) AS hour,
                        COUNT(*) AS hourly_done
                 FROM submissions
                 WHERE task_status = 'done' AND updated_at >= $5
                 GROUP BY project_id,
                          DATE_TRUNC('hour', updated_at AT TIME ZONE 'UTC')
             )
             SELECT 'project'::text AS row_kind,
                    project_id,
                    done,
                    failed,
                    stalled,
                    latest_pr_url,
                    latest_pr_updated_at,
                    NULL::text AS hour,
                    0::bigint AS hourly_done
             FROM project_counts
             UNION ALL
             SELECT 'hour'::text AS row_kind,
                    project_id,
                    0::bigint AS done,
                    0::bigint AS failed,
                    0::bigint AS stalled,
                    NULL::text AS latest_pr_url,
                    NULL::timestamptz AS latest_pr_updated_at,
                    hour,
                    hourly_done
             FROM hourly_counts
             ORDER BY row_kind, project_id, hour NULLS FIRST",
        )
        .bind(&terminal_definition_ids)
        .bind(&terminal_states)
        .bind(&terminal_task_statuses)
        .bind(DECLARATIVE_DEFINITION_METADATA_KIND)
        .bind(since)
        .bind(stalled_reason)
        .bind(stalled_pattern)
        .fetch_all(&self.pool)
        .await?;
        submission_metrics_from_rows(rows)
    }
}

fn submission_metrics_from_rows(
    rows: Vec<SubmissionMetricRow>,
) -> anyhow::Result<WorkflowSubmissionMetrics> {
    let mut metrics = WorkflowSubmissionMetrics::default();
    let mut latest_pr: Option<(DateTime<Utc>, String)> = None;
    for row in rows {
        match row.row_kind.as_str() {
            "project" => {
                let project_metrics = WorkflowSubmissionProjectMetrics {
                    done: nonnegative_count(row.done, "done")?,
                    failed: nonnegative_count(row.failed, "failed")?,
                    stalled: nonnegative_count(row.stalled, "stalled")?,
                    latest_pr_url: row.latest_pr_url.clone(),
                };
                metrics.global_done = metrics
                    .global_done
                    .checked_add(project_metrics.done)
                    .ok_or_else(|| anyhow::anyhow!("global done count overflow"))?;
                metrics.global_failed =
                    metrics
                        .global_failed
                        .checked_add(project_metrics.failed)
                        .ok_or_else(|| anyhow::anyhow!("global failed count overflow"))?;
                metrics.global_stalled = metrics
                    .global_stalled
                    .checked_add(project_metrics.stalled)
                    .ok_or_else(|| anyhow::anyhow!("global stalled count overflow"))?;
                if let (Some(updated_at), Some(url)) = (row.latest_pr_updated_at, row.latest_pr_url)
                {
                    if latest_pr
                        .as_ref()
                        .is_none_or(|(current, _)| updated_at > *current)
                    {
                        latest_pr = Some((updated_at, url));
                    }
                }
                if metrics
                    .by_project
                    .insert(row.project_id.clone(), project_metrics)
                    .is_some()
                {
                    anyhow::bail!("duplicate project metrics row for {}", row.project_id);
                }
            }
            "hour" => {
                let count = nonnegative_count(row.hourly_done, "hourly_done")?;
                let hour = row
                    .hour
                    .ok_or_else(|| anyhow::anyhow!("hour metrics row is missing hour"))?;
                metrics.done_since = metrics
                    .done_since
                    .checked_add(count)
                    .ok_or_else(|| anyhow::anyhow!("done-since count overflow"))?;
                metrics.hourly_done.push(WorkflowSubmissionHourlyDone {
                    project_id: row.project_id,
                    hour,
                    count,
                });
            }
            other => anyhow::bail!("unknown submission metric row kind: {other}"),
        }
    }
    metrics.latest_pr_url = latest_pr.map(|(_, url)| url);
    Ok(metrics)
}

fn nonnegative_count(value: i64, field: &str) -> anyhow::Result<u64> {
    u64::try_from(value).map_err(|_| anyhow::anyhow!("{field} count is negative"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    #[test]
    fn submission_metrics_rows_preserve_overlapping_failed_and_stalled_counts() {
        let now = Utc::now();
        let metrics = submission_metrics_from_rows(vec![
            SubmissionMetricRow {
                row_kind: "project".to_string(),
                project_id: "/repo".to_string(),
                done: 2,
                failed: 3,
                stalled: 1,
                latest_pr_url: Some("https://github.com/owner/repo/pull/2".to_string()),
                latest_pr_updated_at: Some(now),
                hour: None,
                hourly_done: 0,
            },
            SubmissionMetricRow {
                row_kind: "hour".to_string(),
                project_id: "/repo".to_string(),
                done: 0,
                failed: 0,
                stalled: 0,
                latest_pr_url: None,
                latest_pr_updated_at: None,
                hour: Some("2026-07-18T08:00:00Z".to_string()),
                hourly_done: 2,
            },
        ])
        .expect("metric rows should aggregate");

        assert_eq!(metrics.global_done, 2);
        assert_eq!(metrics.global_failed, 3);
        assert_eq!(metrics.global_stalled, 1);
        assert_eq!(metrics.done_since, 2);
        assert_eq!(
            metrics.latest_pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/2")
        );
        assert_eq!(metrics.by_project["/repo"].stalled, 1);
    }

    #[tokio::test]
    async fn submission_metrics_query_aggregates_runtime_workflows() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let subject = WorkflowSubject::new("issue", "owner/repo#1");
        let done = WorkflowInstance::new(GITHUB_ISSUE_PR_DEFINITION_ID, 1, "done", subject.clone())
            .with_id("submission-metrics-done")
            .with_data(json!({
                "submission_id": "submission-metrics-done",
                "project_id": "/repo",
                "pr_url": "https://github.com/owner/repo/pull/1"
            }));
        let failed =
            WorkflowInstance::new(GITHUB_ISSUE_PR_DEFINITION_ID, 1, "failed", subject.clone())
                .with_id("submission-metrics-failed")
                .with_data(json!({
                    "submission_id": "submission-metrics-failed",
                    "project_id": "/repo"
                }));
        let stalled = WorkflowInstance::new(GITHUB_ISSUE_PR_DEFINITION_ID, 1, "failed", subject)
            .with_id("submission-metrics-stalled")
            .with_data(json!({
                "submission_id": "submission-metrics-stalled",
                "project_id": "/repo",
                "failure_reason": "{\"reason\":\"round_budget_exhausted\"}"
            }));
        for workflow in [&done, &failed, &stalled] {
            store.upsert_instance(workflow).await?;
        }

        let metrics = store
            .submission_metrics_since(Utc::now() - chrono::Duration::hours(1))
            .await?;

        assert_eq!(metrics.global_done, 1);
        assert_eq!(metrics.global_failed, 2);
        assert_eq!(metrics.global_stalled, 1);
        assert_eq!(metrics.done_since, 1);
        assert_eq!(metrics.by_project["/repo"].done, 1);
        assert_eq!(metrics.by_project["/repo"].failed, 2);
        assert_eq!(metrics.by_project["/repo"].stalled, 1);
        assert_eq!(
            metrics.latest_pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/1")
        );
        Ok(())
    }
}
