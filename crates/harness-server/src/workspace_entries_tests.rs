use super::*;
use harness_core::types::TaskId;

#[test]
fn workspace_entries_return_active_workspace_metadata_sorted_by_task_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspaces");
    let mgr = WorkspaceManager::new(WorkspaceConfig {
        root: root.clone(),
        ..Default::default()
    })
    .expect("new");
    let created_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(42);

    for task_id in ["task-b", "task-a"] {
        let task_id = TaskId(task_id.to_string());
        let workspace_path = root.join(&task_id.0);
        let source_repo = tmp.path().join("repo");
        mgr.active.insert(
            task_id.clone(),
            ActiveWorkspace {
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.clone(),
                repo: Some("owner/repo".to_string()),
                runtime_workflow_id: Some("workflow-1".to_string()),
                workspace_key: format!("workspace-key-{}", task_id.0),
                project_key: "test-project".to_string(),
                slot_index: 0,
                branch: format!("harness/{}", task_id.0),
                created_at,
                owner_session: mgr.owner_session.clone(),
                run_generation: 1,
                acquisition_id: "test-acquisition".to_string(),
                state: ActiveWorkspaceState::Ready,
                _pool_permit: None,
                _repository_write_lease: None,
            },
        );
        mgr.active_paths.insert(workspace_path, task_id);
    }

    let entries = mgr.entries();

    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.task_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["task-a", "task-b"]
    );
    assert_eq!(entries[0].workspace_path, root.join("task-a"));
    assert_eq!(entries[0].source_repo, tmp.path().join("repo"));
    assert_eq!(entries[0].repo.as_deref(), Some("owner/repo"));
    assert_eq!(
        entries[0].runtime_workflow_id.as_deref(),
        Some("workflow-1")
    );
    assert_eq!(entries[0].branch, "harness/task-a");
    assert_eq!(entries[0].created_at, created_at);
}

#[tokio::test]
async fn cancelled_preparation_marks_workspace_cleanup_required() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_repo = tmp.path().join("repo");
    std::fs::create_dir_all(&source_repo).expect("source repo dir");
    super::test_support::init_git_repo(&source_repo);
    let workspace_path = tmp.path().join("workspaces/task-a");
    std::fs::create_dir_all(&workspace_path).expect("workspace dir");
    let workflow_hook_marker = tmp.path().join("workflow-before-remove");
    let manager_hook_marker = tmp.path().join("manager-before-remove");
    let mgr = WorkspaceManager::new(WorkspaceConfig {
        root: tmp.path().join("workspaces"),
        before_remove_hook: Some(format!("touch {}", manager_hook_marker.display())),
        hook_timeout_secs: 2,
        ..Default::default()
    })
    .expect("manager");
    let pool = Arc::new(tokio::sync::Semaphore::new(1));
    let pool_permit = Arc::clone(&pool).try_acquire_owned().expect("pool permit");
    let task_id = TaskId("task-a".to_string());
    mgr.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path,
            source_repo,
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "workspace-key".to_string(),
            project_key: "project-key".to_string(),
            slot_index: 0,
            branch: "harness/task-a".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: mgr.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "acquisition-a".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: Some(pool_permit),
            _repository_write_lease: None,
        },
    );

    let guard = mgr
        .begin_workspace_preparation(
            &task_id,
            "acquisition-a",
            Some(format!("touch {}", workflow_hook_marker.display())),
            2,
        )
        .expect("begin preparation");
    assert_eq!(
        mgr.active.get(&task_id).expect("active").state,
        ActiveWorkspaceState::Preparing
    );
    drop(guard);
    assert_eq!(pool.available_permits(), 0);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while mgr.active.contains_key(&task_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled preparation cleanup should converge");
    assert_eq!(pool.available_permits(), 1);
    assert!(!tmp.path().join("workspaces/task-a").exists());
    assert!(workflow_hook_marker.exists());
    assert!(manager_hook_marker.exists());
}

