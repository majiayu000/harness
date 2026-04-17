use crate::task_runner::{TaskPhase, TaskState, TaskStatus};
use harness_core::error::TaskDbDecodeError;
use std::path::PathBuf;

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
    pub(super) created_at: Option<String>,
    pub(super) repo: Option<String>,
    pub(super) depends_on: String,
    pub(super) project: Option<String>,
    pub(super) priority: i64,
    pub(super) phase: String,
    pub(super) description: Option<String>,
    pub(super) request_settings: Option<String>,
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
        let decoded_phase = serde_json::from_str::<TaskPhase>(&phase).map_err(|source| {
            TaskDbDecodeError::PhaseDeserialize {
                task_id: id.clone(),
                source,
            }
        })?;

        let decoded_request_settings = request_settings.as_deref().and_then(|json| {
            serde_json::from_str::<crate::task_runner::PersistedRequestSettings>(json)
                .map_err(|e| {
                    tracing::warn!(task_id = %id, "failed to decode request_settings: {e}");
                    e
                })
                .ok()
        });

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
            request_settings: decoded_request_settings,
            repo,
        })
    }
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
    pub(super) created_at: Option<String>,
    pub(super) repo: Option<String>,
    pub(super) depends_on: String,
    pub(super) project: Option<String>,
    pub(super) phase: String,
    pub(super) description: Option<String>,
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
            created_at: self.created_at,
            phase: decoded_phase,
            depends_on,
            subtask_ids: Vec::new(),
            project: self.project,
        })
    }
}
