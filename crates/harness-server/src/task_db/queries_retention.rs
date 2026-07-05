use chrono::{DateTime, Utc};

use super::types::TaskRetentionPruneSummary;
use super::TaskDb;
use crate::task_runner::TaskStatus;

impl TaskDb {
    pub async fn prune_terminal_tasks_before(
        &self,
        cutoff: DateTime<Utc>,
        batch_size: u32,
    ) -> anyhow::Result<TaskRetentionPruneSummary> {
        super::record_task_db_usage();
        if batch_size == 0 {
            return Ok(TaskRetentionPruneSummary::default());
        }

        let terminal_statuses = TaskStatus::terminal_statuses();
        let placeholders = Self::numbered_placeholders(3, terminal_statuses.len());
        let batch_param = terminal_statuses.len() + 3;
        let select_sql = format!(
            "SELECT id FROM tasks \
             WHERE store_key = $1 AND updated_at < $2 AND status IN ({placeholders}) \
             ORDER BY updated_at ASC, id ASC LIMIT ${batch_param} FOR UPDATE SKIP LOCKED"
        );

        let mut tx = self.pool.begin().await?;
        let mut select_query = sqlx::query_as::<_, (String,)>(&select_sql)
            .bind(&self.store_key)
            .bind(cutoff);
        for status in terminal_statuses {
            select_query = select_query.bind(*status);
        }
        let task_ids: Vec<String> = select_query
            .bind(i64::from(batch_size))
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|(id,)| id)
            .collect();

        if task_ids.is_empty() {
            tx.commit().await?;
            return Ok(TaskRetentionPruneSummary::default());
        }

        let artifacts_deleted =
            sqlx::query("DELETE FROM task_artifacts WHERE store_key = $1 AND task_id = ANY($2)")
                .bind(&self.store_key)
                .bind(&task_ids)
                .execute(&mut *tx)
                .await?
                .rows_affected();

        let prompts_deleted =
            sqlx::query("DELETE FROM task_prompts WHERE store_key = $1 AND task_id = ANY($2)")
                .bind(&self.store_key)
                .bind(&task_ids)
                .execute(&mut *tx)
                .await?
                .rows_affected();

        let checkpoints_deleted =
            sqlx::query("DELETE FROM task_checkpoints WHERE store_key = $1 AND task_id = ANY($2)")
                .bind(&self.store_key)
                .bind(&task_ids)
                .execute(&mut *tx)
                .await?
                .rows_affected();

        let tasks_deleted = sqlx::query("DELETE FROM tasks WHERE store_key = $1 AND id = ANY($2)")
            .bind(&self.store_key)
            .bind(&task_ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        tx.commit().await?;
        Ok(TaskRetentionPruneSummary {
            pruned_task_ids: task_ids,
            tasks_deleted,
            artifacts_deleted,
            prompts_deleted,
            checkpoints_deleted,
        })
    }
}
