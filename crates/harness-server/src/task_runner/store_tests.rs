use super::super::state::TaskState;
use super::*;

#[test]
fn collect_recovered_pr_candidates_filters_invalid_urls() {
    let mut valid = TaskState::new(harness_core::types::TaskId("valid".to_string()));
    valid.pr_url = Some("https://github.com/acme/myrepo/pull/42".to_string());

    let mut invalid = TaskState::new(harness_core::types::TaskId("invalid".to_string()));
    invalid.pr_url = Some("not-a-pr-url".to_string());

    let mut inflight = TaskState::new(harness_core::types::TaskId("inflight".to_string()));
    inflight.status = TaskStatus::Implementing;
    inflight.pr_url = Some("https://github.com/acme/myrepo/pull/7".to_string());

    let pending_without_pr = TaskState::new(harness_core::types::TaskId("no-pr".to_string()));
    let tasks = [valid, invalid, inflight, pending_without_pr];
    let candidates: Vec<_> = tasks.iter().filter_map(recovered_pr_candidate).collect();
    assert_eq!(
        candidates,
        vec![RecoveredPrCandidate {
            task_id: harness_core::types::TaskId("valid".to_string()),
            pr_url: "https://github.com/acme/myrepo/pull/42".to_string(),
        }]
    );
}

#[tokio::test]
async fn list_active_summaries_filters_terminal_history_and_cache_overrides() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let active_id = harness_core::types::TaskId("active".to_string());
    let active = TaskState::new(active_id.clone());
    store.insert(&active).await;

    let terminal_id = harness_core::types::TaskId("terminal".to_string());
    let mut terminal = TaskState::new(terminal_id.clone());
    terminal.status = TaskStatus::Done;
    store.insert(&terminal).await;

    let stale_db_id = harness_core::types::TaskId("stale-db-active".to_string());
    let stale_db_active = TaskState::new(stale_db_id.clone());
    store.insert(&stale_db_active).await;
    let mut terminal_cache = stale_db_active;
    terminal_cache.status = TaskStatus::Failed;
    terminal_cache.reconcile_scheduler_with_status();
    store.cache.insert(stale_db_id.clone(), terminal_cache);

    let ids: std::collections::HashSet<_> = store
        .list_active_summaries()
        .await?
        .into_iter()
        .map(|summary| summary.id)
        .collect();

    assert!(ids.contains(&active_id));
    assert!(!ids.contains(&terminal_id));
    assert!(!ids.contains(&stale_db_id));
    Ok(())
}

fn pending_task(id: &str) -> TaskState {
    let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
    task.status = TaskStatus::Pending;
    task
}

#[tokio::test]
async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let id = harness_core::types::TaskId("stream-test".to_string());

    // No stream registered yet.
    assert!(
        store.subscribe_task_stream(&id).is_none(),
        "subscribe before register should return None"
    );

    store.register_task_stream(&id);
    let mut rx = store
        .subscribe_task_stream(&id)
        .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

    store.publish_stream_item(
        &id,
        harness_core::agent::StreamItem::MessageDelta {
            text: "hello\n".into(),
        },
    );
    store.publish_stream_item(&id, harness_core::agent::StreamItem::Done);

    let item1 = rx.recv().await?;
    let item2 = rx.recv().await?;
    assert!(matches!(
        item1,
        harness_core::agent::StreamItem::MessageDelta { .. }
    ));
    assert!(matches!(item2, harness_core::agent::StreamItem::Done));

    // After close_task_stream the channel sender is dropped.
    store.close_task_stream(&id);
    assert!(
        store.subscribe_task_stream(&id).is_none(),
        "subscribe after close should return None"
    );
    Ok(())
}

