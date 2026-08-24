use super::test_support::*;
use super::*;
use harness_core::db::TestSchemaGuard;

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
async fn runtime_cleanup_releases_only_the_exact_candidate_lease() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&dir.path().join("exact-candidate-release")).await?,
    );
    let manager = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let shared_task_id = harness_core::types::TaskId("issue-1300-c1".to_string());
    let record_a = WorkspaceLeaseRecord {
        project_key: "/repo/a".to_string(),
        slot_index: 0,
        task_id: shared_task_id.clone(),
        workspace_key: "repo-a-issue-1300".to_string(),
        workspace_path: dir.path().join("workspaces/repo-a-slot-0"),
        source_repo: dir.path().join("repo-a"),
        repo: Some("owner/repo-a".to_string()),
        runtime_workflow_id: Some("workflow-a".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at,
    };
    let record_b = WorkspaceLeaseRecord {
        project_key: "/repo/b".to_string(),
        workspace_key: "repo-b-issue-1300".to_string(),
        workspace_path: dir.path().join("workspaces/repo-b-slot-0"),
        source_repo: dir.path().join("repo-b"),
        repo: Some("owner/repo-b".to_string()),
        runtime_workflow_id: Some("workflow-b".to_string()),
        owner_session: "session-b".to_string(),
        ..record_a.clone()
    };
    assert!(store.try_acquire_lease(&record_a).await?);
    assert!(store.try_acquire_lease(&record_b).await?);

    let targets = manager
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets.len(), 1);
    manager
        .release_runtime_workspace_cleanup_target(&targets[0])
        .await?;

    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].project_key, record_b.project_key);
    assert_eq!(leased[0].task_id, shared_task_id);
    Ok(())
}

