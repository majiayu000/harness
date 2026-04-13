use crate::http::parse_pr_num_from_url;
use crate::task_runner::TaskStatus;
use sqlx::sqlite::SqlitePool;

/// Persisted phase checkpoint for a task, used to resume after server restart.
///
/// A single row per task (upsert semantics). Each field is populated once the
/// corresponding phase completes; earlier fields are preserved on subsequent writes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskCheckpoint {
    pub task_id: String,
    /// Non-null once the Triage phase has completed.
    pub triage_output: Option<String>,
    /// Non-null once the Plan phase has completed.
    pub plan_output: Option<String>,
    /// Non-null once a PR has been created (most critical for duplicate-PR prevention).
    pub pr_url: Option<String>,
    /// Last completed phase: `triage_done` | `plan_done` | `pr_created`.
    pub last_phase: String,
    pub updated_at: String,
}

/// Result of [`TaskDb::recover_in_progress`].
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Tasks that were in interrupted states and are now `failed`.
    pub failed: u32,
    /// Tasks that were in interrupted states and have checkpoints — now `pending` for resume.
    pub resumed: u32,
    /// Tasks that were `pending` mid-transient-retry at crash time and are now `failed`.
    pub transient_failed: u32,
}

/// Row returned by the checkpoint JOIN query inside [`recover_in_progress`].
#[derive(sqlx::FromRow)]
pub(super) struct RecoveryRow {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) turn: i64,
    pub(super) task_pr_url: Option<String>,
    pub(super) triage_output: Option<String>,
    pub(super) plan_output: Option<String>,
    pub(super) ck_pr_url: Option<String>,
}

fn status_placeholders(len: usize) -> String {
    std::iter::repeat_n("?", len).collect::<Vec<_>>().join(", ")
}

/// Upsert a phase checkpoint for the given task.
///
/// Each call advances `last_phase` and updates `updated_at`. Previously saved fields
/// (`triage_output`, `plan_output`, `pr_url`) are preserved via `COALESCE` when the
/// incoming value is `NULL`, so callers only need to pass the newly available field.
pub(super) async fn write_checkpoint(
    pool: &SqlitePool,
    task_id: &str,
    triage_output: Option<&str>,
    plan_output: Option<&str>,
    pr_url: Option<&str>,
    last_phase: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO task_checkpoints \
             (task_id, triage_output, plan_output, pr_url, last_phase, updated_at) \
         VALUES (?, ?, ?, ?, ?, datetime('now')) \
         ON CONFLICT(task_id) DO UPDATE SET \
             triage_output = COALESCE(excluded.triage_output, task_checkpoints.triage_output), \
             plan_output   = COALESCE(excluded.plan_output,   task_checkpoints.plan_output), \
             pr_url        = COALESCE(excluded.pr_url,        task_checkpoints.pr_url), \
             last_phase    = excluded.last_phase, \
             updated_at    = excluded.updated_at",
    )
    .bind(task_id)
    .bind(triage_output)
    .bind(plan_output)
    .bind(pr_url)
    .bind(last_phase)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the checkpoint for `task_id`, or `None` if no checkpoint exists.
