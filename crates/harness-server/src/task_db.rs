use crate::http::parse_pr_num_from_url;
use crate::task_runner::{TaskState, TaskStatus};
use harness_core::db::{open_pool, Migration, Migrator};
use harness_core::error::TaskDbDecodeError;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Maximum artifact content size in bytes before truncation.
const ARTIFACT_MAX_BYTES: usize = 65_536;

/// Column list for `query_as::<_, TaskRow>` — single source of truth.
///
/// Every SELECT that maps to `TaskRow` MUST use this constant.
/// When adding a field to `TaskRow`, add the column here once and all queries
/// pick it up automatically.  The `task_row_columns_match_struct` test below
/// will fail if this list drifts from the struct definition.
const TASK_ROW_COLUMNS: &str = "id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, description, phase";

/// Versioned migrations for the tasks table.
///
/// v1 – baseline schema (all columns including those added in later iterations)
/// v2/v3/v4 – additive ALTER TABLE for databases that predate v1 tracking;
///   duplicate-column errors are silently ignored by the Migrator.
/// v5 – add task_artifacts table for persisting agent output per task turn.
/// v10 – add composite index on (project, status, updated_at).
/// v11 – add task_checkpoints table for phase recovery.
/// v12 – add priority column for priority-based task scheduling.
/// v13 – add persisted description and authoritative phase columns.
static TASK_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create tasks table",
        sql: "CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            status      TEXT NOT NULL DEFAULT 'pending',
            turn        INTEGER NOT NULL DEFAULT 0,
            pr_url      TEXT,
            rounds      TEXT NOT NULL DEFAULT '[]',
            error       TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    },
    Migration {
        version: 2,
        description: "add source column",
        sql: "ALTER TABLE tasks ADD COLUMN source TEXT",
    },
    Migration {
        version: 3,
        description: "add external_id column",
        sql: "ALTER TABLE tasks ADD COLUMN external_id TEXT",
    },
    Migration {
        version: 4,
        description: "add parent_id column",
        sql: "ALTER TABLE tasks ADD COLUMN parent_id TEXT",
    },
    Migration {
        version: 5,
        description: "create task_artifacts table",
        sql: "CREATE TABLE IF NOT EXISTS task_artifacts (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id       TEXT NOT NULL,
            turn          INTEGER NOT NULL DEFAULT 0,
            artifact_type TEXT NOT NULL,
            content       TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    },
    Migration {
        version: 6,
        description: "add repo column",
        sql: "ALTER TABLE tasks ADD COLUMN repo TEXT",
    },
    Migration {
        version: 7,
        description: "add depends_on column",
        sql: "ALTER TABLE tasks ADD COLUMN depends_on TEXT NOT NULL DEFAULT '[]'",
    },
    Migration {
        version: 8,
        description: "add project column for task observability",
        sql: "ALTER TABLE tasks ADD COLUMN project TEXT",
    },
    Migration {
        version: 10,
        description: "add index on tasks(project, status, updated_at) for dashboard queries",
        sql: "CREATE INDEX IF NOT EXISTS idx_tasks_project_status_updated \
              ON tasks(project, status, updated_at DESC)",
    },
    Migration {
        version: 11,
        description: "create task_checkpoints table for phase recovery",
        sql: "CREATE TABLE IF NOT EXISTS task_checkpoints (
            task_id       TEXT PRIMARY KEY,
            triage_output TEXT,
            plan_output   TEXT,
            pr_url        TEXT,
            last_phase    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        )",
    },
    Migration {
        version: 12,
        description: "add priority column for task scheduling",
        sql: "ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
    },
    Migration {
        version: 13,
        description: "add description and phase columns to tasks",
        sql: "ALTER TABLE tasks ADD COLUMN description TEXT; \
              ALTER TABLE tasks ADD COLUMN phase TEXT NOT NULL DEFAULT 'implement'; \
              UPDATE tasks SET phase = 'terminal' WHERE status IN ('done', 'failed', 'cancelled'); \
              UPDATE tasks SET phase = 'review' WHERE status IN ('agent_review', 'reviewing', 'waiting')",
    },
];

/// A single persisted artifact captured from agent output during task execution.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskArtifact {
    pub task_id: String,
    pub turn: i64,
    pub artifact_type: String,
    pub content: String,
    pub created_at: String,
}

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

/// Row returned by the checkpoint JOIN query inside [`TaskDb::recover_in_progress`].
#[derive(sqlx::FromRow)]
struct RecoveryRow {
    id: String,
    status: String,
    turn: i64,
    task_pr_url: Option<String>,
    triage_output: Option<String>,
    plan_output: Option<String>,
    ck_pr_url: Option<String>,
}

pub struct TaskDb {
    pool: SqlitePool,
}