#[tokio::test]
async fn cancellation_and_retry_serialize_before_remove_hook_without_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_repo = tmp.path().join("repo");
    std::fs::create_dir_all(&source_repo).expect("source repo dir");
    super::test_support::init_git_repo(&source_repo);
    let workspace_path = tmp.path().join("workspaces/task-serialized-hook");
    std::fs::create_dir_all(&workspace_path).expect("workspace dir");
    let hook_log = tmp.path().join("hook-log");
    let hook_release = tmp.path().join("hook-release");
    std::fs::write(
        workspace_path.join("cancellation-hook.sh"),
        format!(
            "printf 'start\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
            hook_log.display(),
            hook_release.display()
        ),
    )
    .expect("hook script");
    let mgr = Arc::new(
        WorkspaceManager::new(WorkspaceConfig {
            root: tmp.path().join("workspaces"),
            ..Default::default()
        })
        .expect("manager"),
    );
    let task_id = TaskId("task-serialized-hook".to_string());
    mgr.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path,
            source_repo,
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "workspace-key".to_string(),
            project_key: "project-key".to_string(),
            slot_index: 0,
            branch: "harness/task-serialized-hook".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: mgr.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "acquisition-a".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        },
    );

    let guard = mgr
        .begin_workspace_preparation(
            &task_id,
            "acquisition-a",
            Some("sh cancellation-hook.sh".to_string()),
            5,
        )
        .expect("begin preparation");
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !hook_log.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancellation hook should start");

    let retry_manager = Arc::clone(&mgr);
    let retry_task_id = task_id.clone();
    let retry = tokio::spawn(async move {
        retry_manager
            .cleanup_required_workspace_for_retry(
                &retry_task_id,
                Some("sh cancellation-hook.sh"),
                5,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!retry.is_finished());
    assert_eq!(
        std::fs::read_to_string(&hook_log)
            .expect("hook log")
            .lines()
            .count(),
        1
    );

    std::fs::write(&hook_release, "release").expect("release hook");
    retry.await.expect("retry join").expect("retry cleanup");
    assert_eq!(
        std::fs::read_to_string(&hook_log)
            .expect("hook log")
            .lines()
            .count(),
        1,
        "the same acquisition hook must not run twice"
    );
    assert!(!mgr.active.contains_key(&task_id));
}

#[test]
fn retry_cleanup_treats_disappeared_acquisition_as_converged() {
    assert!(
        !workspace_active_reuse::retry_cleanup_target_is_current(None, "acquisition-a")
            .expect("a missing acquisition is already converged")
    );
    assert!(workspace_active_reuse::retry_cleanup_target_is_current(
        Some(("acquisition-a", &ActiveWorkspaceState::CleanupRequired)),
        "acquisition-a",
    )
    .expect("the matching cleanup target remains current"));
    assert!(workspace_active_reuse::retry_cleanup_target_is_current(
        Some(("acquisition-b", &ActiveWorkspaceState::CleanupRequired)),
        "acquisition-a",
    )
    .is_err());
}

#[tokio::test]
async fn cleanup_serialization_is_scoped_to_acquisition() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manager = WorkspaceManager::new(WorkspaceConfig {
        root: tmp.path().join("workspaces"),
        ..Default::default()
    })
    .expect("manager");
    let first = manager.workspace_cleanup_operation("acquisition-a");
    let same = manager.workspace_cleanup_operation("acquisition-a");
    let independent = manager.workspace_cleanup_operation("acquisition-b");

    assert!(Arc::ptr_eq(&first, &same));
    manager
        .run_workspace_cleanup_hooks_once(
            Some(&first),
            &TaskId("task-a".to_string()),
            None,
            None,
            1,
            tmp.path(),
        )
        .await
        .expect("hooks without durable cleanup state");
    assert!(first.claim_workflow_hook());
    assert!(!same.claim_workflow_hook());
    assert!(first.claim_manager_hook());
    assert!(!same.claim_manager_hook());
    let _first_guard = first.lock.lock().await;
    assert!(same.lock.try_lock().is_err());
    assert!(independent.lock.try_lock().is_ok());
}

