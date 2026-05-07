//! Startup-recovery and event-replay write paths for `TaskDb`.
//!
//! These methods exclusively act on tasks that are in interrupted/transient
//! states at server start. They live in a separate impl block to keep
//! `queries_tasks.rs` under the project's per-file line ceiling.

use crate::http::parse_pr_num_from_url;
use crate::task_runner::TaskStatus;

use super::types::{RecoveryResult, RecoveryRow};
use super::TaskDb;

impl TaskDb {
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
}
