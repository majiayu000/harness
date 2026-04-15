mod scheduling;
mod store;
mod types;

pub use scheduling::{check_awaiting_deps, spawn_task_awaiting_deps};
pub use store::TaskStore;
pub(crate) use types::{
    effective_turn_timeout, fill_missing_repo_from_project, resolve_canonical_project,
};
#[cfg(test)]
pub(crate) use types::{
    is_non_decomposable_prompt_source, is_transient_error, prompt_requires_plan,
};
pub use types::{
    CompletionCallback, CreateTaskRequest, DashboardCounts, ProjectCounts, RoundResult, TaskPhase,
    TaskState, TaskStatus, TaskSummary,
};
pub use types::{TaskId, MAX_TASK_PRIORITY};

pub(crate) use scheduling::{mutate_and_persist, update_status};
pub use scheduling::{register_pending_task, spawn_preregistered_task, spawn_task};

#[cfg(test)]
pub(crate) use scheduling::spawn_task_with_worktree_detector;

#[cfg(test)]
mod spawn_tests;
#[cfg(test)]
mod tests;