#[test]
fn cancelled_cleanup_retry_delay_is_bounded() {
    assert_eq!(
        workspace_active_reuse::cancelled_cleanup_retry_delay(1),
        std::time::Duration::from_millis(250)
    );
    assert_eq!(
        workspace_active_reuse::cancelled_cleanup_retry_delay(u64::MAX),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn execution_ownership_blocks_reuse_and_stale_finalization() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_path = tmp.path().join("workspaces/task-a");
    std::fs::create_dir_all(&workspace_path).expect("workspace dir");
    let mgr = Arc::new(
        WorkspaceManager::new(WorkspaceConfig {
            root: tmp.path().join("workspaces"),
            ..Default::default()
        })
        .expect("manager"),
    );
    let task_id = TaskId("task-a".to_string());
    mgr.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path,
            source_repo: tmp.path().join("repo"),
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "workspace-key".to_string(),
            project_key: "project-key".to_string(),
            slot_index: 0,
            branch: "harness/task-a".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: mgr.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "acquisition-a".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        },
    );

    let execution = mgr
        .claim_workspace_execution(&task_id, "acquisition-a", None, 0)
        .expect("claim execution");
    assert!(mgr
        .claim_workspace_execution(&task_id, "acquisition-a", None, 0)
        .is_err());
    assert!(mgr
        .begin_workspace_preparation(&task_id, "acquisition-a", None, 0)
        .is_err());
    assert!(mgr
        .begin_workspace_finalization(&task_id, "acquisition-a", "stale-execution")
        .is_err());
    assert!(matches!(
        &mgr.active.get(&task_id).expect("active").state,
        ActiveWorkspaceState::Running(id) if id == execution.execution_id()
    ));

    mgr.begin_workspace_finalization(&task_id, "acquisition-a", execution.execution_id())
        .expect("begin finalization");
    drop(execution);
    assert_eq!(
        mgr.active.get(&task_id).expect("active").state,
        ActiveWorkspaceState::CleanupRequired
    );
}

#[tokio::test]
async fn cancelled_execution_preserves_workflow_cleanup_hook() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_repo = tmp.path().join("repo");
    std::fs::create_dir_all(&source_repo).expect("source repo dir");
    super::test_support::init_git_repo(&source_repo);
    let workspace_path = tmp.path().join("workspaces/task-execution-hook");
    std::fs::create_dir_all(&workspace_path).expect("workspace dir");
    let hook_marker = tmp.path().join("execution-before-remove");
    let mgr = Arc::new(
        WorkspaceManager::new(WorkspaceConfig {
            root: tmp.path().join("workspaces"),
            ..Default::default()
        })
        .expect("manager"),
    );
    let task_id = TaskId("task-execution-hook".to_string());
    mgr.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path,
            source_repo,
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "execution-hook-key".to_string(),
            project_key: "execution-hook-project".to_string(),
            slot_index: 0,
            branch: "harness/task-execution-hook".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: mgr.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "execution-hook-acquisition".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        },
    );

    let execution = mgr
        .claim_workspace_execution(
            &task_id,
            "execution-hook-acquisition",
            Some(format!("touch {}", hook_marker.display())),
            2,
        )
        .expect("claim execution");
    drop(execution);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while mgr.active.contains_key(&task_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled execution cleanup should converge");

    assert!(hook_marker.exists());
}

#[tokio::test]
async fn active_workspace_outside_reduced_capacity_is_not_reused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_path = tmp.path().join("workspaces/task-a");
    std::fs::create_dir_all(&workspace_path).expect("workspace dir");
    let mgr = WorkspaceManager::new(WorkspaceConfig {
        root: tmp.path().join("workspaces"),
        ..Default::default()
    })
    .expect("manager");
    let task_id = TaskId("task-a".to_string());
    mgr.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path,
            source_repo: tmp.path().join("repo"),
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "workspace-key".to_string(),
            project_key: "project-key".to_string(),
            slot_index: 1,
            branch: "harness/task-a".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: mgr.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "acquisition-a".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        },
    );

    let error = mgr
        .try_reuse_active_workspace(&task_id, 1, 1, &mut None)
        .await
        .expect_err("out-of-range slot must not be reused");

    assert!(error.to_string().contains("reduced capacity"));
    assert_eq!(
        mgr.active.get(&task_id).expect("active").state,
        ActiveWorkspaceState::CleanupRequired
    );
}
