use super::row::{TaskRow, TaskSummaryRow};
use super::schema::TASK_ROW_COLUMNS;
use crate::task_runner::{TaskState, TaskStatus};
use sqlx::sqlite::SqlitePool;
use std::collections::HashMap;

pub(super) fn status_placeholders(len: usize) -> String {
    std::iter::repeat_n("?", len).collect::<Vec<_>>().join(", ")
}

pub(super) async fn insert(pool: &SqlitePool, state: &TaskState) -> anyhow::Result<()> {
    let rounds_json = serde_json::to_string(&state.rounds)?;
    let depends_on_json = serde_json::to_string(&state.depends_on)?;
    let status = state.status.as_ref();
    let phase_json = serde_json::to_string(&state.phase)?;
    sqlx::query(
        "INSERT INTO tasks (id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, phase, description)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, datetime('now')), ?, ?, ?, ?, ?, ?)",
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
    .bind(&state.created_at)
    .bind(&state.repo)
    .bind(&depends_on_json)
    .bind(state.project_root.as_ref().map(|p| p.to_string_lossy().into_owned()))
    .bind(state.priority as i64)
    .bind(&phase_json)
    .bind(state.description.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn update(pool: &SqlitePool, state: &TaskState) -> anyhow::Result<()> {
    let rounds_json = serde_json::to_string(&state.rounds)?;
    let depends_on_json = serde_json::to_string(&state.depends_on)?;
    let phase_json = serde_json::to_string(&state.phase)?;
    let status = state.status.as_ref();
    sqlx::query(
        "UPDATE tasks SET status = ?, turn = ?, pr_url = ?, rounds = ?, error = ?,
                source = ?, external_id = ?, repo = ?, depends_on = ?, project = ?,
                priority = ?, phase = ?, description = ?, updated_at = datetime('now')
         WHERE id = ?",
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
    .bind(&state.id.0)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn get(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<TaskState>> {
    let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE id = ?");
    let row = sqlx::query_as::<_, TaskRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(TaskRow::try_into_task_state).transpose()
}

pub(super) async fn list(pool: &SqlitePool) -> anyhow::Result<Vec<TaskState>> {
    let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks ORDER BY created_at DESC");
    let rows = sqlx::query_as::<_, TaskRow>(&sql).fetch_all(pool).await?;
    rows.into_iter().map(TaskRow::try_into_task_state).collect()
}

/// Return tasks whose `status` column matches any value in `statuses`.
///
/// Uses parameterized placeholders — safe for internal status string constants.
/// Returns an empty `Vec` when `statuses` is empty.
pub(super) async fn list_by_status(
    pool: &SqlitePool,
    statuses: &[&str],
) -> anyhow::Result<Vec<TaskState>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = status_placeholders(statuses.len());
    let sql = format!(
        "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE status IN ({placeholders}) ORDER BY created_at DESC"
    );
    let mut q = sqlx::query_as::<_, TaskRow>(&sql);
    for status in statuses {
        q = q.bind(*status);
    }
    let rows = q.fetch_all(pool).await?;
    rows.into_iter().map(TaskRow::try_into_task_state).collect()
}

/// Find an active (non-terminal) task for the same project + external_id.
pub(super) async fn find_active_duplicate(
    pool: &SqlitePool,
    project: &str,
    external_id: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM tasks WHERE project = ? AND external_id = ? \
         AND status IN ('pending', 'awaiting_deps', 'implementing', 'agent_review', 'waiting', 'reviewing') \
         LIMIT 1",
    )
    .bind(project)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Find a `done` task for the same project + external_id that has a non-null `pr_url`.
///
/// Returns `(task_id, pr_url)` when found. Only matches `done` — failed/cancelled
/// tasks are excluded so that retries after failure are always permitted.
pub(super) async fn find_terminal_with_pr(
    pool: &SqlitePool,
    project: &str,
    external_id: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, pr_url FROM tasks \
         WHERE project = ? AND external_id = ? AND status = 'done' AND pr_url IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Return all tasks as lightweight summaries, skipping the heavy `rounds` column.
///
/// Used by the `/tasks` list endpoint to avoid deserializing large round histories
/// when only summary fields are needed.
pub(super) async fn list_summaries(
    pool: &SqlitePool,
) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
    let rows = sqlx::query_as::<_, TaskSummaryRow>(
        "SELECT id, status, turn, pr_url, error, source, external_id, parent_id, \
         created_at, repo, depends_on, project, phase, description \
         FROM tasks ORDER BY created_at DESC",
    )
    .fetch_all(pool)
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

/// Return `(id, status)` pairs for all tasks — skips all heavy columns.
///
/// Used by hot-path callers that only need task status for aggregation
/// (e.g. skill governance scoring).
pub(super) async fn list_id_status(pool: &SqlitePool) -> anyhow::Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM tasks ORDER BY created_at DESC")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

/// Return only task IDs whose `status` matches any value in `statuses`.
///
/// Skips all heavy columns (rounds, error, etc.) — use this when only IDs
/// are needed (e.g. orphan-worktree cleanup at startup).
pub(super) async fn list_ids_by_status(
    pool: &SqlitePool,
    statuses: &[&str],
) -> anyhow::Result<Vec<String>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = status_placeholders(statuses.len());
    let sql = format!("SELECT id FROM tasks WHERE status IN ({placeholders})");
    let mut q = sqlx::query_as::<_, (String,)>(&sql);
    for status in statuses {
        q = q.bind(*status);
    }
    let rows = q.fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Return the status of a single task, fetching only the `status` column.
///
/// Much lighter than `get()` — avoids deserializing the `rounds` JSON.
/// Used by `check_awaiting_deps` to resolve dependency status with a single
/// DB round-trip instead of a full row fetch.
pub(super) async fn get_status_only(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(s,)| s))
}

/// Return `true` if a task row with the given ID exists in the database.
pub(super) async fn exists_by_id(pool: &SqlitePool, id: &str) -> anyhow::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Apply event-replayed state to a task row.
///
/// Called during startup, **before** `recover_in_progress()`, so that
/// event-sourced data takes precedence over checkpoint data.
///
/// - If `terminal_status` is `Some`, the task's status is overwritten
///   (only while the row is still in an interrupted state).
/// - If `pr_url` is `Some` and the row's `pr_url` is currently `NULL`,
///   the value is written back so `recover_in_progress()` can resume the task.
pub(super) async fn apply_replayed_state(
    pool: &SqlitePool,
    task_id: &str,
    pr_url: Option<&str>,
    terminal_status: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(status) = terminal_status {
        // Overwrite to terminal status; only touches tasks still in interrupted states
        // so we never downgrade a task that already reached Done/Failed in the DB.
        let sql = format!(
            "UPDATE tasks SET status = ?, pr_url = COALESCE(?, pr_url), \
             updated_at = datetime('now') \
             WHERE id = ? \
             AND status IN ({})",
            status_placeholders(TaskStatus::resumable_statuses().len())
        );
        let query = sqlx::query(&sql).bind(status).bind(pr_url).bind(task_id);
        let query = TaskStatus::resumable_statuses()
            .iter()
            .fold(query, |query, resumable| query.bind(*resumable));
        query.execute(pool).await?;
    } else if let Some(url) = pr_url {
        // Write pr_url back only when the DB row currently has no pr_url.
        sqlx::query(
            "UPDATE tasks SET pr_url = ? \
             WHERE id = ? AND pr_url IS NULL",
        )
        .bind(url)
        .bind(task_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Return the `pr_url` of the most recently completed Done task that has one, or `None`.
/// Orders by `updated_at DESC` because `updated_at` is written when the task transitions
/// to Done, which correctly reflects completion time rather than creation time.
pub(super) async fn latest_done_pr_url(pool: &SqlitePool) -> anyhow::Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT pr_url FROM tasks WHERE status = 'done' AND pr_url IS NOT NULL \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(pr_url,)| pr_url))
}

/// Return the `pr_url` of the most recent Done task for the given project root path.
pub(super) async fn latest_done_pr_url_by_project(
    pool: &SqlitePool,
    project: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT pr_url FROM tasks \
         WHERE status = 'done' AND pr_url IS NOT NULL AND project = ?1 \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(project)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(pr_url,)| pr_url))
}

/// Return the latest done PR URL for every project that has one, in a single query.
/// The map key is the project root path string; the value is the PR URL.
pub(super) async fn latest_done_pr_urls_all_projects(
    pool: &SqlitePool,
) -> anyhow::Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT project, pr_url FROM (\
           SELECT project, pr_url, \
                  ROW_NUMBER() OVER (PARTITION BY project ORDER BY updated_at DESC) AS rn \
           FROM tasks \
           WHERE status = 'done' AND pr_url IS NOT NULL AND project IS NOT NULL\
         ) WHERE rn = 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Count done/failed tasks globally and per project via SQL aggregation.
///
/// Returns `(global_done, global_failed, per_project_rows)` where each row is
/// `(project_key, done_count, failed_count)`. Tasks with no project are counted
/// in the global totals only. Uses the `idx_tasks_project_status_updated` index.
pub(super) async fn count_done_failed_by_project(
    pool: &SqlitePool,
) -> anyhow::Result<(u64, u64, Vec<(String, u64, u64)>)> {
    let outcome_statuses = [TaskStatus::Done.as_ref(), TaskStatus::Failed.as_ref()];
    let global_sql = format!(
        "SELECT COUNT(CASE WHEN status = 'done' THEN 1 END), \
                COUNT(CASE WHEN status = 'failed' THEN 1 END) \
         FROM tasks WHERE status IN ({})",
        status_placeholders(outcome_statuses.len())
    );
    let global: (i64, i64) = outcome_statuses
        .iter()
        .fold(sqlx::query_as(&global_sql), |query, status| {
            query.bind(*status)
        })
        .fetch_one(pool)
        .await?;

    let rows_sql = format!(
        "SELECT project, \
                COUNT(CASE WHEN status = 'done' THEN 1 END), \
                COUNT(CASE WHEN status = 'failed' THEN 1 END) \
         FROM tasks \
         WHERE status IN ({}) AND project IS NOT NULL \
         GROUP BY project",
        status_placeholders(outcome_statuses.len())
    );
    let rows: Vec<(String, i64, i64)> = outcome_statuses
        .iter()
        .fold(sqlx::query_as(&rows_sql), |query, status| {
            query.bind(*status)
        })
        .fetch_all(pool)
        .await?;

    let by_project = rows
        .into_iter()
        .map(|(p, d, f)| (p, d as u64, f as u64))
        .collect();
    Ok((global.0 as u64, global.1 as u64, by_project))
}

/// Return all tasks whose `parent_id` matches the given parent task ID.
pub(super) async fn list_children(
    pool: &SqlitePool,
    parent_id: &str,
) -> anyhow::Result<Vec<TaskState>> {
    let sql = format!(
        "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE parent_id = ? ORDER BY created_at DESC"
    );
    let rows = sqlx::query_as::<_, TaskRow>(&sql)
        .bind(parent_id)
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(TaskRow::try_into_task_state).collect()
}
