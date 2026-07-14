mod artifacts;
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
pub(crate) use artifacts::TaskArtifactSink;
pub use metrics::{DashboardCounts, LlmMetricsInputs, ProjectCounts};
pub use request::{
    fill_missing_repo_from_project, CreateTaskRequest, PersistedRequestSettings, SystemTaskInput,
    MAX_TASK_PRIORITY,
};
use spawn::{
    check_awaiting_deps as check_awaiting_deps_impl,
    register_pending_task as register_pending_task_impl,
    spawn_preregistered_task as spawn_preregistered_task_impl, spawn_task as spawn_task_impl,
    spawn_task_awaiting_deps as spawn_task_awaiting_deps_impl,
};
pub use spawn::{effective_turn_timeout, prompt_requires_plan, resolve_canonical_project};
pub use state::{
    RecentFailureTask, RoundResult, SchedulerAuthorityState, SchedulerOwner, SchedulerOwnerKind,
    TaskSchedulerState, TaskState, TaskSummary, TaskWorkflowSummary,
};
use store::{
    mark_terminal_once as mark_terminal_once_impl, mutate_and_persist as mutate_and_persist_impl,
    update_status as update_status_impl,
};
pub use store::{TaskStore, TaskSummaryFilter, TaskSummaryPageCursor, TerminalTransition};
pub use types::{
    TaskFailureKind, TaskId, TaskKind, TaskPhase, TaskStatus, TaskTerminalClassification,
    TaskTerminalFailure, TaskTerminalInfo, TaskTerminalOutcome, ROUND_BUDGET_EXHAUSTED_REASON,
};

fn record_task_runner_usage() {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskRunner,
    );
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn harness_core::agent::CodeAgent>,
    reviewer: Option<Arc<dyn harness_core::agent::CodeAgent>>,
    server_config: Arc<harness_core::config::HarnessConfig>,
    skills: Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: impl Into<crate::task_executor::SharedTurnInterceptors>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    allowed_project_roots: Vec<std::path::PathBuf>,
) -> TaskId {
    record_task_runner_usage();
    spawn_task_impl(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors,
        req,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        workflow_runtime_store,
        allowed_project_roots,
    )
    .await
}

pub async fn register_pending_task(store: Arc<TaskStore>, req: &CreateTaskRequest) -> TaskId {
    record_task_runner_usage();
    register_pending_task_impl(store, req).await
}

pub async fn spawn_preregistered_task(
    task_id: TaskId,
    store: Arc<TaskStore>,
    agent: Arc<dyn harness_core::agent::CodeAgent>,
    reviewer: Option<Arc<dyn harness_core::agent::CodeAgent>>,
    server_config: Arc<harness_core::config::HarnessConfig>,
    skills: Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: impl Into<crate::task_executor::SharedTurnInterceptors>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    allowed_project_roots: Vec<std::path::PathBuf>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    record_task_runner_usage();
    spawn_preregistered_task_impl(
        task_id,
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors,
        req,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        workflow_runtime_store,
        allowed_project_roots,
        group_permit,
    )
    .await;
}

pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    record_task_runner_usage();
    spawn_task_awaiting_deps_impl(store, req).await
}

pub async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
    record_task_runner_usage();
    check_awaiting_deps_impl(store).await
}

pub async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) -> anyhow::Result<()> {
    record_task_runner_usage();
    update_status_impl(store, task_id, status, turn).await
}

pub async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    record_task_runner_usage();
    mutate_and_persist_impl(store, id, f).await
}

pub async fn mark_terminal_once(
    store: &TaskStore,
    id: &TaskId,
    outcome: TaskTerminalOutcome,
) -> anyhow::Result<TerminalTransition> {
    record_task_runner_usage();
    mark_terminal_once_impl(store, id, outcome).await
}

impl TaskStore {
    /// Return the most recent `limit` failed tasks, newest first.
    pub async fn list_recent_failed(&self, limit: i64) -> anyhow::Result<Vec<RecentFailureTask>> {
        record_task_runner_usage();
        self.db.list_recent_failed(limit).await
    }

    pub async fn prune_terminal_tasks_before(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        batch_size: u32,
    ) -> anyhow::Result<crate::task_db::TaskRetentionPruneSummary> {
        record_task_runner_usage();
        let summary = self
            .db
            .prune_terminal_tasks_before(cutoff, batch_size)
            .await?;
        for task_id in &summary.pruned_task_ids {
            let task_id = TaskId::from_str(task_id);
            if self
                .cache
                .get(&task_id)
                .is_some_and(|entry| entry.status.is_terminal())
            {
                self.cache.remove(&task_id);
            }
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod usage_probe_tests {
    use super::*;

    fn task_runner_count() -> u64 {
        harness_core::usage_probe::snapshot()
            .into_iter()
            .find(|entry| entry.surface == "task_runner")
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn task_store_cache_public_entries_record_task_runner_usage() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks-store")).await?;
        let task_id = TaskId::from_str("probe-task");
        let mut task = TaskState::new(task_id.clone());
        task.project_root = Some(dir.path().to_path_buf());
        task.repo = Some("owner/repo".to_string());
        task.external_id = Some("issue-1".to_string());
        store.insert(&task).await;
        let mut child = TaskState::new(TaskId::from_str("probe-child"));
        child.parent_id = Some(task_id.clone());
        child.project_root = Some(dir.path().to_path_buf());
        store.insert(&child).await;
        let before = task_runner_count();

        assert!(store.get(&task_id).is_some());
        assert_eq!(store.count(), 2);
        assert_eq!(store.list_all().len(), 2);
        assert!(store
            .list_all_statuses_with_terminal()
            .await?
            .contains_key(&task_id));
        assert!(store
            .list_latest_external_statuses_by_project_and_repo(
                &dir.path().to_string_lossy(),
                Some("owner/repo"),
            )
            .await?
            .contains_key("issue-1"));
        assert_eq!(store.list_siblings(dir.path(), &task_id).len(), 1);
        let since = chrono::Utc::now() - chrono::TimeDelta::days(1);
        assert_eq!(store.count_done_since(since).await, 0);
        assert!(store.done_per_project_hour_since(since).await.is_empty());
        assert_eq!(store.list_children(&task_id).len(), 1);
        assert!(store.list_recent_failed(5).await?.is_empty());
        assert!(!store.abort_task(&task_id));

        assert!(task_runner_count() >= before + 11);
        Ok(())
    }

    #[tokio::test]
    async fn prune_terminal_tasks_before_removes_terminal_cache_entry() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::from_str("retention-cache-terminal");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::Done;
        task.scheduler.mark_terminal(&TaskStatus::Done);
        store.insert(&task).await;
        store
            .db
            .overwrite_updated_at_for_test(
                task_id.as_str(),
                chrono::Utc::now() - chrono::Duration::days(45),
            )
            .await?;

        let summary = store
            .prune_terminal_tasks_before(chrono::Utc::now() - chrono::Duration::days(30), 10)
            .await?;

        assert_eq!(summary.tasks_deleted, 1);
        assert!(store.get(&task_id).is_none());
        assert!(store.db.get(task_id.as_str()).await?.is_none());
        Ok(())
    }
}
