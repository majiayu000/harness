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
const TASK_ROW_COLUMNS: &str = "id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, phase, description, request_settings";

/// Versioned migrations for the tasks table.
///
/// v1 – baseline schema (all columns including those added in later iterations)
/// v2/v3/v4 – additive ALTER TABLE for databases that predate v1 tracking;
///   duplicate-column errors are silently ignored by the Migrator.
/// v5 – add task_artifacts table for persisting agent output per task turn.
/// v10 – add composite index on (project, status, updated_at).
/// v11 – add task_checkpoints table for phase recovery.
/// v12 – add priority column for priority-based task scheduling.
/// v13 – add phase column for consistent phase persistence across cache/DB paths.
/// v14 – add description column for task observability after restart.
/// v15 – add (status, updated_at DESC) index for project-agnostic terminal queries.
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
        description: "add phase column for consistent phase persistence",
        sql: r#"ALTER TABLE tasks ADD COLUMN phase TEXT NOT NULL DEFAULT '"implement"'"#,
    },
    Migration {
        version: 14,
        description: "add description column for task observability",
        sql: "ALTER TABLE tasks ADD COLUMN description TEXT",
    },
    Migration {
        version: 15,
        description: "add index on tasks(status, updated_at) for project-agnostic terminal queries",
        sql: "CREATE INDEX IF NOT EXISTS idx_tasks_status_updated \
              ON tasks(status, updated_at DESC)",
    },
    Migration {
        version: 16,
        description: "add request_settings column for execution limit recovery",
        sql: "ALTER TABLE tasks ADD COLUMN request_settings TEXT",
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
    fn status_placeholders(len: usize) -> String {
        std::iter::repeat_n("?", len).collect::<Vec<_>>().join(", ")
    }

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
        let phase_json = serde_json::to_string(&state.phase)?;
        let settings_json = state
            .request_settings
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok());
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, phase, description, request_settings)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, datetime('now')), ?, ?, ?, ?, ?, ?, ?)",
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
        sqlx::query(
            "UPDATE tasks SET status = ?, turn = ?, pr_url = ?, rounds = ?, error = ?,
                    source = ?, external_id = ?, repo = ?, depends_on = ?, project = ?,
                    priority = ?, phase = ?, description = ?, request_settings = ?,
                    updated_at = datetime('now')
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
        .bind(settings_json.as_deref())
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
        let placeholders = Self::status_placeholders(statuses.len());
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
            "SELECT id FROM tasks WHERE project = ? AND external_id = ? \
             AND status IN ('pending', 'awaiting_deps', 'implementing', 'agent_review', 'waiting', 'reviewing') \
             LIMIT 1",
        )
        .bind(project)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Find a `done` task for the same project + external_id that has a non-null `pr_url`.
    ///
    /// Returns `(task_id, pr_url)` when found. Only matches `done` — failed/cancelled
    /// tasks are excluded so that retries after failure are always permitted.
    pub async fn find_terminal_with_pr(
        &self,
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
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Back-fill `external_id` on a task that was created without one.
    ///
    /// The `AND external_id IS NULL` guard prevents overwriting a legitimately-set
    /// external_id on a re-queued task or any task that already has one.
    pub async fn update_external_id(&self, id: &str, external_id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET external_id = ?, updated_at = datetime('now') WHERE id = ? AND external_id IS NULL",
        )
        .bind(external_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Overwrite `external_id` on an auto-fix task, even if one is already set.
    ///
    /// Unlike [`update_external_id`], this has no `IS NULL` guard — it is used
    /// during streaming to implement "last sentinel wins" behaviour when the agent
    /// self-corrects by emitting a second `CREATED_ISSUE=` line.  The
    /// `AND source = 'auto-fix'` clause limits the blast radius to tasks created
    /// by the periodic reviewer; webhook-sourced tasks are never touched.
    pub async fn overwrite_external_id_auto_fix(
        &self,
        id: &str,
        external_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET external_id = ?, updated_at = datetime('now') WHERE id = ? AND source = 'auto-fix'",
        )
        .bind(external_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return all tasks as lightweight summaries, skipping the heavy `rounds` column.
    ///
    /// Used by the `/tasks` list endpoint to avoid deserializing large round histories
    /// when only summary fields are needed.
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

    /// Return `(task_id, first_token_latency_ms)` pairs for the most-recently
    /// completed terminal (done/failed) tasks, capped at 500 rows.
    ///
    /// The latency scalar is extracted directly in SQL via `json_extract` so the
    /// full `rounds` blob — which may contain large `detail` strings — is never
    /// allocated on the Rust heap.  The correlated subquery skips synthetic
    /// `resumed_checkpoint` rounds and returns the first real latency per task.
    ///
    /// Ordered by `updated_at DESC` because `updated_at` is written at task
    /// completion, so this correctly captures the 500 most-recently-finished
    /// tasks.  Using `created_at` would silently drop long-running tasks that
    /// finished recently but were created earlier.
    ///
    /// The `LIMIT 500` keeps each dashboard poll O(1) rather than
    /// O(total task history).  500 data points are more than sufficient for a
    /// statistically valid p50; they also represent a natural rolling window that
    /// gives more weight to recent latency behaviour.
    pub async fn list_terminal_first_token_latencies_ms(
        &self,
    ) -> anyhow::Result<Vec<(String, Option<i64>)>> {
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT id, \
             (SELECT CAST(json_extract(r.value, '$.first_token_latency_ms') AS INTEGER) \
              FROM json_each(rounds) r \
              WHERE json_extract(r.value, '$.result') != 'resumed_checkpoint' \
                AND json_extract(r.value, '$.first_token_latency_ms') IS NOT NULL \
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

    /// Return `(task_id, turn)` pairs for the 500 most-recently-completed
    /// terminal (done/failed) tasks, ordered by completion time (`updated_at DESC`).
    ///
    /// Only the lightweight `id` and `turn` columns are fetched; the heavy
    /// `rounds` blob is never touched.  The `LIMIT 500` keeps each dashboard
    /// poll O(1) rather than O(total task history).
    pub async fn list_terminal_turn_counts(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, turn FROM tasks \
             WHERE status IN ('done', 'failed') \
             ORDER BY updated_at DESC \
             LIMIT 500",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
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
        let placeholders = Self::status_placeholders(statuses.len());
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
            // Overwrite to terminal status; only touches tasks still in interrupted states
            // so we never downgrade a task that already reached Done/Failed in the DB.
            let sql = format!(
                "UPDATE tasks SET status = ?, pr_url = COALESCE(?, pr_url), \
                 updated_at = datetime('now') \
                 WHERE id = ? \
                 AND status IN ({})",
                Self::status_placeholders(TaskStatus::resumable_statuses().len())
            );
            let query = sqlx::query(&sql).bind(status).bind(pr_url).bind(task_id);
            let query = TaskStatus::resumable_statuses()
                .iter()
                .fold(query, |query, resumable| query.bind(*resumable));
            query.execute(&self.pool).await?;
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
        let rows = {
            let sql = format!(
                "SELECT t.id, t.status, t.turn, t.pr_url AS task_pr_url,
                        c.triage_output, c.plan_output, c.pr_url AS ck_pr_url
                 FROM tasks t
                 LEFT JOIN task_checkpoints c ON t.id = c.task_id
                 WHERE t.status IN ({})",
                Self::status_placeholders(TaskStatus::resumable_statuses().len())
            );
            let query = TaskStatus::resumable_statuses()
                .iter()
                .fold(sqlx::query_as::<_, RecoveryRow>(&sql), |query, status| {
                    query.bind(*status)
                });
            query.fetch_all(&self.pool).await?
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
                        "UPDATE tasks SET status = 'pending', error = NULL, pr_url = ?, \
                         updated_at = datetime('now') WHERE id = ?",
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
                         updated_at = datetime('now') WHERE id = ?",
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
        let outcome_statuses = [TaskStatus::Done.as_ref(), TaskStatus::Failed.as_ref()];
        let global_sql = format!(
            "SELECT COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks WHERE status IN ({})",
            Self::status_placeholders(outcome_statuses.len())
        );
        let global: (i64, i64) = outcome_statuses
            .iter()
            .fold(sqlx::query_as(&global_sql), |query, status| {
                query.bind(*status)
            })
            .fetch_one(&self.pool)
            .await?;

        let rows_sql = format!(
            "SELECT project, \
                    COUNT(CASE WHEN status = 'done' THEN 1 END), \
                    COUNT(CASE WHEN status = 'failed' THEN 1 END) \
             FROM tasks \
             WHERE status IN ({}) AND project IS NOT NULL \
             GROUP BY project",
            Self::status_placeholders(outcome_statuses.len())
        );
        let rows: Vec<(String, i64, i64)> = outcome_statuses
            .iter()
            .fold(sqlx::query_as(&rows_sql), |query, status| {
                query.bind(*status)
            })
            .fetch_all(&self.pool)
            .await?;

        let by_project = rows
            .into_iter()
            .map(|(p, d, f)| (p, d as u64, f as u64))
            .collect();
        Ok((global.0 as u64, global.1 as u64, by_project))
    }

    /// Count tasks that reached `done` status with `updated_at >= since`.
    ///
    /// `since` must be an ISO-8601 UTC timestamp (e.g. `2026-04-17T23:00:00Z`).
    /// Uses the `idx_tasks_status_updated` index. Used by the system overview
    /// endpoint to compute "merged in last 24h" without loading full task rows.
    pub async fn count_done_since(&self, since: &str) -> anyhow::Result<u64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM tasks WHERE status = 'done' AND updated_at >= ?")
                .bind(since)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 as u64)
    }

    /// Per-(project, hour-bucket) count of tasks that reached `done` after `since`.
    ///
    /// Returns rows of `(project_key, hour_iso, count)` where `hour_iso` is the
    /// `updated_at` timestamp truncated to the hour in UTC (format
    /// `YYYY-MM-DDTHH:00:00Z`). Rows with `project IS NULL` are reported with
    /// an empty project key. Used to build the fleet-throughput stacked-area
    /// chart (per project, per hour over the window).
    pub async fn done_per_project_hour_since(
        &self,
        since: &str,
    ) -> anyhow::Result<Vec<(String, String, u64)>> {
        let rows: Vec<(Option<String>, String, i64)> = sqlx::query_as(
            "SELECT project, \
                    strftime('%Y-%m-%dT%H:00:00Z', updated_at) AS hour, \
                    COUNT(*) \
             FROM tasks \
             WHERE status = 'done' AND updated_at >= ? \
             GROUP BY project, hour",
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

    /// Return all `pending` tasks that have a plan or triage checkpoint but no `pr_url`.
    ///
    /// Used at startup to re-dispatch tasks that were recovered from plan/triage checkpoints
    /// but were not picked up by the PR-based redispatch path (which only catches tasks with
    /// `pr_url` set).
    ///
    /// Excludes tasks that already have `tasks.pr_url` set — those are handled by the
    /// existing PR-based redispatch path in `http.rs`.
    pub async fn pending_tasks_with_checkpoint(
        &self,
    ) -> anyhow::Result<Vec<(TaskState, TaskCheckpoint)>> {
        let rows = sqlx::query_as::<_, PendingCheckpointRow>(
            "SELECT t.id, t.status, t.turn, t.pr_url, t.rounds, t.error, t.source, \
                    t.external_id, t.parent_id, t.created_at, t.repo, t.depends_on, \
                    t.project, t.priority, t.phase, t.description, t.request_settings, \
                    c.triage_output, c.plan_output, c.pr_url AS ck_pr_url, \
                    c.last_phase, c.updated_at AS ck_updated_at \
             FROM tasks t \
             JOIN task_checkpoints c ON c.task_id = t.id \
             WHERE t.status = 'pending' \
               AND t.pr_url IS NULL \
               AND (c.plan_output IS NOT NULL OR c.triage_output IS NOT NULL)",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows {
            let task_id = row.id.clone();
            let task_row = TaskRow {
                id: row.id,
                status: row.status,
                turn: row.turn,
                pr_url: row.pr_url,
                rounds: row.rounds,
                error: row.error,
                source: row.source,
                external_id: row.external_id,
                parent_id: row.parent_id,
                created_at: row.created_at,
                repo: row.repo,
                depends_on: row.depends_on,
                project: row.project,
                priority: row.priority,
                phase: row.phase,
                description: row.description,
                request_settings: row.request_settings,
            };
            let task_state = match task_row.try_into_task_state() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(task_id = %task_id, "skipping malformed pending checkpoint task: {e}");
                    continue;
                }
            };
            let checkpoint = TaskCheckpoint {
                task_id,
                triage_output: row.triage_output,
                plan_output: row.plan_output,
                pr_url: row.ck_pr_url,
                last_phase: row.last_phase,
                updated_at: row.ck_updated_at,
            };
            pairs.push((task_state, checkpoint));
        }
        Ok(pairs)
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
    phase: String,
    description: Option<String>,
    request_settings: Option<String>,
}

/// Combined row for the pending-tasks-with-checkpoint JOIN query.
///
/// Aliases checkpoint columns to avoid collision with task columns:
/// `c.pr_url` → `ck_pr_url`, `c.updated_at` → `ck_updated_at`.
#[derive(sqlx::FromRow)]
struct PendingCheckpointRow {
    // Task columns
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
    phase: String,
    description: Option<String>,
    request_settings: Option<String>,
    // Checkpoint columns (aliased)
    triage_output: Option<String>,
    plan_output: Option<String>,
    ck_pr_url: Option<String>,
    last_phase: String,
    ck_updated_at: String,
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
            phase,
            description,
            request_settings,
        } = self;

        let decoded_request_settings: Option<crate::task_runner::PersistedRequestSettings> =
            request_settings
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());

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
        let decoded_phase =
            serde_json::from_str::<crate::task_runner::TaskPhase>(&phase).map_err(|source| {
                TaskDbDecodeError::PhaseDeserialize {
                    task_id: id.clone(),
                    source,
                }
            })?;

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
            request_settings: decoded_request_settings,
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
    phase: String,
    description: Option<String>,
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
        let decoded_phase = serde_json::from_str::<crate::task_runner::TaskPhase>(&self.phase)
            .map_err(
                |source| harness_core::error::TaskDbDecodeError::PhaseDeserialize {
                    task_id: self.id.clone(),
                    source,
                },
            )?;
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
            phase: decoded_phase,
            depends_on,
            subtask_ids: Vec::new(),
            project: self.project,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskDb, TaskRow};
    use crate::task_runner::{RoundResult, TaskState, TaskStatus};
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
            phase: r#""implement""#.to_string(),
            description: None,
            request_settings: None,
        }
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
            request_settings: None,
        }
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
            first_token_latency_ms: None,
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
    async fn update_external_id_back_fills_null() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let task = make_task("task-backfill", TaskStatus::Pending);
        db.insert(&task).await?;

        // Confirm initial state has no external_id
        let before = db.get("task-backfill").await?.expect("should exist");
        assert!(before.external_id.is_none());

        db.update_external_id("task-backfill", "issue:55").await?;

        let after = db.get("task-backfill").await?.expect("should exist");
        assert_eq!(after.external_id.as_deref(), Some("issue:55"));
        Ok(())
    }

    #[tokio::test]
    async fn update_external_id_guard_does_not_overwrite() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-guard", TaskStatus::Pending);
        task.external_id = Some("issue:10".to_string());
        db.insert(&task).await?;

        // Attempt to overwrite — the IS NULL guard must prevent it
        db.update_external_id("task-guard", "issue:99").await?;

        let after = db.get("task-guard").await?.expect("should exist");
        assert_eq!(after.external_id.as_deref(), Some("issue:10"));
        Ok(())
    }

    #[tokio::test]
    async fn overwrite_external_id_auto_fix_updates_existing_value() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-autofix-overwrite", TaskStatus::Pending);
        task.source = Some("auto-fix".to_string());
        task.external_id = Some("issue:10".to_string());
        db.insert(&task).await?;

        // Self-correction: agent emits a second CREATED_ISSUE=20 — must overwrite
        db.overwrite_external_id_auto_fix("task-autofix-overwrite", "issue:20")
            .await?;

        let after = db
            .get("task-autofix-overwrite")
            .await?
            .expect("should exist");
        assert_eq!(after.external_id.as_deref(), Some("issue:20"));
        Ok(())
    }

    #[tokio::test]
    async fn overwrite_external_id_auto_fix_does_not_touch_non_autofix() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // A webhook-sourced task must never have its external_id overwritten
        let mut task = make_task("task-webhook", TaskStatus::Pending);
        task.source = Some("github".to_string());
        task.external_id = Some("issue:5".to_string());
        db.insert(&task).await?;

        db.overwrite_external_id_auto_fix("task-webhook", "issue:99")
            .await?;

        let after = db.get("task-webhook").await?.expect("should exist");
        assert_eq!(after.external_id.as_deref(), Some("issue:5"));
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
        db.insert(&make_task("t-cancelled", TaskStatus::Cancelled))
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
        let cancelled = db.get("t-cancelled").await?.expect("should exist");
        assert!(matches!(cancelled.status, TaskStatus::Cancelled));

        Ok(())
    }

    #[tokio::test]
    async fn apply_replayed_state_does_not_touch_cancelled() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("t-cancelled", TaskStatus::Cancelled);
        task.pr_url = Some("https://github.com/owner/repo/pull/9".to_string());
        db.insert(&task).await?;

        db.apply_replayed_state(
            "t-cancelled",
            Some("https://github.com/owner/repo/pull/10"),
            Some("failed"),
        )
        .await?;

        let loaded = db.get("t-cancelled").await?.expect("should exist");
        assert!(matches!(loaded.status, TaskStatus::Cancelled));
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/9")
        );
        Ok(())
    }

    #[tokio::test]
    async fn count_done_failed_by_project_excludes_cancelled() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let root = "/projects/alpha";
        let mut done = make_task("done", TaskStatus::Done);
        done.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&done).await?;

        let mut failed = make_task("failed", TaskStatus::Failed);
        failed.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&failed).await?;

        let mut cancelled = make_task("cancelled", TaskStatus::Cancelled);
        cancelled.project_root = Some(std::path::PathBuf::from(root));
        db.insert(&cancelled).await?;

        let (global_done, global_failed, by_project) = db.count_done_failed_by_project().await?;
        assert_eq!(global_done, 1);
        assert_eq!(global_failed, 1);
        assert_eq!(by_project, vec![(root.to_string(), 1, 1)]);
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
        assert!(
            impl_pr.error.is_none(),
            "resumed task must not have error set, got {:?}",
            impl_pr.error
        );

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
            task.error.is_none(),
            "resumed task must not have error set, got {:?}",
            task.error
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
            phase: String::new(),
            description: None,
            request_settings: None,
        };

        // Count must match — catches column added to constant but not struct (or vice versa).
        assert_eq!(
            columns.len(),
            17, // bump this when adding a field
            "TASK_ROW_COLUMNS has {} entries but expected 17 — update both the constant and this test",
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

    #[tokio::test]
    async fn find_active_duplicate_returns_pending_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-issue-42", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:42").await?;
        assert_eq!(dup, Some("task-issue-42".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_returns_implementing_task() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-impl-43", TaskStatus::Implementing);
        task.external_id = Some("issue:43".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:43").await?;
        assert_eq!(dup, Some("task-impl-43".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_ignores_terminal_tasks() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-done-42", TaskStatus::Done);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:42").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_different_project_no_match() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-proj-a", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/a"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/b", "issue:42").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn find_active_duplicate_different_external_id_no_match() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;
        let mut task = make_task("task-issue-42", TaskStatus::Pending);
        task.external_id = Some("issue:42".to_string());
        task.project_root = Some(std::path::PathBuf::from("/repo/foo"));
        db.insert(&task).await?;
        let dup = db.find_active_duplicate("/repo/foo", "issue:99").await?;
        assert_eq!(dup, None);
        Ok(())
    }

    #[tokio::test]
    async fn phase_round_trips_through_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-phase-rt", TaskStatus::Implementing);
        task.phase = crate::task_runner::TaskPhase::Review;
        db.insert(&task).await?;

        let loaded = db
            .get("task-phase-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(loaded.phase, crate::task_runner::TaskPhase::Review);
        Ok(())
    }

    #[tokio::test]
    async fn description_round_trips_through_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-desc-rt", TaskStatus::Pending);
        task.description = Some("fix: crash on empty input".to_string());
        db.insert(&task).await?;

        let loaded = db
            .get("task-desc-rt")
            .await?
            .expect("inserted task should exist");
        assert_eq!(
            loaded.description.as_deref(),
            Some("fix: crash on empty input")
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_summaries_includes_phase_and_description() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("task-summary-pd", TaskStatus::Pending);
        task.phase = crate::task_runner::TaskPhase::Plan;
        task.description = Some("architect PR #5".to_string());
        db.insert(&task).await?;

        let summaries = db.list_summaries().await?;
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].phase, crate::task_runner::TaskPhase::Plan);
        assert_eq!(summaries[0].description.as_deref(), Some("architect PR #5"));
        Ok(())
    }

    #[tokio::test]
    async fn phase_defaults_for_legacy_rows() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Insert a row without supplying phase/description — SQLite uses column defaults.
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, rounds, depends_on) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("task-legacy")
        .bind("pending")
        .bind(0_i64)
        .bind("[]")
        .bind("[]")
        .execute(&db.pool)
        .await?;

        let loaded = db
            .get("task-legacy")
            .await?
            .expect("legacy task should exist");
        assert_eq!(
            loaded.phase,
            crate::task_runner::TaskPhase::default(),
            "phase should default to Implement for legacy rows"
        );
        assert!(
            loaded.description.is_none(),
            "description should be None for legacy rows"
        );
        Ok(())
    }

    // --- pending_tasks_with_checkpoint tests ---

    #[tokio::test]
    async fn pending_with_triage_checkpoint_is_returned() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("t-triage", TaskStatus::Pending);
        task.description = Some("issue #10".to_string());
        db.insert(&task).await?;
        db.write_checkpoint("t-triage", Some("triage output"), None, None, "triage_done")
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert_eq!(pairs.len(), 1);
        let (t, ck) = &pairs[0];
        assert_eq!(t.id.0, "t-triage");
        assert_eq!(ck.triage_output.as_deref(), Some("triage output"));
        assert!(ck.plan_output.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn pending_with_plan_checkpoint_is_returned() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("t-plan", TaskStatus::Pending);
        task.description = Some("issue #20".to_string());
        db.insert(&task).await?;
        db.write_checkpoint("t-plan", None, Some("plan text"), None, "plan_done")
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert_eq!(pairs.len(), 1);
        let (t, ck) = &pairs[0];
        assert_eq!(t.id.0, "t-plan");
        assert_eq!(ck.plan_output.as_deref(), Some("plan text"));
        Ok(())
    }

    #[tokio::test]
    async fn pending_with_pr_url_excluded_from_checkpoint_query() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Task with pr_url set — handled by PR-based redispatch, not checkpoint path.
        let mut task_pr = make_task("t-pr", TaskStatus::Pending);
        task_pr.pr_url = Some("https://github.com/o/r/pull/42".to_string());
        db.insert(&task_pr).await?;
        db.write_checkpoint("t-pr", None, Some("plan"), None, "plan_done")
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert!(
            pairs.is_empty(),
            "task with pr_url should be excluded from checkpoint query"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_without_checkpoint_not_returned() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("t-no-ck", TaskStatus::Pending))
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert!(
            pairs.is_empty(),
            "task with no checkpoint should not be returned"
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_pending_with_checkpoint_not_returned() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // Implementing status — should NOT be returned (status filter)
        db.insert(&make_task("t-impl", TaskStatus::Implementing))
            .await?;
        db.write_checkpoint("t-impl", None, Some("plan"), None, "plan_done")
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert!(pairs.is_empty(), "non-pending task should not be returned");
        Ok(())
    }

    #[tokio::test]
    async fn checkpoint_query_returns_task_state_fields() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        let mut task = make_task("t-fields", TaskStatus::Pending);
        task.description = Some("issue #99".to_string());
        task.repo = Some("owner/repo".to_string());
        task.source = Some("github".to_string());
        task.external_id = Some("ext-1".to_string());
        task.priority = 1;
        db.insert(&task).await?;
        db.write_checkpoint("t-fields", Some("triage out"), None, None, "triage_done")
            .await?;

        let pairs = db.pending_tasks_with_checkpoint().await?;
        assert_eq!(pairs.len(), 1);
        let (t, _) = &pairs[0];
        assert_eq!(t.description.as_deref(), Some("issue #99"));
        assert_eq!(t.repo.as_deref(), Some("owner/repo"));
        assert_eq!(t.source.as_deref(), Some("github"));
        assert_eq!(t.external_id.as_deref(), Some("ext-1"));
        assert_eq!(t.priority, 1);
        Ok(())
    }
}
