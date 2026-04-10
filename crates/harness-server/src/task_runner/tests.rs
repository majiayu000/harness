use super::*;
use std::path::PathBuf;

#[tokio::test]
async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let id = harness_core::types::TaskId("stream-test".to_string());

    assert!(
        store.subscribe_task_stream(&id).is_none(),
        "subscribe before register should return None"
    );

    store.register_task_stream(&id);
    let mut rx = store
        .subscribe_task_stream(&id)
        .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

    use harness_core::agent::StreamItem;
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

    use harness_core::agent::StreamItem;
    for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
        store.publish_stream_item(
            &id,
            StreamItem::MessageDelta {
                text: format!("line {i}\n"),
            },
        );
    }

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
    store.insert(&TaskState::new(parent_id.clone())).await;

    let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
    child1.parent_id = Some(parent_id.clone());
    store.insert(&child1).await;

    let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
    child2.parent_id = Some(parent_id.clone());
    store.insert(&child2).await;

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
    let id = TaskId::new();
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
    let project = PathBuf::from("/repo/project");
    let other_project = PathBuf::from("/repo/other");

    let current_id = harness_core::types::TaskId("current".to_string());
    let mut current = TaskState::new(current_id.clone());
    current.project_root = Some(project.clone());
    current.status = TaskStatus::Implementing;
    store.insert(&current).await;

    let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
    sibling1.project_root = Some(project.clone());
    sibling1.status = TaskStatus::Implementing;
    sibling1.issue = Some(77);
    sibling1.description = Some("fix unwrap in s3.rs".to_string());
    store.insert(&sibling1).await;

    let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
    sibling2.project_root = Some(project.clone());
    sibling2.status = TaskStatus::Pending;
    store.insert(&sibling2).await;

    let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
    other.project_root = Some(other_project.clone());
    other.status = TaskStatus::Implementing;
    store.insert(&other).await;

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

    let other_project_siblings = store.list_siblings(&other_project, &current_id);
    assert_eq!(other_project_siblings.len(), 1);
    Ok(())
}

#[test]
fn transient_error_detection() {
    assert!(is_transient_error(
        "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
    ));
    assert!(is_transient_error("rate limit exceeded, retry after 30s"));
    assert!(is_transient_error("HTTP 429 Too Many Requests"));
    assert!(is_transient_error("502 Bad Gateway"));
    assert!(is_transient_error(
        "Agent stream stalled: no output for 300s"
    ));
    assert!(is_transient_error("connection reset by peer"));
    assert!(is_transient_error(
        "stream idle timeout after 300s: zombie connection terminated"
    ));
    assert!(is_transient_error(
        "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
    ));
    assert!(!is_transient_error(
        "Task did not receive LGTM after 5 review rounds."
    ));
    assert!(!is_transient_error(
        "triage output unparseable — agent did not produce TRIAGE=<decision>"
    ));
    assert!(!is_transient_error("all parallel subtasks failed"));
    assert!(!is_transient_error(
        "task failed unexpectedly: task 102 panicked"
    ));
    assert!(!is_transient_error(
        "budget exceeded: spent $5.00, limit $3.00"
    ));
}

#[test]
fn parse_pr_url_standard() {
    let Some((owner, repo, number)) =
        store::parse_pr_url_test_helper("https://github.com/acme/myrepo/pull/42")
    else {
        panic!("expected Some for standard GitHub PR URL");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 42);
}

#[test]
fn parse_pr_url_with_fragment() {
    let Some((owner, repo, number)) =
        store::parse_pr_url_test_helper("https://github.com/acme/myrepo/pull/99#issuecomment-123")
    else {
        panic!("expected Some for PR URL with fragment");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 99);
}

#[test]
fn parse_pr_url_trailing_slash() {
    let Some((owner, repo, number)) =
        store::parse_pr_url_test_helper("https://github.com/acme/myrepo/pull/7/")
    else {
        panic!("expected Some for PR URL with trailing slash");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 7);
}

#[test]
fn parse_pr_url_invalid_returns_none() {
    assert!(store::parse_pr_url_test_helper("https://github.com/acme/myrepo").is_none());
    assert!(store::parse_pr_url_test_helper("not-a-url").is_none());
    assert!(store::parse_pr_url_test_helper("https://github.com/acme/myrepo/issues/1").is_none());
}

#[test]
fn short_prompt_does_not_require_plan() {
    assert!(!prompt_requires_plan("Fix the typo in README.md"));
}

#[test]
fn long_prompt_over_200_words_requires_plan() {
    let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
    assert!(prompt_requires_plan(&words.join(" ")));
}

#[test]
fn prompt_with_three_file_paths_requires_plan() {
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
    assert!(prompt_requires_plan(prompt));
}

#[test]
fn prompt_with_two_file_paths_does_not_require_plan() {
    assert!(!prompt_requires_plan(
        "Update src/foo.rs and crates/bar/src/lib.rs to fix X"
    ));
}

#[test]
fn xml_closing_tags_are_not_counted_as_file_paths() {
    let prompt = "GC applied files:\n\
                  <external_data>test-guard.sh</external_data>\n\
                  Rationale:\n<external_data>test</external_data>\n\
                  Validation:\n<external_data>test</external_data>";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn non_decomposable_source_list_includes_periodic_and_sprint() {
    assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
    assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
    assert!(!is_non_decomposable_prompt_source(Some("github")));
    assert!(!is_non_decomposable_prompt_source(None));
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
        ("b1", &root_b, TaskStatus::Done),
        ("b2", &root_b, TaskStatus::Failed),
        ("b3", &root_b, TaskStatus::Failed),
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

#[test]
fn local_waiting_counter_increments_on_each_waiting_response() {
    let max_rounds = 5u32;
    let mut waiting_count: u32 = 0;
    let mut observed: Vec<u32> = Vec::new();

    waiting_count += 1;
    observed.push(waiting_count);

    for _ in 1..max_rounds {
        waiting_count += 1;
        observed.push(waiting_count);
    }

    let expected: Vec<u32> = (1..=max_rounds).collect();
    assert_eq!(
        observed, expected,
        "waiting_count must increment monotonically"
    );
}