#[tokio::test]
async fn task_stream_backpressure_drops_oldest_on_lag() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let id = harness_core::types::TaskId("backpressure-test".to_string());

    store.register_task_stream(&id);
    let mut rx = store
        .subscribe_task_stream(&id)
        .ok_or_else(|| anyhow::anyhow!("subscribe should succeed after register"))?;

    // Publish more items than TASK_STREAM_CAPACITY to trigger lag.
    for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
        store.publish_stream_item(
            &id,
            harness_core::agent::StreamItem::MessageDelta {
                text: format!("line {i}\n"),
            },
        );
    }

    // Receiver should see RecvError::Lagged on overflow.
    let result = rx.recv().await;
    assert!(
        result.is_err(),
        "expected Lagged error after overflow, got: {:?}",
        result
    );
    Ok(())
}

#[tokio::test]
async fn list_children_returns_subtasks_for_parent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let parent_id = harness_core::types::TaskId("parent-task".to_string());
    let parent = TaskState::new(parent_id.clone());
    store.insert(&parent).await;

    let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
    child1.parent_id = Some(parent_id.clone());
    store.insert(&child1).await;

    let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
    child2.parent_id = Some(parent_id.clone());
    store.insert(&child2).await;

    // Unrelated task.
    store
        .insert(&TaskState::new(harness_core::types::TaskId(
            "other".to_string(),
        )))
        .await;

    let children = store.list_children(&parent_id);
    assert_eq!(children.len(), 2);
    assert!(children
        .iter()
        .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

    let no_children = store.list_children(&harness_core::types::TaskId("nonexistent".to_string()));
    assert!(no_children.is_empty());
    Ok(())
}

#[test]
fn test_task_state_new() {
    let id = super::TaskId::new();
    let state = TaskState::new(id);
    assert!(matches!(state.status, TaskStatus::Pending));
    assert_eq!(state.turn, 0);
    assert!(state.pr_url.is_none());
    assert!(state.project_root.is_none());
    assert!(state.issue.is_none());
    assert!(state.description.is_none());
}

#[tokio::test]
async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project = std::path::PathBuf::from("/repo/project");
    let other_project = std::path::PathBuf::from("/repo/other");

    let current_id = harness_core::types::TaskId("current".to_string());
    let mut current = TaskState::new(current_id.clone());
    current.project_root = Some(project.clone());
    current.status = TaskStatus::Implementing;
    store.insert(&current).await;

    // Sibling on same project in Implementing status.
    let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
    sibling1.project_root = Some(project.clone());
    sibling1.status = TaskStatus::Implementing;
    sibling1.issue = Some(77);
    sibling1.description = Some("fix unwrap in s3.rs".to_string());
    store.insert(&sibling1).await;

    // Sibling on same project in Pending status.
    let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
    sibling2.project_root = Some(project.clone());
    sibling2.status = TaskStatus::Pending;
    store.insert(&sibling2).await;

    // Task on a different project — must not appear.
    let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
    other.project_root = Some(other_project.clone());
    other.status = TaskStatus::Implementing;
    store.insert(&other).await;

    // Done task on same project — must not appear.
    let mut done = TaskState::new(harness_core::types::TaskId("done-task".to_string()));
    done.project_root = Some(project.clone());
    done.status = TaskStatus::Done;
    store.insert(&done).await;

    let siblings = store.list_siblings(&project, &current_id);
    let sibling_ids: Vec<&str> = siblings.iter().map(|s| s.id.0.as_str()).collect();
    assert_eq!(
        siblings.len(),
        2,
        "expected 2 siblings, got: {sibling_ids:?}"
    );
    assert!(
        siblings.iter().all(|s| s.id != current_id),
        "current task must be excluded"
    );
    assert!(siblings
        .iter()
        .all(|s| s.project_root.as_deref() == Some(project.as_path())));

    // One sibling on `other_project`.
    let other_project_siblings = store.list_siblings(&other_project, &current_id);
    assert_eq!(other_project_siblings.len(), 1);

    Ok(())
}

