use super::super::request::{summarize_request_description, CreateTaskRequest};
use super::super::state::TaskState;
use super::super::store::TaskStore;
use super::super::types::{TaskId, TaskStatus};
use super::classify_task_kind;

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
/// Called before the new task is inserted into the store.
pub(crate) fn detect_cycle(store: &TaskStore, new_id: &TaskId, depends_on: &[TaskId]) -> bool {
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
pub(crate) async fn spawn_task_awaiting_deps(
    store: std::sync::Arc<TaskStore>,
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
    state.task_kind = classify_task_kind(&req);
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.phase = state.task_kind.default_phase();
    state.description = summarize_request_description(&req);
    state.request_settings = Some(super::super::request::PersistedRequestSettings::from_req(
        &req,
    ));
    // Persist the caller's resolved project root so that duplicate detection
    // (which keys on project_root + external_id) and the dep-watcher's project
    // path resolution both work correctly for waiting tasks.
    state.project_root = req.project.clone();
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
        state.scheduler = crate::task_runner::TaskSchedulerState::awaiting_dependencies();
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
/// Returns the IDs of tasks that were transitioned to Pending.
///
/// Async because dependency status checks fall back to the database for
/// terminal tasks (Done/Failed) that were evicted from the startup cache.
///
/// Returns `(ready_ids, newly_failed_ids)`. Both sets must be persisted by
/// the caller so that status transitions survive a process restart.
pub(crate) async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
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
                entry.scheduler.mark_terminal(&TaskStatus::Failed);
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
                entry.scheduler.clear_to_queued();
                actually_ready.push(task_id.clone());
            }
        }
    }

    (actually_ready, newly_failed)
}