#[tokio::test]
async fn runtime_cleanup_does_not_remove_same_task_id_from_another_repository() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let repo_a = tempfile::tempdir()?;
    let repo_b = tempfile::tempdir()?;
    init_git_repo(repo_a.path());
    init_git_repo(repo_b.path());
    let branch_a = current_branch(repo_a.path());
    let branch_b = current_branch(repo_b.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("cross-repository-task-id")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let manager_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let manager_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let task_id = harness_core::types::TaskId("issue-1300-c1".to_string());
    let workspace_a = manager_a
        .create_workspace_with_options(
            &task_id,
            repo_a.path(),
            "origin",
            &branch_a,
            1,
            Some("issue:1300"),
            Some("owner/repo-a"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-a".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager_a.release_workspace(&task_id).await;
    let workspace_b = manager_b
        .create_workspace_with_options(
            &task_id,
            repo_b.path(),
            "origin",
            &branch_b,
            1,
            Some("issue:1300"),
            Some("owner/repo-b"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-b".to_string()),
                ..Default::default()
            },
        )
        .await?;

    let _repository_lease = manager_b
        .acquire_repository_write_lease_for_cleanup(repo_a.path())
        .await?;
    manager_b
        .cleanup_workspace_for_retry(&task_id, repo_a.path(), Some(&workspace_a.workspace_path))
        .await?;
    let targets = manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets.len(), 1);
    manager_b
        .release_runtime_workspace_cleanup_target(&targets[0])
        .await?;

    assert!(!workspace_a.workspace_path.exists());
    assert!(workspace_b.workspace_path.exists());
    assert!(manager_b.active.contains_key(&task_id));
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].runtime_workflow_id.as_deref(), Some("workflow-b"));
    manager_b.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn runtime_cleanup_target_survives_slot_reuse_with_a_different_repo_slug(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source_repo = tempfile::tempdir()?;
    init_git_repo(source_repo.path());
    let branch = current_branch(source_repo.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("slot-reuse-cleanup-history")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let manager_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let manager_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let task_a = harness_core::types::TaskId("issue-1300-a".to_string());
    let task_b = harness_core::types::TaskId("issue-1300-b".to_string());

    let workspace_a = manager_a
        .create_workspace_with_options(
            &task_a,
            source_repo.path(),
            "origin",
            &branch,
            1,
            Some("issue:1300"),
            Some("owner/repo-a"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-a".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager_a.release_workspace(&task_a).await;

    let workspace_b = manager_b
        .create_workspace_with_options(
            &task_b,
            source_repo.path(),
            "origin",
            &branch,
            1,
            Some("issue:1300"),
            Some("owner/repo-b"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-b".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager_b.release_workspace(&task_b).await;

    assert_ne!(workspace_a.workspace_path, workspace_b.workspace_path);
    let _repository_lease = manager_b
        .acquire_repository_write_lease_for_cleanup(source_repo.path())
        .await?;
    let targets_a = manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets_a.len(), 1);
    assert_eq!(targets_a[0].workspace_path, workspace_a.workspace_path);
    assert!(
        !manager_b
            .runtime_workspace_cleanup_target_is_superseded(&targets_a[0])
            .await?
    );
    manager_b
        .cleanup_workspace_for_retry(
            &targets_a[0].task_id,
            source_repo.path(),
            Some(&targets_a[0].workspace_path),
        )
        .await?;
    manager_b
        .release_runtime_workspace_cleanup_target(&targets_a[0])
        .await?;

    assert!(!workspace_a.workspace_path.exists());
    assert!(workspace_b.workspace_path.exists());
    assert!(manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?
        .is_empty());
    assert_eq!(
        manager_b
            .workspace_targets_for_runtime_workflow("workflow-b")
            .await?
            .len(),
        1
    );
    let current = store
        .current_workspace_lease_for_slot(
            &crate::workspace_pool::project_limit_key(source_repo.path()),
            0,
        )
        .await?
        .expect("workflow B should retain the current slot record");
    assert_eq!(current.runtime_workflow_id.as_deref(), Some("workflow-b"));
    assert_eq!(current.workspace_path, workspace_b.workspace_path);
    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_shared_schema_keeps_data_dirs_isolated() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let shared_context =
        harness_core::db::PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
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

    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn legacy_backfill_copies_runtime_workspace_cleanup_targets() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let legacy_path = dir.path().join("legacy-task-db");
    let _legacy_db =
        crate::task_db::TaskDb::open_with_database_url(&legacy_path, Some(database_url.as_str()))
            .await?;
    let legacy_store = WorkspaceLeaseStore::open(&legacy_path).await?;
    let record = WorkspaceLeaseRecord {
        project_key: "/repo/legacy".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("legacy-runtime-task".to_string()),
        workspace_key: "legacy-runtime-workspace".to_string(),
        workspace_path: dir.path().join("workspaces/legacy-runtime-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/legacy-repo".to_string()),
        runtime_workflow_id: Some("legacy-runtime-workflow".to_string()),
        owner_session: "legacy-session".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(legacy_store.try_acquire_lease(&record).await?);

    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "cleanup_target_backfill_test")?;
    let shared_context =
        harness_core::db::PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
    let shared_data_dir = dir.path().join("shared-data");
    std::fs::create_dir_all(&shared_data_dir)?;
    let shared_db = crate::task_db::TaskDb::open_shared_with_data_dir(
        &shared_context,
        &setup_pool,
        &shared_data_dir,
    )
    .await?;
    crate::task_db::migrate_legacy_task_db_if_needed(
        &legacy_path,
        Some(database_url.as_str()),
        &shared_db,
    )
    .await?;
    let shared_store = WorkspaceLeaseStore::open_shared_with_data_dir(
        &shared_context,
        &setup_pool,
        &shared_data_dir,
    )
    .await?;

    let targets = shared_store
        .workspace_cleanup_targets_for_runtime_workflow("legacy-runtime-workflow")
        .await?;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].workspace_path, record.workspace_path);

    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn repository_lease_modes_are_global_across_postgres_schemas() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut schema_a = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let mut schema_b = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let context_a =
        harness_core::db::PgStoreContext::from_schema(schema_a.schema(), Some(&database_url))?;
    let context_b =
        harness_core::db::PgStoreContext::from_schema(schema_b.schema(), Some(&database_url))?;
    let data_a = dir.path().join("instance-a");
    let data_b = dir.path().join("instance-b");
    std::fs::create_dir_all(&data_a)?;
    std::fs::create_dir_all(&data_b)?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&context_a, &setup_pool, &data_a).await?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&context_b, &setup_pool, &data_b).await?;
    let store_a =
        WorkspaceLeaseStore::open_shared_with_data_dir(&context_a, &setup_pool, &data_a).await?;
    let store_b =
        WorkspaceLeaseStore::open_shared_with_data_dir(&context_b, &setup_pool, &data_b).await?;

    let lease_a = store_a
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("first Harness instance should acquire the repository write lease");
    assert!(
        store_b
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "a second Harness instance in another schema must not acquire the same repository"
    );
    drop(lease_a);
    let lease_b = store_b
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("lease should be released when the owning connection is dropped");
    drop(lease_b);

    let shared_a = store_a
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("the first multi-writer should acquire a shared repository lease");
    let shared_b = store_b
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("a second multi-writer should share the repository lease");
    assert!(
        store_b
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "an N-to-1 transition must wait for every shared writer"
    );
    drop(shared_a);
    assert!(
        store_a
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "an exclusive writer must keep waiting while one shared writer remains"
    );
    drop(shared_b);
    let exclusive = store_a
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("the single-writer transition should acquire after shared writers drain");
    assert!(
        store_b
            .try_acquire_repository_shared_lease("/repo/owner/project")
            .await?
            .is_none(),
        "a 1-to-N transition must wait for the exclusive writer"
    );
    drop(exclusive);
    let shared_after_transition = store_b
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("shared writers should resume after the exclusive writer drains");
    drop(shared_after_transition);

    schema_a.cleanup_with_pool(&setup_pool).await?;
    schema_b.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_waits_when_dedicated_pool_is_full() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("repository-lock-pool")).await?;
    let pool_capacity = store.repository_lock_pool_capacity();
    let mut held_leases = Vec::with_capacity(pool_capacity as usize);
    for index in 0..pool_capacity {
        let lease = store
            .try_acquire_repository_write_lease(&format!("/repo/owner/held-{index}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("repository lease {index} should acquire"))?;
        held_leases.push(lease);
    }
    let second = store.try_acquire_repository_write_lease_for_test(
        "/repo/owner/second",
        std::time::Duration::from_millis(100),
    );
    tokio::pin!(second);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), &mut second)
            .await
            .is_err(),
        "pool exhaustion must keep waiting instead of returning a timeout error"
    );
    held_leases.pop();
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), &mut second)
        .await
        .map_err(|_| anyhow::anyhow!("waiting repository lease did not resume"))??
        .ok_or_else(|| anyhow::anyhow!("unrelated repository lease should acquire"))?;
    drop(second);
    drop(held_leases);
    Ok(())
}