#[test]
fn task_status_semantics_are_centralized() {
    let cases = [
        (
            TaskStatus::Pending,
            false,
            false,
            false,
            false,
            false,
            false,
        ),
        (
            TaskStatus::AwaitingDeps,
            false,
            false,
            false,
            false,
            false,
            false,
        ),
        (TaskStatus::Triaging, false, true, true, false, false, false),
        (TaskStatus::Planning, false, true, true, false, false, false),
        (
            TaskStatus::Implementing,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::ReviewGenerating,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::ReviewWaiting,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::PlannerGenerating,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::PlannerWaiting,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::AgentReview,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (TaskStatus::Waiting, false, true, true, false, false, false),
        (
            TaskStatus::Reviewing,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (TaskStatus::Done, true, false, false, true, false, false),
        (TaskStatus::Failed, true, false, false, false, true, false),
        (
            TaskStatus::Cancelled,
            true,
            false,
            false,
            false,
            false,
            true,
        ),
    ];

    for (status, terminal, inflight, resumable, success, failure, cancelled) in cases {
        assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
        assert_eq!(status.is_inflight(), inflight, "{status:?} inflight");
        assert_eq!(
            status.is_resumable_after_restart(),
            resumable,
            "{status:?} resumable"
        );
        assert_eq!(status.is_success(), success, "{status:?} success");
        assert_eq!(status.is_failure(), failure, "{status:?} failure");
        assert_eq!(status.is_cancelled(), cancelled, "{status:?} cancelled");
    }

    assert_eq!(
        TaskStatus::terminal_statuses(),
        &["done", "failed", "cancelled"]
    );
    assert_eq!(
        TaskStatus::resumable_statuses(),
        &[
            "triaging",
            "planning",
            "implementing",
            "review_generating",
            "review_waiting",
            "planner_generating",
            "planner_waiting",
            "agent_review",
            "waiting",
            "reviewing",
        ]
    );
}

#[tokio::test]
async fn count_by_project_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    assert!(store.count_for_dashboard().await.by_project.is_empty());
    Ok(())
}

#[tokio::test]
async fn count_by_project_none_root_excluded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let mut task = TaskState::new(harness_core::types::TaskId("no-root".to_string()));
    task.status = TaskStatus::Done;
    // project_root stays None
    store.insert(&task).await;

    assert!(
        store.count_for_dashboard().await.by_project.is_empty(),
        "tasks with no project_root must not appear in per-project counts"
    );
    Ok(())
}

#[tokio::test]
async fn count_by_project_groups_correctly() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let root_a = std::path::PathBuf::from("/projects/alpha");
    let root_b = std::path::PathBuf::from("/projects/beta");

    for (id, root, status) in [
        ("a1", &root_a, TaskStatus::Done),
        ("a2", &root_a, TaskStatus::Done),
        ("a3", &root_a, TaskStatus::Failed),
        ("a4", &root_a, TaskStatus::Cancelled),
        ("b1", &root_b, TaskStatus::Done),
        ("b2", &root_b, TaskStatus::Failed),
        ("b3", &root_b, TaskStatus::Failed),
        ("b4", &root_b, TaskStatus::Cancelled),
    ] {
        let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
        task.status = status;
        task.project_root = Some(root.clone());
        store.insert(&task).await;
    }

    let counts = store.count_for_dashboard().await.by_project;
    let key_a = root_a.to_string_lossy().into_owned();
    let key_b = root_b.to_string_lossy().into_owned();

    assert!(counts.contains_key(&key_a), "alpha counts missing");
    assert_eq!(counts[&key_a].done, 2, "alpha done");
    assert_eq!(counts[&key_a].failed, 1, "alpha failed");

    assert!(counts.contains_key(&key_b), "beta counts missing");
    assert_eq!(counts[&key_b].done, 1, "beta done");
    assert_eq!(counts[&key_b].failed, 2, "beta failed");
    Ok(())
}

