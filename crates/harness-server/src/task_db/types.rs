use crate::task_runner::{TaskState, TaskStatus};
use chrono::{DateTime, Utc};
use harness_core::error::TaskDbDecodeError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Maximum artifact content size in bytes before truncation.
pub(super) const ARTIFACT_MAX_BYTES: usize = 65_536;

/// Maximum prompt content size in bytes before truncation (2 MB).
pub(super) const PROMPT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// A single persisted artifact captured from agent output during task execution.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskArtifact {
    pub task_id: String,
    pub turn: i64,
    pub artifact_type: String,
    pub content: String,
    pub created_at: String,
}

/// A single persisted agent prompt captured per turn for UI observability.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskPrompt {
    pub task_id: String,
    pub turn: i64,
    pub phase: String,
    pub prompt: String,
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

/// Result of [`crate::task_db::TaskDb::recover_in_progress`].
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Tasks that were in interrupted states and are now `failed`.
    pub failed: u32,
    /// Tasks that were in interrupted states and have checkpoints — now `pending` for resume.
    pub resumed: u32,
    /// Tasks that were `pending` mid-transient-retry at crash time and are now `failed`.
    pub transient_failed: u32,
}

/// Row returned by the checkpoint JOIN query inside `recover_in_progress`.
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

/// Column list for `query_as::<_, TaskRow>` — single source of truth.
///
/// Every SELECT that maps to `TaskRow` MUST use this constant.
/// When adding a field to `TaskRow`, add the column here once and all queries
/// pick it up automatically.  The `task_row_columns_match_struct` test below
/// will fail if this list drifts from the struct definition.
pub(super) const TASK_ROW_COLUMNS: &str = "id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, phase, description, request_settings, issue";

#[derive(sqlx::FromRow)]
pub(super) struct TaskRow {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) turn: i64,
    pub(super) pr_url: Option<String>,
    pub(super) rounds: String,
    pub(super) error: Option<String>,
    pub(super) source: Option<String>,
    pub(super) external_id: Option<String>,
    pub(super) parent_id: Option<String>,
    pub(super) created_at: Option<DateTime<Utc>>,
    pub(super) repo: Option<String>,
    pub(super) depends_on: String,
    pub(super) project: Option<String>,
    pub(super) priority: i64,
    pub(super) phase: String,
    pub(super) description: Option<String>,
    pub(super) request_settings: Option<String>,
    pub(super) issue: Option<i64>,
}

/// Combined row for the pending-tasks-with-checkpoint JOIN query.
///
/// Aliases checkpoint columns to avoid collision with task columns:
/// `c.pr_url` → `ck_pr_url`, `c.updated_at` → `ck_updated_at`.
#[derive(sqlx::FromRow)]
pub(super) struct PendingCheckpointRow {
    // Task columns
    pub(super) id: String,
    pub(super) status: String,
    pub(super) turn: i64,
    pub(super) pr_url: Option<String>,
    pub(super) rounds: String,
    pub(super) error: Option<String>,
    pub(super) source: Option<String>,
    pub(super) external_id: Option<String>,
    pub(super) parent_id: Option<String>,
    pub(super) created_at: Option<DateTime<Utc>>,
    pub(super) repo: Option<String>,
    pub(super) depends_on: String,
    pub(super) project: Option<String>,
    pub(super) priority: i64,
    pub(super) phase: String,
    pub(super) description: Option<String>,
    pub(super) request_settings: Option<String>,
    pub(super) issue: Option<i64>,
    // Checkpoint columns (aliased)
    pub(super) triage_output: Option<String>,
    pub(super) plan_output: Option<String>,
    pub(super) ck_pr_url: Option<String>,
    pub(super) last_phase: String,
    pub(super) ck_updated_at: String,
}

/// Lightweight row for summary queries — omits the heavy `rounds` column.
#[derive(sqlx::FromRow)]
pub(super) struct TaskSummaryRow {
    pub(super) id: String,
    pub(super) status: String,
    pub(super) turn: i64,
    pub(super) pr_url: Option<String>,
    pub(super) error: Option<String>,
    pub(super) source: Option<String>,
    pub(super) external_id: Option<String>,
    pub(super) parent_id: Option<String>,
    pub(super) created_at: Option<DateTime<Utc>>,
    pub(super) repo: Option<String>,
    pub(super) depends_on: String,
    pub(super) project: Option<String>,
    pub(super) phase: String,
    pub(super) description: Option<String>,
}

impl TaskRow {
    pub(super) fn try_into_task_state(self) -> anyhow::Result<TaskState> {
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
            issue,
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
            issue: issue.map(|n| n as u64),
            description,
            created_at: created_at.map(|dt| dt.to_rfc3339()),
            priority: priority.clamp(0, 255) as u8,
            phase: decoded_phase,
            triage_output: None,
            plan_output: None,
            repo,
            request_settings: decoded_request_settings,
        })
    }
}

impl TaskSummaryRow {
    pub(super) fn try_into_summary(self) -> anyhow::Result<crate::task_runner::TaskSummary> {
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
            created_at: self.created_at.map(|dt| dt.to_rfc3339()),
            phase: decoded_phase,
            depends_on,
            subtask_ids: Vec::new(),
            project: self.project,
        })
    }
}