pub(super) async fn load_checkpoint(
    pool: &SqlitePool,
    task_id: &str,
) -> anyhow::Result<Option<TaskCheckpoint>> {
    let row = sqlx::query_as::<_, TaskCheckpoint>(
        "SELECT task_id, triage_output, plan_output, pr_url, last_phase, updated_at \
         FROM task_checkpoints WHERE task_id = ?",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Recovery on server restart.
///
/// For each interrupted task (`implementing`, `agent_review`, `reviewing`, `waiting`),
/// checks the `task_checkpoints` table and the existing `tasks.pr_url` column to
/// decide between resuming and failing:
///
/// - Has PR (tasks.pr_url or checkpoint.pr_url) → `pending`, `resumed += 1`
///   (executor will skip implement and jump to agent review)
/// - Has plan checkpoint → `pending`, `resumed += 1`
///   (executor will use saved plan, skip triage/plan pipeline)
/// - Has triage checkpoint only → `pending`, `resumed += 1`
///   (executor will re-run from plan phase)
/// - No checkpoint → `failed`, `failed += 1`
///   (existing fail-closed behavior, no safe resume point)
///
/// Tasks in `AwaitingDeps` are left unchanged — `check_awaiting_deps()` handles them.
/// `pending` tasks in transient-retry are failed (crashed during backoff, cannot re-dispatch).
///
/// Returns a [`RecoveryResult`] with counts for each outcome.
pub(super) async fn recover_in_progress(pool: &SqlitePool) -> anyhow::Result<RecoveryResult> {
    // Collect all interrupted tasks with their checkpoint data via LEFT JOIN.
    let rows = {
        let sql = format!(
            "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url,
                    c.triage_output, c.plan_output, c.pr_url AS ck_pr_url
             FROM tasks t
             LEFT JOIN task_checkpoints c ON t.id = c.task_id
             WHERE t.status IN ({})",
            status_placeholders(TaskStatus::resumable_statuses().len())
        );
        let query = TaskStatus::resumable_statuses()
            .iter()
            .fold(sqlx::query_as::<_, RecoveryRow>(&sql), |query, status| {
                query.bind(*status)
            });
        query.fetch_all(pool).await?
    };

    let mut result = RecoveryResult::default();

    for row in rows {
        // Fall back to ck_pr_url only when task_pr_url is absent *or* unparseable,
        // so a corrupted task_pr_url does not mask a valid checkpoint URL.
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
            // Resume: set back to pending with a diagnostic error field.
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

            // If task_pr_url was absent or unparseable but ck_pr_url is valid,
            // write the effective URL back to tasks.pr_url so that re-dispatch
            // (http.rs) and validate_recovered_tasks() (task_runner.rs) — which
            // both filter on pr_url.is_some() — can actually see the PR.
            let task_pr_url_valid = row
                .task_pr_url
                .as_deref()
                .map(|u| parse_pr_num_from_url(u).is_some())
                .unwrap_or(false);
            let needs_pr_url_writeback = !task_pr_url_valid && effective_pr_url.is_some();

            if needs_pr_url_writeback {
                sqlx::query(
                    "UPDATE tasks SET status = 'pending', pr_url = ?, error = ?, \
                     updated_at = datetime('now') WHERE id = ?",
                )
                .bind(effective_pr_url)
                .bind(&reason)
                .bind(&row.id)
                .execute(pool)
                .await?;
                tracing::info!(
                    task_id = %row.id,
                    was = %row.status,
                    pr_url = ?effective_pr_url,
                    "startup recovery: wrote back pr_url from checkpoint"
                );
            } else {
                sqlx::query(
                    "UPDATE tasks SET status = 'pending', error = ?, updated_at = datetime('now') \
                     WHERE id = ?",
                )
                .bind(&reason)
                .bind(&row.id)
                .execute(pool)
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
            // Fail: no checkpoint, no safe resume point.
            let err = format!(
                "recovered after restart (was: {}, round: {}, pr: {})",
                row.status,
                row.turn,
                row.task_pr_url.as_deref().unwrap_or("none")
            );
            sqlx::query(
                "UPDATE tasks SET status = 'failed', error = ?, updated_at = datetime('now') \
                 WHERE id = ?",
            )
            .bind(&err)
            .bind(&row.id)
            .execute(pool)
            .await?;
            result.failed += 1;
        }
    }

    // Tasks that were mid-transient-retry (status=pending, error starts with
    // "retrying after transient failure") crashed during the backoff window.
    // They have no PR yet and no persisted issue/prompt, so they cannot be
    // re-dispatched. Mark them failed so they don't silently stay pending forever.
    let transient_failed = sqlx::query(
        "UPDATE tasks \
         SET status = 'failed', \
             error = 'recovered after restart (was: pending in transient retry): ' \
                  || COALESCE(error, ''), \
             updated_at = datetime('now') \
         WHERE status = 'pending' \
           AND error LIKE 'retrying after transient failure%'",
    )
    .execute(pool)
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