#[tokio::test]
async fn count_by_project_excludes_cancelled_from_failed_totals() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let root = std::path::PathBuf::from("/projects/alpha");
    for (id, status) in [
        ("done", TaskStatus::Done),
        ("failed", TaskStatus::Failed),
        ("cancelled", TaskStatus::Cancelled),
    ] {
        let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
        task.status = status;
        task.project_root = Some(root.clone());
        store.insert(&task).await;
    }

    let counts = store.count_for_dashboard().await;
    assert_eq!(counts.global_done, 1);
    assert_eq!(counts.global_failed, 1);
    let key = root.to_string_lossy().into_owned();
    assert_eq!(counts.by_project[&key].done, 1);
    assert_eq!(counts.by_project[&key].failed, 1);
    Ok(())
}

#[tokio::test]
async fn restore_status_preserve_staleness_mirrors_version_in_cache_and_db() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("restore-version".to_string());
    let task = TaskState::new(task_id.clone());
    store.insert(&task).await;

    store
        .restore_status_preserve_staleness(&task_id, TaskStatus::Failed)
        .await?;

    let cached = store
        .get(&task_id)
        .expect("task should remain cached after status restore");
    assert_eq!(cached.status, TaskStatus::Failed);
    assert_eq!(cached.version, 1);

    let persisted = store
        .db
        .get(task_id.as_str())
        .await?
        .expect("task should remain persisted after status restore");
    assert_eq!(persisted.status, TaskStatus::Failed);
    assert_eq!(persisted.version, 1);
    Ok(())
}

#[tokio::test]
async fn overwrite_external_id_auto_fix_mirrors_version_in_cache_and_db() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("auto-fix-version".to_string());
    let mut task = TaskState::new(task_id.clone());
    task.source = Some("auto-fix".to_string());
    task.external_id = Some("old-external-id".to_string());
    store.insert(&task).await;

    store
        .overwrite_external_id_auto_fix(&task_id, "new-external-id")
        .await?;

    let cached = store
        .get(&task_id)
        .expect("task should remain cached after external_id overwrite");
    assert_eq!(cached.external_id.as_deref(), Some("new-external-id"));
    assert_eq!(cached.version, 1);

    let persisted = store
        .db
        .get(task_id.as_str())
        .await?
        .expect("task should remain persisted after external_id overwrite");
    assert_eq!(persisted.external_id.as_deref(), Some("new-external-id"));
    assert_eq!(persisted.version, 1);
    Ok(())
}

#[tokio::test]
async fn mutate_and_persist_rolls_back_cache_on_optimistic_lock_conflict() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("mutate-conflict".to_string());
    let task = pending_task(task_id.as_str());
    store.insert(&task).await;

    let scheduler_json = serde_json::to_string(&crate::task_runner::TaskSchedulerState::queued())?;
    store
        .db
        .update_status_only(
            task_id.as_str(),
            TaskStatus::Failed.as_ref(),
            &scheduler_json,
            0,
        )
        .await?;

    let error = mutate_and_persist(&store, &task_id, |state| {
        state.status = TaskStatus::Done;
        state.turn = 7;
        state.error = Some("not persisted".to_string());
    })
    .await
    .expect_err("stale cache version should fail optimistic locking");

    assert!(format!("{error}").contains("optimistic-lock conflict"));
    let cached = store
        .get(&task_id)
        .expect("task should remain cached after rollback");
    assert_eq!(cached.status, TaskStatus::Pending);
    assert_eq!(cached.turn, 0);
    assert_eq!(cached.error, None);
    assert_eq!(cached.version, 0);

    let persisted = store
        .db
        .get(task_id.as_str())
        .await?
        .expect("task should remain persisted after conflict");
    assert_eq!(persisted.status, TaskStatus::Failed);
    assert_eq!(persisted.version, 1);
    Ok(())
}

