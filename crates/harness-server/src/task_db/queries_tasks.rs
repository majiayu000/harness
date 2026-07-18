use crate::task_runner::{TaskState, TaskStatus, TaskSummaryFilter, TaskSummaryPageCursor};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder};

use super::types::{ExternalStatusRow, TaskRow, TaskSummaryRow, TASK_ROW_COLUMNS};
use super::TaskDb;

impl TaskDb {
    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        super::record_task_db_usage();
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
            "INSERT INTO tasks (store_key, id, task_kind, status, failure_kind, turn, pr_url, rounds, error, source, \
             external_id, parent_id, created_at, repo, depends_on, project, workspace_path, \
             workspace_owner, run_generation, priority, phase, description, request_settings, scheduler_state) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                     COALESCE($13, CURRENT_TIMESTAMP), $14, $15, $16, $17, $18, $19, \
                     $20, $21, $22, $23, $24)",
        )
        .bind(&self.store_key)
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
        super::record_task_db_usage();
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
        let result = sqlx::query(
            "UPDATE tasks SET task_kind = $1, status = $2, failure_kind = $3, turn = $4, pr_url = $5, rounds = $6, \
                    error = $7, source = $8, external_id = $9, repo = $10, depends_on = $11, \
                    project = $12, workspace_path = $13, workspace_owner = $14, \
                    run_generation = $15, priority = $16, phase = $17, description = $18, \
                    request_settings = $19, scheduler_state = $20, updated_at = CURRENT_TIMESTAMP, \
                    version = version + 1 WHERE store_key = $21 AND id = $22 AND version = $23",
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
        .bind(&self.store_key)
        .bind(&state.id.0)
        .bind(state.version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "optimistic-lock conflict updating task {} (expected version {}); reload before retrying",
                state.id.0,
                state.version
            ));
        }
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        super::record_task_db_usage();
        let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE store_key = $1 AND id = $2");
        let row = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(&self.store_key)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TaskRow::try_into_task_state).transpose()
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TaskState>> {
        super::record_task_db_usage();
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE store_key = $1 ORDER BY created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(&self.store_key)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return tasks whose `status` column matches any value in `statuses`.
    ///
    /// Returns an empty `Vec` when `statuses` is empty.
    pub async fn list_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<TaskState>> {
        super::record_task_db_usage();
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = Self::numbered_placeholders(2, statuses.len());
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE store_key = $1 AND status IN ({placeholders}) ORDER BY created_at DESC"
        );
        let mut q = sqlx::query_as::<_, TaskRow>(&sql).bind(&self.store_key);
        for status in statuses {
            q = q.bind(*status);
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Return pending tasks that have no PR URL and no checkpoint row.
    pub async fn pending_orphan_tasks(&self) -> anyhow::Result<Vec<TaskState>> {
        super::record_task_db_usage();
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} \
             FROM tasks t \
             WHERE t.store_key = $1 \
               AND t.status = 'pending' \
               AND t.pr_url IS NULL \
               AND NOT EXISTS (
                   SELECT 1 FROM task_checkpoints c
                   WHERE c.store_key = t.store_key AND c.task_id = t.id
               ) \
             ORDER BY t.created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(&self.store_key)
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
        super::record_task_db_usage();
        let base_sql = "SELECT external_id, status, created_at FROM (\
                 SELECT external_id, status, created_at, \
                        ROW_NUMBER() OVER (PARTITION BY external_id ORDER BY created_at DESC, id DESC) AS rn \
                 FROM tasks \
                 WHERE store_key = $1 AND project = $2 AND external_id IS NOT NULL";
        let rows: Vec<ExternalStatusRow> = if let Some(repo) = repo {
            let sql = format!("{base_sql} AND repo = $3) latest WHERE rn = 1");
            sqlx::query_as::<_, ExternalStatusRow>(&sql)
                .bind(&self.store_key)
                .bind(project)
                .bind(repo)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!("{base_sql} AND repo IS NULL) latest WHERE rn = 1");
            sqlx::query_as::<_, ExternalStatusRow>(&sql)
                .bind(&self.store_key)
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
        super::record_task_db_usage();
        let active_statuses: Vec<&str> = std::iter::once(TaskStatus::Pending.as_ref())
            .chain(std::iter::once(TaskStatus::AwaitingDeps.as_ref()))
            .chain(TaskStatus::resumable_statuses().iter().copied())
            .collect();
        let placeholders = Self::numbered_placeholders(4, active_statuses.len());
        let sql = format!(
            "SELECT id FROM tasks WHERE store_key = $1 AND project = $2 AND external_id = $3 \
             AND status IN ({placeholders}) LIMIT 1"
        );
        let mut query = sqlx::query_as::<_, (String,)>(&sql)
            .bind(&self.store_key)
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
        super::record_task_db_usage();
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, pr_url FROM tasks \
             WHERE store_key = $1 AND project = $2 AND external_id = $3 \
               AND status = 'done' AND pr_url IS NOT NULL \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(project)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Update only the `status` column without touching `updated_at`. Still
    /// bumps `version` so that any concurrent reader-then-writer using
    /// optimistic locking observes this write and refuses to overwrite it.
    #[cfg(test)]
    pub(crate) async fn update_status_only(
        &self,
        id: &str,
        status: &str,
        scheduler_state: &str,
        expected_version: i32,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE tasks SET status = $1, scheduler_state = $2, version = version + 1 \
             WHERE store_key = $3 AND id = $4 AND version = $5",
        )
        .bind(status)
        .bind(scheduler_state)
        .bind(&self.store_key)
        .bind(id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "optimistic-lock conflict updating task status {} (expected version {}); reload before retrying",
                id,
                expected_version
            ));
        }
        Ok(())
    }

    /// Back-fill `external_id` on a task that was created without one.
    pub async fn update_external_id(
        &self,
        id: &str,
        external_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<()> {
        super::record_task_db_usage();
        let result = sqlx::query(
            "UPDATE tasks SET external_id = $1, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 \
             WHERE store_key = $2 AND id = $3 AND external_id IS NULL AND version = $4",
        )
        .bind(external_id)
        .bind(&self.store_key)
        .bind(id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "optimistic-lock conflict updating task external_id {} (expected version {}); reload before retrying",
                id,
                expected_version
            ));
        }
        Ok(())
    }

    /// Overwrite `external_id` on an auto-fix task, even if one is already set.
    pub async fn overwrite_external_id_auto_fix(
        &self,
        id: &str,
        external_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<()> {
        super::record_task_db_usage();
        let result = sqlx::query(
            "UPDATE tasks SET external_id = $1, updated_at = CURRENT_TIMESTAMP, \
             version = version + 1 \
             WHERE store_key = $2 AND id = $3 AND source = 'auto-fix' AND version = $4",
        )
        .bind(external_id)
        .bind(&self.store_key)
        .bind(id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow::anyhow!(
                "optimistic-lock conflict overwriting auto-fix task external_id {} (expected version {}); reload before retrying",
                id,
                expected_version
            ));
        }
        Ok(())
    }

    /// Return all tasks as lightweight summaries, skipping the heavy `rounds` column.
    pub async fn list_summaries(&self) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        super::record_task_db_usage();
        let rows = sqlx::query_as::<_, TaskSummaryRow>(
            "SELECT id, task_kind, status, failure_kind, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, workspace_path, workspace_owner, \
             run_generation, phase, description, scheduler_state \
             FROM tasks WHERE store_key = $1 ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(decode_task_summary_rows(rows, "list_summaries"))
    }

    /// Return active tasks as lightweight summaries, skipping terminal history and `rounds`.
    pub async fn list_active_summaries(
        &self,
    ) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        super::record_task_db_usage();
        let rows = sqlx::query_as::<_, TaskSummaryRow>(
            "SELECT id, task_kind, status, failure_kind, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, workspace_path, workspace_owner, \
             run_generation, phase, description, scheduler_state \
             FROM tasks \
             WHERE store_key = $1 \
               AND status NOT IN ('done', 'failed', 'cancelled') \
             ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(decode_task_summary_rows(rows, "list_active_summaries"))
    }

    pub async fn list_summaries_filtered(
        &self,
        filter: &TaskSummaryFilter,
    ) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        super::record_task_db_usage();
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT id, task_kind, status, failure_kind, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, workspace_path, workspace_owner, \
             run_generation, phase, description, scheduler_state FROM tasks",
        );
        let mut has_where = push_task_store_filter(&mut query, self.store_key.as_str());
        push_task_summary_filter(&mut query, filter, &mut has_where);
        query.push(" ORDER BY created_at DESC, id DESC");

        let rows = query
            .build_query_as::<TaskSummaryRow>()
            .fetch_all(&self.pool)
            .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.id.clone();
            match TaskSummaryRow::try_into_summary(row) {
                Ok(summary) => summaries.push(summary),
                Err(error) => {
                    tracing::warn!(
                        task_id = %id,
                        "skipping malformed task row in list_summaries_filtered: {error}"
                    );
                }
            }
        }
        Ok(summaries)
    }

    pub async fn list_summaries_filtered_page(
        &self,
        filter: &TaskSummaryFilter,
        cursor: Option<&TaskSummaryPageCursor>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        super::record_task_db_usage();
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT id, task_kind, status, failure_kind, turn, pr_url, error, source, external_id, parent_id, \
             created_at, repo, depends_on, project, workspace_path, workspace_owner, \
             run_generation, phase, description, scheduler_state FROM tasks",
        );
        let mut has_where = push_task_store_filter(&mut query, self.store_key.as_str());
        push_task_summary_filter(&mut query, filter, &mut has_where);
        if let Some(cursor) = cursor {
            push_where_prefix(&mut query, &mut has_where);
            query.push("(created_at < ");
            query.push_bind(cursor.created_at);
            query.push(" OR (created_at = ");
            query.push_bind(cursor.created_at);
            query.push(" AND id < ");
            query.push_bind(cursor.id.as_str());
            query.push("))");
        }
        query.push(" ORDER BY created_at DESC, id DESC LIMIT ");
        query.push_bind(i64::try_from(limit.max(1)).unwrap_or(i64::MAX));

        let rows = query
            .build_query_as::<TaskSummaryRow>()
            .fetch_all(&self.pool)
            .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.id.clone();
            match TaskSummaryRow::try_into_summary(row) {
                Ok(summary) => summaries.push(summary),
                Err(error) => {
                    tracing::warn!(
                        task_id = %id,
                        "skipping malformed task row in list_summaries_filtered_page: {error}"
                    );
                }
            }
        }
        Ok(summaries)
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
        sqlx::query("UPDATE tasks SET rounds = $1 WHERE store_key = $2 AND id = $3")
            .bind(raw_rounds)
            .bind(&self.store_key)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn decode_task_summary_rows(
    rows: Vec<TaskSummaryRow>,
    context: &str,
) -> Vec<crate::task_runner::TaskSummary> {
    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.id.clone();
        match TaskSummaryRow::try_into_summary(row) {
            Ok(summary) => summaries.push(summary),
            Err(e) => {
                tracing::warn!(task_id = %id, "{context}: skipping malformed task row: {e}");
            }
        }
    }
    summaries
}

