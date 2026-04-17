use super::helpers::summarize_request_description;
use super::store::TaskStore;
use super::types::{CreateTaskRequest, PersistedRequestSettings, TaskId, TaskState, TaskStatus};
use std::sync::Arc;

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
/// Called before the new task is inserted into the store.
fn detect_cycle(store: &TaskStore, new_id: &TaskId, depends_on: &[TaskId]) -> bool {
    let mut stack: Vec<TaskId> = depends_on.to_vec();
    let mut visited: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    while let Some(dep_id) = stack.pop() {
        if dep_id == *new_id {
            return true;
        }
        if visited.contains(&dep_id) {
            continue;
        }
        visited.insert(dep_id.clone());
        if let Some(dep_state) = store.get(&dep_id) {
            for transitive in &dep_state.depends_on {
                stack.push(transitive.clone());
            }
        }
    }
    false
}

/// Create a task that waits for its dependencies before starting.
///
/// If all deps are already Done, registers as Pending immediately.
/// If deps are unresolved, registers as AwaitingDeps.
/// Returns Err if a circular dependency is detected.
pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    let depends_on = req.depends_on.clone();
    let task_id = TaskId::new();

    if detect_cycle(&store, &task_id, &depends_on) {
        anyhow::bail!("circular dependency detected for task {}", task_id.0);
    }

    let mut all_done = true;
    for dep_id in &depends_on {
        if !matches!(store.dep_status(dep_id).await, Some(TaskStatus::Done)) {
            all_done = false;
            break;
        }
    }

    let mut state = TaskState::new(task_id.clone());
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.description = summarize_request_description(&req);
    state.request_settings = Some(PersistedRequestSettings::from_req(&req));
    // Persist the caller's resolved project root so that duplicate detection
    // (which keys on project_root + external_id) and the dep-watcher's project
    // path resolution both work correctly for waiting tasks.
    state.project_root = req.project.clone();
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
///
/// Async because dependency status checks fall back to the database for
/// terminal tasks (Done/Failed) that were evicted from the startup cache.
///
/// Returns `(ready_ids, newly_failed_ids)`. Both sets must be persisted by
/// the caller so that status transitions survive a process restart.
pub async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
    let mut ready = Vec::new();
    // (task_id, dep_id, dep_terminal_status_label) — label is "failed" or "cancelled".
    let mut failed_deps: Vec<(TaskId, TaskId, &'static str)> = Vec::new();

    // Snapshot AwaitingDeps tasks from cache before async work.
    let awaiting: Vec<(TaskId, Vec<TaskId>)> = store
        .cache
        .iter()
        .filter_map(|e| {
            let task = e.value();
            if matches!(task.status, TaskStatus::AwaitingDeps) {
                Some((task.id.clone(), task.depends_on.clone()))
            } else {
                None
            }
        })
        .collect();

    for (task_id, depends_on) in awaiting {
        // Single pass: one DB lookup per dep (cache-first, then lightweight
        // `SELECT status` — no `rounds` JSON decode).
        let mut failed_dep_id: Option<(TaskId, &'static str)> = None;
        let mut all_done = true;
        for dep_id in &depends_on {
            match store.dep_status(dep_id).await {
                Some(TaskStatus::Failed) => {
                    failed_dep_id = Some((dep_id.clone(), "failed"));
                    break;
                }
                Some(TaskStatus::Cancelled) => {
                    // A cancelled dependency hard-fails its dependents. When a
                    // task is re-queued it always receives a new TaskId, so the
                    // original (now-cancelled) TaskId in `depends_on` can never
                    // transition to Done. Leaving the dependent in AwaitingDeps
                    // would block it indefinitely; failing it immediately lets
                    // the caller re-submit with correct dependency IDs.
                    failed_dep_id = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                Some(TaskStatus::Done) => {}
                _ => {
                    // Pending, Implementing, or unknown — not ready yet.
                    // Keep scanning in case a later dep is Failed.
                    all_done = false;
                }
            }
        }
        if let Some((fd, label)) = failed_dep_id {
            failed_deps.push((task_id, fd, label));
            continue;
        }
        if all_done {
            ready.push(task_id);
        }
    }

    let mut newly_failed: Vec<TaskId> = Vec::new();
    for (task_id, failed_dep_id, label) in &failed_deps {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            // Only overwrite if the task is still AwaitingDeps — a concurrent
            // cancel or status transition must not be clobbered.
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Failed;
                entry.error = Some(format!("dependency {} {}", failed_dep_id.0, label));
                newly_failed.push(task_id.clone());
            }
        }
    }
    // Only return IDs where the transition actually happened. If a task was
    // cancelled or otherwise moved out of AwaitingDeps between the snapshot
    // and this guard, it must NOT be included — the dep-watcher would otherwise
    // launch an already-terminal or concurrently-cancelled task.
    let mut actually_ready: Vec<TaskId> = Vec::with_capacity(ready.len());
    for task_id in &ready {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Pending;
                actually_ready.push(task_id.clone());
            }
        }
    }

    (actually_ready, newly_failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> TaskId {
        harness_core::types::TaskId(s.to_string())
    }

    #[tokio::test]
    async fn spawn_awaiting_all_deps_done_creates_pending() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let req = CreateTaskRequest {
            prompt: Some("task with done dep".into()),
            depends_on: vec![dep_id],
            ..Default::default()
        };

        let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
        let state = store.get(&task_id).expect("task should be in store");
        assert!(
            matches!(state.status, TaskStatus::Pending),
            "expected Pending when all deps Done, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_unresolved_dep_creates_awaiting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-pending");
        let dep = TaskState::new(dep_id.clone());
        store.insert(&dep).await;

        let req = CreateTaskRequest {
            prompt: Some("task with pending dep".into()),
            depends_on: vec![dep_id],
            ..Default::default()
        };

        let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
        let state = store.get(&task_id).expect("task should be in store");
        assert!(
            matches!(state.status, TaskStatus::AwaitingDeps),
            "expected AwaitingDeps when dep not Done, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_detects_direct_cycle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let new_id = tid("new-task");
        let a_id = tid("task-a");

        let mut task_a = TaskState::new(a_id.clone());
        task_a.depends_on = vec![new_id.clone()];
        store.insert(&task_a).await;

        assert!(
            detect_cycle(&store, &new_id, &[a_id]),
            "expected direct cycle to be detected"
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_detects_transitive_cycle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let new_id = tid("new-task");
        let a_id = tid("task-a");
        let b_id = tid("task-b");

        let mut task_b = TaskState::new(b_id.clone());
        task_b.depends_on = vec![new_id.clone()];
        store.insert(&task_b).await;

        let mut task_a = TaskState::new(a_id.clone());
        task_a.depends_on = vec![b_id];
        store.insert(&task_a).await;

        assert!(
            detect_cycle(&store, &new_id, &[a_id]),
            "expected transitive cycle to be detected"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_no_tasks_returns_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "expected no ready tasks");
        assert!(failed.is_empty(), "expected no failed tasks");
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_ready_transitions_to_pending() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.contains(&task_id), "expected task in ready_ids");
        assert!(failed.is_empty(), "expected no failed tasks");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Pending),
            "expected Pending after transition, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_failed_dep_transitions_to_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-failed");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Failed;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "expected no ready tasks");
        assert!(failed.contains(&task_id), "expected task in failed_ids");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed after dep failure, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_partial_deps_stays_awaiting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_done_id = tid("dep-done");
        let mut dep_done = TaskState::new(dep_done_id.clone());
        dep_done.status = TaskStatus::Done;
        store.insert(&dep_done).await;

        let dep_pending_id = tid("dep-pending");
        let dep_pending = TaskState::new(dep_pending_id.clone());
        store.insert(&dep_pending).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_done_id, dep_pending_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(!ready.contains(&task_id), "task must not be in ready_ids");
        assert!(!failed.contains(&task_id), "task must not be in failed_ids");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::AwaitingDeps),
            "expected AwaitingDeps with partial deps, got {:?}",
            state.status
        );
        Ok(())
    }

    /// A cancelled dependency hard-fails its dependents. Re-queued tasks always
    /// get new TaskIds, so the old cancelled ID would never resolve — leaving the
    /// dependent in AwaitingDeps would block it indefinitely.
    #[tokio::test]
    async fn check_awaiting_cancelled_dep_hard_fails_dependent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-cancelled");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Cancelled;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id.clone()];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "cancelled dep must not unblock dependent");
        assert!(
            failed.contains(&task_id),
            "cancelled dep must hard-fail its dependent"
        );

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "dependent must be Failed when dep is Cancelled, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_concurrent_cancel_excluded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        // Simulate concurrent cancel: flip status before check_awaiting_deps runs.
        if let Some(mut entry) = store.cache.get_mut(&task_id) {
            entry.status = TaskStatus::Cancelled;
        }

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(
            !ready.contains(&task_id),
            "cancelled task must not be in ready_ids"
        );
        assert!(
            !failed.contains(&task_id),
            "cancelled task must not be in failed_ids"
        );
        Ok(())
    }
}
