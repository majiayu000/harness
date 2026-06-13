use super::test_support::*;
use super::*;

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

#[tokio::test]
async fn workspace_lease_store_persists_and_releases_active_slots() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let task_id = harness_core::types::TaskId("lease-store-task".to_string());
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: task_id.clone(),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at,
    };

    assert!(
        store.try_acquire_lease(&record).await?,
        "initial lease should acquire an empty slot"
    );
    assert_eq!(store.list_leased().await?.len(), 1);
    assert_eq!(
        store.latest_workspace_path_for_task(&task_id).await?,
        Some(record.workspace_path.clone())
    );
    assert!(
        store
            .release_slot(&record.project_key, record.slot_index, &task_id)
            .await?,
        "release should update the active lease"
    );
    assert!(store.list_leased().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_shared_schema_keeps_data_dirs_isolated() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let shared_schema = unique_test_schema("workspace_lease_scope_test");
    let shared_context =
        harness_core::db::PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir)
        .await?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir)
        .await?;
    let store_a =
        WorkspaceLeaseStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir)
            .await?;
    let store_b =
        WorkspaceLeaseStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir)
            .await?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let record_a = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("store-a-task".to_string()),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/store-a"),
        source_repo: dir.path().join("repo-a"),
        repo: Some("owner/repo-a".to_string()),
        runtime_workflow_id: Some("workflow-a".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at,
    };
    let record_b = WorkspaceLeaseRecord {
        task_id: harness_core::types::TaskId("store-b-task".to_string()),
        workspace_key: "workspace-b".to_string(),
        workspace_path: dir.path().join("workspaces/store-b"),
        source_repo: dir.path().join("repo-b"),
        repo: Some("owner/repo-b".to_string()),
        runtime_workflow_id: Some("workflow-b".to_string()),
        owner_session: "session-b".to_string(),
        ..record_a.clone()
    };

    assert!(store_a.try_acquire_lease(&record_a).await?);
    assert!(
        store_b.try_acquire_lease(&record_b).await?,
        "same project slot should be isolated by store_key"
    );

    let leased_a = store_a.list_leased().await?;
    let leased_b = store_b.list_leased().await?;
    assert_eq!(leased_a.len(), 1);
    assert_eq!(leased_b.len(), 1);
    assert_eq!(leased_a[0].task_id, record_a.task_id);
    assert_eq!(leased_b[0].task_id, record_b.task_id);

    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_does_not_steal_live_foreign_slot() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let first_task = harness_core::types::TaskId("lease-store-first".to_string());
    let second_task = harness_core::types::TaskId("lease-store-second".to_string());
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let first_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: first_task,
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at,
    };
    let second_record = WorkspaceLeaseRecord {
        task_id: second_task,
        owner_session: "session-b".to_string(),
        runtime_workflow_id: Some("workflow-2".to_string()),
        ..first_record.clone()
    };

    assert!(store.try_acquire_lease(&first_record).await?);
    assert!(
        !store.try_acquire_lease(&second_record).await?,
        "foreign live lease must not be overwritten"
    );
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, first_record.task_id);
    assert_eq!(leased[0].owner_session, first_record.owner_session);

    assert!(
        store
            .release_slot(
                &first_record.project_key,
                first_record.slot_index,
                &first_record.task_id
            )
            .await?
    );
    assert!(
        store.try_acquire_lease(&second_record).await?,
        "released slots should be reusable"
    );

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_releases_only_dead_foreign_processes() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let live_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("live-foreign-task".to_string()),
        workspace_key: "workspace-live".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-live".to_string()),
        owner_session: "session-live".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at,
    };
    let dead_record = WorkspaceLeaseRecord {
        slot_index: 1,
        task_id: harness_core::types::TaskId("dead-foreign-task".to_string()),
        workspace_key: "workspace-dead".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-1"),
        runtime_workflow_id: Some("workflow-dead".to_string()),
        owner_session: "session-dead".to_string(),
        process_id: u32::MAX,
        process_started_at: 1,
        ..live_record.clone()
    };

    assert!(store.try_acquire_lease(&live_record).await?);
    assert!(store.try_acquire_lease(&dead_record).await?);

    let released = store
        .release_foreign_orphaned_leases("current-session")
        .await?;
    assert_eq!(released, 1);
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, live_record.task_id);

    Ok(())
}