#[tokio::test]
async fn workspace_creation_accepts_preacquired_repository_write_lease() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let mgr = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let task_id = harness_core::types::TaskId("preacquired-repository-lease".to_string());
    let repository_write_lease = mgr.acquire_repository_write_lease(source.path()).await?;

    let workspace = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        mgr.create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:1302"),
            Some("owner/repo"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                repository_write_lease: RepositoryWriteLeaseInput::Held(repository_write_lease),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("workspace creation tried to acquire the repository lease twice")
    })??;
    assert!(workspace.workspace_path.exists());
    mgr.remove_workspace(&task_id).await?;

    let reused_task_id = harness_core::types::TaskId("attach-preacquired-lease".to_string());
    mgr.create_workspace_with_options(
        &reused_task_id,
        source.path(),
        "origin",
        &branch,
        1,
        Some("issue:1303"),
        Some("owner/repo"),
        WorkspaceCreateOptions {
            require_remote_head: false,
            repository_write_lease: RepositoryWriteLeaseInput::NotRequired,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        mgr.active
            .get(&reused_task_id)
            .is_some_and(|active| active._repository_write_lease.is_none()),
        "the multi-writer workspace should start without a repository lease"
    );
    let repository_write_lease = mgr.acquire_repository_write_lease(source.path()).await?;
    mgr.create_workspace_with_options(
        &reused_task_id,
        source.path(),
        "origin",
        &branch,
        1,
        Some("issue:1303"),
        Some("owner/repo"),
        WorkspaceCreateOptions {
            require_remote_head: false,
            repository_write_lease: RepositoryWriteLeaseInput::Held(repository_write_lease),
            ..Default::default()
        },
    )
    .await?;
    assert!(
        mgr.active
            .get(&reused_task_id)
            .is_some_and(|active| active._repository_write_lease.is_some()),
        "a live capacity change to single-writer must attach the preacquired lease to a reused workspace"
    );
    let project_key = crate::workspace_pool::project_limit_key(source.path());
    assert!(
        store
            .try_acquire_repository_write_lease(&project_key)
            .await?
            .is_none(),
        "the reused workspace must retain the attached repository lease"
    );
    mgr.remove_workspace(&reused_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn cross_manager_reconciliation_waits_for_repository_writer() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("reconcile-race")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let manager_a = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config.clone(),
        pool_config.clone(),
        Some(store.clone()),
    )?);
    let manager_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store),
    )?);
    let task_id = harness_core::types::TaskId("reconcile-race-writer".to_string());
    let workspace = manager_a
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:1304"),
            Some("owner/repo"),
        )
        .await?;

    let manager_b_for_cleanup = manager_b.clone();
    let source_path = source.path().to_path_buf();
    let workspace_path = workspace.workspace_path.clone();
    let cleanup = tokio::spawn(async move {
        manager_b_for_cleanup
            .cleanup_reconciliation_workspace_path(&source_path, &workspace_path)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !cleanup.is_finished(),
        "reconciliation must wait while another manager holds a shared writer lease"
    );
    assert!(workspace.workspace_path.exists());

    manager_a.release_workspace(&task_id).await;
    let removed = tokio::time::timeout(std::time::Duration::from_secs(5), cleanup)
        .await
        .map_err(|_| anyhow::anyhow!("reconciliation did not resume after writer release"))???;
    assert!(removed);
    assert!(!workspace.workspace_path.exists());
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_propagates_sqlx_pool_timeout() -> anyhow::Result<()> {
    let unavailable_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://postgres@127.0.0.1:1/harness_test")?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(unavailable_pool);

    let result = store
        .try_acquire_repository_write_lease_for_test(
            "/repo/owner/unavailable",
            std::time::Duration::from_millis(200),
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => {
            anyhow::bail!("database connectivity timeout must not be retried as pool saturation")
        }
    };
    assert!(
        error.to_string().contains("timed out acquiring"),
        "unexpected database timeout error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_propagates_outer_acquire_timeout() -> anyhow::Result<()> {
    let unavailable_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(1))
        .connect_lazy("postgres://postgres@127.0.0.1:1/harness_test")?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(unavailable_pool);

    let result = store
        .try_acquire_repository_write_lease_for_test(
            "/repo/owner/unavailable",
            std::time::Duration::from_millis(50),
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => anyhow::bail!("outer connectivity timeout must not be retried as pool saturation"),
    };
    assert!(
        error.to_string().contains("timed out acquiring"),
        "unexpected outer database timeout error: {error}"
    );
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