impl TaskDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(path).await?;
        let db = Self { pool };
        Migrator::new(&db.pool, TASK_MIGRATIONS).run().await?;
        Ok(db)
    }

    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let status = state.status.as_ref();
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, description, phase)
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
        .bind(&state.description)
        .bind(encode_phase(&state.phase))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let depends_on_json = serde_json::to_string(&state.depends_on)?;
        let status = state.status.as_ref();
        sqlx::query(
            "UPDATE tasks SET status = ?, turn = ?, pr_url = ?, rounds = ?, error = ?,
                    source = ?, external_id = ?, repo = ?, depends_on = ?, project = ?,
                    priority = ?, description = ?, phase = ?, updated_at = datetime('now')
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
        .bind(&state.description)
        .bind(encode_phase(&state.phase))
        .bind(&state.id.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        let sql = format!("SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE id = ?");
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
    /// Uses parameterized placeholders — safe for internal status string constants.
    /// Returns an empty `Vec` when `statuses` is empty.
    pub async fn list_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<TaskState>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
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

    /// Return all tasks as lightweight summaries, skipping the heavy `rounds` column.
    ///
    /// Used by the `/tasks` list endpoint to avoid deserializing large round histories
    /// when only summary fields are needed.
    pub async fn list_summaries(&self) -> anyhow::Result<Vec<crate::task_runner::TaskSummary>> {
        let sql = format!(
            "SELECT {} FROM tasks ORDER BY created_at DESC",
            task_summary_select_columns()
        );
        let rows = sqlx::query_as::<_, TaskSummaryRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            match row.try_into_summary() {
                Ok(summary) => summaries.push(summary),
                Err(error) => {
                    tracing::error!(
                        "task_db.list_summaries: skipping malformed summary row: {error}"
                    );
                }
            }
        }
        Ok(summaries)
    }

    /// Return `(id, status)` pairs for all tasks — skips all heavy columns.
    ///
    /// Used by hot-path callers that only need task status for aggregation
    /// (e.g. skill governance scoring).
    pub async fn list_id_status(&self) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM tasks ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// Return only task IDs whose `status` matches any value in `statuses`.
    ///
    /// Skips all heavy columns (rounds, error, etc.) — use this when only IDs
    /// are needed (e.g. orphan-worktree cleanup at startup).
    pub async fn list_ids_by_status(&self, statuses: &[&str]) -> anyhow::Result<Vec<String>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id FROM tasks WHERE status IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String,)>(&sql);
        for status in statuses {
            q = q.bind(*status);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Return the status of a single task, fetching only the `status` column.
    ///
    /// Much lighter than `get()` — avoids deserializing the `rounds` JSON.
    /// Used by `check_awaiting_deps` to resolve dependency status with a single
    /// DB round-trip instead of a full row fetch.
    pub async fn get_status_only(&self, id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT status FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(s,)| s))
    }

    /// Return `true` if a task row with the given ID exists in the database.
    pub async fn exists_by_id(&self, id: &str) -> anyhow::Result<bool> {
        let row: Option<(String,)> = sqlx::query_as("SELECT id FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
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
    pub async fn apply_replayed_state(
        &self,
        task_id: &str,
        pr_url: Option<&str>,
        terminal_status: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(status) = terminal_status {
            let phase = match status {
                "done" | "failed" | "cancelled" => "terminal",
                other => anyhow::bail!(
                    "apply_replayed_state only accepts terminal statuses, got `{other}`"
                ),
            };
            // Overwrite to terminal status; only touches tasks still in interrupted states
            // so we never downgrade a task that already reached Done/Failed in the DB.
            sqlx::query(
                "UPDATE tasks SET status = ?, phase = ?, pr_url = COALESCE(?, pr_url), \
                 updated_at = datetime('now') \
                 WHERE id = ? \
                 AND status IN ('implementing', 'agent_review', 'reviewing', 'waiting')",
            )
            .bind(status)
            .bind(phase)
            .bind(pr_url)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        } else if let Some(url) = pr_url {
            // Write pr_url back only when the DB row currently has no pr_url.
            sqlx::query(
                "UPDATE tasks SET pr_url = ? \
                 WHERE id = ? AND pr_url IS NULL",
            )
            .bind(url)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
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
    pub async fn recover_in_progress(&self) -> anyhow::Result<RecoveryResult> {
        // Collect all interrupted tasks with their checkpoint data via LEFT JOIN.
        let rows = sqlx::query_as::<_, RecoveryRow>(
            "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url,
                    c.triage_output, c.plan_output, c.pr_url AS ck_pr_url
             FROM tasks t
             LEFT JOIN task_checkpoints c ON t.id = c.task_id
             WHERE t.status IN ('implementing', 'agent_review', 'reviewing', 'waiting')",
        )
        .fetch_all(&self.pool)
        .await?;

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
                let resumed_phase = if has_plan {
                    "plan"
                } else if has_triage {
                    "triage"
                } else {
                    "implement"
                };
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
                        "UPDATE tasks SET status = 'pending', phase = ?, pr_url = ?, error = ?, \
                         updated_at = datetime('now') WHERE id = ?",
                    )
                    .bind(resumed_phase)
                    .bind(effective_pr_url)
                    .bind(&reason)
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
                        "UPDATE tasks SET status = 'pending', phase = ?, error = ?, updated_at = datetime('now') \
                         WHERE id = ?",
                    )
                    .bind(resumed_phase)
                    .bind(&reason)
                    .bind(&row.id)
                    .execute(&self.pool)
                    .await?;
                }
                result.resumed += 1;

                tracing::debug!(
                    task_id = %row.id,
                    resumed_phase,
                    has_pr,
                    has_plan,
                    has_triage,
                    "startup recovery: synced phase with resumed status"
                );
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
                    "UPDATE tasks SET status = 'failed', phase = 'terminal', error = ?, updated_at = datetime('now') \
                     WHERE id = ?",
                )
                .bind(&err)
                .bind(&row.id)
                .execute(&self.pool)
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
                 phase = 'terminal', \
                 error = 'recovered after restart (was: pending in transient retry): ' \
                      || COALESCE(error, ''), \
                 updated_at = datetime('now') \
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

    /// Upsert a phase checkpoint for the given task.
    ///
    /// Each call advances `last_phase` and updates `updated_at`. Previously saved fields
    /// (`triage_output`, `plan_output`, `pr_url`) are preserved via `COALESCE` when the
    /// incoming value is `NULL`, so callers only need to pass the newly available field.
    pub async fn write_checkpoint(
        &self,
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the checkpoint for `task_id`, or `None` if no checkpoint exists.
    pub async fn load_checkpoint(&self, task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        let row = sqlx::query_as::<_, TaskCheckpoint>(
            "SELECT task_id, triage_output, plan_output, pr_url, last_phase, updated_at \
             FROM task_checkpoints WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Return the `pr_url` of the most recently completed Done task that has one, or `None`.
    /// Orders by `updated_at DESC` because `updated_at` is written when the task transitions
    /// to Done, which correctly reflects completion time rather than creation time.
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
             WHERE status = 'done' AND pr_url IS NOT NULL AND project = ?1 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(project)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(pr_url,)| pr_url))
    }

    /// Return the latest done PR URL for every project that has one, in a single query.
    /// The map key is the project root path string; the value is the PR URL.
    pub async fn latest_done_pr_urls_all_projects(
        &self,
    ) -> anyhow::Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT project, pr_url FROM (\
               SELECT project, pr_url, \
                      ROW_NUMBER() OVER (PARTITION BY project ORDER BY updated_at DESC) AS rn \
               FROM tasks \
               WHERE status = 'done' AND pr_url IS NOT NULL AND project IS NOT NULL\
             ) WHERE rn = 1",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Count done/failed tasks globally and per project via SQL aggregation.
    ///
    /// Returns `(global_done, global_failed, per_project_rows)` where each row is
    /// `(project_key, done_count, failed_count)`. Tasks with no project are counted
    /// in the global totals only. Uses the `idx_tasks_project_status_updated` index.
    pub async fn count_done_failed_by_project(
        &self,
    ) -> anyhow::Result<(u64, u64, Vec<(String, u64, u64)>)> {
        let global: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks WHERE status IN ('done', 'failed')",
        )
        .fetch_one(&self.pool)
        .await?;

        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT project, \
                    COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks \
             WHERE status IN ('done', 'failed') AND project IS NOT NULL \
             GROUP BY project",
        )
        .fetch_all(&self.pool)
        .await?;

        let by_project = rows
            .into_iter()
            .map(|(p, d, f)| (p, d as u64, f as u64))
            .collect();
        Ok((global.0 as u64, global.1 as u64, by_project))
    }

    /// Return all tasks whose `parent_id` matches the given parent task ID.
    pub async fn list_children(&self, parent_id: &str) -> anyhow::Result<Vec<TaskState>> {
        let sql = format!(
            "SELECT {TASK_ROW_COLUMNS} FROM tasks WHERE parent_id = ? ORDER BY created_at DESC"
        );
        let rows = sqlx::query_as::<_, TaskRow>(&sql)
            .bind(parent_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Persist a single artifact captured from agent output.
    ///
    /// Content larger than [`ARTIFACT_MAX_BYTES`] is truncated to avoid
    /// unbounded database growth without requiring an external compression
    /// dependency.
    pub async fn insert_artifact(
        &self,
        task_id: &str,
        turn: u32,
        artifact_type: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let stored = if content.len() > ARTIFACT_MAX_BYTES {
            let mut boundary = ARTIFACT_MAX_BYTES;
            while boundary > 0 && !content.is_char_boundary(boundary) {
                boundary -= 1;
            }
            format!(
                "{}\n[truncated: {} bytes total]",
                &content[..boundary],
                content.len()
            )
        } else {
            content.to_string()
        };

        sqlx::query(
            "INSERT INTO task_artifacts (task_id, turn, artifact_type, content)
             VALUES (?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(turn as i64)
        .bind(artifact_type)
        .bind(&stored)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return all artifacts for a task ordered by insertion time.
    pub async fn list_artifacts(&self, task_id: &str) -> anyhow::Result<Vec<TaskArtifact>> {
        let rows = sqlx::query_as::<_, TaskArtifact>(
            "SELECT task_id, turn, artifact_type, content, created_at
             FROM task_artifacts WHERE task_id = ? ORDER BY id ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: String,
    status: String,
    turn: i64,
    pr_url: Option<String>,
    rounds: String,
    error: Option<String>,
    source: Option<String>,
    external_id: Option<String>,
    parent_id: Option<String>,
    created_at: Option<String>,
    repo: Option<String>,
    depends_on: String,
    project: Option<String>,
    priority: i64,
    description: Option<String>,
    phase: String,
}

fn decode_phase(task_id: &str, phase: &str) -> anyhow::Result<crate::task_runner::TaskPhase> {
    match phase {
        "triage" => Ok(crate::task_runner::TaskPhase::Triage),
        "plan" => Ok(crate::task_runner::TaskPhase::Plan),
        "implement" => Ok(crate::task_runner::TaskPhase::Implement),
        "review" => Ok(crate::task_runner::TaskPhase::Review),
        "terminal" => Ok(crate::task_runner::TaskPhase::Terminal),
        _ => Err(TaskDbDecodeError::InvalidPhase {
            task_id: task_id.to_string(),
            phase: phase.to_string(),
        }
        .into()),
    }
}

fn encode_phase(phase: &crate::task_runner::TaskPhase) -> &'static str {
    match phase {
        crate::task_runner::TaskPhase::Triage => "triage",
        crate::task_runner::TaskPhase::Plan => "plan",
        crate::task_runner::TaskPhase::Implement => "implement",
        crate::task_runner::TaskPhase::Review => "review",
        crate::task_runner::TaskPhase::Terminal => "terminal",
    }
}

fn task_summary_select_columns() -> &'static str {
    "id, status, turn, pr_url, error, source, external_id, parent_id, created_at, repo, depends_on, project, description, phase"
}

#[cfg(test)]
fn task_summary_row_from_state(
    state: &crate::task_runner::TaskState,
) -> anyhow::Result<TaskSummaryRow> {
    Ok(TaskSummaryRow {
        id: state.id.0.clone(),
        status: state.status.as_ref().to_string(),
        turn: state.turn as i64,
        pr_url: state.pr_url.clone(),
        error: state.error.clone(),
        source: state.source.clone(),
        external_id: state.external_id.clone(),
        parent_id: state.parent_id.as_ref().map(|id| id.0.clone()),
        created_at: state.created_at.clone(),
        repo: state.repo.clone(),
        depends_on: serde_json::to_string(&state.depends_on)?,
        project: state
            .project_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        description: state.description.clone(),
        phase: encode_phase(&state.phase).to_string(),
    })
}

#[cfg(test)]
fn task_row_from_state(state: &crate::task_runner::TaskState) -> anyhow::Result<TaskRow> {
    Ok(TaskRow {
        id: state.id.0.clone(),
        status: state.status.as_ref().to_string(),
        turn: state.turn as i64,
        pr_url: state.pr_url.clone(),
        rounds: serde_json::to_string(&state.rounds)?,
        error: state.error.clone(),
        source: state.source.clone(),
        external_id: state.external_id.clone(),
        parent_id: state.parent_id.as_ref().map(|id| id.0.clone()),
        created_at: state.created_at.clone(),
        repo: state.repo.clone(),
        depends_on: serde_json::to_string(&state.depends_on)?,
        project: state
            .project_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        priority: state.priority as i64,
        description: state.description.clone(),
        phase: encode_phase(&state.phase).to_string(),
    })
}

impl TaskRow {
    fn try_into_task_state(self) -> anyhow::Result<TaskState> {
        let Self {
            id,
            status,
            turn,
            pr_url,
            rounds,
            error,
            source,
            external_id,
            parent_id,
            created_at,
            repo,
            depends_on,
            project,
            priority,
            description,
            phase,
        } = self;

        let decoded_rounds = serde_json::from_str(&rounds).map_err(|source| {
            TaskDbDecodeError::RoundsDeserialize {
                task_id: id.clone(),
                source,
            }
        })?;
        let decoded_depends_on = serde_json::from_str(&depends_on).map_err(|source| {
            TaskDbDecodeError::DependsOnDeserialize {
                task_id: id.clone(),
                source,
            }
        })?;

        let decoded_phase = decode_phase(&id, &phase)?;

        Ok(TaskState {
            id: harness_core::types::TaskId(id),
            status: status.parse::<TaskStatus>()?,
            turn: turn as u32,
            pr_url,
            rounds: decoded_rounds,
            error,
            source,
            external_id,
            parent_id: parent_id.map(harness_core::types::TaskId),
            depends_on: decoded_depends_on,
            subtask_ids: Vec::new(),
            project_root: project.map(PathBuf::from),
            issue: None,
            description,
            created_at,
            priority: priority.clamp(0, 255) as u8,
            phase: decoded_phase,
            triage_output: None,
            plan_output: None,
            repo,
        })
    }
}

/// Lightweight row for summary queries — omits the heavy `rounds` column.
#[derive(sqlx::FromRow)]
struct TaskSummaryRow {
    id: String,
    status: String,
    turn: i64,
    pr_url: Option<String>,
    error: Option<String>,
    source: Option<String>,
    external_id: Option<String>,
    parent_id: Option<String>,
    created_at: Option<String>,
    repo: Option<String>,
    depends_on: String,
    project: Option<String>,
    description: Option<String>,
    phase: String,
}

impl TaskSummaryRow {
    fn try_into_summary(self) -> anyhow::Result<crate::task_runner::TaskSummary> {
        use harness_core::types::TaskId;
        let depends_on: Vec<TaskId> = serde_json::from_str(&self.depends_on).map_err(|source| {
            harness_core::error::TaskDbDecodeError::DependsOnDeserialize {
                task_id: self.id.clone(),
                source,
            }
        })?;
        let phase = decode_phase(&self.id, &self.phase)?;
        Ok(crate::task_runner::TaskSummary {
            id: TaskId(self.id),
            status: self.status.parse::<crate::task_runner::TaskStatus>()?,
            turn: self.turn as u32,
            pr_url: self.pr_url,
            error: self.error,
            source: self.source,
            parent_id: self.parent_id.map(TaskId),
            external_id: self.external_id,
            repo: self.repo,
            description: self.description,
            created_at: self.created_at,
            phase,
            depends_on,
            subtask_ids: Vec::new(),
            project: self.project,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        task_row_from_state, task_summary_row_from_state, TaskDb, TaskRow, TaskSummaryRow,
    };
    use crate::task_runner::{RoundResult, TaskPhase, TaskState, TaskStatus, TaskStore};
    use harness_core::error::TaskDbDecodeError;

    fn build_task_row(rounds: &str, depends_on: &str) -> TaskRow {
        TaskRow {
            id: "task-1".to_string(),
            status: "pending".to_string(),
            turn: 1,
            pr_url: None,
            rounds: rounds.to_string(),
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            created_at: None,
            repo: None,
            depends_on: depends_on.to_string(),
            project: None,
            priority: 0,
            description: Some("persisted description".to_string()),
            phase: "review".to_string(),
        }
    }

    fn build_task_summary_row(depends_on: &str) -> TaskSummaryRow {
        TaskSummaryRow {
            id: "task-summary-1".to_string(),
            status: "pending".to_string(),
            turn: 2,
            pr_url: None,
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            created_at: None,
            repo: Some("acme/harness".to_string()),
            depends_on: depends_on.to_string(),
            project: Some("/repo/project".to_string()),
            description: Some("summary description".to_string()),
            phase: "plan".to_string(),
        }
    }

    fn make_task(id: &str, status: TaskStatus) -> TaskState {
        TaskState {
            id: harness_core::types::TaskId(id.to_string()),
            status,
            turn: 0,
            pr_url: None,
            rounds: vec![],
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            depends_on: vec![],
            subtask_ids: vec![],
            project_root: None,
            issue: None,
            description: None,
            created_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            repo: None,
        }
    }

    fn make_task_with_metadata(id: &str) -> TaskState {
        let mut task = make_task(id, TaskStatus::Pending);
        task.description = Some("persisted task description".to_string());
        task.phase = TaskPhase::Review;
        task
    }

    #[test]
    fn task_row_decode_preserves_description_and_phase() -> anyhow::Result<()> {
        let state = build_task_row("[]", "[]").try_into_task_state()?;
        assert_eq!(state.description.as_deref(), Some("persisted description"));
        assert_eq!(state.phase, TaskPhase::Review);
        Ok(())
    }

    #[test]
    fn task_summary_row_decode_preserves_description_and_phase() -> anyhow::Result<()> {
        let summary = build_task_summary_row("[]").try_into_summary()?;
        assert_eq!(summary.description.as_deref(), Some("summary description"));
        assert_eq!(summary.phase, TaskPhase::Plan);
        Ok(())
    }

    #[test]
    fn invalid_phase_in_task_row_returns_distinguishable_error() {
        let mut row = build_task_row("[]", "[]");
        row.phase = "bogus".to_string();

        let err = row
            .try_into_task_state()
            .expect_err("invalid phase should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::InvalidPhase { task_id, phase }
                if task_id == "task-1" && phase == "bogus"
        ));
    }

    #[test]
    fn invalid_phase_in_task_summary_row_returns_distinguishable_error() {
        let mut row = build_task_summary_row("[]");
        row.phase = "bogus".to_string();

        let err = row
            .try_into_summary()
            .expect_err("invalid phase should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::InvalidPhase { task_id, phase }
                if task_id == "task-summary-1" && phase == "bogus"
        ));
    }

    #[test]
    fn row_builders_encode_persisted_description_and_phase() -> anyhow::Result<()> {
        let task = make_task_with_metadata("task-builder");
        let row = task_row_from_state(&task)?;
        let summary_row = task_summary_row_from_state(&task)?;

        assert_eq!(
            row.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(row.phase, "review");
        assert_eq!(
            summary_row.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(summary_row.phase, "review");
        Ok(())
    }

    #[tokio::test]
    async fn migration_adds_description_and_phase_defaults() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("tasks.db");
        let pool = harness_core::db::open_pool(&db_path).await?;

        sqlx::query(
            "CREATE TABLE tasks (
                id          TEXT PRIMARY KEY,
                status      TEXT NOT NULL DEFAULT 'pending',
                turn        INTEGER NOT NULL DEFAULT 0,
                pr_url      TEXT,
                rounds      TEXT NOT NULL DEFAULT '[]',
                error       TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
                source      TEXT,
                external_id TEXT,
                parent_id   TEXT,
                repo        TEXT,
                depends_on  TEXT NOT NULL DEFAULT '[]',
                project     TEXT,
                priority    INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO schema_migrations (version, description) VALUES (?, ?)")
            .bind(12_i64)
            .bind("seed v12")
            .execute(&pool)
            .await
            .ok();
        drop(pool);

        let db = TaskDb::open(&db_path).await?;
        sqlx::query("INSERT INTO tasks (id, status, turn, rounds) VALUES (?, ?, ?, ?)")
            .bind("task-default-phase")
            .bind("pending")
            .bind(0_i64)
            .bind("[]")
            .execute(&db.pool)
            .await?;

        let loaded = db
            .get("task-default-phase")
            .await?
            .expect("migrated task should load");
        assert!(loaded.description.is_none());
        assert_eq!(loaded.phase, TaskPhase::Implement);
        Ok(())
    }

    #[tokio::test]
    async fn list_summaries_skips_invalid_phase_rows() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let good = make_task_with_metadata("task-good-summary");
        db.insert(&good).await?;

        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on, description, phase) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("task-bad-summary")
        .bind("pending")
        .bind(0_i64)
        .bind("[]")
        .bind("[]")
        .bind("bad summary")
        .bind("bogus")
        .execute(&db.pool)
        .await?;

        let summaries = db.list_summaries().await?;
        let good_summary = summaries
            .iter()
            .find(|summary| summary.id.0 == "task-good-summary")
            .expect("valid summary should still be returned");
        assert_eq!(
            good_summary.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(good_summary.phase, TaskPhase::Review);
        assert!(
            summaries
                .iter()
                .all(|summary| summary.id.0 != "task-bad-summary"),
            "invalid summary row should be skipped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_summaries_invalid_phase_error_is_distinguishable() -> anyhow::Result<()> {
        let row = build_task_summary_row("[]");
        let mut bad_row = row;
        bad_row.id = "task-bad-summary".to_string();
        bad_row.phase = "bogus".to_string();

        let err = bad_row
            .try_into_summary()
            .expect_err("invalid phase must still decode as a distinguishable row error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");
        assert!(matches!(
            decode_error,
            TaskDbDecodeError::InvalidPhase { task_id, phase }
                if task_id == "task-bad-summary" && phase == "bogus"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip_preserves_description_and_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let task = make_task_with_metadata("task-rt-meta");
        db.insert(&task).await?;

        let loaded = db
            .get("task-rt-meta")
            .await?
            .expect("inserted task should exist");
        assert_eq!(
            loaded.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(loaded.phase, TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn update_persists_description_and_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-upd-meta", TaskStatus::Pending);
        db.insert(&task).await?;

        task.description = Some("updated description".to_string());
        task.phase = TaskPhase::Terminal;
        db.update(&task).await?;

        let loaded = db
            .get("task-upd-meta")
            .await?
            .expect("updated task should exist");
        assert_eq!(loaded.description.as_deref(), Some("updated description"));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn list_summaries_preserves_description_and_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let task = make_task_with_metadata("task-summary-meta");
        db.insert(&task).await?;

        let summaries = db.list_summaries().await?;
        let summary = summaries
            .into_iter()
            .find(|summary| summary.id.0 == "task-summary-meta")
            .expect("summary should exist");
        assert_eq!(
            summary.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(summary.phase, TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_error_when_phase_is_corrupted() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on, phase) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("task-corrupted-phase")
        .bind("pending")
        .bind(1_i64)
        .bind("[]")
        .bind("[]")
        .bind("bogus")
        .execute(&db.pool)
        .await?;

        let err = db
            .get("task-corrupted-phase")
            .await
            .expect_err("corrupted phase should return an error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");
        assert!(matches!(
            decode_error,
            TaskDbDecodeError::InvalidPhase { task_id, phase }
                if task_id == "task-corrupted-phase" && phase == "bogus"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn get_with_db_fallback_prefers_cache_metadata_over_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task_with_metadata("task-cache-precedence");
        task.status = TaskStatus::Done;
        store.insert(&task).await;

        sqlx::query("UPDATE tasks SET description = ?, phase = ? WHERE id = ?")
            .bind("db description")
            .bind("plan")
            .bind("task-cache-precedence")
            .execute(&store.db.pool)
            .await?;

        let loaded = store
            .get_with_db_fallback(&harness_core::types::TaskId(
                "task-cache-precedence".to_string(),
            ))
            .await?
            .expect("task should exist");
        assert_eq!(
            loaded.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(loaded.phase, TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn get_with_db_fallback_uses_db_metadata_when_cache_missing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task_with_metadata("task-db-fallback");
        task.status = TaskStatus::Done;
        store.insert(&task).await;
        store
            .cache
            .remove(&harness_core::types::TaskId("task-db-fallback".to_string()));

        let loaded = store
            .get_with_db_fallback(&harness_core::types::TaskId("task-db-fallback".to_string()))
            .await?
            .expect("task should exist");
        assert_eq!(
            loaded.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(loaded.phase, TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn list_all_summaries_with_terminal_preserves_metadata_and_cache_precedence(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::open(&tmp.path().join("tasks.db")).await?;

        let mut terminal = make_task_with_metadata("task-terminal-summary");
        terminal.status = TaskStatus::Done;
        store.insert(&terminal).await;
        store.cache.remove(&terminal.id);

        let mut active = make_task_with_metadata("task-active-summary");
        active.status = TaskStatus::Pending;
        active.description = Some("cache description".to_string());
        active.phase = TaskPhase::Review;
        store.insert(&active).await;

        sqlx::query("UPDATE tasks SET description = ?, phase = ? WHERE id = ?")
            .bind("db stale description")
            .bind("plan")
            .bind("task-active-summary")
            .execute(&store.db.pool)
            .await?;

        let summaries = store.list_all_summaries_with_terminal().await?;
        let terminal_summary = summaries
            .iter()
            .find(|summary| summary.id.0 == "task-terminal-summary")
            .expect("terminal summary should exist");
        assert_eq!(
            terminal_summary.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(terminal_summary.phase, TaskPhase::Review);

        let active_summary = summaries
            .iter()
            .find(|summary| summary.id.0 == "task-active-summary")
            .expect("active summary should exist");
        assert_eq!(
            active_summary.description.as_deref(),
            Some("cache description")
        );
        assert_eq!(active_summary.phase, TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn list_all_with_terminal_preserves_metadata_and_cache_precedence() -> anyhow::Result<()>
    {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::open(&tmp.path().join("tasks.db")).await?;

        let mut terminal = make_task_with_metadata("task-terminal-full");
        terminal.status = TaskStatus::Done;
        store.insert(&terminal).await;
        store.cache.remove(&terminal.id);

        let mut active = make_task_with_metadata("task-active-full");
        active.status = TaskStatus::Pending;
        active.description = Some("live cache description".to_string());
        active.phase = TaskPhase::Terminal;
        store.insert(&active).await;

        sqlx::query("UPDATE tasks SET description = ?, phase = ? WHERE id = ?")
            .bind("db stale description")
            .bind("plan")
            .bind("task-active-full")
            .execute(&store.db.pool)
            .await?;

        let tasks = store.list_all_with_terminal().await?;
        let terminal_task = tasks
            .iter()
            .find(|task| task.id.0 == "task-terminal-full")
            .expect("terminal task should exist");
        assert_eq!(
            terminal_task.description.as_deref(),
            Some("persisted task description")
        );
        assert_eq!(terminal_task.phase, TaskPhase::Review);

        let active_task = tasks
            .iter()
            .find(|task| task.id.0 == "task-active-full")
            .expect("active task should exist");
        assert_eq!(
            active_task.description.as_deref(),
            Some("live cache description")
        );
        assert_eq!(active_task.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[test]
    fn invalid_rounds_json_returns_distinguishable_error() {
        let err = build_task_row("{not-json", "[]")
            .try_into_task_state()
            .expect_err("invalid rounds JSON should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::RoundsDeserialize { task_id, .. } if task_id == "task-1"
        ));
    }

    #[test]
    fn invalid_depends_on_json_returns_distinguishable_error() {
        let err = build_task_row("[]", "{not-json")
            .try_into_task_state()
            .expect_err("invalid depends_on JSON should return error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");

        assert!(matches!(
            decode_error,
            TaskDbDecodeError::DependsOnDeserialize { task_id, .. } if task_id == "task-1"
        ));
    }

    #[tokio::test]
    async fn get_distinguishes_missing_task_from_corrupted_rounds() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        assert!(db.get("missing-task").await?.is_none());

        sqlx::query("INSERT INTO tasks (id, status, turn, rounds) VALUES (?, ?, ?, ?)")
            .bind("task-corrupted")
            .bind("pending")
            .bind(1_i64)
            .bind("{not-json")
            .execute(&db.pool)
            .await?;

        let err = db
            .get("task-corrupted")
            .await
            .expect_err("corrupted rounds should return an error");
        assert!(err.downcast_ref::<TaskDbDecodeError>().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_error_when_depends_on_json_is_corrupted() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("task-corrupted-deps")
        .bind("pending")
        .bind(1_i64)
        .bind("[]")
        .bind("{not-json")
        .execute(&db.pool)
        .await?;

        let err = db
            .get("task-corrupted-deps")
            .await
            .expect_err("corrupted depends_on should return an error");
        let decode_error = err
            .downcast_ref::<TaskDbDecodeError>()
            .expect("error should expose task-db decode type");
        assert!(matches!(
            decode_error,
            TaskDbDecodeError::DependsOnDeserialize { task_id, .. } if task_id == "task-corrupted-deps"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let task = make_task("task-rt", TaskStatus::Pending);
        db.insert(&task).await?;

        let loaded = db
            .get("task-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(loaded.id.0, "task-rt");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.turn, 0);
        assert!(loaded.pr_url.is_none());
        assert!(loaded.rounds.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn update_persists_status_and_pr_url() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-upd", TaskStatus::Pending);
        db.insert(&task).await?;

        task.status = TaskStatus::Implementing;
        task.turn = 1;
        task.pr_url = Some("https://github.com/org/repo/pull/42".to_string());
        task.rounds.push(RoundResult {
            turn: 1,
            action: "implement".to_string(),
            result: "created PR".to_string(),
            detail: None,
        });
        db.update(&task).await?;

        let loaded = db
            .get("task-upd")
            .await?
            .expect("updated task should exist");
        assert!(matches!(loaded.status, TaskStatus::Implementing));
        assert_eq!(loaded.turn, 1);
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/42")
        );
        assert_eq!(loaded.rounds.len(), 1);
        assert_eq!(loaded.rounds[0].action, "implement");
        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_sets_terminal_phase_for_failed_rows() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("task-replayed-failed", TaskStatus::Reviewing))
            .await?;

        db.apply_replayed_state("task-replayed-failed", None, Some("failed"))
            .await?;

        let loaded = db
            .get("task-replayed-failed")
            .await?
            .expect("task should exist after replay update");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_updates_phase_with_status_rewrites() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut resumable = make_task("task-recover-phase-pending", TaskStatus::Implementing);
        resumable.phase = TaskPhase::Review;
        resumable.pr_url = Some("https://github.com/owner/repo/pull/77".to_string());
        db.insert(&resumable).await?;

        let mut failed = make_task("task-recover-phase-failed", TaskStatus::Reviewing);
        failed.phase = TaskPhase::Review;
        db.insert(&failed).await?;

        db.recover_in_progress().await?;

        let resumed = db
            .get("task-recover-phase-pending")
            .await?
            .expect("resumed task should exist");
        assert!(matches!(resumed.status, TaskStatus::Pending));
        assert_eq!(resumed.phase, TaskPhase::Implement);

        let failed = db
            .get("task-recover-phase-failed")
            .await?
            .expect("failed task should exist");
        assert!(matches!(failed.status, TaskStatus::Failed));
        assert_eq!(failed.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_marks_transient_retry_rows_terminal() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-transient-terminal", TaskStatus::Pending);
        task.phase = TaskPhase::Review;
        task.error = Some("retrying after transient failure: boom".to_string());
        db.insert(&task).await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.transient_failed, 1);

        let loaded = db
            .get("task-transient-terminal")
            .await?
            .expect("transient retry task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_plan_phase_for_resumed_plan_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-recover-plan-phase", TaskStatus::Implementing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-recover-plan-phase",
            None,
            Some("## Plan\nStep 1"),
            None,
            "plan_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-recover-plan-phase")
            .await?
            .expect("plan checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_triage_phase_for_resumed_triage_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-recover-triage-phase", TaskStatus::Implementing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-recover-triage-phase",
            Some("triage output"),
            None,
            None,
            "triage_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-recover-triage-phase")
            .await?
            .expect("triage checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Triage);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_keeps_replayed_terminal_status_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-replayed-done", TaskStatus::Reviewing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.apply_replayed_state(
            "task-replayed-done",
            Some("https://github.com/owner/repo/pull/88"),
            Some("done"),
        )
        .await?;
        db.recover_in_progress().await?;

        let loaded = db
            .get("task-replayed-done")
            .await?
            .expect("replayed terminal task should exist");
        assert!(matches!(loaded.status, TaskStatus::Done));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_plan_phase_on_pr_writeback() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-pr-writeback-phase", TaskStatus::Implementing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-pr-writeback-phase",
            None,
            Some("## Plan\nStep 1"),
            Some("https://github.com/owner/repo/pull/99"),
            "pr_created",
        )
        .await?;

        sqlx::query("UPDATE tasks SET pr_url = ? WHERE id = ?")
            .bind("not-a-pr-url")
            .bind("task-pr-writeback-phase")
            .execute(&db.pool)
            .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-pr-writeback-phase")
            .await?
            .expect("writeback task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/99")
        );
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_implement_phase_for_pr_resume_without_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-pr-resume-implement-phase", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        task.pr_url = Some("https://github.com/owner/repo/pull/123".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-pr-resume-implement-phase")
            .await?
            .expect("resumed PR task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Implement);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_no_checkpoint_failures(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-recover-failed-terminal", TaskStatus::Implementing);
        task.phase = TaskPhase::Plan;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-recover-failed-terminal")
            .await?
            .expect("failed recovery task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_implement_phase_for_planless_resume() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-recover-implement-phase", TaskStatus::Waiting);
        task.phase = TaskPhase::Review;
        task.pr_url = Some("https://github.com/owner/repo/pull/555".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-recover-implement-phase")
            .await?
            .expect("resumed task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Implement);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_phase_after_terminal_replay_then_pending_resume(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-terminal-replay-phase", TaskStatus::Implementing);
        task.phase = TaskPhase::Plan;
        db.insert(&task).await?;

        db.apply_replayed_state("task-terminal-replay-phase", None, Some("failed"))
            .await?;
        db.recover_in_progress().await?;

        let loaded = db
            .get("task-terminal-replay-phase")
            .await?
            .expect("terminal replay task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_plan_phase_when_checkpoint_and_pr_both_exist(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-plan-pr-phase", TaskStatus::Reviewing);
        task.phase = TaskPhase::Review;
        task.pr_url = Some("https://github.com/owner/repo/pull/321".to_string());
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-plan-pr-phase",
            None,
            Some("## Plan\nStep 1"),
            Some("https://github.com/owner/repo/pull/321"),
            "pr_created",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-plan-pr-phase")
            .await?
            .expect("plan+pr task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_triage_phase_when_no_plan_but_checkpoint_exists(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-triage-only-phase", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-triage-only-phase",
            Some("triage output"),
            None,
            None,
            "triage_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-triage-only-phase")
            .await?
            .expect("triage-only task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Triage);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_transient_failure_rows(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-transient-failed-terminal", TaskStatus::Pending);
        task.phase = TaskPhase::Plan;
        task.error = Some("retrying after transient failure: network".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-transient-failed-terminal")
            .await?
            .expect("transient failure task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_leaves_terminal_rows_untouched() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-terminal-untouched", TaskStatus::Done);
        task.phase = TaskPhase::Terminal;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-terminal-untouched")
            .await?
            .expect("terminal task should exist");
        assert!(matches!(loaded.status, TaskStatus::Done));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_plan_phase_when_last_phase_is_pr_created(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-pr-created-phase", TaskStatus::Waiting);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-pr-created-phase",
            None,
            Some("## Plan\nStep 1"),
            Some("https://github.com/owner/repo/pull/404"),
            "pr_created",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-pr-created-phase")
            .await?
            .expect("pr_created task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_replayed_failed_without_resume(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-replayed-failed-terminal", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.apply_replayed_state("task-replayed-failed-terminal", None, Some("failed"))
            .await?;

        let loaded = db
            .get("task-replayed-failed-terminal")
            .await?
            .expect("replayed failed task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_replayed_done_without_resume(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-replayed-done-terminal", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.apply_replayed_state("task-replayed-done-terminal", None, Some("done"))
            .await?;

        let loaded = db
            .get("task-replayed-done-terminal")
            .await?
            .expect("replayed done task should exist");
        assert!(matches!(loaded.status, TaskStatus::Done));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_implement_phase_for_resumed_task_without_checkpoint_even_if_stale_review(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-stale-review-phase", TaskStatus::Reviewing);
        task.phase = TaskPhase::Review;
        task.pr_url = Some("https://github.com/owner/repo/pull/808".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-stale-review-phase")
            .await?
            .expect("stale review task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Implement);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_stale_plan_failure() -> anyhow::Result<()>
    {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-stale-plan-failure", TaskStatus::Waiting);
        task.phase = TaskPhase::Plan;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-stale-plan-failure")
            .await?
            .expect("stale plan failure task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_triage_phase_from_checkpoint_even_with_stale_review(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-stale-review-triage", TaskStatus::Waiting);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-stale-review-triage",
            Some("triage output"),
            None,
            None,
            "triage_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-stale-review-triage")
            .await?
            .expect("stale review triage task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Triage);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_plan_phase_from_checkpoint_even_with_stale_review(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-stale-review-plan", TaskStatus::Waiting);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-stale-review-plan",
            None,
            Some("## Plan\nStep 1"),
            None,
            "plan_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-stale-review-plan")
            .await?
            .expect("stale review plan task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_corrupted_pr_failures(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-corrupted-pr-terminal", TaskStatus::Implementing);
        task.phase = TaskPhase::Plan;
        task.pr_url = Some("not-a-pr-url".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-corrupted-pr-terminal")
            .await?
            .expect("corrupted PR task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_plan_phase_for_checkpoint_pr_writeback_with_stale_review(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-checkpoint-writeback-stale", TaskStatus::Reviewing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-checkpoint-writeback-stale",
            None,
            Some("## Plan\nStep 1"),
            Some("https://github.com/owner/repo/pull/900"),
            "pr_created",
        )
        .await?;
        sqlx::query("UPDATE tasks SET pr_url = ? WHERE id = ?")
            .bind("bad-pr-url")
            .bind("task-checkpoint-writeback-stale")
            .execute(&db.pool)
            .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-checkpoint-writeback-stale")
            .await?
            .expect("checkpoint writeback stale task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_transient_retry_with_stale_plan(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-transient-stale-plan", TaskStatus::Pending);
        task.phase = TaskPhase::Plan;
        task.error = Some("retrying after transient failure: timeout".to_string());
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-transient-stale-plan")
            .await?
            .expect("transient stale plan task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_sets_terminal_phase_for_done_rows() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task(
            "task-replayed-done-phase",
            TaskStatus::Reviewing,
        ))
        .await?;

        db.apply_replayed_state("task-replayed-done-phase", None, Some("done"))
            .await?;

        let loaded = db
            .get("task-replayed-done-phase")
            .await?
            .expect("task should exist after replay done update");
        assert!(matches!(loaded.status, TaskStatus::Done));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_preserves_pr_url_when_marking_terminal() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-replayed-pr-preserve", TaskStatus::Reviewing);
        task.pr_url = Some("https://github.com/owner/repo/pull/707".to_string());
        db.insert(&task).await?;

        db.apply_replayed_state(
            "task-replayed-pr-preserve",
            Some("https://github.com/owner/repo/pull/707"),
            Some("failed"),
        )
        .await?;

        let loaded = db
            .get("task-replayed-pr-preserve")
            .await?
            .expect("task should exist after replay terminal update");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/707")
        );
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_plan_phase_when_checkpoint_exists_and_pr_missing(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-checkpoint-no-pr", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-checkpoint-no-pr",
            None,
            Some("## Plan\nStep 1"),
            None,
            "plan_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-checkpoint-no-pr")
            .await?
            .expect("checkpoint no-pr task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Plan);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_preserves_triage_phase_when_checkpoint_exists_and_pr_missing(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-triage-no-pr", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;
        db.write_checkpoint(
            "task-triage-no-pr",
            Some("triage output"),
            None,
            None,
            "triage_done",
        )
        .await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-triage-no-pr")
            .await?
            .expect("triage no-pr task should exist");
        assert!(matches!(loaded.status, TaskStatus::Pending));
        assert_eq!(loaded.phase, TaskPhase::Triage);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_agent_review_no_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-agent-review-no-checkpoint", TaskStatus::AgentReview);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-agent-review-no-checkpoint")
            .await?
            .expect("agent review no-checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_waiting_no_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-waiting-no-checkpoint", TaskStatus::Waiting);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-waiting-no-checkpoint")
            .await?
            .expect("waiting no-checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_reviewing_no_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-reviewing-no-checkpoint", TaskStatus::Reviewing);
        task.phase = TaskPhase::Review;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-reviewing-no-checkpoint")
            .await?
            .expect("reviewing no-checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn recover_in_progress_sets_terminal_phase_for_implementing_no_checkpoint(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-implementing-no-checkpoint", TaskStatus::Implementing);
        task.phase = TaskPhase::Plan;
        db.insert(&task).await?;

        db.recover_in_progress().await?;

        let loaded = db
            .get("task-implementing-no-checkpoint")
            .await?
            .expect("implementing no-checkpoint task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_terminal_update_is_authoritative_for_phase() -> anyhow::Result<()>
    {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task(
            "task-authoritative-terminal-phase",
            TaskStatus::Implementing,
        );
        task.phase = TaskPhase::Plan;
        db.insert(&task).await?;

        db.apply_replayed_state("task-authoritative-terminal-phase", None, Some("failed"))
            .await?;

        let loaded = db
            .get("task-authoritative-terminal-phase")
            .await?
            .expect("authoritative terminal task should exist");
        assert!(matches!(loaded.status, TaskStatus::Failed));
        assert_eq!(loaded.phase, TaskPhase::Terminal);
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_tasks_in_order() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("task-a", TaskStatus::Pending)).await?;
        db.insert(&make_task("task-b", TaskStatus::Done)).await?;
        db.insert(&make_task("task-c", TaskStatus::Failed)).await?;

        let all = db.list().await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        assert!(db.get("nonexistent").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn status_roundtrip_all_variants() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let variants = [
            ("s-pending", TaskStatus::Pending),
            ("s-impl", TaskStatus::Implementing),
            ("s-review", TaskStatus::AgentReview),
            ("s-waiting", TaskStatus::Waiting),
            ("s-reviewing", TaskStatus::Reviewing),
            ("s-done", TaskStatus::Done),
            ("s-failed", TaskStatus::Failed),
        ];

        for (id, status) in &variants {
            db.insert(&make_task(id, status.clone())).await?;
        }

        for (id, expected) in &variants {
            let loaded = db.get(id).await?.expect("task should exist");
            assert_eq!(
                std::mem::discriminant(&loaded.status),
                std::mem::discriminant(expected),
                "status mismatch for task {id}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_with_error_persists() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-err", TaskStatus::Failed);
        task.error = Some("agent panicked".to_string());
        db.insert(&task).await?;

        let loaded = db
            .get("task-err")
            .await?
            .expect("task with error should exist");
        assert_eq!(loaded.error.as_deref(), Some("agent panicked"));
        Ok(())
    }

    #[tokio::test]
    async fn source_and_external_id_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-meta", TaskStatus::Pending);
        task.source = Some("github".to_string());
        task.external_id = Some("20".to_string());
        task.repo = Some("acme/harness".to_string());
        db.insert(&task).await?;

        let loaded = db.get("task-meta").await?.expect("task should exist");
        assert_eq!(loaded.source.as_deref(), Some("github"));
        assert_eq!(loaded.external_id.as_deref(), Some("20"));
        assert_eq!(loaded.repo.as_deref(), Some("acme/harness"));
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("tasks.db");

        {
            let db = TaskDb::open(&db_path).await?;
            db.insert(&make_task("task-persist", TaskStatus::Done))
                .await?;
        }

        let db = TaskDb::open(&db_path).await?;
        let loaded = db
            .get("task-persist")
            .await?
            .expect("task should survive reopen");
        assert!(matches!(loaded.status, TaskStatus::Done));
        Ok(())
    }

    #[tokio::test]
    async fn list_children_returns_subtasks_by_parent_id() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let parent = make_task("parent-1", TaskStatus::Pending);
        db.insert(&parent).await?;

        let mut child1 = make_task("child-1", TaskStatus::Pending);
        child1.parent_id = Some(harness_core::types::TaskId("parent-1".to_string()));
        db.insert(&child1).await?;

        let mut child2 = make_task("child-2", TaskStatus::Done);
        child2.parent_id = Some(harness_core::types::TaskId("parent-1".to_string()));
        db.insert(&child2).await?;

        // Unrelated task.
        db.insert(&make_task("unrelated", TaskStatus::Pending))
            .await?;

        let children = db.list_children("parent-1").await?;
        assert_eq!(children.len(), 2);
        assert!(children.iter().all(|c| c
            .parent_id
            .as_ref()
            .map(|id| id.0 == "parent-1")
            .unwrap_or(false)));

        let no_children = db.list_children("nonexistent").await?;
        assert!(no_children.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn recover_no_checkpoint_marks_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("t-pending", TaskStatus::Pending))
            .await?;
        // All four interrupted statuses, no checkpoint → should all fail
        db.insert(&make_task("t-implementing", TaskStatus::Implementing))
            .await?;
        db.insert(&make_task("t-agent-review", TaskStatus::AgentReview))
            .await?;
        db.insert(&make_task("t-reviewing", TaskStatus::Reviewing))
            .await?;
        db.insert(&make_task("t-waiting", TaskStatus::Waiting))
            .await?;
        db.insert(&make_task("t-done", TaskStatus::Done)).await?;
        db.insert(&make_task("t-failed", TaskStatus::Failed))
            .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(
            result.failed, 4,
            "implementing + agent_review + reviewing + waiting without checkpoints should fail"
        );
        assert_eq!(result.resumed, 0);
        assert_eq!(result.transient_failed, 0);

        // pending stays pending, no error set
        let pending = db.get("t-pending").await?.expect("should exist");
        assert!(matches!(pending.status, TaskStatus::Pending));
        assert!(pending.error.is_none());

        // implementing (no checkpoint, no PR) → failed with diagnostic info
        let implementing = db
            .get("t-implementing")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-implementing should exist"))?;
        assert!(matches!(implementing.status, TaskStatus::Failed));
        let err = implementing.error.as_deref().unwrap_or("");
        assert!(
            err.contains("was: implementing"),
            "error should contain original status"
        );
        assert!(err.contains("round:"), "error should contain round info");

        // agent_review (no checkpoint, no PR) → failed
        let agent_review = db
            .get("t-agent-review")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-agent-review should exist"))?;
        assert!(matches!(agent_review.status, TaskStatus::Failed));
        assert!(agent_review
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: agent_review"));

        // reviewing → failed
        let reviewing = db.get("t-reviewing").await?.expect("should exist");
        assert!(matches!(reviewing.status, TaskStatus::Failed));
        assert!(reviewing
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: reviewing"));

        // waiting → failed
        let waiting = db.get("t-waiting").await?.expect("should exist");
        assert!(matches!(waiting.status, TaskStatus::Failed));
        assert!(waiting
            .error
            .as_deref()
            .unwrap_or("")
            .contains("was: waiting"));

        // terminal states unchanged
        let done = db.get("t-done").await?.expect("should exist");
        assert!(matches!(done.status, TaskStatus::Done));
        let failed = db.get("t-failed").await?.expect("should exist");
        assert!(matches!(failed.status, TaskStatus::Failed));

        Ok(())
    }

    #[tokio::test]
    async fn recover_with_tasks_pr_url_resumes_pending() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // implementing WITH tasks.pr_url → resumed (not failed)
        let mut with_pr = make_task("t-impl-pr", TaskStatus::Implementing);
        with_pr.pr_url = Some("https://github.com/owner/repo/pull/42".to_string());
        db.insert(&with_pr).await?;

        // agent_review WITH tasks.pr_url → resumed
        let mut review_pr = make_task("t-review-pr", TaskStatus::AgentReview);
        review_pr.pr_url = Some("https://github.com/owner/repo/pull/43".to_string());
        db.insert(&review_pr).await?;

        // implementing WITHOUT PR and no checkpoint → still failed
        db.insert(&make_task("t-impl-no-pr", TaskStatus::Implementing))
            .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 2, "tasks with pr_url should be resumed");
        assert_eq!(
            result.failed, 1,
            "task without pr_url or checkpoint should fail"
        );
        assert_eq!(result.transient_failed, 0);

        // Verify: implementing with PR → pending (resumed)
        let impl_pr = db
            .get("t-impl-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-pr should exist"))?;
        assert!(
            matches!(impl_pr.status, TaskStatus::Pending),
            "implementing with PR should be resumed to pending"
        );
        assert!(
            impl_pr.pr_url.as_deref() == Some("https://github.com/owner/repo/pull/42"),
            "pr_url should be preserved"
        );
        let err = impl_pr.error.as_deref().unwrap_or("");
        assert!(
            err.contains("resumed after restart"),
            "error should note resumption"
        );
        assert!(err.contains("pull/42"), "should reference PR URL");

        // Verify: agent_review with PR → pending (resumed)
        let review = db
            .get("t-review-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-review-pr should exist"))?;
        assert!(
            matches!(review.status, TaskStatus::Pending),
            "agent_review with PR should be resumed to pending"
        );

        // Verify: implementing without PR → failed
        let no_pr = db
            .get("t-impl-no-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-no-pr should exist"))?;
        assert!(matches!(no_pr.status, TaskStatus::Failed));

        Ok(())
    }

    #[tokio::test]
    async fn recover_with_plan_checkpoint_resumes_pending() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Task interrupted at implementing stage, plan was completed and checkpointed
        db.insert(&make_task("t-impl-plan", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint(
            "t-impl-plan",
            None,
            Some("## Plan\nStep 1: do X"),
            None,
            "plan_done",
        )
        .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 1);
        assert_eq!(result.failed, 0);

        let task = db
            .get("t-impl-plan")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-plan should exist"))?;
        assert!(matches!(task.status, TaskStatus::Pending));
        assert!(
            task.error
                .as_deref()
                .unwrap_or("")
                .contains("plan checkpoint"),
            "error should mention plan checkpoint"
        );

        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_write_and_load_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // No checkpoint yet → None
        assert!(db.load_checkpoint("task-1").await?.is_none());

        // Write triage checkpoint
        db.write_checkpoint(
            "task-1",
            Some("triage output here"),
            None,
            None,
            "triage_done",
        )
        .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist"))?;
        assert_eq!(ck.task_id, "task-1");
        assert_eq!(ck.triage_output.as_deref(), Some("triage output here"));
        assert!(ck.plan_output.is_none());
        assert!(ck.pr_url.is_none());
        assert_eq!(ck.last_phase, "triage_done");

        // Advance to plan checkpoint — triage_output preserved via COALESCE
        db.write_checkpoint("task-1", None, Some("plan text"), None, "plan_done")
            .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist after plan write"))?;
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage output here"),
            "triage preserved"
        );
        assert_eq!(ck.plan_output.as_deref(), Some("plan text"));
        assert!(ck.pr_url.is_none());
        assert_eq!(ck.last_phase, "plan_done");

        // Advance to pr_created — all previous fields preserved
        db.write_checkpoint(
            "task-1",
            None,
            None,
            Some("https://github.com/o/r/pull/5"),
            "pr_created",
        )
        .await?;

        let ck = db
            .load_checkpoint("task-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint should exist after pr write"))?;
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage output here"),
            "triage preserved"
        );
        assert_eq!(
            ck.plan_output.as_deref(),
            Some("plan text"),
            "plan preserved"
        );
        assert_eq!(ck.pr_url.as_deref(), Some("https://github.com/o/r/pull/5"));
        assert_eq!(ck.last_phase, "pr_created");

        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_upsert_replaces_last_phase() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.write_checkpoint("task-x", Some("triage"), None, None, "triage_done")
            .await?;
        db.write_checkpoint("task-x", None, Some("plan"), None, "plan_done")
            .await?;

        let ck = db
            .load_checkpoint("task-x")
            .await?
            .ok_or_else(|| anyhow::anyhow!("checkpoint for task-x should exist"))?;
        assert_eq!(
            ck.last_phase, "plan_done",
            "last_phase should advance to plan_done"
        );
        assert_eq!(
            ck.triage_output.as_deref(),
            Some("triage"),
            "triage preserved"
        );
        assert_eq!(ck.plan_output.as_deref(), Some("plan"));

        Ok(())
    }

    #[tokio::test]
    async fn recover_result_separates_resumed_from_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Will be resumed: has a plan checkpoint
        db.insert(&make_task("t-resumable", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint("t-resumable", None, Some("plan output"), None, "plan_done")
            .await?;

        // Will be failed: no checkpoint, no PR
        db.insert(&make_task("t-no-checkpoint", TaskStatus::Reviewing))
            .await?;

        // Will be resumed: has PR in tasks table
        let mut with_pr = make_task("t-has-pr", TaskStatus::AgentReview);
        with_pr.pr_url = Some("https://github.com/o/r/pull/99".to_string());
        db.insert(&with_pr).await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(
            result.resumed, 2,
            "t-resumable and t-has-pr should be resumed"
        );
        assert_eq!(result.failed, 1, "t-no-checkpoint should fail");
        assert_eq!(result.transient_failed, 0);

        Ok(())
    }

    #[tokio::test]
    async fn recover_corrupted_pr_url_goes_to_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Corrupted: pr_url is Some("") — is_some() passes but parse fails
        let mut empty_url = make_task("t-empty-url", TaskStatus::Reviewing);
        empty_url.pr_url = Some(String::new());
        db.insert(&empty_url).await?;

        // Corrupted: pr_url is garbage with no pull number
        let mut garbage_url = make_task("t-garbage-url", TaskStatus::Implementing);
        garbage_url.pr_url = Some("not-a-pr-url".to_string());
        db.insert(&garbage_url).await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.resumed, 0, "corrupted URLs must not be resumed");
        assert_eq!(result.failed, 2, "both corrupted-URL tasks must fail");

        Ok(())
    }

    #[tokio::test]
    async fn task_db_rejects_unknown_status() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("tasks.db");
        let db = TaskDb::open(&db_path).await?;

        let task = make_task("task-unknown", TaskStatus::Pending);
        db.insert(&task).await?;

        sqlx::query("UPDATE tasks SET status = ? WHERE id = ?")
            .bind("unknown_status")
            .bind("task-unknown")
            .execute(&db.pool)
            .await?;

        let err = db
            .get("task-unknown")
            .await
            .expect_err("unknown status must return an explicit error");
        let message = format!("{err:#}");
        assert!(message.contains("unknown task status"));
        Ok(())
    }

    /// Guard: TASK_ROW_COLUMNS must list exactly the same fields as TaskRow.
    /// If a field is added to TaskRow but not to the constant, this test fails
    /// at compile time (struct construction) or at runtime (count mismatch).
    #[test]
    fn task_row_columns_match_struct() {
        let columns: Vec<&str> = super::TASK_ROW_COLUMNS.split(", ").collect();

        // Build a TaskRow using every column name — compile error if a field is missing.
        let _row = TaskRow {
            id: String::new(),
            status: String::new(),
            turn: 0,
            pr_url: None,
            rounds: String::new(),
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            created_at: None,
            repo: None,
            depends_on: String::new(),
            project: None,
            priority: 0,
            description: None,
            phase: String::new(),
        };

        // Count must match — catches column added to constant but not struct (or vice versa).
        assert_eq!(
            columns.len(),
            16, // bump this when adding a field
            "TASK_ROW_COLUMNS has {} entries but expected 16 — update both the constant and this test",
            columns.len()
        );
    }

    /// Regression: list_by_status must map to TaskRow correctly (including priority).
    /// This was the exact crash path: startup calls list_by_status to load active tasks.
    #[tokio::test]
    async fn list_by_status_returns_matching_tasks() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-pending", TaskStatus::Pending);
        task.priority = 2;
        db.insert(&task).await?;
        db.insert(&make_task("task-done", TaskStatus::Done)).await?;

        let pending = db.list_by_status(&["pending"]).await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.0, "task-pending");
        assert_eq!(
            pending[0].priority, 2,
            "priority must survive list_by_status roundtrip"
        );

        let multi = db.list_by_status(&["pending", "done"]).await?;
        assert_eq!(multi.len(), 2);

        let empty = db.list_by_status(&[]).await?;
        assert!(empty.is_empty());
        Ok(())
    }
}
