use super::test_support::*;
use super::*;

#[tokio::test]
async fn pool_slot_reuse_preserves_existing_directory_for_same_workspace_identity() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let first_task = harness_core::types::TaskId("task-first".to_string());
    let second_task = harness_core::types::TaskId("task-second".to_string());

    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first workspace");
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "keep this file").expect("write marker");
    mgr.release_workspace(&first_task).await;

    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse deterministic workspace");

    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "same issue workspace identity should preserve non-terminal handoff state"
    );
    let owner = read_owner_record(&second.workspace_path).expect("owner record");
    assert_eq!(owner.task_id, "issue:42");
    let expected_owner_key = derive_workspace_key(
        &second_task,
        Some("issue:42"),
        Some("owner/repo"),
        Some(source.path()),
    );
    assert_eq!(
        owner.workspace_key.as_deref(),
        Some(expected_owner_key.as_str())
    );
    assert_eq!(
        owner
            .workspace_key
            .as_deref()
            .and_then(repo_slug_from_workspace_key)
            .as_deref(),
        Some("owner/repo")
    );

    mgr.remove_workspace(&second_task).await.expect("remove");
}

#[tokio::test]
async fn pool_slot_reuse_preserves_released_same_task_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("runtime-workflow-task".to_string());

    let first = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first workspace");
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "preserve this file").expect("write marker");
    mgr.release_workspace(&task_id).await;

    let second = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse released workflow workspace");

    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "same workflow workspace reuse must preserve non-terminal activity state"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn pool_slot_reuse_uses_workspace_identity_for_new_task_id() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new_with_pool(
        config,
        crate::workspace_pool::WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        None,
    )
    .expect("new");
    let blocker_task = harness_core::types::TaskId("slot-zero-blocker".to_string());
    let first_task = harness_core::types::TaskId("issue-task-old-internal-id".to_string());
    let second_task = harness_core::types::TaskId("issue-task-new-internal-id".to_string());

    let blocker = mgr
        .create_workspace(
            &blocker_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:7"),
            Some("owner/repo"),
        )
        .await
        .expect("create blocker workspace");
    assert_eq!(blocker.slot_index, 0);
    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first issue workspace");
    assert_eq!(first.slot_index, 1);
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "preserve this file").expect("write marker");
    mgr.release_workspace(&first_task).await;
    mgr.remove_workspace(&blocker_task)
        .await
        .expect("remove blocker so slot 0 is free");

    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse released issue workspace by workspace identity");

    assert_eq!(second.slot_index, 1);
    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "successor issue task should reuse the preserved workspace identity"
    );

    mgr.remove_workspace(&second_task).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_allocates_distinct_slots_for_concurrent_same_repo_tasks() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let first_task = harness_core::types::TaskId("task-first".to_string());
    let second_task = harness_core::types::TaskId("task-second".to_string());
    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("first task should acquire a pool slot");
    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:43"),
            Some("owner/repo"),
        )
        .await
        .expect("second task should acquire a different pool slot");

    assert_ne!(first.workspace_path, second.workspace_path);
    assert_eq!(first.slot_index, 0);
    assert_eq!(second.slot_index, 1);
    assert_eq!(mgr.live_count(), 2);

    mgr.remove_workspace(&first_task)
        .await
        .expect("remove first");
    mgr.remove_workspace(&second_task)
        .await
        .expect("remove second");
}

#[tokio::test]
async fn create_workspace_waits_when_project_pool_is_full() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(
        WorkspaceManager::new_with_pool(
            config,
            WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
            None,
        )
        .expect("new"),
    );
    let first_task = harness_core::types::TaskId("pool-full-first".to_string());
    let second_task = harness_core::types::TaskId("pool-full-second".to_string());

    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("first task should acquire the only slot");
    assert_eq!(first.slot_index, 0);

    let mgr_for_second = mgr.clone();
    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let second_handle = tokio::spawn(async move {
        mgr_for_second
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "second acquire should wait while the only slot is leased"
    );

    mgr.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")
        .expect("second task should acquire after release");
    assert_eq!(second.slot_index, 0);
    assert_eq!(second.decision, WorkspaceAcquireDecision::RecreatedStale);

    mgr.remove_workspace(&second_task)
        .await
        .expect("remove second");
}

#[tokio::test]
async fn create_workspace_enforces_project_capacity_across_repo_slugs() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(
        WorkspaceManager::new_with_pool(
            config,
            WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
            None,
        )
        .expect("new"),
    );
    let first_task = harness_core::types::TaskId("pool-repo-a".to_string());
    let second_task = harness_core::types::TaskId("pool-repo-b".to_string());

    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo-a"),
        )
        .await
        .expect("first repo should acquire the only project permit");
    assert_eq!(first.slot_index, 0);

    let mgr_for_second = mgr.clone();
    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let second_handle = tokio::spawn(async move {
        mgr_for_second
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo-b"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "repo-specific workspace keys must still share the source-project capacity"
    );

    mgr.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")
        .expect("second repo should acquire after project permit release");
    assert_eq!(second.slot_index, 0);
    assert_ne!(
        first.workspace_path, second.workspace_path,
        "repo slug remains part of the workspace slot path"
    );

    mgr.remove_workspace(&second_task)
        .await
        .expect("remove second");
}