fn push_task_summary_filter<'a>(
    query: &mut QueryBuilder<'a, Postgres>,
    filter: &'a TaskSummaryFilter,
    has_where: &mut bool,
) {
    if !filter.statuses.is_empty() {
        push_where_prefix(query, has_where);
        query.push("status IN (");
        let mut separated = query.separated(", ");
        for status in &filter.statuses {
            separated.push_bind(status.as_ref());
        }
        separated.push_unseparated(")");
    }

    if filter.active {
        push_where_prefix(query, has_where);
        query.push("status NOT IN (");
        let mut separated = query.separated(", ");
        for status in TaskStatus::terminal_statuses() {
            separated.push_bind(*status);
        }
        separated.push_unseparated(")");
    }

    if !filter.scheduler_states.is_empty() {
        push_where_prefix(query, has_where);
        query.push("(scheduler_state::jsonb ->> 'authority_state') IN (");
        let mut separated = query.separated(", ");
        for state in &filter.scheduler_states {
            separated.push_bind(state.as_ref());
        }
        separated.push_unseparated(")");
    }

    if !filter.kinds.is_empty() {
        push_where_prefix(query, has_where);
        query.push("task_kind IN (");
        let mut separated = query.separated(", ");
        for kind in &filter.kinds {
            separated.push_bind(kind.as_ref());
        }
        separated.push_unseparated(")");
    }

    if let Some(source) = filter.source.as_deref() {
        push_where_prefix(query, has_where);
        query.push("source = ");
        query.push_bind(source.to_string());
    }

    if let Some(repo) = filter.repo.as_deref() {
        push_where_prefix(query, has_where);
        query.push("repo = ");
        query.push_bind(repo.to_string());
    }

    if let Some(project) = filter.project.as_deref() {
        push_where_prefix(query, has_where);
        query.push("project = ");
        query.push_bind(project.to_string());
    }
}

fn push_task_store_filter<'a>(query: &mut QueryBuilder<'a, Postgres>, store_key: &'a str) -> bool {
    let mut has_where = false;
    push_where_prefix(query, &mut has_where);
    query.push("store_key = ");
    query.push_bind(store_key);
    has_where
}

fn push_where_prefix(query: &mut QueryBuilder<'_, Postgres>, has_where: &mut bool) {
    if *has_where {
        query.push(" AND ");
    } else {
        query.push(" WHERE ");
        *has_where = true;
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