#[tokio::test]
async fn release_workspace_family_releases_bounded_synthetic_subtask_leases() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        std::sync::Arc::new(WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?);
    let manager = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        },
        WorkspacePoolConfig::new(8, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let parent_task = harness_core::types::TaskId("lease-family-parent".to_string());
    let bounded_ids = [
        parent_task.clone(),
        crate::parallel_dispatch::sequential_subtask_id(&parent_task),
        crate::parallel_dispatch::parallel_subtask_id(&parent_task, 0),
        crate::parallel_dispatch::parallel_subtask_id(
            &parent_task,
            crate::parallel_dispatch::MAX_PARALLEL - 1,
        ),
    ];
    let unrelated_prefix_task = crate::parallel_dispatch::parallel_subtask_id(
        &parent_task,
        crate::parallel_dispatch::MAX_PARALLEL,
    );
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;

    for (slot_index, task_id) in bounded_ids
        .iter()
        .cloned()
        .chain(std::iter::once(unrelated_prefix_task.clone()))
        .enumerate()
    {
        let record = WorkspaceLeaseRecord {
            project_key: "project-a".to_string(),
            slot_index: u32::try_from(slot_index)?,
            task_id,
            workspace_key: format!("workspace-{slot_index}"),
            workspace_path: dir
                .path()
                .join(format!("workspaces/project-a-slot-{slot_index}")),
            source_repo: dir.path().join("repo"),
            repo: Some("owner/repo".to_string()),
            runtime_workflow_id: Some("workflow-1".to_string()),
            owner_session: "session-a".to_string(),
            run_generation: 1,
            process_id: std::process::id(),
            process_started_at,
        };
        assert!(store.try_acquire_lease(&record).await?);
    }

    manager.release_workspace_family(&parent_task).await;

    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, unrelated_prefix_task);

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_releases_pid_reuse_mismatch() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let current_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let stale_started_at = current_started_at.saturating_add(1);
    let record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("pid-reuse-task".to_string()),
        workspace_key: "workspace-reused-pid".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-reused-pid".to_string()),
        owner_session: "session-stale".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at: stale_started_at,
    };

    assert!(store.try_acquire_lease(&record).await?);
    let released = store
        .release_foreign_orphaned_leases("current-session")
        .await?;

    assert_eq!(released, 1);
    assert!(
        store.list_leased().await?.is_empty(),
        "same pid with different start time should be treated as stale"
    );

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_allocates_next_slot_without_stealing_live_foreign_lease(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let first_task = harness_core::types::TaskId("shared-store-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    let second = mgr_b
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:43"),
            Some("owner/repo"),
        )
        .await?;

    assert_eq!(first.slot_index, 0);
    assert_eq!(second.slot_index, 1);
    assert_ne!(first.workspace_path, second.workspace_path);
    assert_eq!(store.list_leased().await?.len(), 2);

    mgr_a.remove_workspace(&first_task).await?;
    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_waits_when_persisted_slots_are_full() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store.clone()),
    )?);
    let first_task = harness_core::types::TaskId("shared-store-full-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-full-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    assert_eq!(first.slot_index, 0);

    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let mgr_b_for_spawn = mgr_b.clone();
    let second_handle = tokio::spawn(async move {
        mgr_b_for_spawn
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
        "second manager should wait while the shared lease table is full"
    );

    mgr_a.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")?;
    assert_eq!(second.slot_index, 0);
    assert_eq!(second.workspace_path, first.workspace_path);

    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_enforces_project_capacity_across_repo_slugs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store.clone()),
    )?);
    let first_task = harness_core::types::TaskId("shared-cross-repo-first".to_string());
    let second_task = harness_core::types::TaskId("shared-cross-repo-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo-a"),
        )
        .await?;
    assert_eq!(first.slot_index, 0);

    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let mgr_b_for_spawn = mgr_b.clone();
    let second_handle = tokio::spawn(async move {
        mgr_b_for_spawn
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
        "second manager should wait for the shared source-project persisted slot even with a different repo slug"
    );

    mgr_a.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")?;
    assert_eq!(second.slot_index, 0);
    assert_eq!(
        first.project_key, second.project_key,
        "persisted lease capacity key should be source-project scoped"
    );
    assert_ne!(
        first.workspace_path, second.workspace_path,
        "repo slug remains part of the workspace slot path"
    );

    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn remove_workspace_releases_persisted_slot_after_cleanup_hook_finishes() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        before_remove_hook: Some("sh hold-remove.sh".to_string()),
        hook_timeout_secs: 5,
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("remove-holds-lease".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    std::fs::write(lease.workspace_path.join("hold-remove.sh"), "sleep 1\n")?;
    assert_eq!(store.list_leased().await?.len(), 1);

    let task_id_for_spawn = task_id.clone();
    let mgr_for_spawn = mgr.clone();
    let remove_handle =
        tokio::spawn(async move { mgr_for_spawn.remove_workspace(&task_id_for_spawn).await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while removal hook is still running"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), remove_handle)
        .await
        .expect("remove should finish")
        .expect("remove task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after cleanup completes"
    );

    Ok(())
}

#[tokio::test]
async fn cleanup_workspace_for_retry_releases_persisted_slot_after_cleanup_finishes(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        crate::workspace_pool::WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("retry-cleanup-holds-lease".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    assert_eq!(store.list_leased().await?.len(), 1);

    let git_ops_guard = mgr.git_ops.lock().await;
    let mgr_for_spawn = mgr.clone();
    let task_id_for_spawn = task_id.clone();
    let source_path = source.path().to_path_buf();
    let workspace_path = lease.workspace_path.clone();
    let cleanup_handle = tokio::spawn(async move {
        mgr_for_spawn
            .cleanup_workspace_for_retry(&task_id_for_spawn, &source_path, Some(&workspace_path))
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while retry cleanup is waiting"
    );

    drop(git_ops_guard);
    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup_handle)
        .await
        .expect("retry cleanup should finish")
        .expect("retry cleanup task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after retry cleanup finishes"
    );
    assert!(
        !lease.workspace_path.exists(),
        "retry cleanup should remove the workspace"
    );

    Ok(())
}
