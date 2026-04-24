mod metrics;
mod request;
pub(super) mod spawn;
mod state;
mod store;
mod types;

// CompletionCallback references TaskState (state.rs) which depends on types.rs.
// Declaring it here avoids a circular import between the two leaf modules.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
pub type CompletionCallback =
    Arc<dyn Fn(TaskState) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

// Re-export everything that was previously public from the flat task_runner.rs.
pub use metrics::{DashboardCounts, LlmMetricsInputs, ProjectCounts};
pub use request::{
    fill_missing_repo_from_project, CreateTaskRequest, PersistedRequestSettings, SystemTaskInput,
    MAX_TASK_PRIORITY,
};
pub use spawn::{
    check_awaiting_deps, effective_turn_timeout, prompt_requires_plan, register_pending_task,
    resolve_canonical_project, spawn_preregistered_task, spawn_task, spawn_task_awaiting_deps,
};
pub use state::{RecentFailureTask, RoundResult, TaskState, TaskSummary};
pub use store::{mutate_and_persist, update_status, TaskStore};
pub use types::{TaskId, TaskKind, TaskPhase, TaskStatus};

impl TaskStore {
    /// Return the most recent `limit` failed tasks, newest first.
    pub async fn list_recent_failed(&self, limit: i64) -> anyhow::Result<Vec<RecentFailureTask>> {
        self.db.list_recent_failed(limit).await
    }
}
