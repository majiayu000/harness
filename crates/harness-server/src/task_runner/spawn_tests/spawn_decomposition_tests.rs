use super::super::*;
use super::tid;

#[test]
fn non_decomposable_source_list_includes_periodic_and_sprint() {
    assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
    assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
    assert!(!is_non_decomposable_prompt_source(Some("github")));
    assert!(!is_non_decomposable_prompt_source(None));
}

// --- dependency scheduling tests ---

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
    let dep = TaskState::new(dep_id.clone()); // Pending by default
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

    // Chain: new → A → new  (A's depends_on contains new_id)
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

    // Chain: new → A → B → new
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
    let dep_pending = TaskState::new(dep_pending_id.clone()); // Pending by default
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
