//! TaskService — task lifecycle, stream subscriptions, and task-project association.

use crate::task_runner::{TaskId, TaskState, TaskStore};
use async_trait::async_trait;
use harness_core::StreamItem;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Trait interface for task lifecycle operations.
///
/// Implementations may use the SQLite-backed [`DefaultTaskService`] or an
/// in-memory mock for unit tests.
#[async_trait]
pub trait TaskService: Send + Sync {
    /// Retrieve a task snapshot by ID.
    fn get(&self, id: &TaskId) -> Option<TaskState>;

    /// List all known tasks.
    fn list(&self) -> Vec<TaskState>;

    /// List tasks that are subtasks of the given parent.
    fn list_children(&self, parent_id: &TaskId) -> Vec<TaskState>;

    /// Return the PR URL of the most recently completed task, if any.
    async fn latest_done_pr_url(&self) -> Option<String>;

    /// Total number of tracked tasks.
    fn count(&self) -> usize;

    /// Subscribe to the real-time stream of a running task.
    /// Returns `None` when no stream channel is registered for the task.
    fn subscribe_stream(&self, id: &TaskId) -> Option<broadcast::Receiver<StreamItem>>;
}

/// Production implementation backed by [`TaskStore`].
pub struct DefaultTaskService {
    store: Arc<TaskStore>,
}

impl DefaultTaskService {
    pub fn new(store: Arc<TaskStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }

    /// Expose the underlying store for callers that need direct access
    /// (e.g. `spawn_task` and mutation helpers).
    pub fn store(&self) -> Arc<TaskStore> {
        self.store.clone()
    }
}

#[async_trait]
impl TaskService for DefaultTaskService {
    fn get(&self, id: &TaskId) -> Option<TaskState> {
        self.store.get(id)
    }

    fn list(&self) -> Vec<TaskState> {
        self.store.list_all()
    }

    fn list_children(&self, parent_id: &TaskId) -> Vec<TaskState> {
        self.store.list_children(parent_id)
    }

    async fn latest_done_pr_url(&self) -> Option<String> {
        self.store.latest_done_pr_url().await
    }

    fn count(&self) -> usize {
        self.store.count()
    }

    fn subscribe_stream(&self, id: &TaskId) -> Option<broadcast::Receiver<StreamItem>> {
        self.store.subscribe_task_stream(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskStatus;

    #[tokio::test]
    async fn default_task_service_empty_on_open() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let svc = DefaultTaskService::new(store);

        assert_eq!(svc.count(), 0);
        assert!(svc.list().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn default_task_service_get_after_insert() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let svc = DefaultTaskService::new(store.clone());

        let id = TaskId("test-task".to_string());
        let mut state = crate::task_runner::TaskState {
            id: id.clone(),
            status: TaskStatus::Pending,
            turn: 0,
            pr_url: None,
            rounds: vec![],
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            subtask_ids: vec![],
            project_id: None,
            project_root: None,
            issue: None,
            description: None,
        };
        state.source = Some("github".to_string());
        store.insert(&state).await;

        let fetched = svc.get(&id).expect("task should exist");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.source.as_deref(), Some("github"));
        Ok(())
    }

    #[tokio::test]
    async fn default_task_service_list_children() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let svc = DefaultTaskService::new(store.clone());

        let parent_id = TaskId("parent".to_string());
        let child_id = TaskId("child".to_string());

        let parent_state = crate::task_runner::TaskState {
            id: parent_id.clone(),
            status: TaskStatus::Pending,
            turn: 0,
            pr_url: None,
            rounds: vec![],
            error: None,
            source: None,
            external_id: None,
            parent_id: None,
            subtask_ids: vec![child_id.clone()],
            project_id: None,
            project_root: None,
            issue: None,
            description: None,
        };
        store.insert(&parent_state).await;

        let child_state = crate::task_runner::TaskState {
            id: child_id.clone(),
            status: TaskStatus::Pending,
            turn: 0,
            pr_url: None,
            rounds: vec![],
            error: None,
            source: None,
            external_id: None,
            parent_id: Some(parent_id.clone()),
            subtask_ids: vec![],
            project_id: None,
            project_root: None,
            issue: None,
            description: None,
        };
        store.insert(&child_state).await;

        let children = svc.list_children(&parent_id);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child_id);
        Ok(())
    }

    #[tokio::test]
    async fn default_task_service_subscribe_stream() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let svc = DefaultTaskService::new(store.clone());

        let id = TaskId("stream-task".to_string());

        // No stream registered yet.
        assert!(svc.subscribe_stream(&id).is_none());

        store.register_task_stream(&id);
        assert!(svc.subscribe_stream(&id).is_some());

        store.close_task_stream(&id);
        assert!(svc.subscribe_stream(&id).is_none());

        Ok(())
    }
}
