use super::*;
use harness_core::agent::StreamItem;
use std::path::PathBuf;

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
        StreamItem::MessageDelta {
            text: "hello\n".into(),
        },
    );
    store.publish_stream_item(&id, StreamItem::Done);

    let item1 = rx.recv().await?;
    let item2 = rx.recv().await?;
    assert!(matches!(item1, StreamItem::MessageDelta { .. }));
    assert!(matches!(item2, StreamItem::Done));

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
            StreamItem::MessageDelta {
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
    let parent = super::super::types::TaskState::new(parent_id.clone());
    store.insert(&parent).await;

    let mut child1 =
        super::super::types::TaskState::new(harness_core::types::TaskId("child-1".to_string()));
    child1.parent_id = Some(parent_id.clone());
    store.insert(&child1).await;

    let mut child2 =
        super::super::types::TaskState::new(harness_core::types::TaskId("child-2".to_string()));
    child2.parent_id = Some(parent_id.clone());
    store.insert(&child2).await;

    // Unrelated task.
    store
        .insert(&super::super::types::TaskState::new(
            harness_core::types::TaskId("other".to_string()),
        ))
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

#[tokio::test]
async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project = PathBuf::from("/repo/project");
    let other_project = PathBuf::from("/repo/other");

    let current_id = harness_core::types::TaskId("current".to_string());
    let mut current = super::super::types::TaskState::new(current_id.clone());
    current.project_root = Some(project.clone());
    current.status = super::super::types::TaskStatus::Implementing;
    store.insert(&current).await;

    // Sibling on same project in Implementing status.
    let mut sibling1 =
        super::super::types::TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
    sibling1.project_root = Some(project.clone());
    sibling1.status = super::super::types::TaskStatus::Implementing;
    sibling1.issue = Some(77);
    sibling1.description = Some("fix unwrap in s3.rs".to_string());
    store.insert(&sibling1).await;

    // Sibling on same project in Pending status.
    let mut sibling2 =
        super::super::types::TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
    sibling2.project_root = Some(project.clone());
    sibling2.status = super::super::types::TaskStatus::Pending;
    store.insert(&sibling2).await;

    // Task on a different project — must not appear.
    let mut other = super::super::types::TaskState::new(harness_core::types::TaskId(
        "other-project".to_string(),
    ));
    other.project_root = Some(other_project.clone());
    other.status = super::super::types::TaskStatus::Implementing;
    store.insert(&other).await;

    // Done task on same project — must not appear.
    let mut done =
        super::super::types::TaskState::new(harness_core::types::TaskId("done-task".to_string()));
    done.project_root = Some(project.clone());
    done.status = super::super::types::TaskStatus::Done;
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

    let mut task =
        super::super::types::TaskState::new(harness_core::types::TaskId("no-root".to_string()));
    task.status = super::super::types::TaskStatus::Done;
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
        ("a1", &root_a, super::super::types::TaskStatus::Done),
        ("a2", &root_a, super::super::types::TaskStatus::Done),
        ("a3", &root_a, super::super::types::TaskStatus::Failed),
        ("a4", &root_a, super::super::types::TaskStatus::Cancelled),
        ("b1", &root_b, super::super::types::TaskStatus::Done),
        ("b2", &root_b, super::super::types::TaskStatus::Failed),
        ("b3", &root_b, super::super::types::TaskStatus::Failed),
        ("b4", &root_b, super::super::types::TaskStatus::Cancelled),
    ] {
        let mut task =
            super::super::types::TaskState::new(harness_core::types::TaskId(id.to_string()));
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
        ("done", super::super::types::TaskStatus::Done),
        ("failed", super::super::types::TaskStatus::Failed),
        ("cancelled", super::super::types::TaskStatus::Cancelled),
    ] {
        let mut task =
            super::super::types::TaskState::new(harness_core::types::TaskId(id.to_string()));
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

// --- parse_pr_url tests (helper private to recovery module, tested via recovery::tests) ---
// parse_pr_url lives in recovery.rs; its tests live there too.
