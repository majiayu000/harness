use crate::http::parse_pr_num_from_url;
use crate::task_runner::{TaskState, TaskStatus};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use super::types::{
    ExternalStatusRow, RecentFailureRow, RecoveryRow, TaskRow, TaskSummaryRow, TASK_ROW_COLUMNS,
};
use super::{RecoveryResult, TaskDb};

impl TaskDb {
    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let status = state.status.as_ref();
        let task_kind = state.task_kind.as_ref();
        let phase_json = serde_json::to_string(&state.phase)?;
        let scheduler_json = serde_json::to_string(&state.scheduler)?;
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
            "INSERT INTO tasks (id, task_kind, status, failure_kind, turn, pr_url, rounds, error, source, \
             external_id, parent_id, created_at, repo, depends_on, project, workspace_path, \
             workspace_owner, run_generation, priority, phase, description, request_settings, scheduler_state) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
                     COALESCE($12, CURRENT_TIMESTAMP), $13, $14, $15, $16, $17, $18, \
                     $19, $20, $21, $22, $23)",
        )
        .bind(&state.id.0)
        .bind(task_kind)
        .bind(status)
        .bind(state.failure_kind.as_ref().map(|kind| kind.as_ref()))
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
        .bind(
            state
                .workspace_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(state.workspace_owner.as_deref())
        .bind(state.run_generation as i64)
        .bind(state.priority as i64)
        .bind(&phase_json)
        .bind(state.description.as_deref())
        .bind(settings_json.as_deref())
        .bind(&scheduler_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let phase_json = serde_json::to_string(&state.phase)?;
        let scheduler_json = serde_json::to_string(&state.scheduler)?;
        let status = state.status.as_ref();
        let task_kind = state.task_kind.as_ref();
        let settings_json = state
            .request_settings
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok());
        sqlx::query(
            "UPDATE tasks SET task_kind = $1, status = $2, failure_kind = $3, turn = $4, pr_url = $5, rounds = $6, \
                    error = $7, source = $8, external_id = $9, repo = $10, depends_on = $11, \
                    project = $12, workspace_path = $13, workspace_owner = $14, \
                    run_generation = $15, priority = $16, phase = $17, description = $18, \
                    request_settings = $19, scheduler_state = $20, updated_at = CURRENT_TIMESTAMP, \
                    version = version + 1 WHERE id = $21",
        )
        .bind(task_kind)
        .bind(status)
        .bind(state.failure_kind.as_ref().map(|kind| kind.as_ref()))
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
        .bind(
            state
                .workspace_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(state.workspace_owner.as_deref())
        .bind(state.run_generation as i64)
        .bind(state.priority as i64)
        .bind(&phase_json)
        .bind(state.description.as_deref())
        .bind(settings_json.as_deref())
        .bind(&scheduler_json)
        .bind(&state.id.0)
        .execute(&self.pool)
        .await?;
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

    /// Return pending tasks that have no PR URL and no checkpoint row.
    pub async fn pending_orphan_tasks(&self) -> anyhow::Result<Vec<TaskState>> {
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} \
             FROM tasks t \
             WHERE t.status = 'pending' \
               AND t.pr_url IS NULL \
               AND NOT EXISTS (SELECT 1 FROM task_checkpoints c WHERE c.task_id = t.id) \
             ORDER BY t.created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return the latest `(external_id, status, created_at)` row for each issue task
    /// in a single project/repo namespace.
    pub async fn list_latest_external_statuses_by_project_and_repo(
        &self,
        project: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
        let base_sql = "SELECT external_id, status, created_at FROM (\
                 SELECT external_id, status, created_at, \
                        ROW_NUMBER() OVER (PARTITION BY external_id ORDER BY created_at DESC, id DESC) AS rn \
                 FROM tasks \
                 WHERE project = $1 AND external_id IS NOT NULL";
        let rows: Vec<ExternalStatusRow> = if let Some(repo) = repo {
            let sql = format!("{base_sql} AND repo = $2) latest WHERE rn = 1");
            sqlx::query_as::<_, ExternalStatusRow>(&sql)
                .bind(project)
                .bind(repo)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!("{base_sql} AND repo IS NULL) latest WHERE rn = 1");
            sqlx::query_as::<_, ExternalStatusRow>(&sql)
                .bind(project)
                .fetch_all(&self.pool)
                .await?
        };

        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.external_id,
                    row.status,
                    row.created_at.map(|dt| dt.to_rfc3339()),
                )
            })
            .collect())
    }

    /// Find an active (non-terminal) task for the same project + external_id.
    pub async fn find_active_duplicate(
        &self,
        project: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let active_statuses: Vec<&str> = std::iter::once(TaskStatus::Pending.as_ref())
            .chain(std::iter::once(TaskStatus::AwaitingDeps.as_ref()))
            .chain(TaskStatus::resumable_statuses().iter().copied())
            .collect();
        let placeholders = Self::numbered_placeholders(3, active_statuses.len());
        let sql = format!(
            "SELECT id FROM tasks WHERE project = $1 AND external_id = $2 \
             AND status IN ({placeholders}) LIMIT 1"
        );
        let mut query = sqlx::query_as::<_, (String,)>(&sql)
            .bind(project)
            .bind(external_id);
        for status in active_statuses {
            query = query.bind(status);
        }
        let row = query.fetch_optional(&self.pool).await?;
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
    pub(crate) async fn update_status_only(
        &self,
        id: &str,
        status: &str,
        scheduler_state: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET status = $1, scheduler_state = $2 WHERE id = $3")
            .bind(status)
            .bind(scheduler_state)
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
            "SELECT id, task_kind, status, failure_kind, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, workspace_path, workspace_owner, \
             run_generation, phase, description, scheduler_state \
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
            let scheduler_state_row: Option<(String,)> =
                sqlx::query_as("SELECT scheduler_state FROM tasks WHERE id = $1")
                    .bind(task_id)
                    .fetch_optional(&self.pool)
                    .await?;
            let mut scheduler = scheduler_state_row
                .and_then(|(value,)| {
                    serde_json::from_str::<crate::task_runner::TaskSchedulerState>(&value).ok()
                })
                .unwrap_or_default();
            if let Ok(task_status) = status.parse::<TaskStatus>() {
                scheduler.mark_terminal(&task_status);
            }
            let scheduler_json = serde_json::to_string(&scheduler)?;
            let resumable = TaskStatus::resumable_statuses();
            let placeholders = Self::numbered_placeholders(5, resumable.len());
            let sql = format!(
                "UPDATE tasks SET status = $1, pr_url = COALESCE($2, pr_url), \
                 scheduler_state = $3, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = $4 AND status IN ({})",
                placeholders
            );
            let query = sqlx::query(&sql)
                .bind(status)
                .bind(pr_url)
                .bind(&scheduler_json)
                .bind(task_id);
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
                "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url, t.scheduler_state, \
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
            let mut scheduler = serde_json::from_str::<crate::task_runner::TaskSchedulerState>(
                &row.scheduler_state,
            )
            .unwrap_or_default();
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
                scheduler.mark_recovering("startup-recovery");
                let task_pr_url_valid = row
                    .task_pr_url
                    .as_deref()
                    .map(|u| parse_pr_num_from_url(u).is_some())
                    .unwrap_or(false);
                let needs_pr_url_writeback = !task_pr_url_valid && effective_pr_url.is_some();
                let scheduler_json = serde_json::to_string(&scheduler)?;
                if needs_pr_url_writeback {
                    sqlx::query(
                        "UPDATE tasks SET status = 'pending', error = NULL, pr_url = $1, \
                         scheduler_state = $2, updated_at = CURRENT_TIMESTAMP WHERE id = $3",
                    )
                    .bind(effective_pr_url)
                    .bind(&scheduler_json)
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
                         scheduler_state = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
                    )
                    .bind(&scheduler_json)
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
                scheduler.mark_terminal(&TaskStatus::Failed);
                let scheduler_json = serde_json::to_string(&scheduler)?;
                sqlx::query(
                    "UPDATE tasks SET status = 'failed', error = $1, \
                     scheduler_state = $2, updated_at = CURRENT_TIMESTAMP WHERE id = $3",
                )
                .bind(&err)
                .bind(&scheduler_json)
                .bind(&row.id)
                .execute(&self.pool)
                .await?;
                result.failed += 1;
            }
        }

        let transient_failed_rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT id, error, scheduler_state FROM tasks \
             WHERE status = 'pending' \
               AND error LIKE 'retrying after transient failure%'",
        )
        .fetch_all(&self.pool)
        .await?;
        for (id, error, scheduler_state) in &transient_failed_rows {
            let mut scheduler =
                serde_json::from_str::<crate::task_runner::TaskSchedulerState>(scheduler_state)
                    .unwrap_or_default();
            scheduler.mark_terminal(&TaskStatus::Failed);
            let scheduler_json = serde_json::to_string(&scheduler)?;
            let err = format!(
                "recovered after restart (was: pending in transient retry): {}",
                error.as_deref().unwrap_or_default()
            );
            sqlx::query(
                "UPDATE tasks SET status = 'failed', error = $1, scheduler_state = $2, \
                 updated_at = CURRENT_TIMESTAMP WHERE id = $3",
            )
            .bind(err)
            .bind(&scheduler_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        let transient_failed = transient_failed_rows.len() as u32;
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
                 WHERE status IN ('triaging', 'planning', 'implementing', \
                                  'review_generating', 'review_waiting', \
                                  'planner_generating', 'planner_waiting', 'agent_review', \
                                  'waiting', 'reviewing') \
                 AND external_id IS NOT NULL \
                 AND updated_at < $1 AND project = $2 \
                 ORDER BY updated_at ASC LIMIT 100"
            )
        } else {
            format!(
                "SELECT {TASK_ROW_COLUMNS} FROM tasks \
                 WHERE status IN ('triaging', 'planning', 'implementing', \
                                  'review_generating', 'review_waiting', \
                                  'planner_generating', 'planner_waiting', 'agent_review', \
                                  'waiting', 'reviewing') \
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

    /// Return the most recent `limit` failed tasks, newest first.
    pub async fn list_recent_failed(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<crate::task_runner::RecentFailureTask>> {
        let rows = sqlx::query_as::<_, RecentFailureRow>(
            "SELECT id, failure_kind, external_id, project, workspace_path, workspace_owner, \
             run_generation, error, updated_at FROM tasks \
             WHERE status = 'failed' \
             ORDER BY updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(RecentFailureRow::into_recent_failure)
            .collect())
    }

    /// Expose the raw pool for test-only SQL setup (e.g. back-dating `updated_at`).
    #[cfg(test)]
    pub(crate) fn pool_for_test(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// Overwrite the `rounds` column with arbitrary raw text.
    ///
    /// Used by integration tests to simulate a corrupted DB payload without going
    /// through the normal serialization path. The name suffix `_for_test` signals
    /// that production code must never call this.
    #[doc(hidden)]
    pub async fn overwrite_rounds_for_test(
        &self,
        task_id: &str,
        raw_rounds: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET rounds = $1 WHERE id = $2")
            .bind(raw_rounds)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::{TaskId, TaskState, TaskStatus};

    #[tokio::test]
    async fn pending_orphan_tasks_returns_only_true_orphans() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = TaskDb::open(&dir.path().join("tasks.db")).await?;

        let mut orphan_issue = TaskState::new(TaskId::new());
        orphan_issue.status = TaskStatus::Pending;
        orphan_issue.external_id = Some("issue:944".to_string());
        db.insert(&orphan_issue).await?;

        let mut orphan_prompt = TaskState::new(TaskId::new());
        orphan_prompt.status = TaskStatus::Pending;
        db.insert(&orphan_prompt).await?;

        let mut with_pr = TaskState::new(TaskId::new());
        with_pr.status = TaskStatus::Pending;
        with_pr.pr_url = Some("https://github.com/org/repo/pull/944".to_string());
        db.insert(&with_pr).await?;

        let mut with_checkpoint = TaskState::new(TaskId::new());
        with_checkpoint.status = TaskStatus::Pending;
        db.insert(&with_checkpoint).await?;
        db.write_checkpoint(&with_checkpoint.id.0, None, Some("plan"), None, "plan")
            .await?;

        let mut failed = TaskState::new(TaskId::new());
        failed.status = TaskStatus::Failed;
        db.insert(&failed).await?;

        let orphan_ids: std::collections::HashSet<_> = db
            .pending_orphan_tasks()
            .await?
            .into_iter()
            .map(|task| task.id)
            .collect();

        assert_eq!(orphan_ids.len(), 2);
        assert!(orphan_ids.contains(&orphan_issue.id));
        assert!(orphan_ids.contains(&orphan_prompt.id));
        assert!(!orphan_ids.contains(&with_pr.id));
        assert!(!orphan_ids.contains(&with_checkpoint.id));
        assert!(!orphan_ids.contains(&failed.id));
        Ok(())
    }
}
