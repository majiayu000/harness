use crate::http::parse_pr_num_from_url;
use crate::task_runner::{TaskState, TaskStatus};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use super::types::{RecoveryRow, TaskRow, TaskSummaryRow, TASK_ROW_COLUMNS};
use super::{RecoveryResult, TaskDb};

impl TaskDb {
    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let status = state.status.as_ref();
        let phase_json = serde_json::to_string(&state.phase)?;
        let settings_json = state
            .request_settings
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok());
        let created_at_dt: Option<DateTime<Utc>> = state
            .created_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, pr_url, rounds, error, source, external_id, \
             parent_id, created_at, repo, depends_on, project, priority, phase, description, \
             request_settings) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                     COALESCE($10, CURRENT_TIMESTAMP), $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&state.id.0)
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .bind(&state.source)
        .bind(&state.external_id)
        .bind(state.parent_id.as_ref().map(|id| &id.0))
        .bind(created_at_dt)
        .bind(&state.repo)
        .bind(&depends_on_json)
        .bind(
            state
                .project_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(state.priority as i64)
        .bind(&phase_json)
        .bind(state.description.as_deref())
        .bind(settings_json.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let phase_json = serde_json::to_string(&state.phase)?;
        let status = state.status.as_ref();
        let settings_json = state
            .request_settings
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok());
        let result = sqlx::query(
            "UPDATE tasks SET status = $1, turn = $2, pr_url = $3, rounds = $4, error = $5, \
                    source = $6, external_id = $7, repo = $8, depends_on = $9, project = $10, \
                    priority = $11, phase = $12, description = $13, request_settings = $14, \
                    updated_at = CURRENT_TIMESTAMP, version = version + 1 \
             WHERE id = $15 AND version = $16",
        )
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .bind(&state.source)
        .bind(&state.external_id)
        .bind(&state.repo)
        .bind(&depends_on_json)
        .bind(
            state
                .project_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(state.priority as i64)
        .bind(&phase_json)
        .bind(state.description.as_deref())
        .bind(settings_json.as_deref())
        .bind(&state.id.0)
        .bind(state.version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "concurrent update conflict on task {}",
                state.id.0
            ));
        }
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE id = $1");
        let row = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TaskRow::try_into_task_state).transpose()
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TaskState>> {
        let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks ORDER BY created_at DESC");
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return tasks whose `status` column matches any value in `statuses`.
    ///
    /// Returns an empty `Vec` when `statuses` is empty.
    pub async fn list_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<TaskState>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = Self::numbered_placeholders(1, statuses.len());
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE status IN ({placeholders}) ORDER BY created_at DESC"
        );
        let mut q = sqlx::query_as::<_, TaskRow>(&sql);
        for status in statuses {
            q = q.bind(*status);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Find an active (non-terminal) task for the same project + external_id.
    pub async fn find_active_duplicate(
        &self,
        project: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM tasks WHERE project = $1 AND external_id = $2 \
             AND status IN ('pending', 'awaiting_deps', 'implementing', \
                           'agent_review', 'waiting', 'reviewing') \
             LIMIT 1",
        )
        .bind(project)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Find a `done` task for the same project + external_id that has a non-null `pr_url`.
    pub async fn find_terminal_with_pr(
        &self,
        project: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, pr_url FROM tasks \
             WHERE project = $1 AND external_id = $2 \
               AND status = 'done' AND pr_url IS NOT NULL \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(project)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Update only the `status` column without touching `updated_at`.
    pub(crate) async fn update_status_only(&self, id: &str, status: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET status = $1 WHERE id = $2")
            .bind(status)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Back-fill `external_id` on a task that was created without one.
    pub async fn update_external_id(&self, id: &str, external_id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET external_id = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND external_id IS NULL",
        )
        .bind(external_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Overwrite `external_id` on an auto-fix task, even if one is already set.
    pub async fn overwrite_external_id_auto_fix(
        &self,
        id: &str,
        external_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET external_id = $1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $2 AND source = 'auto-fix'",
        )
        .bind(external_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return all tasks as lightweight summaries, skipping the heavy `rounds` column.
    pub async fn list_summaries(&self) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        let rows = sqlx::query_as::<_, TaskSummaryRow>(
            "SELECT id, status, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, phase, description \
             FROM tasks ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.id.clone();
            match TaskSummaryRow::try_into_summary(row) {
                Ok(summary) => summaries.push(summary),
                Err(e) => {
                    tracing::warn!(task_id = %id, "skipping malformed task row in list_summaries: {e}");
                }
            }
        }
        Ok(summaries)
    }

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
             WHERE status IN ('done', 'failed') \
             ORDER BY updated_at DESC \
             LIMIT 500",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Return `(task_id, turn)` pairs for the 500 most-recently-completed terminal tasks.
    pub async fn list_terminal_turn_counts(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, turn FROM tasks \
             WHERE status IN ('done', 'failed') \
             ORDER BY updated_at DESC LIMIT 500",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Return `(id, status)` pairs for all tasks.
    pub async fn list_id_status(&self) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM tasks ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// Return only task IDs whose `status` matches any value in `statuses`.
    pub async fn list_ids_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<String>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = Self::numbered_placeholders(1, statuses.len());
        let sql = format!("SELECT id FROM tasks WHERE status IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String,)>(&sql);
        for status in statuses {
            q = q.bind(*status);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Return the status of a single task, fetching only the `status` column.
    pub async fn get_status_only(&self, id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(s,)| s))
    }

    /// Return `true` if a task row with the given ID exists in the database.
    pub async fn exists_by_id(&self, id: &str) -> anyhow::Result<bool> {
        let row: Option<(String,)> = sqlx::query_as("SELECT id FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Apply event-replayed state to a task row (called during startup replay).
    pub async fn apply_replayed_state(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        terminal_status: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(status) = terminal_status {
            let resumable = TaskStatus::resumable_statuses();
            let placeholders = Self::numbered_placeholders(4, resumable.len());
            let sql = format!(
                "UPDATE tasks SET status = $1, pr_url = COALESCE($2, pr_url), \
                 updated_at = CURRENT_TIMESTAMP \
                 WHERE id = $3 AND status IN ({})",
                placeholders
            );
            let query = sqlx::query(&sql).bind(status).bind(pr_url).bind(task_id);
            let query = resumable
                .iter()
                .fold(query, |q, resumable| q.bind(*resumable));
            query.execute(&self.pool).await?;
        } else if let Some(url) = pr_url {
            sqlx::query("UPDATE tasks SET pr_url = $1 WHERE id = $2 AND pr_url IS NULL")
                .bind(url)
                .bind(task_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Recovery on server restart: resume or fail interrupted tasks.
    pub async fn recover_in_progress(&self) -> anyhow::Result<RecoveryResult> {
        let rows = {
            let resumable = TaskStatus::resumable_statuses();
            let placeholders = Self::numbered_placeholders(1, resumable.len());
            let sql = format!(
                "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url, \
                        c.triage_output, c.plan_output, c.pr_url AS ck_pr_url \
                 FROM tasks t \
                 LEFT JOIN task_checkpoints c ON t.id = c.task_id \
                 WHERE t.status IN ({})",
                placeholders
            );
            let query = resumable
                .iter()
                .fold(sqlx::query_as::<_, RecoveryRow>(&sql), |q, status| {
                    q.bind(*status)
                });
            query.fetch_all(&self.pool).await?
        };

        let mut result = RecoveryResult::default();

        for row in rows {
            let effective_pr_url = row
                .task_pr_url
                .as_deref()
                .filter(|u| parse_pr_num_from_url(u).is_some())
                .or_else(|| {
                    row.ck_pr_url
                        .as_deref()
                        .filter(|u| parse_pr_num_from_url(u).is_some())
                });
            let has_pr = effective_pr_url.is_some();
            let has_plan = row.plan_output.is_some();
            let has_triage = row.triage_output.is_some();

            if has_pr || has_plan || has_triage {
                let reason = if has_pr {
                    format!(
                        "resumed after restart (was: {}, pr: {})",
                        row.status,
                        effective_pr_url.unwrap_or("checkpoint")
                    )
                } else if has_plan {
                    format!(
                        "resumed after restart (was: {}, plan checkpoint)",
                        row.status
                    )
                } else {
                    format!(
                        "resumed after restart (was: {}, triage checkpoint)",
                        row.status
                    )
                };
                let task_pr_url_valid = row
                    .task_pr_url
                    .as_deref()
                    .map(|u| parse_pr_num_from_url(u).is_some())
                    .unwrap_or(false);
                let needs_pr_url_writeback = !task_pr_url_valid && effective_pr_url.is_some();
                if needs_pr_url_writeback {
                    sqlx::query(
                        "UPDATE tasks SET status = 'pending', error = NULL, pr_url = $1, \
                         updated_at = CURRENT_TIMESTAMP WHERE id = $2",
                    )
                    .bind(effective_pr_url)
                    .bind(&row.id)
                    .execute(&self.pool)
                    .await?;
                    tracing::info!(
                        task_id = %row.id,
                        was = %row.status,
                        pr_url = ?effective_pr_url,
                        "startup recovery: wrote back pr_url from checkpoint"
                    );
                } else {
                    sqlx::query(
                        "UPDATE tasks SET status = 'pending', error = NULL, \
                         updated_at = CURRENT_TIMESTAMP WHERE id = $1",
                    )
                    .bind(&row.id)
                    .execute(&self.pool)
                    .await?;
                }
                result.resumed += 1;
                tracing::info!(
                    task_id = %row.id,
                    was = %row.status,
                    reason = %reason,
                    "startup recovery: resumed task"
                );
            } else {
                let err = format!(
                    "recovered after restart (was: {}, round: {}, pr: {})",
                    row.status,
                    row.turn,
                    row.task_pr_url.as_deref().unwrap_or("none")
                );
                sqlx::query(
                    "UPDATE tasks SET status = 'failed', error = $1, \
                     updated_at = CURRENT_TIMESTAMP WHERE id = $2",
                )
                .bind(&err)
                .bind(&row.id)
                .execute(&self.pool)
                .await?;
                result.failed += 1;
            }
        }

        let transient_failed = sqlx::query(
            "UPDATE tasks \
             SET status = 'failed', \
                 error = 'recovered after restart (was: pending in transient retry): ' \
                      || COALESCE(error, ''), \
                 updated_at = CURRENT_TIMESTAMP \
             WHERE status = 'pending' \
               AND error LIKE 'retrying after transient failure%'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected() as u32;
        if transient_failed > 0 {
            tracing::info!(
                "startup recovery: failed {} task(s) that were pending mid-transient-retry",
                transient_failed
            );
        }
        result.transient_failed = transient_failed;
        Ok(result)
    }

    /// Return the `pr_url` of the most recently completed Done task that has one.
    pub async fn latest_done_pr_url(&self) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT pr_url FROM tasks WHERE status = 'done' AND pr_url IS NOT NULL \
             ORDER BY updated_at DESC LIMIT 1",
        )
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
             WHERE status = 'done' AND pr_url IS NOT NULL AND project = $1 \
             ORDER BY updated_at DESC LIMIT 1",
        )
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
               WHERE status = 'done' AND pr_url IS NOT NULL AND project IS NOT NULL\
             ) AS subq WHERE rn = 1",
        )
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
             FROM tasks WHERE status IN ({})",
            Self::numbered_placeholders(1, outcome_statuses.len())
        );
        let global: (i64, i64) = outcome_statuses
            .iter()
            .fold(sqlx::query_as(&global_sql), |q, status| q.bind(*status))
            .fetch_one(&self.pool)
            .await?;
        let rows_sql = format!(
            "SELECT project, \
                    COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks \
             WHERE status IN ({}) AND project IS NOT NULL \
             GROUP BY project",
            Self::numbered_placeholders(1, outcome_statuses.len())
        );
        let rows: Vec<(String, i64, i64)> = outcome_statuses
            .iter()
            .fold(sqlx::query_as(&rows_sql), |q, status| q.bind(*status))
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
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM tasks WHERE status = 'done' AND updated_at >= $1")
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
             WHERE status = 'done' AND updated_at >= $1 \
             GROUP BY project, date_trunc('hour', updated_at AT TIME ZONE 'UTC')",
        )
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
            "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE parent_id = $1 ORDER BY created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
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
                 WHERE status IN ('implementing', 'agent_review', 'waiting', 'reviewing') \
                 AND external_id IS NOT NULL \
                 AND updated_at < $1 AND project = $2 \
                 ORDER BY updated_at ASC LIMIT 100"
            )
        } else {
            format!(
                "SELECT {TASK_ROW_COLUMNS} FROM tasks \
                 WHERE status IN ('implementing', 'agent_review', 'waiting', 'reviewing') \
                 AND external_id IS NOT NULL \
                 AND updated_at < $1 \
                 ORDER BY updated_at ASC LIMIT 100"
            )
        };
        let mut q = sqlx::query_as::<_, TaskRow>(&sql).bind(cutoff);
        if let Some(proj) = project {
            q = q.bind(proj);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Expose the raw pool for test-only SQL setup (e.g. back-dating `updated_at`).
    #[cfg(test)]
    pub(crate) fn pool_for_test(&self) -> &sqlx::PgPool {
        &self.pool
    }
}
