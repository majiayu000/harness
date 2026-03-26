use crate::db::{open_pool, Migration, Migrator};
use crate::task_runner::{TaskId, TaskState, TaskStatus};
use harness_core::TaskDbDecodeError;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use std::path::Path;

/// Maximum artifact content size in bytes before truncation.
const ARTIFACT_MAX_BYTES: usize = 65_536;

/// Versioned migrations for the tasks table.
///
/// v1 – baseline schema (all columns including those added in later iterations)
/// v2/v3/v4 – additive ALTER TABLE for databases that predate v1 tracking;
///   duplicate-column errors are silently ignored by the Migrator.
/// v5 – add task_artifacts table for persisting agent output per task turn.
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

/// Result of [`TaskDb::recover_in_progress`].
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Tasks that were in interrupted states and are now `failed`.
    pub failed: u32,
    /// Tasks that were `pending` mid-transient-retry at crash time and are now `failed`.
    pub transient_failed: u32,
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
            "INSERT INTO tasks (id, status, turn, pr_url, rounds, error, parent_id, created_at, repo, depends_on)
             VALUES (?, ?, ?, ?, ?, ?, ?, COALESCE(?, datetime('now')), ?, ?)",
        )
        .bind(&state.id.0)
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .bind(state.parent_id.as_ref().map(|id| &id.0))
        .bind(&state.created_at)
        .bind(&state.repo)
        .bind(&depends_on_json)
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
                    repo = ?, depends_on = ?, updated_at = datetime('now')
             WHERE id = ?",
        )
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .bind(&state.repo)
        .bind(&depends_on_json)
        .bind(&state.id.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        let row = sqlx::query_as::<_, TaskRow>(
            "SELECT id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on
             FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TaskRow::try_into_task_state).transpose()
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TaskState>> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on
             FROM tasks ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TaskRow::try_into_task_state).collect()
    }

    /// Recovery on server restart.
    ///
    /// - interrupted states (`implementing`, `agent_review`, `reviewing`, `waiting`) → `failed`
    ///   (agent process died mid-flight; no safe resume path)
    /// - `pending` with transient-retry error → `failed` (crashed mid-backoff, no PR to resume)
    /// - `pending` otherwise → unchanged (will be picked up by the re-dispatch loop)
    ///
    /// Diagnostic context is embedded in the `error` field so operators can correlate
    /// recovery events with their original execution state.
    ///
    /// Returns a [`RecoveryResult`] with counts for failed outcomes.
    pub async fn recover_in_progress(&self) -> anyhow::Result<RecoveryResult> {
        // Interrupted tasks cannot be resumed safely after restart without a fresh
        // agent invocation, so mark them failed with diagnostic context.
        let failed = sqlx::query(
            "UPDATE tasks \
             SET status = 'failed', \
                 error = 'recovered after restart (was: ' || status \
                      || ', round: ' || turn \
                      || ', pr: ' || COALESCE(pr_url, 'none') || ')', \
                 updated_at = datetime('now') \
             WHERE status IN ('implementing', 'agent_review', 'reviewing', 'waiting')",
        )
        .execute(&self.pool)
        .await?
        .rows_affected() as u32;

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

        Ok(RecoveryResult {
            failed,
            transient_failed,
        })
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

    /// Return all tasks whose `parent_id` matches the given parent task ID.
    pub async fn list_children(&self, parent_id: &str) -> anyhow::Result<Vec<TaskState>> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on
             FROM tasks WHERE parent_id = ? ORDER BY created_at DESC",
        )
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
        } = self;

        let decoded_rounds = serde_json::from_str(&rounds).map_err(|source| {
            TaskDbDecodeError::RoundsDeserialize {
                task_id: id.clone(),
                source,
            }
        })?;

        Ok(TaskState {
            id: TaskId(id),
            status: status.parse::<TaskStatus>()?,
            turn: turn as u32,
            pr_url,
            rounds: decoded_rounds,
            error,
            source,
            external_id,
            parent_id: parent_id.map(TaskId),
            depends_on: serde_json::from_str(&depends_on).unwrap_or_default(),
            subtask_ids: Vec::new(),
            project_root: None,
            issue: None,
            description: None,
            created_at,
            phase: crate::task_runner::TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            repo,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskDb, TaskRow};
    use crate::task_runner::{RoundResult, TaskId, TaskState, TaskStatus};
    use harness_core::TaskDbDecodeError;

    fn build_task_row(rounds: &str) -> TaskRow {
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
            depends_on: "[]".to_string(),
        }
    }

    #[test]
    fn invalid_rounds_json_returns_distinguishable_error() {
        let err = build_task_row("{not-json")
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

    fn make_task(id: &str, status: TaskStatus) -> TaskState {
        TaskState {
            id: TaskId(id.to_string()),
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
            phase: crate::task_runner::TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            repo: None,
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
        child1.parent_id = Some(TaskId("parent-1".to_string()));
        db.insert(&child1).await?;

        let mut child2 = make_task("child-2", TaskStatus::Done);
        child2.parent_id = Some(TaskId("parent-1".to_string()));
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
    async fn recover_in_progress_marks_all_interrupted_as_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        db.insert(&make_task("t-pending", TaskStatus::Pending))
            .await?;
        // implementing WITHOUT PR → should fail
        db.insert(&make_task("t-implementing", TaskStatus::Implementing))
            .await?;
        // agent_review WITHOUT PR → should fail
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
            "implementing + agent_review + reviewing + waiting should all be failed"
        );
        assert_eq!(result.transient_failed, 0);

        // pending stays pending, no error set
        let pending = db.get("t-pending").await?.expect("should exist");
        assert!(matches!(pending.status, TaskStatus::Pending));
        assert!(pending.error.is_none());

        // implementing (no PR) → failed with diagnostic info
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

        // agent_review (no PR) → failed with diagnostic info
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

        // reviewing → failed with diagnostic info
        let reviewing = db.get("t-reviewing").await?.expect("should exist");
        assert!(matches!(reviewing.status, TaskStatus::Failed));
        let err = reviewing.error.as_deref().unwrap_or("");
        assert!(
            err.contains("was: reviewing"),
            "error should note original status"
        );

        // waiting → failed with diagnostic info
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
    async fn recover_marks_tasks_with_pr_as_failed() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = TaskDb::open(&tmp.path().join("tasks.db")).await?;

        // implementing WITH PR → should fail
        let mut with_pr = make_task("t-impl-pr", TaskStatus::Implementing);
        with_pr.pr_url = Some("https://github.com/owner/repo/pull/42".to_string());
        db.insert(&with_pr).await?;

        // agent_review WITH PR → should fail
        let mut review_pr = make_task("t-review-pr", TaskStatus::AgentReview);
        review_pr.pr_url = Some("https://github.com/owner/repo/pull/43".to_string());
        db.insert(&review_pr).await?;

        // implementing WITHOUT PR → should fail
        db.insert(&make_task("t-impl-no-pr", TaskStatus::Implementing))
            .await?;

        let result = db.recover_in_progress().await?;
        assert_eq!(result.failed, 3, "all interrupted tasks should fail");
        assert_eq!(result.transient_failed, 0);

        // Verify: implementing with PR → failed
        let impl_pr = db
            .get("t-impl-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-impl-pr should exist"))?;
        assert!(
            matches!(impl_pr.status, TaskStatus::Failed),
            "implementing with PR should be marked failed"
        );
        let err = impl_pr.error.as_deref().unwrap_or("");
        assert!(
            err.contains("was: implementing"),
            "should contain original status"
        );
        assert!(
            err.contains("pull/42"),
            "should preserve PR URL in error context"
        );

        // Verify: agent_review with PR → failed
        let review = db
            .get("t-review-pr")
            .await?
            .ok_or_else(|| anyhow::anyhow!("t-review-pr should exist"))?;
        assert!(
            matches!(review.status, TaskStatus::Failed),
            "agent_review with PR should be marked failed"
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
}
