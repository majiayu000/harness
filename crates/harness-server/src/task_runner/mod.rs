mod dependencies;
mod helpers;
mod lifecycle;
mod recovery;
mod store;
mod types;

pub use dependencies::{check_awaiting_deps, spawn_task_awaiting_deps};
pub use lifecycle::{register_pending_task, spawn_preregistered_task, spawn_task};
pub use store::TaskStore;
pub use types::{
    CompletionCallback, CreateTaskRequest, DashboardCounts, LlmMetricsInputs,
    PersistedRequestSettings, ProjectCounts, RoundResult, TaskId, TaskPhase, TaskState, TaskStatus,
    TaskSummary, MAX_TASK_PRIORITY,
};

pub(crate) use helpers::{
    effective_turn_timeout, fill_missing_repo_from_project, resolve_canonical_project,
};
pub(crate) use lifecycle::{mutate_and_persist, update_status};
