use crate::task_runner::{TaskState, TaskStatus};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use super::types::{RecentFailureRow, TaskRow, TASK_ROW_COLUMNS};
use super::TaskDb;

impl TaskDb {
    /// Return `(task_id, first_token_latency_ms)` pairs for the 500 most-recently
    /// completed terminal tasks. Latency extracted via jsonb correlated subquery.
    pub async fn list_terminal_first_token_latencies_ms(
        &self,
    ) -> anyhow::Result<Vec<(String, Option<i64>)>> {
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT id, \
             (SELECT (r.value ->> 'first_token_latency_ms')::BIGINT \
              FROM jsonb_array_elements(rounds::jsonb) AS r(value) \
              WHERE r.value ->> 'result' != 'resumed_checkpoint' \
                AND r.value ->> 'first_token_latency_ms' IS NOT NULL \
              LIMIT 1) AS first_token_latency_ms \
             FROM tasks \
             WHERE store_key = $1 AND status IN ('done', 'failed') \
             ORDER BY updated_at DESC \
             LIMIT 500",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Return `(task_id, turn)` pairs for the 500 most-recently-completed terminal tasks.
    pub async fn list_terminal_turn_counts(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, turn FROM tasks \
             WHERE store_key = $1 AND status IN ('done', 'failed') \
             ORDER BY updated_at DESC LIMIT 500",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Return `(id, status)` pairs for all tasks.
    pub async fn list_id_status(&self) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, status FROM tasks WHERE store_key = $1 ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Return only task IDs whose `status` matches any value in `statuses`.
    pub async fn list_ids_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<String>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = Self::numbered_placeholders(2, statuses.len());
        let sql =
            format!("SELECT id FROM tasks WHERE store_key = $1 AND status IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String,)>(&sql).bind(&self.store_key);
        for status in statuses {
            q = q.bind(*status);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Return the status of a single task, fetching only the `status` column.
    pub async fn get_status_only(&self, id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM tasks WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(s,)| s))
    }

    /// Return the current optimistic-locking version of a single task.
    pub(crate) async fn get_version_only(&self, id: &str) -> anyhow::Result<Option<i32>> {
        let row: Option<(i32,)> =
            sqlx::query_as("SELECT version FROM tasks WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(version,)| version))
    }

    /// Return `true` if a task row with the given ID exists in the database.
    pub async fn exists_by_id(&self, id: &str) -> anyhow::Result<bool> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM tasks WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    /// Return the `pr_url` of the most recently completed Done task that has one.
    pub async fn latest_done_pr_url(&self) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT pr_url FROM tasks \
             WHERE store_key = $1 AND status = 'done' AND pr_url IS NOT NULL \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(&self.store_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(pr_url,)| pr_url))
    }

    /// Return the `pr_url` of the most recent Done task for the given project root path.
    pub async fn latest_done_pr_url_by_project(
        &self,
        project: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT pr_url FROM tasks \
             WHERE store_key = $1 AND status = 'done' AND pr_url IS NOT NULL AND project = $2 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(project)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(pr_url,)| pr_url))
    }

    /// Return the latest done PR URL for every project that has one, in a single query.
    pub async fn latest_done_pr_urls_all_projects(
        &self,
    ) -> anyhow::Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT project, pr_url FROM (\
               SELECT project, pr_url, \
                      ROW_NUMBER() OVER (PARTITION BY project ORDER BY updated_at DESC) AS rn \
               FROM tasks \
               WHERE store_key = $1 \
                 AND status = 'done' \
                 AND pr_url IS NOT NULL \
                 AND project IS NOT NULL\
             ) AS subq WHERE rn = 1",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Count done/failed tasks globally and per project.
    pub async fn count_done_failed_by_project(
        &self,
    ) -> anyhow::Result<(u64, u64, Vec<(String, u64, u64)>)> {
        let outcome_statuses = [TaskStatus::Done.as_ref(), TaskStatus::Failed.as_ref()];
        let global_sql = format!(
            "SELECT COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks WHERE store_key = $1 AND status IN ({})",
            Self::numbered_placeholders(2, outcome_statuses.len())
        );
        let global: (i64, i64) = outcome_statuses
            .iter()
            .fold(
                sqlx::query_as(&global_sql).bind(&self.store_key),
                |q, status| q.bind(*status),
            )
            .fetch_one(&self.pool)
            .await?;
        let rows_sql = format!(
            "SELECT project, \
                    COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks \
             WHERE store_key = $1 AND status IN ({}) AND project IS NOT NULL \
             GROUP BY project",
            Self::numbered_placeholders(2, outcome_statuses.len())
        );
        let rows: Vec<(String, i64, i64)> = outcome_statuses
            .iter()
            .fold(
                sqlx::query_as(&rows_sql).bind(&self.store_key),
                |q, status| q.bind(*status),
            )
            .fetch_all(&self.pool)
            .await?;
        let by_project = rows
            .into_iter()
            .map(|(p, d, f)| (p, d as u64, f as u64))
            .collect();
        Ok((global.0 as u64, global.1 as u64, by_project))
    }

    /// Count tasks that reached `done` status with `updated_at >= since`.
    pub async fn count_done_since(&self, since: DateTime<Utc>) -> anyhow::Result<u64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM tasks \
             WHERE store_key = $1 AND status = 'done' AND updated_at >= $2",
        )
        .bind(&self.store_key)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 as u64)
    }

    /// Per-(project, hour-bucket) count of tasks that reached `done` after `since`.
    pub async fn done_per_project_hour_since(
        &self,
        since: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(String, String, u64)>> {
        let rows: Vec<(Option<String>, String, i64)> = sqlx::query_as(
            "SELECT project, \
                    TO_CHAR(date_trunc('hour', updated_at AT TIME ZONE 'UTC'), \
                            'YYYY-MM-DD\"T\"HH24:00:00\"Z\"') AS hour, \
                    COUNT(*) \
             FROM tasks \
             WHERE store_key = $1 AND status = 'done' AND updated_at >= $2 \
             GROUP BY project, date_trunc('hour', updated_at AT TIME ZONE 'UTC')",
        )
        .bind(&self.store_key)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p, h, c)| (p.unwrap_or_default(), h, c as u64))
            .collect())
    }

    /// Return all tasks whose `parent_id` matches the given parent task ID.
    pub async fn list_children(&self, parent_id: &str) -> anyhow::Result<Vec<TaskState>> {
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} FROM tasks \
             WHERE store_key = $1 AND parent_id = $2 ORDER BY created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(&self.store_key)
            .bind(parent_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return tasks that are active and have not been updated for at least `stale_threshold`.
    pub async fn list_stalled_tasks(
        &self,
        stale_threshold: std::time::Duration,
        project: Option<&str>,
    ) -> anyhow::Result<Vec<TaskState>> {
        let cutoff = Utc::now() - chrono::Duration::from_std(stale_threshold)?;
        let sql = if project.is_some() {
            format!(
                "SELECT {TASK_ROW_COLUMNS} FROM tasks \
                 WHERE store_key = $1 \
                 AND status IN ('triaging', 'planning', 'implementing', \
                                  'review_generating', 'review_waiting', \
                                  'planner_generating', 'planner_waiting', 'agent_review', \
                                  'waiting', 'reviewing') \
                 AND external_id IS NOT NULL \
                 AND updated_at < $2 AND project = $3 \
                 ORDER BY updated_at ASC LIMIT 100"
            )
        } else {
            format!(
                "SELECT {TASK_ROW_COLUMNS} FROM tasks \
                 WHERE store_key = $1 \
                 AND status IN ('triaging', 'planning', 'implementing', \
                                  'review_generating', 'review_waiting', \
                                  'planner_generating', 'planner_waiting', 'agent_review', \
                                  'waiting', 'reviewing') \
                 AND external_id IS NOT NULL \
                 AND updated_at < $2 \
                 ORDER BY updated_at ASC LIMIT 100"
            )
        };
        let mut q = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(&self.store_key)
            .bind(cutoff);
        if let Some(proj) = project {
            q = q.bind(proj);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return the most recent `limit` failed tasks, newest first.
    pub async fn list_recent_failed(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<crate::task_runner::RecentFailureTask>> {
        let rows = sqlx::query_as::<_, RecentFailureRow>(
            "SELECT id, failure_kind, external_id, project, workspace_path, workspace_owner, \
             run_generation, error, updated_at FROM tasks \
             WHERE store_key = $1 AND status = 'failed' \
             ORDER BY updated_at DESC LIMIT $2",
        )
        .bind(&self.store_key)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(RecentFailureRow::into_recent_failure)
            .collect())
    }
}