#[tokio::test]
async fn mutate_and_persist_does_not_rollback_over_newer_cache_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = std::sync::Arc::new(TaskStore::open(&dir.path().join("tasks.db")).await?);
    let task_id = harness_core::types::TaskId("mutate-newer-cache".to_string());
    let task = pending_task(task_id.as_str());
    store.insert(&task).await;

    let scheduler_json = serde_json::to_string(&crate::task_runner::TaskSchedulerState::queued())?;
    store
        .db
        .update_status_only(
            task_id.as_str(),
            TaskStatus::Failed.as_ref(),
            &scheduler_json,
            0,
        )
        .await?;

    let persist_lock = store
        .persist_locks
        .entry(task_id.clone())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let guard = persist_lock.lock().await;

    let worker_store = store.clone();
    let worker_task_id = task_id.clone();
    let worker = tokio::spawn(async move {
        mutate_and_persist(worker_store.as_ref(), &worker_task_id, |state| {
            state.status = TaskStatus::Done;
            state.turn = 7;
            state.error = Some("failed mutation".to_string());
        })
        .await
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let mutated = store
            .get(&task_id)
            .is_some_and(|state| state.status == TaskStatus::Done && state.turn == 7);
        if mutated {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mutate_and_persist did not reach the cache mutation before persist"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    if let Some(mut entry) = store.cache.get_mut(&task_id) {
        entry.status = TaskStatus::AwaitingDeps;
        entry.turn = 9;
        entry.error = Some("newer cache state".to_string());
    }

    drop(guard);
    let error = worker
        .await
        .expect("mutate task should not panic")
        .expect_err("stale cache version should fail optimistic locking");

    assert!(format!("{error}").contains("optimistic-lock conflict"));
    let cached = store
        .get(&task_id)
        .expect("task should remain cached after failed persist");
    assert_eq!(cached.status, TaskStatus::AwaitingDeps);
    assert_eq!(cached.turn, 9);
    assert_eq!(cached.error.as_deref(), Some("newer cache state"));
    Ok(())
}

// --- rate-limit circuit-breaker tests ---

#[tokio::test]
async fn wait_for_rate_limit_no_op_when_none() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        store.wait_for_rate_limit(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("wait_for_rate_limit must return immediately when no limit is active")
    })?;
    Ok(())
}

#[tokio::test]
async fn rate_limit_cleared_after_deadline() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    store
        .set_rate_limit(std::time::Duration::from_millis(50))
        .await;
    store.wait_for_rate_limit().await;
    // After the limit expires, a subsequent call must return immediately.
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        store.wait_for_rate_limit(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("rate limit must be cleared after its deadline passes"))?;
    Ok(())
}

/// Regression for issue #1046: once a task is Cancelled, a late
/// `update_status` from an execution path that started before
/// cancellation must not resurrect it back to a non-terminal status
/// nor overwrite it with a different terminal status.
#[tokio::test]
async fn update_status_does_not_overwrite_cancelled_terminal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("cancelled-task".to_string());
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::Cancelled;
    task.scheduler.mark_terminal(&TaskStatus::Cancelled);
    store.insert(&task).await;

    // A late execution path tries to mark the task Implementing.
    update_status(&store, &task_id, TaskStatus::Implementing, 0).await?;
    let after = store.get(&task_id).expect("task in cache");
    assert_eq!(after.status, TaskStatus::Cancelled, "must remain Cancelled");

    // Reviewing must also be refused.
    update_status(&store, &task_id, TaskStatus::Reviewing, 1).await?;
    let after = store.get(&task_id).expect("task in cache");
    assert_eq!(after.status, TaskStatus::Cancelled);

    // Cross-terminal overwrites (Cancelled -> Done) must also be refused.
    update_status(&store, &task_id, TaskStatus::Done, 2).await?;
    let after = store.get(&task_id).expect("task in cache");
    assert_eq!(after.status, TaskStatus::Cancelled);
    Ok(())
}

/// Idempotent: writing the same terminal status is a no-op (no error).
#[tokio::test]
async fn update_status_idempotent_on_same_terminal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("done-task".to_string());
    let mut task = TaskState::new(task_id.clone());
    task.status = TaskStatus::Done;
    task.scheduler.mark_terminal(&TaskStatus::Done);
    store.insert(&task).await;

    update_status(&store, &task_id, TaskStatus::Done, 5).await?;
    let after = store.get(&task_id).expect("task in cache");
    assert_eq!(after.status, TaskStatus::Done);
    Ok(())
}
